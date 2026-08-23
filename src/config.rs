//! Daemon configuration: `$XDG_CONFIG_HOME/cs/config.toml` (default
//! `~/.config/cs/config.toml`). Every key is optional; a missing file means
//! all defaults.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

use crate::util;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    #[serde(rename = "auto-switcher")]
    pub auto_switcher: AutoSwitcher,
    pub report: Report,
    pub warmup: Warmup,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AutoSwitcher {
    pub enabled: bool,
    /// Switch when the current account's 5h usage crosses a multiple of this (%).
    pub step: u8,
    pub poll_secs: u64,
    /// Never switch twice within this window (anti-flap).
    pub min_switch_gap_secs: u64,
    /// Never switch TO an account at or above this 5h usage (%).
    pub ceiling: u8,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Report {
    pub enabled: bool,
    pub interval_secs: u64,
    pub verbose: bool,
}

/// Reserved for the warmup feature; parsed but not acted on yet.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Warmup {
    pub enabled: bool,
    pub model: String,
    pub effort: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            auto_switcher: AutoSwitcher::default(),
            report: Report::default(),
            warmup: Warmup::default(),
        }
    }
}

impl Default for AutoSwitcher {
    fn default() -> Self {
        AutoSwitcher {
            enabled: true,
            step: 15,
            poll_secs: 60,
            min_switch_gap_secs: 300,
            ceiling: 90,
        }
    }
}

impl Default for Report {
    fn default() -> Self {
        Report {
            enabled: true,
            interval_secs: 300,
            verbose: false,
        }
    }
}

impl Default for Warmup {
    fn default() -> Self {
        Warmup {
            enabled: false,
            model: "opus-5".into(),
            effort: "low".into(),
        }
    }
}

/// The default config, written by `cs daemon install` when none exists.
pub const DEFAULT_TOML: &str = r#"# cs daemon configuration — every key is optional.

[auto-switcher]
enabled = true
# Switch when the current account's 5h usage crosses a multiple of `step` (%).
step = 15
poll_secs = 60
# Never switch twice within this window (anti-flap).
min_switch_gap_secs = 300
# Never switch TO an account at or above this 5h usage (%).
ceiling = 90

[report]
enabled = true
interval_secs = 300
verbose = false

# Reserved — not implemented yet.
[warmup]
enabled = false
model = "opus-5"
effort = "low"
"#;

/// `$XDG_CONFIG_HOME/cs/config.toml`, falling back to `~/.config/cs/config.toml`.
pub fn config_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(d) if !d.is_empty() && Path::new(&d).is_absolute() => PathBuf::from(d),
        _ => util::home()?.join(".config"),
    };
    Ok(base.join("cs").join("config.toml"))
}

pub fn load() -> Result<Config> {
    load_from(&config_path()?)
}

pub fn load_from(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse(&text).with_context(|| format!("parsing {}", path.display()))
}

fn parse(text: &str) -> Result<Config> {
    let cfg: Config = toml::from_str(text)?;
    anyhow::ensure!(cfg.auto_switcher.step > 0, "auto-switcher.step must be > 0");
    anyhow::ensure!(cfg.auto_switcher.poll_secs > 0, "auto-switcher.poll_secs must be > 0");
    anyhow::ensure!(cfg.report.interval_secs > 0, "report.interval_secs must be > 0");
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_from(&dir.path().join("nope.toml")).unwrap(), Config::default());
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let cfg = parse("[auto-switcher]\nstep = 20\n\n[report]\nverbose = true\n").unwrap();
        assert_eq!(cfg.auto_switcher.step, 20);
        assert_eq!(cfg.auto_switcher.poll_secs, 60);
        assert!(cfg.report.verbose);
        assert_eq!(cfg.report.interval_secs, 300);
        assert!(!cfg.warmup.enabled);
    }

    #[test]
    fn default_toml_round_trips_to_default_config() {
        assert_eq!(parse(DEFAULT_TOML).unwrap(), Config::default());
    }

    #[test]
    fn invalid_values_error() {
        assert!(parse("[auto-switcher]\nstep = 0\n").is_err());
        assert!(parse("[auto-switcher]\nstep = \"x\"\n").is_err());
        assert!(parse("[auto-switcher]\nbogus = 1\n").is_err());
        assert!(parse("not toml ===").is_err());
    }
}
