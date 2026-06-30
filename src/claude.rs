//! Reading and writing Claude Code's active login.
//!
//! A Claude login is two pieces, stored separately:
//!
//! - the OAuth tokens (`claudeAiOauth`) — on Linux in the `claudeAiOauth` key of
//!   `~/.claude/.credentials.json`; on macOS in the login Keychain under service
//!   `Claude Code-credentials` (Claude Code reads the Keychain there, not a file).
//! - the account identity (`oauthAccount`) — in `~/.claude.json` on both platforms.
//!
//! `cs` only ever touches those two values; everything else (MCP tokens, project
//! history, caches) is left untouched.

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process;

fn home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("could not determine home directory"))
}

/// Claude Code's config home: `$CLAUDE_CONFIG_DIR` or `~/.claude`.
fn config_home() -> Result<PathBuf> {
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

/// `~/.claude.json` — holds `oauthAccount`. Note it sits at `$HOME`
/// (or `$CLAUDE_CONFIG_DIR`), NOT inside `~/.claude/`.
fn global_config_path() -> Result<PathBuf> {
    let base = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home()?,
    };
    Ok(base.join(".claude.json"))
}

/// Write `contents` to `path` atomically with mode 600 (temp file in the same
/// directory, then rename). Never leaves a partially-written file behind.
pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("cs");
    let tmp = dir.join(format!(".{file_name}.{}.tmp", process::id()));
    fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Read a single top-level key from a JSON object file. `Ok(None)` if the file
/// is absent or the key is missing.
fn read_json_key(path: &Path, key: &str) -> Result<Option<Value>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let root: Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    Ok(root.get(key).cloned())
}

/// Insert or replace `key` in a JSON object, preserving every other key and its
/// order. Starts from `{}` when `existing` is `None`.
fn patch_value(existing: Option<Value>, key: &str, value: Value) -> Result<Value> {
    let mut root = existing.unwrap_or_else(|| Value::Object(Default::default()));
    root.as_object_mut()
        .ok_or_else(|| anyhow!("expected a JSON object"))?
        .insert(key.to_string(), value);
    Ok(root)
}

/// Set a single top-level `key` to `value` in a JSON object file, preserving
/// every other key. Creates the file as `{key: value}` if it is absent.
fn patch_json(path: &Path, key: &str, value: Value) -> Result<()> {
    let existing = if path.exists() {
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Some(
            serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?,
        )
    } else {
        None
    };
    let root = patch_value(existing, key, value)?;
    atomic_write(path, &serde_json::to_vec(&root)?)
}

/// A snapshot of the currently-active Claude account.
pub struct Captured {
    pub claude_ai_oauth: Value,
    pub oauth_account: Value,
    pub email: String,
}

/// Read the live account's tokens + identity.
pub fn capture() -> Result<Captured> {
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
    Ok(Captured {
        claude_ai_oauth,
        oauth_account,
        email,
    })
}

/// Make `claude_ai_oauth` + `oauth_account` the live account.
pub fn restore(claude_ai_oauth: &Value, oauth_account: &Value) -> Result<()> {
    write_active_oauth(claude_ai_oauth)?;
    patch_json(
        &global_config_path()?,
        "oauthAccount",
        oauth_account.clone(),
    )?;
    Ok(())
}

// --- Active OAuth credential backend: file on Linux, Keychain on macOS --------

/// Read the active `claudeAiOauth` value from `~/.claude/.credentials.json`.
#[cfg(not(target_os = "macos"))]
fn read_active_oauth() -> Result<Option<Value>> {
    read_json_key(&credentials_path()?, "claudeAiOauth")
}

/// Write the active `claudeAiOauth` value, preserving other keys (e.g. `mcpOAuth`).
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
    use super::patch_value;
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
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return pids,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else { continue };
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

/// Whether a process is alive, via `kill(pid, 0)` — works on Linux and macOS.
/// Signal 0 delivers nothing; it only probes existence/permission. An `EPERM`
/// result means the process exists but we may not signal it — still alive.
fn pid_alive(pid: i64) -> bool {
    // SAFETY: kill() with signal 0 performs no signal delivery and has no
    // memory effects; it only returns an existence/permission status.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn patch_value_inserts_and_replaces_in_place() {
        assert_eq!(patch_value(None, "a", json!(1)).unwrap(), json!({"a": 1}));
        // existing key is updated in place; order and siblings preserved
        let r = patch_value(Some(json!({"a": 1, "b": 2})), "a", json!(9)).unwrap();
        assert_eq!(r, json!({"a": 9, "b": 2}));
        let keys: Vec<_> = r.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, vec!["a", "b"]);
    }

    #[test]
    fn patch_preserves_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let original = json!({"a": 1, "keep": {"nested": true}, "oauthAccount": {"emailAddress": "old@x.com"}});
        fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();

        patch_json(&path, "oauthAccount", json!({"emailAddress": "new@x.com"})).unwrap();

        let back: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(back["a"], json!(1));
        assert_eq!(back["keep"], json!({"nested": true}));
        assert_eq!(back["oauthAccount"]["emailAddress"], json!("new@x.com"));
        let keys: Vec<_> = back.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, vec!["a", "keep", "oauthAccount"]);
    }

    #[test]
    fn patch_creates_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.json");
        patch_json(&path, "claudeAiOauth", json!({"accessToken": "t"})).unwrap();
        let back: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(back["claudeAiOauth"]["accessToken"], json!("t"));
    }

    #[test]
    fn read_missing_key_or_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.json");
        assert!(read_json_key(&path, "claudeAiOauth").unwrap().is_none());
        fs::write(&path, b"{\"other\": 1}").unwrap();
        assert!(read_json_key(&path, "claudeAiOauth").unwrap().is_none());
        assert!(read_json_key(&path, "other").unwrap().is_some());
    }

    #[test]
    fn atomic_write_sets_mode_600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.json");
        atomic_write(&path, b"{}").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn current_process_is_alive() {
        assert!(pid_alive(std::process::id() as i64));
    }
}
