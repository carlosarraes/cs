//! Implementations of the subcommands: add, switch, del, list, whoami, usage.

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::io::{self, IsTerminal, Write};

use crate::state::{self, Account, ProviderState, State};
use crate::usage::{self, Observation};
use crate::{claude, config, policy};

/// Aliases with special meaning to `switch` that can't be saved as accounts.
const RESERVED: &[&str] = &["-", "next"];

fn validate_alias(alias: &str) -> Result<()> {
    let ok = !alias.is_empty()
        && !RESERVED.contains(&alias)
        && alias
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(anyhow!(
            "invalid alias '{alias}': use letters, digits, '.', '_', '-' (not `-`/`next`)"
        ))
    }
}

/// Outcome of re-capturing the live credentials into a saved alias.
#[derive(Debug, PartialEq)]
enum Refreshed {
    Wrote,
    /// Nothing live to capture, or the alias is not saved.
    Skipped,
    /// The live login is a different account, so the alias was left alone.
    Mismatch {
        live: String,
        saved: String,
    },
}

/// Snapshot the live account into `ps.accounts[alias]` (no-op if there is no
/// live account, so it is safe to call before destructive ops).
fn refresh_snapshot(ps: &mut ProviderState, alias: &str) -> Refreshed {
    match claude::capture() {
        Ok((email, data)) => apply_snapshot(ps, alias, &email, data),
        Err(_) => Refreshed::Skipped,
    }
}

/// Write captured credentials into `alias`, but only when they belong to the
/// same account. Logging in as someone else (Claude now forces a fresh login
/// every 7 days) must not silently repoint an alias at another account.
fn apply_snapshot(ps: &mut ProviderState, alias: &str, email: &str, data: Value) -> Refreshed {
    let Some(acct) = ps.accounts.get_mut(alias) else {
        return Refreshed::Skipped;
    };
    if acct.email != email && acct.email != "unknown" {
        return Refreshed::Mismatch {
            live: email.to_string(),
            saved: acct.email.clone(),
        };
    }
    acct.email = email.to_string();
    acct.data = data;
    Refreshed::Wrote
}

/// Whether the live credential is newer than the one saved under an alias.
/// `stored` is a saved account's `data`; `live` is the live `claudeAiOauth`.
fn live_is_newer(stored: &Value, live: &Value) -> bool {
    let expires_at = |v: &Value| v.get("expiresAt").and_then(Value::as_i64);
    match (
        stored.get("claudeAiOauth").and_then(expires_at),
        expires_at(live),
    ) {
        (Some(saved), Some(live)) => live > saved,
        (None, Some(_)) => true,
        _ => false,
    }
}

/// Resolve a requested switch target: `-` means the previous alias.
fn resolve_target(ps: &ProviderState, requested: &str) -> Result<String> {
    if requested == "-" {
        ps.previous
            .clone()
            .ok_or_else(|| anyhow!("no previous account to switch to"))
    } else {
        Ok(requested.to_string())
    }
}

/// Update `current`/`previous` after a switch to `target` — the outgoing alias
/// becomes `previous`, so `switch -` toggles back (the caller handles no-ops).
fn record_switch(ps: &mut ProviderState, target: &str) {
    ps.previous = ps.current.take();
    ps.current = Some(target.to_string());
}

pub fn add(alias: &str, current: bool, force: bool) -> Result<()> {
    validate_alias(alias)?;
    let _lock = state::lock()?;
    let mut state = State::load()?;
    if state.claude().accounts.contains_key(alias) && !force {
        return Err(anyhow!(
            "alias '{alias}' already exists (run `cs refresh` to update its saved \
             credentials, `--force` to overwrite it, or `cs del {alias}` first)"
        ));
    }

    if !current {
        // Keep the outgoing account's snapshot fresh before login overwrites
        // the live credentials.
        if let Some(cur) = state.claude().current.clone() {
            refresh_snapshot(state.claude_mut(), &cur);
        }
        println!("Launching claude login for '{alias}'…");
        claude::login()?;
    }

    let (email, data) = claude::capture().context(if current {
        "no active account to snapshot (log in first, or drop --current)"
    } else {
        "could not read the account after login"
    })?;

    let ps = state.claude_mut();
    if ps.current.as_deref() != Some(alias) {
        ps.previous = ps.current.take();
    }
    ps.current = Some(alias.to_string());
    ps.accounts.insert(
        alias.to_string(),
        Account {
            email: email.clone(),
            data,
        },
    );
    state.save()?;
    println!("Saved account '{alias}' ({email}) and set it active.");
    Ok(())
}

