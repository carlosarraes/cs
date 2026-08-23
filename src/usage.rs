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

use crate::state::{self, ProviderState};
use crate::{claude, util};

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

/// Poll every saved Claude account. The current one is polled with the live
/// token (Claude Code keeps it fresh); the others with their stored tokens.
/// `prev` carries the running/idle timers forward.
pub fn observe(ps: &ProviderState, prev: Option<&Observation>, now: i64) -> Observation {
    let mut obs = Observation {
        observed_at: now,
        current: ps.current.clone(),
        ..Default::default()
    };
    let live = claude::live_oauth().ok().flatten();
    for (alias, acct) in &ps.accounts {
        let is_current = ps.current.as_deref() == Some(alias.as_str());
        let oauth = match (&live, is_current) {
            (Some(l), true) => Some(l.clone()),
            _ => acct.data.get("claudeAiOauth").cloned(),
        };
        let reading = match oauth {
            Some(o) => poll(&o, now),
            None => Reading::Unknown {
                reason: "no claudeAiOauth saved".into(),
            },
        };
        obs.accounts.insert(
            alias.clone(),
            Entry {
                reading,
                last_active_at: None,
            },
        );
    }
    carry_timers(prev, &mut obs, now);
    obs
}

/// Carry `current_since` / `last_switch_at` / per-alias `last_active_at` from the
/// previous observation, stamping `now` wherever the current alias changed.
pub fn carry_timers(prev: Option<&Observation>, obs: &mut Observation, now: i64) {
    let Some(prev) = prev else {
        return;
    };
    obs.last_switch_at = prev.last_switch_at;
    if prev.current == obs.current {
        obs.current_since = prev.current_since;
    } else {
        obs.current_since = Some(now);
        obs.last_switch_at = Some(now);
    }
    for (alias, entry) in &mut obs.accounts {
        if obs.current.as_deref() == Some(alias.as_str()) {
            continue;
        }
        entry.last_active_at = if prev.current.as_deref() == Some(alias.as_str()) {
            Some(now)
        } else {
            prev.accounts.get(alias).and_then(|e| e.last_active_at)
        };
    }
}

/// One line per account, e.g. `* carraes  45%  resets 2h14m  7d 12%  running 3h02m`.
/// Markers: `*` current, `-` other, `~` unknown.
pub fn format_lines(obs: &Observation, now: i64) -> Vec<String> {
    let width = obs.accounts.keys().map(String::len).max().unwrap_or(0);
    obs.accounts
        .iter()
        .map(|(alias, e)| {
            let is_current = obs.current.as_deref() == Some(alias.as_str());
            let since = if is_current {
                match obs.current_since {
                    Some(t) => format!("running {}", fmt_duration(now - t)),
                    None => "running".into(),
                }
            } else {
                match e.last_active_at {
                    Some(t) => format!("idle {}", fmt_duration(now - t)),
                    None => "idle".into(),
                }
            };
            match &e.reading {
                Reading::Known(u) => {
                    let seven = u
                        .seven_day
                        .as_ref()
                        .map(|w| format!("  7d {:>3.0}%", w.utilization))
                        .unwrap_or_default();
                    format!(
                        "{} {alias:<width$}  {:>3.0}%  resets {:<6}{seven}  {since}",
                        if is_current { '*' } else { '-' },
                        u.five_hour.utilization,
                        fmt_countdown(&u.five_hour.resets_at, now),
                    )
                }
                Reading::Unknown { reason } => {
                    format!("~ {alias:<width$}   ??   {reason}  {since}")
                }
            }
        })
        .collect()
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

    const FIXTURE_RESET: &str = "2026-08-23T19:50:00.188677+00:00";
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
    fn timers_carry_and_stamp_on_switch() {
        let mut prev = Observation {
            observed_at: 100,
            current: Some("a".into()),
            current_since: Some(50),
            last_switch_at: Some(50),
            ..Default::default()
        };
        for k in ["a", "b", "c"] {
            prev.accounts.insert(
                k.into(),
                Entry {
                    reading: Reading::Unknown { reason: "x".into() },
                    last_active_at: if k == "c" { Some(10) } else { None },
                },
            );
        }
        // Same current: everything carries.
        let mut same = prev.clone();
        same.current_since = None;
        same.last_switch_at = None;
        carry_timers(Some(&prev), &mut same, 200);
        assert_eq!(same.current_since, Some(50));
        assert_eq!(same.last_switch_at, Some(50));
        assert_eq!(same.accounts["c"].last_active_at, Some(10));
        // Switched a → b: b starts now, a stopped now, c untouched.
        let mut next = prev.clone();
        next.current = Some("b".into());
        carry_timers(Some(&prev), &mut next, 200);
        assert_eq!(next.current_since, Some(200));
        assert_eq!(next.last_switch_at, Some(200));
        assert_eq!(next.accounts["a"].last_active_at, Some(200));
        assert_eq!(next.accounts["c"].last_active_at, Some(10));
        // No previous observation: nothing to carry.
        let mut fresh = prev.clone();
        fresh.current_since = None;
        carry_timers(None, &mut fresh, 200);
        assert_eq!(fresh.current_since, None);
    }

    #[test]
    fn format_lines_marks_current_and_unknown() {
        let t = parse_rfc3339(FIXTURE_RESET).unwrap();
        let now = t - 3600;
        let mut obs = Observation {
            observed_at: now,
            current: Some("aa".into()),
            current_since: Some(now - 3 * 3600),
            ..Default::default()
        };
        obs.accounts.insert(
            "aa".into(),
            Entry {
                reading: Reading::Known(parse_usage(FIXTURE).unwrap()),
                last_active_at: None,
            },
        );
        obs.accounts.insert(
            "b".into(),
            Entry {
                reading: Reading::Unknown { reason: "token expired".into() },
                last_active_at: Some(now - 3 * 3600),
            },
        );
        let lines = format_lines(&obs, now);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("* aa "), "{}", lines[0]);
        assert!(lines[0].contains("  5%  resets 1h00m"), "{}", lines[0]);
        assert!(lines[0].contains("running 3h00m"), "{}", lines[0]);
        assert!(lines[1].starts_with("~ b  "), "{}", lines[1]);
        assert!(lines[1].contains("token expired"), "{}", lines[1]);
        assert!(lines[1].contains("idle 3h00m"), "{}", lines[1]);
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
