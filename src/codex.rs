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

/// Restore a saved account: write `auth.json` verbatim (atomic, mode 600).
pub fn restore(data: &Value) -> Result<()> {
    util::atomic_write(&auth_path()?, &serde_json::to_vec_pretty(data)?)
}

/// The live account's email, or `None` if not signed in.
pub fn live_email() -> Result<Option<String>> {
    Ok(read_auth()?.map(|a| identity(&a)))
}

/// Drive Codex's interactive login (ChatGPT sign-in).
pub fn login() -> Result<()> {
    let status = Command::new("codex")
        .arg("login")
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
}
