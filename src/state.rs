//! Persistent alias store: `$XDG_DATA_HOME/cs/state.json` (default
//! `~/.local/share/cs/state.json`), written atomically with mode 600.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::claude;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub current: Option<String>,
    #[serde(default)]
    pub previous: Option<String>,
    #[serde(default)]
    pub accounts: BTreeMap<String, Account>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub email: String,
    #[serde(rename = "claudeAiOauth")]
    pub claude_ai_oauth: Value,
    #[serde(rename = "oauthAccount")]
    pub oauth_account: Value,
}

/// `$XDG_DATA_HOME/cs/state.json`, falling back to `~/.local/share/cs/state.json`.
pub fn state_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() && Path::new(&d).is_absolute() => PathBuf::from(d),
        _ => dirs::home_dir()
            .ok_or_else(|| anyhow!("could not determine home directory"))?
            .join(".local/share"),
    };
    Ok(base.join("cs").join("state.json"))
}

impl State {
    pub fn load() -> Result<State> {
        let path = state_path()?;
        if !path.exists() {
            return Ok(State::default());
        }
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        if bytes.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(State::default());
        }
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = state_path()?;
        let data = serde_json::to_vec_pretty(self)?;
        claude::atomic_write(&path, &data)
    }
}
