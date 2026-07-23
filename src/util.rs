//! Shared filesystem + JSON helpers used by the credential backends and the
//! alias store.

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process;

pub fn home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("could not determine home directory"))
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

/// Insert or replace `key` in a JSON object, preserving every other key and its
/// order. Starts from `{}` when `existing` is `None`.
pub fn patch_value(existing: Option<Value>, key: &str, value: Value) -> Result<Value> {
    let mut root = existing.unwrap_or_else(|| Value::Object(Default::default()));
    root.as_object_mut()
        .ok_or_else(|| anyhow!("expected a JSON object"))?
        .insert(key.to_string(), value);
    Ok(root)
}

/// Set a single top-level `key` to `value` in a JSON object file, preserving
/// every other key. Creates the file as `{key: value}` if it is absent.
pub fn patch_json(path: &Path, key: &str, value: Value) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn patch_value_inserts_and_replaces_in_place() {
        assert_eq!(patch_value(None, "a", json!(1)).unwrap(), json!({"a": 1}));
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
}
