//! Rate-limit usage: fetching it from Anthropic's OAuth usage endpoint (the same
//! one Claude Code's `/usage` uses) and the daemon's last observation, persisted
//! next to `state.json` as `usage.json` so `cs usage` can read it from any shell.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{state, util};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// One rate-limit window as reported by the API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// 0–100.
    pub utilization: f64,
    /// RFC 3339 timestamp of the next reset.
    pub resets_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub five_hour: Window,
    #[serde(default)]
    pub seven_day: Option<Window>,
}

/// What we know about one account's usage after a poll.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Reading {
    Known(Usage),
    Unknown { reason: String },
}

impl Reading {
    pub fn known(&self) -> Option<&Usage> {
        match self {
            Reading::Known(u) => Some(u),
            Reading::Unknown { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub reading: Reading,
    /// Epoch seconds when this alias last stopped being (or became) current.
    #[serde(default)]
    pub last_active_at: Option<i64>,
}

/// The daemon's last poll of every saved Claude account. All times are epoch seconds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub observed_at: i64,
    #[serde(default)]
    pub current: Option<String>,
    #[serde(default)]
    pub current_since: Option<i64>,
    #[serde(default)]
    pub last_switch_at: Option<i64>,
    #[serde(default)]
    pub accounts: BTreeMap<String, Entry>,
}

impl Observation {
    pub fn load() -> Result<Option<Observation>> {
        Self::load_from(&usage_path()?)
    }

    pub fn load_from(path: &Path) -> Result<Option<Observation>> {
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Ok(Some(
            serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?,
        ))
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&usage_path()?)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        util::atomic_write(path, &serde_json::to_vec_pretty(self)?)
    }

    pub fn reading(&self, alias: &str) -> Option<&Reading> {
        self.accounts.get(alias).map(|e| &e.reading)
    }
}

/// `usage.json` lives beside `state.json`.
pub fn usage_path() -> Result<PathBuf> {
    Ok(state::state_path()?.with_file_name("usage.json"))
}

/// Fetch usage for one account's `claudeAiOauth` payload. Skips the network when
/// the access token is already expired (`expiresAt` is epoch millis), since the
/// API would only answer 401.
pub fn poll(oauth: &Value, now: i64) -> Reading {
    let Some(token) = oauth.get("accessToken").and_then(Value::as_str) else {
        return Reading::Unknown {
            reason: "no access token saved".into(),
        };
    };
    if let Some(exp_ms) = oauth.get("expiresAt").and_then(Value::as_i64) {
        if exp_ms / 1000 <= now {
            return Reading::Unknown {
                reason: "token expired (refreshes on next switch)".into(),
            };
        }
    }
    match fetch(token) {
        Ok(u) => Reading::Known(u),
        Err(e) => Reading::Unknown {
            reason: e.to_string(),
        },
    }
}

pub fn fetch(access_token: &str) -> Result<Usage> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .build();
    let resp = agent
        .get(USAGE_URL)
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .set("Accept", "application/json")
        .call();
    let body = match resp {
        Ok(r) => r.into_string().context("reading usage response")?,
        Err(ureq::Error::Status(401, _)) => return Err(anyhow!("token rejected (401)")),
        Err(ureq::Error::Status(code, _)) => return Err(anyhow!("usage API returned HTTP {code}")),
        Err(ureq::Error::Transport(t)) => return Err(anyhow!("network: {t}")),
    };
    parse_usage(&body)
}

pub fn parse_usage(body: &str) -> Result<Usage> {
    let v: Value = serde_json::from_str(body).context("parsing usage response")?;
    let window = |key: &str| -> Option<Window> {
        let w = v.get(key)?;
        Some(Window {
            utilization: w.get("utilization")?.as_f64()?,
            resets_at: w.get("resets_at")?.as_str()?.to_string(),
        })
    };
    Ok(Usage {
        five_hour: window("five_hour").ok_or_else(|| anyhow!("usage response has no five_hour"))?,
        seven_day: window("seven_day"),
    })
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn parse_rfc3339(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp())
}

/// `2h14m`, `45m`, `3d2h`, `<1m`.
pub fn fmt_duration(secs: i64) -> String {
    let secs = secs.max(0);
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    match (d, h, m) {
        (0, 0, 0) => "<1m".into(),
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h{m:02}m"),
        (d, h, _) => format!("{d}d{h}h"),
    }
}

/// Time until `resets_at`, or `?` if it can't be parsed. Past resets read `<1m`.
pub fn fmt_countdown(resets_at: &str, now: i64) -> String {
    parse_rfc3339(resets_at)
        .map(|t| fmt_duration(t - now))
        .unwrap_or_else(|| "?".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Trimmed from a real response; the many null buckets are omitted.
    const FIXTURE: &str = r#"{
        "five_hour": {"utilization": 5.0, "resets_at": "2026-08-23T19:50:00.188677+00:00",
                      "limit_dollars": null},
        "seven_day": {"utilization": 0.0, "resets_at": "2026-08-30T04:00:00.188701+00:00"},
        "seven_day_opus": null,
        "extra_usage": {"is_enabled": false}
    }"#;

    #[test]
    fn parses_real_response_shape() {
        let u = parse_usage(FIXTURE).unwrap();
        assert_eq!(u.five_hour.utilization, 5.0);
        assert_eq!(u.five_hour.resets_at, "2026-08-23T19:50:00.188677+00:00");
        assert_eq!(u.seven_day.unwrap().utilization, 0.0);
    }

    #[test]
    fn missing_five_hour_errors_but_seven_day_is_optional() {
        assert!(parse_usage(r#"{"seven_day": null}"#).is_err());
        let u = parse_usage(r#"{"five_hour": {"utilization": 1, "resets_at": "x"}}"#).unwrap();
        assert!(u.seven_day.is_none());
    }

    #[test]
    fn expired_token_is_unknown_without_network() {
        let oauth = json!({"accessToken": "t", "expiresAt": 1_000_000});
        assert!(matches!(poll(&oauth, 2_000), Reading::Unknown { .. }));
        assert!(matches!(poll(&json!({}), 0), Reading::Unknown { .. }));
    }

    #[test]
    fn observation_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        let mut obs = Observation {
            observed_at: 100,
            current: Some("a".into()),
            ..Default::default()
        };
        obs.accounts.insert(
            "a".into(),
            Entry {
                reading: Reading::Known(parse_usage(FIXTURE).unwrap()),
                last_active_at: Some(90),
            },
        );
        obs.accounts.insert(
            "b".into(),
            Entry {
                reading: Reading::Unknown {
                    reason: "x".into(),
                },
                last_active_at: None,
            },
        );
        obs.save_to(&path).unwrap();
        assert_eq!(Observation::load_from(&path).unwrap().unwrap(), obs);
        assert!(Observation::load_from(&dir.path().join("none")).unwrap().is_none());
    }

    #[test]
    fn duration_and_countdown_formatting() {
        assert_eq!(fmt_duration(0), "<1m");
        assert_eq!(fmt_duration(59), "<1m");
        assert_eq!(fmt_duration(45 * 60), "45m");
        assert_eq!(fmt_duration(2 * 3600 + 14 * 60), "2h14m");
        assert_eq!(fmt_duration(3 * 86400 + 2 * 3600 + 5 * 60), "3d2h");
        let t = parse_rfc3339("2026-08-23T19:50:00.188677+00:00").unwrap();
        assert_eq!(fmt_countdown("2026-08-23T19:50:00.188677+00:00", t - 3600), "1h00m");
        assert_eq!(fmt_countdown("garbage", 0), "?");
    }
}
