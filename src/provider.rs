//! The agent whose accounts we're switching. Each variant dispatches to its own
//! credential backend; `cs` defaults to Claude, `--codex` selects Codex.

use anyhow::Result;
use serde_json::Value;

use crate::{claude, codex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }

    /// The flag needed to target this provider, for use in help/error text.
    pub fn flag_suffix(self) -> &'static str {
        match self {
            Provider::Claude => "",
            Provider::Codex => " --codex",
        }
    }

    /// Drive the agent's interactive login.
    pub fn login(self) -> Result<()> {
        match self {
            Provider::Claude => claude::login(),
            Provider::Codex => codex::login(),
        }
    }

    /// Read the live account as `(email, provider-specific data)`.
    pub fn capture(self) -> Result<(String, Value)> {
        match self {
            Provider::Claude => claude::capture(),
            Provider::Codex => codex::capture(),
        }
    }

    /// Make a saved account's data the live account.
    pub fn restore(self, data: &Value) -> Result<()> {
        match self {
            Provider::Claude => claude::restore(data),
            Provider::Codex => codex::restore(data),
        }
    }

    /// The live account's email, or `None` if not signed in.
    pub fn live_email(self) -> Result<Option<String>> {
        match self {
            Provider::Claude => claude::live_email(),
            Provider::Codex => codex::live_email(),
        }
    }
}
