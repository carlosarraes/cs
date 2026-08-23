//! Persistent alias store: `$XDG_DATA_HOME/cs/state.json` (default
//! `~/.local/share/cs/state.json`), written atomically with mode 600.
//!
//! Accounts are split per provider so `work` on Claude and `work` on Codex are
//! independent.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use crate::provider::Provider;
use crate::util;

/// A saved account: a display email plus a provider-specific credential payload
/// (Claude: `{claudeAiOauth, oauthAccount}`; Codex: the raw `auth.json`).
#[derive(Debug, Clone, Serialize)]
pub struct Account {
    pub email: String,
    pub data: Value,
}

/// Deserialize tolerantly: accept the current `{email, data}` shape, and also the
/// legacy `{email, claudeAiOauth, oauthAccount}` (pre-0.1.1) by folding the old
/// fields into `data`. This lets an older state.json migrate without data loss.
impl<'de> Deserialize<'de> for Account {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut v = Value::deserialize(deserializer)?;
        let obj = v.as_object_mut().ok_or_else(|| {
            <D::Error as serde::de::Error>::custom("account must be a JSON object")
        })?;
        let email = obj
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let data = match obj.remove("data") {
            Some(data) => data,
            None => {
                let mut d = serde_json::Map::new();
                for k in ["claudeAiOauth", "oauthAccount"] {
                    if let Some(val) = obj.remove(k) {
                        d.insert(k.to_string(), val);
                    }
                }
                Value::Object(d)
            }
        };
        Ok(Account { email, data })
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ProviderState {
    #[serde(default)]
    pub current: Option<String>,
    #[serde(default)]
    pub previous: Option<String>,
    #[serde(default)]
    pub accounts: BTreeMap<String, Account>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Providers {
    #[serde(default)]
    pub claude: ProviderState,
    #[serde(default)]
    pub codex: ProviderState,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub providers: Providers,
}

impl State {
    pub fn provider(&self, p: Provider) -> &ProviderState {
        match p {
            Provider::Claude => &self.providers.claude,
            Provider::Codex => &self.providers.codex,
        }
    }

    pub fn provider_mut(&mut self, p: Provider) -> &mut ProviderState {
        match p {
            Provider::Claude => &mut self.providers.claude,
            Provider::Codex => &mut self.providers.codex,
        }
    }

    pub fn load() -> Result<State> {
        let path = state_path()?;
        if !path.exists() {
            return Ok(State::default());
        }
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        parse(&bytes).with_context(|| format!("reading {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        util::atomic_write(&state_path()?, &serde_json::to_vec_pretty(self)?)
    }
}

/// Parse `state.json` bytes into `State`, migrating the legacy flat schema
/// (`{current, previous, accounts}` for Claude) into `providers.claude`.
fn parse(bytes: &[u8]) -> Result<State> {
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(State::default());
    }
    let v: Value = serde_json::from_slice(bytes).context("parsing state.json")?;
    if v.get("providers").is_none() && (v.get("accounts").is_some() || v.get("current").is_some()) {
        let claude: ProviderState =
            serde_json::from_value(v).context("migrating legacy state.json")?;
        return Ok(State {
            providers: Providers {
                claude,
                ..Default::default()
            },
        });
    }
    serde_json::from_value(v).context("parsing state.json")
}

/// Held while reading-modifying-writing `state.json`; released on drop. Serialises
/// the daemon against a concurrent `cs switch` so neither overwrites the other's
/// freshly captured tokens.
pub struct LockGuard(#[allow(dead_code)] File);

pub fn lock() -> Result<LockGuard> {
    lock_at(&state_path()?.with_file_name("state.lock"), true)
}

fn lock_at(path: &Path, block: bool) -> Result<LockGuard> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let op = if block {
        libc::LOCK_EX
    } else {
        libc::LOCK_EX | libc::LOCK_NB
    };
    // SAFETY: flock on an fd we own; it only manipulates the kernel lock table.
    if unsafe { libc::flock(file.as_raw_fd(), op) } != 0 {
        return Err(std::io::Error::last_os_error()).context("locking state.json");
    }
    Ok(LockGuard(file))
}

/// `$XDG_DATA_HOME/cs/state.json`, falling back to `~/.local/share/cs/state.json`.
pub fn state_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() && Path::new(&d).is_absolute() => PathBuf::from(d),
        _ => util::home()?.join(".local/share"),
    };
    Ok(base.join("cs").join("state.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn migrates_legacy_flat_state_with_old_accounts() {
        // A pre-0.1.1 state.json: flat schema, `{claudeAiOauth, oauthAccount}` account.
        let legacy = br#"{"current":"work","previous":null,"accounts":{
            "work":{"email":"a@b.com","claudeAiOauth":{"accessToken":"x"},
            "oauthAccount":{"emailAddress":"a@b.com"}}}}"#;
        let state = parse(legacy).unwrap();
        let claude = &state.providers.claude;
        assert_eq!(claude.current.as_deref(), Some("work"));
        let acct = claude.accounts.get("work").unwrap();
        assert_eq!(acct.email, "a@b.com");
        assert_eq!(acct.data["claudeAiOauth"]["accessToken"], json!("x"));
        assert_eq!(acct.data["oauthAccount"]["emailAddress"], json!("a@b.com"));
        assert!(state.providers.codex.accounts.is_empty());
    }

    #[test]
    fn parses_new_provider_schema() {
        let s = br#"{"providers":{"claude":{"current":"w","accounts":{
            "w":{"email":"e","data":{"claudeAiOauth":{},"oauthAccount":{}}}}},
            "codex":{"accounts":{}}}}"#;
        let state = parse(s).unwrap();
        assert_eq!(state.providers.claude.current.as_deref(), Some("w"));
        assert_eq!(state.providers.claude.accounts["w"].email, "e");
    }

    #[test]
    fn blank_state_is_default() {
        assert!(parse(b"   \n")
            .unwrap()
            .providers
            .claude
            .accounts
            .is_empty());
    }

    #[test]
    fn lock_is_exclusive_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.lock");
        let held = lock_at(&path, true).unwrap();
        assert!(lock_at(&path, false).is_err());
        drop(held);
        assert!(lock_at(&path, false).is_ok());
    }

    #[test]
    fn account_accepts_legacy_and_modern_shapes() {
        let legacy: Account =
            serde_json::from_value(json!({"email":"a","claudeAiOauth":{"t":1},"oauthAccount":{}}))
                .unwrap();
        assert_eq!(legacy.data["claudeAiOauth"]["t"], json!(1));
        let modern: Account =
            serde_json::from_value(json!({"email":"a","data":{"foo":1}})).unwrap();
        assert_eq!(modern.data["foo"], json!(1));
    }
}
