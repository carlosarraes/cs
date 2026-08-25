//! Claude Code credential backend.
//!
//! A Claude login is two pieces:
//!
//! - the OAuth tokens (`claudeAiOauth`) — on Linux in the `claudeAiOauth` key of
//!   `~/.claude/.credentials.json`; on macOS in the login Keychain under service
//!   `Claude Code-credentials` (Claude Code reads the Keychain there, not a file).
//! - the account identity (`oauthAccount`) — in `~/.claude.json` on both platforms.
//!
//! `capture` bundles both into `{claudeAiOauth, oauthAccount}`; `restore` writes
//! them back, patching only those keys so everything else is left untouched.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Command;

use crate::util::{home, patch_json, read_json_key};

/// Claude Code's config home: `$CLAUDE_CONFIG_DIR` or `~/.claude`.
pub fn config_home() -> Result<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir)),
        _ => Ok(home()?.join(".claude")),
    }
}

/// `~/.claude/.credentials.json` — the Linux credentials file (`claudeAiOauth`).
#[cfg(not(target_os = "macos"))]
fn credentials_path() -> Result<PathBuf> {
    Ok(config_home()?.join(".credentials.json"))
}

/// `~/.claude.json` — holds `oauthAccount`. Sits at `$HOME` (or `$CLAUDE_CONFIG_DIR`),
/// NOT inside `~/.claude/`.
fn global_config_path() -> Result<PathBuf> {
    let base = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home()?,
    };
    Ok(base.join(".claude.json"))
}

/// Read the live account as `(email, {claudeAiOauth, oauthAccount})`.
pub fn capture() -> Result<(String, Value)> {
    let claude_ai_oauth = read_active_oauth()?
        .ok_or_else(|| anyhow!("no active Claude login found (run `claude auth login` first)"))?;
    let config = global_config_path()?;
    let oauth_account = read_json_key(&config, "oauthAccount")?.ok_or_else(|| {
        anyhow!(
            "no account identity found (missing `oauthAccount` in {})",
            config.display()
        )
    })?;
    let email = oauth_account
        .get("emailAddress")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    Ok((
        email,
        json!({ "claudeAiOauth": claude_ai_oauth, "oauthAccount": oauth_account }),
    ))
}

/// Make a captured `{claudeAiOauth, oauthAccount}` payload the live account.
pub fn restore(data: &Value) -> Result<()> {
    let oauth = data
        .get("claudeAiOauth")
        .cloned()
        .ok_or_else(|| anyhow!("saved Claude account is missing `claudeAiOauth`"))?;
    let account = data
        .get("oauthAccount")
        .cloned()
        .ok_or_else(|| anyhow!("saved Claude account is missing `oauthAccount`"))?;
    write_active_oauth(&oauth)?;
    patch_json(&global_config_path()?, "oauthAccount", account)?;
    Ok(())
}

/// The live `claudeAiOauth` payload (tokens + expiry), or `None` if not signed in.
/// Claude Code keeps this one fresh, so it's the token to poll usage with.
pub fn live_oauth() -> Result<Option<Value>> {
    read_active_oauth()
}

/// The live account's email, or `None` if no OAuth login is active.
pub fn live_email() -> Result<Option<String>> {
    if read_active_oauth()?.is_none() {
        return Ok(None);
    }
    Ok(
        read_json_key(&global_config_path()?, "oauthAccount")?.and_then(|a| {
            a.get("emailAddress")
                .and_then(|v| v.as_str())
                .map(String::from)
        }),
    )
}

/// Drive Claude Code's interactive login.
pub fn login() -> Result<()> {
    let status = Command::new("claude")
        .args(["auth", "login"])
        .status()
        .context("failed to run `claude` (is Claude Code installed and on PATH?)")?;
    if !status.success() {
        return Err(anyhow!("`claude auth login` did not complete"));
    }
    Ok(())
}

// --- Active OAuth credential backend: file on Linux, Keychain on macOS --------

#[cfg(not(target_os = "macos"))]
fn read_active_oauth() -> Result<Option<Value>> {
    read_json_key(&credentials_path()?, "claudeAiOauth")
}

#[cfg(not(target_os = "macos"))]
fn write_active_oauth(value: &Value) -> Result<()> {
    patch_json(&credentials_path()?, "claudeAiOauth", value.clone())
}

#[cfg(target_os = "macos")]
fn read_active_oauth() -> Result<Option<Value>> {
    macos::read_oauth()
}

#[cfg(target_os = "macos")]
fn write_active_oauth(value: &Value) -> Result<()> {
    macos::write_oauth(value)
}

/// macOS Keychain backend for the active OAuth credential. Shells out to the
/// pinned `/usr/bin/security`, mirroring how Claude Code (and claude-swap)
/// access the `Claude Code-credentials` generic-password item.
#[cfg(target_os = "macos")]
mod macos {
    use crate::util::patch_value;
    use anyhow::{anyhow, Context, Result};
    use serde_json::Value;
    use std::io::Write;
    use std::process::{Command, Stdio};

    const SECURITY: &str = "/usr/bin/security";
    const SERVICE: &str = "Claude Code-credentials";
    const NOT_FOUND_RC: i32 = 44; // errSecItemNotFound
    const STDIN_LINE_LIMIT: usize = 4096 - 64; // security -i reads stdin with a BUFSIZ fgets buffer

