//! Codex CLI credential backend.
//!
//! Codex keeps everything in one file, `~/.codex/auth.json` (`$CODEX_HOME`
//! relocates it): `{ OPENAI_API_KEY, tokens: {id_token, access_token,
//! refresh_token, account_id}, last_refresh }`. Account identity lives inside
//! that file, so a switch is a whole-file swap — no separate identity file.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;

use crate::util;

/// `$CODEX_HOME` or `~/.codex`.
fn codex_home() -> Result<PathBuf> {
    match std::env::var_os("CODEX_HOME") {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir)),
        _ => Ok(util::home()?.join(".codex")),
    }
}

fn auth_path() -> Result<PathBuf> {
    Ok(codex_home()?.join("auth.json"))
}

fn read_auth() -> Result<Option<Value>> {
    let path = auth_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Some(
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?,
    ))
}

/// Read the live account as `(email, auth.json)`.
pub fn capture() -> Result<(String, Value)> {
    let auth = read_auth()?
        .ok_or_else(|| anyhow!("no active Codex login found (run `codex login` first)"))?;
    Ok((identity(&auth), auth))
}

/// Restore a saved account: write `auth.json` (atomic, mode 600) — UNLESS the
/// live login is a newer copy of the SAME account, in which case keep the live
/// tokens. Writing an older (rotated-past) refresh token over a fresher one is
/// exactly what triggers OpenAI's `refresh_token_reused` revocation.
pub fn restore(data: &Value) -> Result<()> {
    if let Some(live) = read_auth()? {
        if is_stale_downgrade(&live, data) {
            return Ok(());
        }
    }
    util::atomic_write(&auth_path()?, &serde_json::to_vec_pretty(data)?)
}

fn account_id(auth: &Value) -> Option<&str> {
    auth.get("tokens")?.get("account_id")?.as_str()
}

fn last_refresh(auth: &Value) -> Option<&str> {
    auth.get("last_refresh").and_then(|v| v.as_str())
}

/// True if `data` is an OLDER snapshot of the SAME account that is currently
/// live — restoring it would downgrade a freshly-rotated credential to a stale
/// one. `last_refresh` is ISO-8601 written consistently by Codex, so lexical
/// order matches chronological order.
fn is_stale_downgrade(live: &Value, data: &Value) -> bool {
    match (account_id(live), account_id(data)) {
        (Some(a), Some(b)) if a == b => match (last_refresh(live), last_refresh(data)) {
            (Some(live_ts), Some(snap_ts)) => live_ts > snap_ts,
            _ => false,
        },
        _ => false,
    }
}

/// The live account's email, or `None` if not signed in.
pub fn live_email() -> Result<Option<String>> {
    Ok(read_auth()?.map(|a| identity(&a)))
}

/// Drive Codex's interactive login (ChatGPT sign-in).
pub fn login(device_auth: bool) -> Result<()> {
    let mut cmd = Command::new("codex");
    cmd.arg("login");
    if device_auth {
        // Device-code flow — works over SSH / headless (no localhost callback).
        cmd.arg("--device-auth");
    }
    let status = cmd
        .status()
        .context("failed to run `codex` (is Codex CLI installed and on PATH?)")?;
    if !status.success() {
        return Err(anyhow!("`codex login` did not complete"));
    }
    Ok(())
}

/// Display identity for an `auth.json`: the ChatGPT email decoded from the
/// `id_token`, else a note for API-key auth.
fn identity(auth: &Value) -> String {
    let id_token = auth
        .get("tokens")
        .and_then(|t| t.get("id_token"))
        .and_then(|v| v.as_str());
    if let Some(email) = id_token.and_then(email_from_id_token) {
        return email;
    }
    if auth
        .get("OPENAI_API_KEY")
        .and_then(|v| v.as_str())
        .is_some()
    {
        return "(api key)".to_string();
    }
    "unknown".to_string()
}

/// Decode the `email` claim from a JWT `id_token` payload (no signature check).
fn email_from_id_token(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("email")
        .and_then(|v| v.as_str())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use serde_json::json;

    fn make_id_token(payload: &serde_json::Value) -> String {
        let b = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(payload).unwrap());
        format!("header.{b}.sig")
    }

    #[test]
    fn identity_prefers_id_token_email() {
        let tok = make_id_token(&json!({"email": "alice@example.com"}));
        let auth = json!({"tokens": {"id_token": tok, "account_id": "x"}});
        assert_eq!(identity(&auth), "alice@example.com");
    }

    #[test]
    fn identity_falls_back_to_api_key() {
        let auth = json!({"OPENAI_API_KEY": "sk-abc", "tokens": null});
        assert_eq!(identity(&auth), "(api key)");
    }

    fn auth(acct: &str, ts: &str) -> Value {
        json!({"tokens": {"account_id": acct}, "last_refresh": ts})
    }

    #[test]
    fn downgrade_blocked_for_same_account_older_snapshot() {
        let live = auth("acc1", "2026-02-01T10:00:00Z");
        let snap = auth("acc1", "2026-01-01T10:00:00Z");
        assert!(is_stale_downgrade(&live, &snap));
    }

    #[test]
    fn switch_to_different_account_is_allowed() {
        let live = auth("acc1", "2026-02-01T10:00:00Z");
        let snap = auth("acc2", "2026-01-01T10:00:00Z");
        assert!(!is_stale_downgrade(&live, &snap));
    }

    #[test]
    fn newer_or_equal_snapshot_is_allowed() {
        let live = auth("acc1", "2026-01-01T10:00:00Z");
        let newer = auth("acc1", "2026-02-01T10:00:00Z");
        assert!(!is_stale_downgrade(&live, &newer));
        assert!(!is_stale_downgrade(&live, &live.clone()));
    }

    #[test]
    fn missing_timestamps_never_block() {
        let live = json!({"tokens": {"account_id": "acc1"}});
        let snap = json!({"tokens": {"account_id": "acc1"}});
        assert!(!is_stale_downgrade(&live, &snap));
    }
}