pub fn switch(requested: &str, yes: bool) -> Result<()> {
    let _lock = state::lock()?;
    let mut state = State::load()?;
    let target = if requested == "next" {
        next_target(state.claude())?
    } else {
        resolve_target(state.claude(), requested)?
    };

    if !state.claude().accounts.contains_key(&target) {
        return Err(anyhow!("unknown alias '{target}' (see `cs list`)"));
    }
    if state.claude().current.as_deref() == Some(target.as_str()) {
        println!("Already on '{target}'.");
        return Ok(());
    }

    if !yes && !confirm_if_running()? {
        println!("Aborted.");
        return Ok(());
    }

    let email = perform_switch(&mut state, &target)?;
    println!("Switched to '{target}' ({email}).");
    Ok(())
}

/// The least-used (5h) other account, per the configured ceiling.
fn next_target(ps: &ProviderState) -> Result<String> {
    let cfg = config::load()?;
    // Fresh numbers for everyone (idle 0), but still honoring any 429 backoff.
    let obs = usage::observe(ps, Observation::load()?.as_ref(), usage::now(), 0);
    let current = ps.current.as_deref();
    policy::pick_target(&obs, cfg.auto_switcher.ceiling, current)
        .or_else(|| policy::pick_expired_fallback(&obs, current))
        .map(String::from)
        .ok_or_else(|| {
            anyhow!(
                "no other account is known and below {}%:\n{}",
                cfg.auto_switcher.ceiling,
                usage::format_lines(&obs, usage::now()).join("\n")
            )
        })
}

/// Re-capture the outgoing account, make `target` live, and record the switch.
/// The caller holds `state::lock()` and has validated `target`. Returns the
/// target's email.
pub fn perform_switch(state: &mut State, target: &str) -> Result<String> {
    // Re-capture the outgoing account so rotated refresh tokens aren't lost.
    if let Some(cur) = state.claude().current.clone() {
        if let Refreshed::Mismatch { live, saved } = refresh_snapshot(state.claude_mut(), &cur) {
            eprintln!(
                "⚠ live login is {live} but alias '{cur}' is {saved}; leaving its saved \
                 credentials alone (run `cs refresh` to resync)."
            );
        }
    }
    let account = state
        .claude()
        .accounts
        .get(target)
        .ok_or_else(|| anyhow!("unknown alias '{target}'"))?
        .clone();
    claude::restore(&account.data).with_context(|| format!("switching to '{target}'"))?;
    record_switch(state.claude_mut(), target);
    state.save()?;
    Ok(account.email)
}

