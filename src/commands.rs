//! Implementations of the subcommands: add, switch, del, list, whoami.

use anyhow::{anyhow, Context, Result};
use std::io::{self, IsTerminal, Write};

use crate::claude;
use crate::provider::Provider;
use crate::state::{Account, ProviderState, State};

fn validate_alias(alias: &str) -> Result<()> {
    let ok = !alias.is_empty()
        && alias != "-"
        && alias
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(anyhow!(
            "invalid alias '{alias}': use letters, digits, '.', '_', '-'"
        ))
    }
}

/// Snapshot the live account into `ps.accounts[alias]` (no-op if there is no
/// live account, so it is safe to call before destructive ops).
fn refresh_snapshot(ps: &mut ProviderState, alias: &str, provider: Provider) {
    if let Some(acct) = ps.accounts.get_mut(alias) {
        if let Ok((email, data)) = provider.capture() {
            acct.email = email;
            acct.data = data;
        }
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

pub fn add(provider: Provider, alias: &str, current: bool, force: bool) -> Result<()> {
    validate_alias(alias)?;
    let mut state = State::load()?;
    if state.provider(provider).accounts.contains_key(alias) && !force {
        return Err(anyhow!(
            "{} alias '{alias}' already exists (use --force, or `cs del {alias}{}` first)",
            provider.name(),
            provider.flag_suffix()
        ));
    }

    if !current {
        // Keep the outgoing account's snapshot fresh before login overwrites
        // the live credentials.
        if let Some(cur) = state.provider(provider).current.clone() {
            refresh_snapshot(state.provider_mut(provider), &cur, provider);
        }
        println!("Launching {} login for '{alias}'…", provider.name());
        provider.login()?;
    }

    let (email, data) = provider.capture().context(if current {
        "no active account to snapshot (log in first, or drop --current)"
    } else {
        "could not read the account after login"
    })?;

    let ps = state.provider_mut(provider);
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
    println!(
        "Saved {} account '{alias}' ({email}) and set it active.",
        provider.name()
    );
    Ok(())
}

pub fn switch(provider: Provider, requested: &str, yes: bool) -> Result<()> {
    let mut state = State::load()?;
    let target = resolve_target(state.provider(provider), requested)?;

    if !state.provider(provider).accounts.contains_key(&target) {
        return Err(anyhow!(
            "unknown {} alias '{target}' (see `cs list{}`)",
            provider.name(),
            provider.flag_suffix()
        ));
    }
    if state.provider(provider).current.as_deref() == Some(target.as_str()) {
        println!("Already on '{target}'.");
        return Ok(());
    }

    // The running-warning only applies to Claude (Codex has no pid session files).
    if provider == Provider::Claude && !yes && !confirm_if_running()? {
        println!("Aborted.");
        return Ok(());
    }

    // Re-capture the outgoing account so rotated refresh tokens aren't lost.
    if let Some(cur) = state.provider(provider).current.clone() {
        refresh_snapshot(state.provider_mut(provider), &cur, provider);
    }

    let account = state
        .provider(provider)
        .accounts
        .get(&target)
        .expect("checked above")
        .clone();
    provider
        .restore(&account.data)
        .with_context(|| format!("switching to '{target}'"))?;

    record_switch(state.provider_mut(provider), &target);
    state.save()?;
    println!(
        "Switched {} to '{target}' ({}).",
        provider.name(),
        account.email
    );
    Ok(())
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

pub fn del(provider: Provider, alias: &str) -> Result<()> {
    let mut state = State::load()?;
    let ps = state.provider_mut(provider);
    if ps.accounts.remove(alias).is_none() {
        return Err(anyhow!("unknown {} alias '{alias}'", provider.name()));
    }
    if ps.current.as_deref() == Some(alias) {
        ps.current = None;
    }
    if ps.previous.as_deref() == Some(alias) {
        ps.previous = None;
    }
    state.save()?;
    println!(
        "Deleted {} alias '{alias}'. (Live credentials were not changed.)",
        provider.name()
    );
    Ok(())
}

pub fn list(provider: Provider) -> Result<()> {
    let state = State::load()?;
    let ps = state.provider(provider);
    if ps.accounts.is_empty() {
        println!(
            "No {} accounts saved yet. Add one with `cs add <alias>{}`.",
            provider.name(),
            provider.flag_suffix()
        );
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

/// Show the live signed-in account on both providers, with the matching alias.
pub fn whoami() -> Result<()> {
    let state = State::load()?;
    for provider in [Provider::Claude, Provider::Codex] {
        let ps = state.provider(provider);
        let line = match provider.live_email() {
            Ok(Some(email)) => {
                let tag = matched_alias(ps, &email)
                    .map(|a| format!("({a})"))
                    .unwrap_or_else(|| "(not saved)".to_string());
                format!("{email:<30} {tag}")
            }
            Ok(None) => "— not signed in".to_string(),
            Err(e) => format!("— {e}"),
        };
        println!("{:<8} {line}", provider.name());
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
        for bad in ["", "-", "a/b", "a b", "a:b"] {
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
