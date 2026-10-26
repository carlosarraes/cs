use crate::util;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub routing: Routing,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Routing {
    pub port: u16,
    pub poll_secs: u64,
    pub report_secs: u64,
    pub max_usage_age_secs: u64,
    pub five_hour_ceiling: f64,
    pub weekly_ceiling: f64,
}
impl Default for Routing {
    fn default() -> Self {
        Self {
            port: 18437,
            poll_secs: 60,
            report_secs: 300,
            max_usage_age_secs: 300,
            five_hour_ceiling: 90.,
            weekly_ceiling: 95.,
        }
    }
}
pub const DEFAULT_TOML:&str="[routing]\nport = 18437\npoll_secs = 60\nreport_secs = 300\nmax_usage_age_secs = 300\nfive_hour_ceiling = 90\nweekly_ceiling = 95\n";
pub fn config_home() -> Result<PathBuf> {
    Ok(match std::env::var_os("XDG_CONFIG_HOME") {
        Some(d) if !d.is_empty() && Path::new(&d).is_absolute() => PathBuf::from(d),
        _ => util::home()?.join(".config"),
    })
}
pub fn config_path() -> Result<PathBuf> {
    Ok(config_home()?.join("cs/config.toml"))
}
pub fn load() -> Result<Config> {
    load_from(&config_path()?)
}
fn load_from(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }
    let mut value: toml::Value =
        toml::from_str(&fs::read_to_string(path)?).context("parsing cs config")?;
    if let Some(table) = value.as_table_mut() {
        let mut obsolete = false;
        for key in ["auto-switcher", "report", "warmup"] {
            obsolete |= table.remove(key).is_some();
        }
        if obsolete {
            eprintln!("cs: obsolete auto-switcher/report/warmup settings ignored; configure [routing] instead");
        }
    }
    let cfg: Config = value.try_into()?;
    let r = &cfg.routing;
    if r.poll_secs == 0
        || r.max_usage_age_secs < r.poll_secs
        || r.max_usage_age_secs > 86400
        || ![r.five_hour_ceiling, r.weekly_ceiling]
            .iter()
            .all(|x| x.is_finite() && *x > 0. && *x <= 100.)
    {
        bail!("invalid routing settings: positive poll_secs <= max_usage_age_secs <= 86400 and ceilings in (0, 100] required");
    }
    Ok(cfg)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_switch_settings_never_enable_global_switching() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        fs::write(
            &p,
            "[auto-switcher]\nenabled=true\nswitch_at=1\n[routing]\nport=12345\n",
        )
        .unwrap();
        let c = load_from(&p).unwrap();
        assert_eq!(c.routing.port, 12345);
        assert_eq!(c.routing.five_hour_ceiling, 90.);
        fs::write(&p, "[routing]\npoll_secs=0").unwrap();
        assert!(load_from(&p).is_err());
    }
    #[test]
    fn defaults_and_unknown_settings() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        assert_eq!(load_from(&p).unwrap().routing.port, 18437);
        fs::write(&p, DEFAULT_TOML).unwrap();
        assert_eq!(load_from(&p).unwrap().routing.weekly_ceiling, 95.);
        fs::write(&p, "[routing]\nwrong=true").unwrap();
        assert!(load_from(&p).is_err());
    }
}
