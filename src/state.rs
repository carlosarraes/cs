//! Persistent alias store: `$XDG_DATA_HOME/cs/state.json` (default
//! `~/.local/share/cs/state.json`), written atomically with mode 600.
//!
//! Accounts are split per provider so `work` on Claude and `work` on Codex are
//! independent.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::provider::Provider;
use crate::util;

/// A saved account: a display email plus a provider-specific credential payload
/// (Claude: `{claudeAiOauth, oauthAccount}`; Codex: the raw `auth.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub email: String,
    pub data: Value,
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
        if bytes.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(State::default());
        }
        let v: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        // Migrate the legacy flat schema (a bare `{current, previous, accounts}`
        // for Claude) into `providers.claude`.
        if v.get("providers").is_none()
            && (v.get("accounts").is_some() || v.get("current").is_some())
        {
            let claude: ProviderState =
                serde_json::from_value(v).context("migrating legacy state.json")?;
            return Ok(State {
                providers: Providers {
                    claude,
                    ..Default::default()
                },
            });
        }
        serde_json::from_value(v).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        util::atomic_write(&state_path()?, &serde_json::to_vec_pretty(self)?)
    }
}

/// `$XDG_DATA_HOME/cs/state.json`, falling back to `~/.local/share/cs/state.json`.
pub fn state_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() && Path::new(&d).is_absolute() => PathBuf::from(d),
        _ => util::home()?.join(".local/share"),
    };
    Ok(base.join("cs").join("state.json"))
}
