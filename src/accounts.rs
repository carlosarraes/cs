use crate::{claude, commands, routing::store::Store, state, usage};
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct KeySecrets {
    pub key: String,
}

pub fn validate_key(key: &str) -> Result<()> {
    let valid = key.len() > 20
        && key.len() <= 512
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        && key.starts_with("sk-ant-")
        && !key.starts_with("sk-ant-admin");
    anyhow::ensure!(valid, "invalid Anthropic API key");
    Ok(())
}

pub fn key_hint(key: &str) -> String {
    let tail = key.get(key.len().saturating_sub(4)..).unwrap_or("????");
    format!("sk-ant-…{tail}")
}

#[cfg(not(target_os = "macos"))]
fn key_credentials_path() -> Result<std::path::PathBuf> {
    Ok(state::state_path()?.with_file_name("key-credentials.json"))
}

#[cfg(not(target_os = "macos"))]
fn key_credentials() -> Result<std::collections::BTreeMap<String, KeySecrets>> {
    match std::fs::read(key_credentials_path()?) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid key credential storage")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(_) => anyhow::bail!("could not read key credential storage"),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn read_key(alias: &str) -> Result<KeySecrets> {
    key_credentials()?
        .remove(alias)
        .ok_or_else(|| anyhow::anyhow!("API key is not saved"))
}

#[cfg(not(target_os = "macos"))]
pub fn save_key(alias: &str, secret: &KeySecrets) -> Result<()> {
    let mut saved = key_credentials()?;
    saved.insert(
        alias.into(),
        KeySecrets {
            key: secret.key.clone(),
        },
    );
    crate::util::atomic_write(&key_credentials_path()?, &serde_json::to_vec(&saved)?)
}

#[cfg(not(target_os = "macos"))]
pub fn delete_key(alias: &str) -> Result<()> {
    let mut saved = key_credentials()?;
    if saved.remove(alias).is_none() {
        return Ok(());
    }
    if saved.is_empty() {
        std::fs::remove_file(key_credentials_path()?)?;
        Ok(())
    } else {
        crate::util::atomic_write(&key_credentials_path()?, &serde_json::to_vec(&saved)?)
    }
}

#[cfg(target_os = "macos")]
pub fn read_key(alias: &str) -> Result<KeySecrets> {
    let value =
        claude::macos::read_key(alias)?.ok_or_else(|| anyhow::anyhow!("API key is not saved"))?;
    serde_json::from_str(&value).map_err(|_| anyhow::anyhow!("invalid key credential storage"))
}

#[cfg(target_os = "macos")]
pub fn save_key(alias: &str, secret: &KeySecrets) -> Result<()> {
    claude::macos::save_key(alias, &serde_json::to_string(secret)?)
}

#[cfg(target_os = "macos")]
pub fn delete_key(alias: &str) -> Result<()> {
    claude::macos::delete_key(alias)
}

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

const CURRENT_RENEW_MARGIN_MS: i64 = 30 * 60 * 1000;

fn renew_current_with(
    accounts: &state::ProviderState,
    now_ms: i64,
    mut renew: impl FnMut(&str, &str, i64) -> Result<()>,
) -> Result<()> {
    let Some(alias) = accounts.current.as_deref() else {
        return Ok(());
    };
    let Some(account) = accounts.accounts.get(alias) else {
        return Ok(());
    };
    if !account.is_key()
        && account.data["claudeAiOauth"]["expiresAt"]
            .as_i64()
            .is_some_and(|expiry| expiry <= now_ms.saturating_add(CURRENT_RENEW_MARGIN_MS))
    {
        renew(alias, &account.email, CURRENT_RENEW_MARGIN_MS)?;
    }
    Ok(())
}

pub fn log_renewal_failure(alias: &str, error: &anyhow::Error) {
    // cs composes renewal errors; OAuth HTTP failures include only status and
    // the error code, never response bodies or credential contents.
    crate::daemon::log("credentials", &format!("{alias}: {error:#}"));
}

pub fn poll() -> Result<()> {
    if let Err(error) = commands::refresh_current_if_stale() {
        crate::daemon::log(
            "credentials",
            &format!("live login unavailable: {error}; using saved credentials"),
        );
    }
    let current = state::State::load()?;
    if let Err(error) = renew_current_with(
        current.claude(),
        usage::now() * 1000,
        commands::renew_native_account_within,
    ) {
        if let Some(alias) = current.claude().current.as_deref() {
            log_renewal_failure(alias, &error);
        }
    }
    for (alias, result) in commands::refresh_idle_expired(|_| false)? {
        if let Err(error) = result {
            log_renewal_failure(&alias, &error);
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
    fn poll_renews_current_only_when_due() {
        let now_ms = 10_000_000;
        let mut accounts = crate::state::ProviderState {
            current: Some("current".into()),
            ..Default::default()
        };
        for (expiry, expected_calls) in [
            (now_ms - 1, 1),
            (now_ms, 1),
            (now_ms + super::CURRENT_RENEW_MARGIN_MS - 1, 1),
            (now_ms + super::CURRENT_RENEW_MARGIN_MS, 1),
            (now_ms + super::CURRENT_RENEW_MARGIN_MS + 1, 0),
            (now_ms + 8 * 60 * 60 * 1000, 0),
        ] {
            accounts.accounts.insert(
                "current".into(),
                crate::state::Account {
                    kind: None,
                    email: "current@example.test".into(),
                    data: serde_json::json!({"claudeAiOauth": {"expiresAt": expiry}}),
                },
            );
            let mut calls = 0;
            super::renew_current_with(&accounts, now_ms, |alias, email, margin| {
                assert_eq!(alias, "current");
                assert_eq!(email, "current@example.test");
                assert_eq!(margin, super::CURRENT_RENEW_MARGIN_MS);
                calls += 1;
                Ok(())
            })
            .unwrap();
            assert_eq!(calls, expected_calls, "expiry={expiry}");
        }
    }

    #[test]
    fn expired_tokens_are_unavailable_even_when_present() {
        let account = crate::state::Account {
            kind: None,
            email: "a@x".into(),
            data: serde_json::json!({"claudeAiOauth":{"accessToken":"token","expiresAt":1000}}),
        };
        assert!(crate::routing::store::token_valid(&account, 0));
        assert!(!crate::routing::store::token_valid(&account, 1));
    }
}
