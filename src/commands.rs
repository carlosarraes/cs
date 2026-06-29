//! Implementations of the four subcommands: add, switch, del, list.

use anyhow::{anyhow, Context, Result};
use std::io::{self, IsTerminal, Write};
use std::process::Command;

use crate::claude;
use crate::state::{Account, State};

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

/// Snapshot the live account into `state.accounts[alias]` (no-op if there is no
/// live account, so it is safe to call opportunistically before destructive ops).
fn refresh_snapshot(state: &mut State, alias: &str) {
    if let Some(acct) = state.accounts.get_mut(alias) {
        if let Ok(cap) = claude::capture() {
            acct.email = cap.email;
            acct.claude_ai_oauth = cap.claude_ai_oauth;
            acct.oauth_account = cap.oauth_account;
        }
    }
}

/// Resolve a requested switch target: `-` means the previous alias.
fn resolve_target(state: &State, requested: &str) -> Result<String> {
    if requested == "-" {
        state
            .previous
            .clone()
            .ok_or_else(|| anyhow!("no previous account to switch to"))
    } else {
        Ok(requested.to_string())
    }
}

/// Update `current`/`previous` after a successful switch to `target`.
///
/// This is what makes `cs switch -` behave like `cd -`: the alias you were just
/// on must become `previous`, so a second `switch -` toggles back to where you
/// started. The no-op case (`target` already current) is handled by the caller,
/// so here you can assume `target` differs from the current alias.
fn record_switch(state: &mut State, target: &str) {
    // `take()` reads the outgoing alias and leaves `None` behind in one move, so
    // the borrow checker is happy assigning it to `previous` on the same line.
    state.previous = state.current.take();
    state.current = Some(target.to_string());
}

pub fn add(alias: &str, current: bool, force: bool) -> Result<()> {
    validate_alias(alias)?;
    let mut state = State::load()?;
    if state.accounts.contains_key(alias) && !force {
        return Err(anyhow!(
            "alias '{alias}' already exists (use --force to overwrite, or `cs del {alias}` first)"
        ));
    }

    if !current {
        // Keep the outgoing account's snapshot fresh before `claude auth login`
        // overwrites the live credentials.
        if let Some(cur) = state.current.clone() {
            refresh_snapshot(&mut state, &cur);
        }
        println!("Launching `claude auth login` to authenticate the account for '{alias}'…");
        let status = Command::new("claude")
            .args(["auth", "login"])
            .status()
            .context("failed to run `claude` (is Claude Code installed and on PATH?)")?;
        if !status.success() {
            return Err(anyhow!(
                "`claude auth login` did not complete; nothing saved"
            ));
        }
    }

    let captured = claude::capture().context(if current {
        "no active account to snapshot (log in first, or drop --current)"
    } else {
        "could not read the account after login"
    })?;
    let email = captured.email.clone();
    let account = Account {
        email: captured.email,
        claude_ai_oauth: captured.claude_ai_oauth,
        oauth_account: captured.oauth_account,
    };

    if state.current.as_deref() != Some(alias) {
        state.previous = state.current.take();
    }
    state.current = Some(alias.to_string());
    state.accounts.insert(alias.to_string(), account);
    state.save()?;
    println!("Saved '{alias}' ({email}) and set it active.");
    Ok(())
}

pub fn switch(requested: &str, yes: bool) -> Result<()> {
    let mut state = State::load()?;
    let target = resolve_target(&state, requested)?;

    if !state.accounts.contains_key(&target) {
        return Err(anyhow!("unknown alias '{target}' (see `cs list`)"));
    }
    if state.current.as_deref() == Some(target.as_str()) {
        println!("Already on '{target}'.");
        return Ok(());
    }

    if !yes && !confirm_if_running()? {
        println!("Aborted.");
        return Ok(());
    }

    // Re-capture the outgoing account so rotated refresh tokens aren't lost.
    if let Some(cur) = state.current.clone() {
        refresh_snapshot(&mut state, &cur);
    }

    let account = state.accounts.get(&target).expect("checked above").clone();
    claude::restore(&account.claude_ai_oauth, &account.oauth_account)
        .with_context(|| format!("switching to '{target}'"))?;

    record_switch(&mut state, &target);
    state.save()?;
    println!("Switched to '{target}' ({}).", account.email);
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

pub fn del(alias: &str) -> Result<()> {
    let mut state = State::load()?;
    if state.accounts.remove(alias).is_none() {
        return Err(anyhow!("unknown alias '{alias}' (see `cs list`)"));
    }
    if state.current.as_deref() == Some(alias) {
        state.current = None;
    }
    if state.previous.as_deref() == Some(alias) {
        state.previous = None;
    }
    state.save()?;
    println!("Deleted '{alias}'. (Live Claude credentials were not changed.)");
    Ok(())
}

pub fn list() -> Result<()> {
    let state = State::load()?;
    if state.accounts.is_empty() {
        println!("No accounts saved yet. Add one with `cs add <alias>`.");
        return Ok(());
    }
    for (alias, acct) in &state.accounts {
        let marker = if state.current.as_deref() == Some(alias) {
            '*'
        } else if state.previous.as_deref() == Some(alias) {
            '-'
        } else {
            ' '
        };
        println!("{marker} {alias:<16} {}", acct.email);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Account;
    use serde_json::json;

    fn acct(email: &str) -> Account {
        Account {
            email: email.into(),
            claude_ai_oauth: json!({"accessToken": email}),
            oauth_account: json!({"emailAddress": email}),
        }
    }

    fn state_with(current: Option<&str>, previous: Option<&str>) -> State {
        let mut s = State::default();
        for name in ["work", "personal"] {
            s.accounts
                .insert(name.into(), acct(&format!("{name}@x.com")));
        }
        s.current = current.map(Into::into);
        s.previous = previous.map(Into::into);
        s
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
        let s = state_with(Some("work"), Some("personal"));
        assert_eq!(resolve_target(&s, "-").unwrap(), "personal");
        assert_eq!(resolve_target(&s, "work").unwrap(), "work");
    }

    #[test]
    fn resolve_dash_without_previous_errors() {
        let s = state_with(Some("work"), None);
        assert!(resolve_target(&s, "-").is_err());
    }

    #[test]
    fn switch_sets_previous_to_outgoing() {
        let mut s = state_with(Some("work"), Some("personal"));
        record_switch(&mut s, "personal");
        assert_eq!(s.current.as_deref(), Some("personal"));
        assert_eq!(s.previous.as_deref(), Some("work"));
    }

    #[test]
    fn switch_dash_toggles_back() {
        // work <-> personal should toggle on repeated `switch -`
        let mut s = state_with(Some("work"), Some("personal"));
        let t1 = resolve_target(&s, "-").unwrap();
        record_switch(&mut s, &t1); // now on personal, previous = work
        let t2 = resolve_target(&s, "-").unwrap();
        record_switch(&mut s, &t2); // back on work, previous = personal
        assert_eq!(s.current.as_deref(), Some("work"));
        assert_eq!(s.previous.as_deref(), Some("personal"));
    }
}