/// Returns whether the switch should proceed. If Claude Code is running, warn;
/// when attached to a terminal, ask for confirmation.
fn confirm_if_running() -> Result<bool> {
    let pids = claude::running_pids();
    if pids.is_empty() {
        return Ok(true);
    }
    let list = pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!("⚠ Claude Code appears to be running (pid {list}). Switching now may disrupt it.");
    if !io::stdin().is_terminal() {
        eprintln!("(continuing; pass --yes to silence this)");
        return Ok(true);
    }
    eprint!("Switch anyway? [y/N] ");
    io::stderr().flush().ok();
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

pub fn del(alias: &str) -> Result<()> {
    let _lock = state::lock()?;
    let mut state = State::load()?;
    let ps = state.claude_mut();
    if ps.accounts.remove(alias).is_none() {
        return Err(anyhow!("unknown alias '{alias}'"));
    }
    if ps.current.as_deref() == Some(alias) {
        ps.current = None;
    }
    if ps.previous.as_deref() == Some(alias) {
        ps.previous = None;
    }
    state.save()?;
    println!("Deleted alias '{alias}'. (Live credentials were not changed.)");
    Ok(())
}

pub fn list() -> Result<()> {
    let state = State::load()?;
    let ps = state.claude();
    if ps.accounts.is_empty() {
        println!("No accounts saved yet. Add one with `cs add <alias>`.");
        return Ok(());
    }
    for (alias, acct) in &ps.accounts {
        let marker = if ps.current.as_deref() == Some(alias) {
            '*'
        } else if ps.previous.as_deref() == Some(alias) {
            '-'
        } else {
            ' '
        };
        println!("{marker} {alias:<16} {}", acct.email);
    }
    Ok(())
}

/// Show the live signed-in account, with the matching alias.
pub fn whoami() -> Result<()> {
    let state = State::load()?;
    let ps = state.claude();
    let line = match claude::live_email() {
        Ok(Some(email)) => {
            let tag = matched_alias(ps, &email)
                .map(|a| format!("({a})"))
                .unwrap_or_else(|| "(not saved)".to_string());
            format!("{email:<30} {tag}")
        }
        Ok(None) => "— not signed in".to_string(),
        Err(e) => format!("— {e}"),
    };
    println!("{line}");
    Ok(())
}

/// Re-capture the live credentials into the alias they belong to. The alias is
/// matched by email, so this can never write one account's tokens into another
/// account's alias. Also fixes `current` when the live login drifted (a `/login`
/// run outside cs).
pub fn refresh() -> Result<()> {
    let _lock = state::lock()?;
    let mut state = State::load()?;
    let (email, data) = claude::capture()
        .context("no active Claude login to capture (run `claude auth login` first)")?;
    let ps = state.claude_mut();
    let alias = matched_alias(ps, &email).ok_or_else(|| {
        anyhow!("live account {email} is not saved yet (run `cs add <alias> --current`)")
    })?;
    if apply_snapshot(ps, &alias, &email, data) != Refreshed::Wrote {
        return Err(anyhow!("could not refresh '{alias}'"));
    }
    let moved = ps.current.as_deref() != Some(alias.as_str());
    if moved {
        record_switch(ps, &alias);
    }
    state.save()?;
    println!("Refreshed '{alias}' ({email}).");
    if moved {
        println!("It is now the current account (the live login had drifted).");
    }
    Ok(())
}

/// Keep the current alias's saved credentials fresh while it stays active.
/// Claude rotates the live token every few hours, and a saved copy left behind
/// can expire outright, so the daemon re-captures it whenever the live one is
/// newer. Returns the alias when it wrote. A live login belonging to a different
/// account is left to the daemon's own identity warning.
pub fn refresh_current_if_stale() -> Result<Option<String>> {
    let state = State::load()?;
    let Some(alias) = state.claude().current.clone() else {
        return Ok(None);
    };
    let Some(acct) = state.claude().accounts.get(&alias) else {
        return Ok(None);
    };
    let Some(live) = claude::live_oauth()? else {
        return Ok(None);
    };
    if !live_is_newer(&acct.data, &live) {
        return Ok(None);
    }
    let _lock = state::lock()?;
    let mut state = State::load()?;
    if refresh_snapshot(state.claude_mut(), &alias) != Refreshed::Wrote {
        return Ok(None);
    }
    state.save()?;
    Ok(Some(alias))
}

/// Show every Claude account's 5h/7d usage — from the daemon's last observation,
/// or polled right now with `--live`.
pub fn usage(live: bool) -> Result<()> {
    let now = usage::now();
    let obs = if live {
        let state = State::load()?;
        let ps = state.claude();
        if ps.accounts.is_empty() {
            return Err(anyhow!("no accounts saved yet (see `cs add`)"));
        }
        let obs = usage::observe(ps, Observation::load()?.as_ref(), now, 0);
        obs.save()?;
        obs
    } else {
        Observation::load()?
            .ok_or_else(|| anyhow!("no observation yet — run `cs start` (or `cs usage --live`)"))?
    };
    for line in usage::format_lines(&obs, now) {
        println!("{line}");
    }
    if !live {
        println!(
            "observed {} ago",
            usage::fmt_duration(now - obs.observed_at)
        );
    }
    Ok(())
}

/// The saved alias whose email matches `email`, preferring the current one.
fn matched_alias(ps: &ProviderState, email: &str) -> Option<String> {
    if let Some(cur) = &ps.current {
        if ps.accounts.get(cur).map(|a| a.email.as_str()) == Some(email) {
            return Some(cur.clone());
        }
    }
    ps.accounts
        .iter()
        .find(|(_, a)| a.email == email)
        .map(|(name, _)| name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn acct(email: &str) -> Account {
        Account {
            email: email.into(),
            data: json!({"token": email}),
        }
    }

    fn ps_with(current: Option<&str>, previous: Option<&str>) -> ProviderState {
        let mut ps = ProviderState::default();
        for name in ["work", "personal"] {
            ps.accounts
                .insert(name.into(), acct(&format!("{name}@x.com")));
        }
        ps.current = current.map(Into::into);
        ps.previous = previous.map(Into::into);
        ps
    }

    #[test]
    fn alias_validation() {
        for good in ["work", "a.b_c-1", "Personal"] {
            assert!(validate_alias(good).is_ok(), "{good} should be valid");
        }
        for bad in ["", "-", "next", "a/b", "a b", "a:b"] {
            assert!(validate_alias(bad).is_err(), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn resolve_dash_uses_previous() {
        let ps = ps_with(Some("work"), Some("personal"));
        assert_eq!(resolve_target(&ps, "-").unwrap(), "personal");
        assert_eq!(resolve_target(&ps, "work").unwrap(), "work");
    }

    #[test]
    fn resolve_dash_without_previous_errors() {
        let ps = ps_with(Some("work"), None);
        assert!(resolve_target(&ps, "-").is_err());
    }

    #[test]
    fn switch_sets_previous_to_outgoing() {
        let mut ps = ps_with(Some("work"), Some("personal"));
        record_switch(&mut ps, "personal");
        assert_eq!(ps.current.as_deref(), Some("personal"));
        assert_eq!(ps.previous.as_deref(), Some("work"));
    }

    #[test]
    fn switch_dash_toggles_back() {
        let mut ps = ps_with(Some("work"), Some("personal"));
        let t1 = resolve_target(&ps, "-").unwrap();
        record_switch(&mut ps, &t1);
        let t2 = resolve_target(&ps, "-").unwrap();
        record_switch(&mut ps, &t2);
        assert_eq!(ps.current.as_deref(), Some("work"));
        assert_eq!(ps.previous.as_deref(), Some("personal"));
    }

    #[test]
    fn snapshot_refuses_to_repoint_an_alias_at_another_account() {
        let mut ps = ps_with(Some("work"), None);
        let fresh = json!({"claudeAiOauth": {"accessToken": "new"}});
        // Same account: written.
        assert_eq!(
            apply_snapshot(&mut ps, "work", "work@x.com", fresh.clone()),
            Refreshed::Wrote
        );
        assert_eq!(
            ps.accounts["work"].data["claudeAiOauth"]["accessToken"],
            json!("new")
        );
        // Different account: refused, alias untouched.
        assert_eq!(
            apply_snapshot(
                &mut ps,
                "work",
                "someone@else.com",
                json!({"claudeAiOauth": {}})
            ),
            Refreshed::Mismatch {
                live: "someone@else.com".into(),
                saved: "work@x.com".into(),
            }
        );
        assert_eq!(ps.accounts["work"].email, "work@x.com");
        assert_eq!(
            ps.accounts["work"].data["claudeAiOauth"]["accessToken"],
            json!("new")
        );
        // Unknown alias is a no-op.
        assert_eq!(
            apply_snapshot(&mut ps, "nope", "work@x.com", json!({})),
            Refreshed::Skipped
        );
    }

    #[test]
    fn live_is_newer_compares_expiry() {
        let stored = |ms: i64| json!({"claudeAiOauth": {"expiresAt": ms}});
        assert!(live_is_newer(&stored(100), &json!({"expiresAt": 101})));
        assert!(!live_is_newer(&stored(100), &json!({"expiresAt": 100})));
        assert!(!live_is_newer(&stored(100), &json!({"expiresAt": 99})));
        // Nothing saved yet, but something live: worth capturing.
        assert!(live_is_newer(&json!({}), &json!({"expiresAt": 1})));
        // No usable live timestamp: leave the saved copy alone.
        assert!(!live_is_newer(&stored(100), &json!({})));
    }

    #[test]
    fn whoami_matches_alias_by_email() {
        let ps = ps_with(Some("work"), None);
        assert_eq!(matched_alias(&ps, "work@x.com").as_deref(), Some("work"));
        assert_eq!(
            matched_alias(&ps, "personal@x.com").as_deref(),
            Some("personal")
        );
        assert_eq!(matched_alias(&ps, "nobody@x.com"), None);
    }
}