    pub fn read_oauth() -> Result<Option<Value>> {
        let Some(secret) = get(&account(), SERVICE)? else {
            return Ok(None);
        };
        let root: Value = serde_json::from_str(&secret)
            .context("parsing the Claude Code keychain credential as JSON")?;
        Ok(root.get("claudeAiOauth").cloned())
    }

    pub fn write_oauth(value: &Value) -> Result<()> {
        // Read-modify-write so other keys in the secret (e.g. mcpOAuth) survive.
        let existing = match get(&account(), SERVICE)? {
            Some(s) => Some(
                serde_json::from_str(&s)
                    .context("parsing the existing Claude Code keychain credential")?,
            ),
            None => None,
        };
        let root = patch_value(existing, "claudeAiOauth", value.clone())?;
        set(&account(), SERVICE, &serde_json::to_string(&root)?)
    }

    /// The keychain `-a` account. Claude Code keys its item on the unix username:
    /// `$USER`, else the effective-uid name, else the literal `claude-code-user`.
    fn account() -> String {
        if let Ok(u) = std::env::var("USER") {
            if !u.is_empty() {
                return u;
            }
        }
        // SAFETY: geteuid/getpwuid are read-only libc calls; pw_name is copied
        // into an owned String immediately and the returned pointer is not kept.
        unsafe {
            let pw = libc::getpwuid(libc::geteuid());
            if !pw.is_null() && !(*pw).pw_name.is_null() {
                if let Ok(name) = std::ffi::CStr::from_ptr((*pw).pw_name).to_str() {
                    if !name.is_empty() {
                        return name.to_string();
                    }
                }
            }
        }
        "claude-code-user".to_string()
    }

    /// `security find-generic-password -a <acct> -w -s <service>`.
    fn get(account: &str, service: &str) -> Result<Option<String>> {
        let out = Command::new(SECURITY)
            .args(["find-generic-password", "-a", account, "-w", "-s", service])
            .output()
            .with_context(|| format!("running {SECURITY}"))?;
        match out.status.code() {
            Some(0) => {
                let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
                // `-w` appends exactly one newline; strip just that one.
                if s.ends_with('\n') {
                    s.pop();
                }
                Ok(Some(s))
            }
            Some(NOT_FOUND_RC) => Ok(None),
            _ => Err(anyhow!(
                "security find-generic-password failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )),
        }
    }

    /// `security add-generic-password -U -a <acct> -s <service> -X <hex>`.
    /// The secret is passed as hex via `security -i` stdin so it never appears in
    /// argv; falls back to argv only if the stdin line would overflow the buffer.
    fn set(account: &str, service: &str, secret: &str) -> Result<()> {
        let hex: String = secret.bytes().map(|b| format!("{b:02x}")).collect();
        let line = format!(
            "add-generic-password -U -a {} -s {} -X {hex}\n",
            quote(account),
            quote(service),
        );
        let out = if line.len() <= STDIN_LINE_LIMIT {
            let mut child = Command::new(SECURITY)
                .arg("-i")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .with_context(|| format!("running {SECURITY} -i"))?;
            child
                .stdin
                .take()
                .expect("stdin was piped")
                .write_all(line.as_bytes())?;
            child.wait_with_output()?
        } else {
            Command::new(SECURITY)
                .args([
                    "add-generic-password",
                    "-U",
                    "-a",
                    account,
                    "-s",
                    service,
                    "-X",
                    &hex,
                ])
                .output()
                .with_context(|| format!("running {SECURITY}"))?
        };
        if out.status.success() {
            Ok(())
        } else {
            Err(anyhow!(
                "security add-generic-password failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    /// Quote a value for the `security -i` line parser (it re-parses shell-style).
    fn quote(value: &str) -> String {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// PIDs of live Claude Code sessions, found via `~/.claude/sessions/*.json`
/// (each carries a `pid`) cross-checked for liveness with `kill(pid, 0)`.
pub fn running_pids() -> Vec<i64> {
    let mut pids = Vec::new();
    let dir = match config_home() {
        Ok(h) => h.join("sessions"),
        Err(_) => return pids,
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return pids,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if let Some(pid) = v.get("pid").and_then(|p| p.as_i64()) {
            if pid_alive(pid) {
                pids.push(pid);
            }
        }
    }
    pids
}

/// Whether any live Claude Code session is mid-turn (`status: "busy"` in its
/// session file). Used to prefer switching between turns.
pub fn any_session_busy() -> bool {
    let Ok(dir) = config_home().map(|h| h.join("sessions")) else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            return false;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            return false;
        };
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
            return false;
        };
        v.get("status").and_then(Value::as_str) == Some("busy")
            && v.get("pid").and_then(Value::as_i64).is_some_and(pid_alive)
    })
}

/// Whether a process is alive, via `kill(pid, 0)` — works on Linux and macOS.
/// Signal 0 delivers nothing; it only probes existence/permission. An `EPERM`
/// result means the process exists but we may not signal it — still alive.
pub fn pid_alive(pid: i64) -> bool {
    // SAFETY: kill() with signal 0 performs no signal delivery and has no
    // memory effects; it only returns an existence/permission status.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_alive() {
        assert!(pid_alive(std::process::id() as i64));
    }
}
