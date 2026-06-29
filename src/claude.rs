//! Reading and writing Claude Code's on-disk login state.
//!
//! A Claude login lives in two files:
//!
//! - `~/.claude/.credentials.json` — key `claudeAiOauth` (the OAuth tokens)
//! - `~/.claude.json` — key `oauthAccount` (account identity)
//!
//! `cs` only ever touches those two keys; everything else in those files (MCP
//! tokens, project history, caches) is left untouched.

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

/// `~/.claude/.credentials.json` — holds `claudeAiOauth`.
pub fn credentials_path() -> Result<PathBuf> {
    Ok(config_home()?.join(".credentials.json"))
}

/// `~/.claude.json` — holds `oauthAccount`. Note it sits at `$HOME`
/// (or `$CLAUDE_CONFIG_DIR`), NOT inside `~/.claude/`.
pub fn global_config_path() -> Result<PathBuf> {
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
pub fn read_json_key(path: &Path, key: &str) -> Result<Option<Value>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let root: Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    Ok(root.get(key).cloned())
}

/// Set a single top-level `key` to `value`, preserving every other key in the
/// file (and their order). Creates the file as `{key: value}` if it is absent.
pub fn patch_json(path: &Path, key: &str, value: Value) -> Result<()> {
    let mut root: Value = if path.exists() {
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?
    } else {
        Value::Object(Default::default())
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} is not a JSON object", path.display()))?;
    obj.insert(key.to_string(), value);
    let serialized = serde_json::to_vec(&root)?;
    atomic_write(path, &serialized)
}

/// A snapshot of the currently-active Claude account.
pub struct Captured {
    pub claude_ai_oauth: Value,
    pub oauth_account: Value,
    pub email: String,
}

/// Read the live account's tokens + identity from disk.
pub fn capture() -> Result<Captured> {
    let creds = credentials_path()?;
    let claude_ai_oauth = read_json_key(&creds, "claudeAiOauth")?.ok_or_else(|| {
        anyhow!(
            "no active Claude login found (missing `claudeAiOauth` in {})",
            creds.display()
        )
    })?;
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
    patch_json(
        &credentials_path()?,
        "claudeAiOauth",
        claude_ai_oauth.clone(),
    )?;
    patch_json(
        &global_config_path()?,
        "oauthAccount",
        oauth_account.clone(),
    )?;
    Ok(())
}

/// PIDs of live Claude Code sessions, found via `~/.claude/sessions/*.json`
/// (each carries a `pid`) cross-checked against `/proc/<pid>`.
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
            if Path::new(&format!("/proc/{pid}")).exists() {
                pids.push(pid);
            }
        }
    }
    pids
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn patch_preserves_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        // preserve_order keeps insertion order: a, keep, oauthAccount
        let original = json!({"a": 1, "keep": {"nested": true}, "oauthAccount": {"emailAddress": "old@x.com"}});
        fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();

        patch_json(&path, "oauthAccount", json!({"emailAddress": "new@x.com"})).unwrap();

        let back: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(back["a"], json!(1));
        assert_eq!(back["keep"], json!({"nested": true}));
        assert_eq!(back["oauthAccount"]["emailAddress"], json!("new@x.com"));
        // key order preserved (our key updated in place, not moved/sorted)
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
}
