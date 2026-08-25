//! Daemon configuration: `$XDG_CONFIG_HOME/cs/config.toml` (default
//! `~/.config/cs/config.toml`). Every key is optional; a missing file means
//! all defaults.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

use crate::util;

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
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
    /// Switch once the current account's 5h usage reaches this (%). Every switch
    /// costs a full uncached re-read of every open session's context on the new
    /// account, so switch as late as safely possible.
    pub switch_at: u8,
    /// Never switch TO an account at or above this 5h usage (%). A switch burns
    /// ~15% rebuilding the cache, so a target needs real headroom to be worth it.
    pub ceiling: u8,
    /// Don't pay for a switch just to bridge a short gap: stay if the current
    /// account's 5h window resets within this many seconds.
    pub skip_if_reset_within_secs: u64,
    /// Wait for every live Claude Code session to go idle before switching
    /// (a swap between turns is clean), up to `idle_wait_max_secs`.
    pub prefer_idle: bool,
    pub idle_wait_max_secs: u64,
    /// How often the current account is polled.
    pub poll_secs: u64,
    /// How often the other accounts are polled (only the current one drives the
    /// trigger, and the usage API has a small request budget).
    pub idle_poll_secs: u64,
    /// Never switch twice within this window (anti-flap).
    pub min_switch_gap_secs: u64,
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

impl Default for AutoSwitcher {
    fn default() -> Self {
        AutoSwitcher {
            enabled: true,
            switch_at: 95,
            ceiling: 75,
            skip_if_reset_within_secs: 1800,
            prefer_idle: true,
            idle_wait_max_secs: 300,
            poll_secs: 60,
            idle_poll_secs: 300,
            min_switch_gap_secs: 900,
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
# Switch once the current account's 5h usage reaches this (%). Every switch
# costs a full uncached re-read of each open session's context on the new
# account (~15% of a window), so switch as late as safely possible.
switch_at = 95
# Never switch TO an account at or above this 5h usage (%). A target needs
# ~25% headroom for the switch to gain anything.
ceiling = 75
# Don't pay for a switch just to bridge a short gap: stay if the current
# account's window resets within this many seconds.
skip_if_reset_within_secs = 1800
# Wait for every live Claude Code session to go idle before switching, up to
# idle_wait_max_secs.
prefer_idle = true
idle_wait_max_secs = 300
# How often the current account is polled. The usage API rate-limits bursts
# (it answers 429 + Retry-After, which cs honors), so don't go much lower.
poll_secs = 60
# How often the other accounts are polled; only the current one drives the trigger.
idle_poll_secs = 300
# Never switch twice within this window (anti-flap).
min_switch_gap_secs = 900

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

/// `$XDG_CONFIG_HOME`, falling back to `~/.config`.
pub fn config_home() -> Result<PathBuf> {
    Ok(match std::env::var_os("XDG_CONFIG_HOME") {
        Some(d) if !d.is_empty() && Path::new(&d).is_absolute() => PathBuf::from(d),
        _ => util::home()?.join(".config"),
    })
}

/// `$XDG_CONFIG_HOME/cs/config.toml`, falling back to `~/.config/cs/config.toml`.
pub fn config_path() -> Result<PathBuf> {
    Ok(config_home()?.join("cs").join("config.toml"))
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
    anyhow::ensure!(
        (1..=100).contains(&cfg.auto_switcher.switch_at),
        "auto-switcher.switch_at must be 1..=100"
    );
    anyhow::ensure!(
        cfg.auto_switcher.ceiling < cfg.auto_switcher.switch_at,
        "auto-switcher.ceiling must be below switch_at"
    );
    anyhow::ensure!(
        cfg.auto_switcher.poll_secs > 0,
        "auto-switcher.poll_secs must be > 0"
    );
    anyhow::ensure!(
        cfg.auto_switcher.idle_poll_secs > 0,
        "auto-switcher.idle_poll_secs must be > 0"
    );
    anyhow::ensure!(
        cfg.report.interval_secs > 0,
        "report.interval_secs must be > 0"
    );
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_from(&dir.path().join("nope.toml")).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let cfg = parse("[auto-switcher]\nswitch_at = 90\n\n[report]\nverbose = true\n").unwrap();
        assert_eq!(cfg.auto_switcher.switch_at, 90);
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
        assert!(parse("[auto-switcher]\nswitch_at = 0\n").is_err());
        assert!(parse("[auto-switcher]\nswitch_at = 70\n").is_err()); // below default ceiling
        assert!(parse("[auto-switcher]\nswitch_at = \"x\"\n").is_err());
        assert!(parse("[auto-switcher]\nbogus = 1\n").is_err());
        assert!(parse("not toml ===").is_err());
    }
}
