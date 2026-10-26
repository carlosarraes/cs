use crate::{claude, commands, routing::store::Store, state, usage};
use anyhow::Result;

pub fn credential_lock() -> Result<state::LockGuard> {
    state::lock_at(
        &state::state_path()?.with_file_name("credentials.lock"),
        true,
    )
}

pub fn unmanaged_claude_running() -> bool {
    let managed = Store::default_path()
        .and_then(|store| {
            store.update(|s| {
                Ok(s.leases
                    .values()
                    .map(|l| l.process.pid as i64)
                    .collect::<Vec<_>>())
            })
        })
        .unwrap_or_default();
    claude::running_pids()
        .iter()
        .any(|pid| !managed.contains(pid))
}

pub fn poll() -> Result<()> {
    if let Err(error) = commands::refresh_current_if_stale() {
        crate::daemon::log(
            "credentials",
            &format!("live login unavailable: {error}; using saved credentials"),
        );
    }
    for (alias, result) in commands::refresh_idle_expired(|_| false)? {
        if let Err(error) = result {
            crate::daemon::log("credentials", &format!("{alias}: {error}"));
        }
    }
    crate::native::sync_all()?;
    let state = state::State::load()?;
    let previous = usage::Observation::load()?;
    let mut ps = state.claude().clone();
    // Poll saved credentials, never a possibly drifted shared login.
    ps.current = None;
    let mut observed = usage::observe(&ps, previous.as_ref(), usage::now(), 60);
    observed.current = state.claude().current.clone();
    usage::carry_timers(previous.as_ref(), &mut observed, usage::now());
    observed.save()
}

#[cfg(test)]
mod tests {
    #[test]
    fn expired_tokens_are_unavailable_even_when_present() {
        let account = crate::state::Account {
            email: "a@x".into(),
            data: serde_json::json!({"claudeAiOauth":{"accessToken":"token","expiresAt":1000}}),
        };
        assert!(crate::routing::store::token_valid(&account, 0));
        assert!(!crate::routing::store::token_valid(&account, 1));
    }
}
