//! Pure auto-switch policy: no I/O, no clock — everything comes in as arguments
//! so it can be tested exhaustively.
//!
//! Rule: every switch costs a full uncached re-read of each open session's
//! context on the new account (~15% of a 5h window), so switch as *rarely* as
//! possible: only once the current account reaches `switch_at`, only to an
//! account with real headroom (below `ceiling`), not when the current window
//! is about to reset anyway, and preferably between turns.

use crate::config::AutoSwitcher;
use crate::usage::{self, Observation, Reading};

/// Daemon memory carried between ticks.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Mem {
    pub last_switch_at: Option<i64>,
    /// When we first wanted to switch but held off for busy sessions.
    pub waiting_since: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Stay(Option<String>),
    Switch { to: String, reason: String },
}

/// Least-used Known account below `ceiling`, excluding `exclude`. Ties go to the
/// alphabetically first alias (BTreeMap order).
pub fn pick_target<'a>(
    obs: &'a Observation,
    ceiling: u8,
    exclude: Option<&str>,
) -> Option<&'a str> {
    obs.accounts
        .iter()
        .filter(|(alias, _)| Some(alias.as_str()) != exclude)
        .filter_map(|(alias, e)| e.reading.known().map(|u| (alias, u.five_hour.utilization)))
        .filter(|(_, util)| *util < f64::from(ceiling))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(alias, _)| alias.as_str())
}

/// Expired-token accounts as a last resort: their real usage is unknown, but
/// Claude Code refreshes the token as soon as they become current, so they're
/// better than being stuck. Least-recently-active first (most likely to have
/// reset); other Unknown reasons (429, network) stay ineligible.
pub fn pick_expired_fallback<'a>(obs: &'a Observation, exclude: Option<&str>) -> Option<&'a str> {
    obs.accounts
        .iter()
        .filter(|(alias, _)| Some(alias.as_str()) != exclude)
        .filter(|(_, e)| {
            matches!(&e.reading, Reading::Unknown { reason } if reason == usage::TOKEN_EXPIRED)
        })
        .min_by_key(|(alias, e)| (e.last_active_at.unwrap_or(i64::MIN), alias.as_str()))
        .map(|(alias, _)| alias.as_str())
}

/// `busy` = some live Claude Code session is mid-turn right now.
pub fn decide(
    obs: &Observation,
    mem: &mut Mem,
    cfg: &AutoSwitcher,
    now: i64,
    busy: bool,
) -> Decision {
    let Some(current) = obs.current.as_deref() else {
        return Decision::Stay(Some("no current account set (run `cs add`)".into()));
    };
    let reading = match obs.reading(current) {
        Some(Reading::Known(u)) => u,
        // The daemon already logs Unknown transitions; nothing to add here.
        Some(Reading::Unknown { .. }) => return Decision::Stay(None),
        None => return Decision::Stay(Some(format!("{current}: not polled"))),
    };
    let util = reading.five_hour.utilization;
    if util < f64::from(cfg.switch_at) {
        mem.waiting_since = None;
        return Decision::Stay(None);
    }

    // Stable texts (no countdowns) so the daemon logs each reason once, not every tick.
    let reset_in = reading
        .five_hour
        .resets_at
        .as_deref()
        .and_then(usage::parse_rfc3339)
        .map(|t| t - now);
    if reset_in.is_some_and(|secs| secs <= cfg.skip_if_reset_within_secs as i64) {
        mem.waiting_since = None;
        return Decision::Stay(Some(format!(
            "{current} at {util:.0}% but its window resets within {} — riding it out",
            usage::fmt_duration(cfg.skip_if_reset_within_secs as i64)
        )));
    }
    if let Some(last) = mem.last_switch_at {
        if last + cfg.min_switch_gap_secs as i64 > now {
            return Decision::Stay(Some(format!(
                "{current} at {util:.0}% but the last switch was under {}s ago — waiting",
                cfg.min_switch_gap_secs
            )));
        }
    }
    let target = match pick_target(obs, cfg.ceiling, Some(current)) {
        Some(to) => {
            let to_util = obs
                .reading(to)
                .and_then(Reading::known)
                .map(|u| u.five_hour.utilization)
                .unwrap_or(0.0);
            (to, format!("{current} {util:.0}% · {to} {to_util:.0}%"))
        }
        None => match pick_expired_fallback(obs, Some(current)) {
            Some(to) => (
                to,
                format!("{current} {util:.0}% · {to} token expired, usage unknown"),
            ),
            None => {
                mem.waiting_since = None;
                return Decision::Stay(Some(format!(
                    "{current} at {util:.0}% but no other account is known and below {}% — a switch would gain nothing",
                    cfg.ceiling
                )));
            }
        },
    };
    if cfg.prefer_idle && busy {
        let since = *mem.waiting_since.get_or_insert(now);
        if now - since < cfg.idle_wait_max_secs as i64 {
            return Decision::Stay(Some(format!(
                "{current} at {util:.0}% — waiting for sessions to go idle before switching to {}",
                target.0
            )));
        }
    }
    mem.waiting_since = None;
    Decision::Switch {
        to: target.0.to_string(),
        reason: target.1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{Entry, Usage, Window};

    const NOW: i64 = 1_787_500_000;
    /// A reset comfortably far away (2h).
    const FAR: i64 = NOW + 7200;

    fn reset(at: i64) -> String {
        chrono::DateTime::from_timestamp(at, 0)
            .unwrap()
            .to_rfc3339()
    }

    fn known_at(util: f64, resets_at: i64) -> Entry {
        Entry::new(
            Reading::Known(Usage {
                five_hour: Window {
                    utilization: util,
                    resets_at: Some(reset(resets_at)),
                },
                seven_day: None,
            }),
            0,
        )
    }

    fn known(util: f64) -> Entry {
        known_at(util, FAR)
    }

    fn unknown() -> Entry {
        Entry::new(
            Reading::Unknown {
                reason: "rate limited by usage API (429)".into(),
            },
            0,
        )
    }

    fn obs(current: &str, accounts: &[(&str, Entry)]) -> Observation {
        let mut o = Observation {
            current: Some(current.into()),
            ..Default::default()
        };
        for (a, e) in accounts {
            o.accounts.insert((*a).into(), e.clone());
        }
        o
    }

    fn cfg() -> AutoSwitcher {
        AutoSwitcher::default() // switch_at 95, ceiling 75, skip 1800s, gap 900s, idle wait 300s
    }

    fn switch_to(d: Decision) -> String {
        match d {
            Decision::Switch { to, .. } => to,
            other => panic!("expected switch, got {other:?}"),
        }
    }

    #[test]
    fn stays_below_switch_at() {
        let o = obs("a", &[("a", known(94.9)), ("b", known(0.0))]);
        let mut m = Mem::default();
        assert_eq!(decide(&o, &mut m, &cfg(), NOW, false), Decision::Stay(None));
    }

    #[test]
    fn switches_at_threshold_to_least_used() {
        let o = obs(
            "a",
            &[("a", known(95.0)), ("b", known(40.0)), ("c", known(10.0))],
        );
        let mut m = Mem::default();
        assert_eq!(switch_to(decide(&o, &mut m, &cfg(), NOW, false)), "c");
    }

    #[test]
    fn target_needs_real_headroom() {
        // b at 80% is above the 75% ceiling: a switch would burn more than it gains.
        let o = obs("a", &[("a", known(97.0)), ("b", known(80.0))]);
        let mut m = Mem::default();
        assert!(matches!(
            decide(&o, &mut m, &cfg(), NOW, false),
            Decision::Stay(Some(r)) if r.contains("gain nothing")
        ));
        let o = obs("a", &[("a", known(97.0)), ("b", known(74.0))]);
        assert_eq!(switch_to(decide(&o, &mut m, &cfg(), NOW, false)), "b");
    }

    #[test]
    fn rides_out_an_imminent_reset() {
        let o = obs(
            "a",
            &[("a", known_at(96.0, NOW + 20 * 60)), ("b", known(0.0))],
        );
        let mut m = Mem::default();
        assert!(matches!(
            decide(&o, &mut m, &cfg(), NOW, false),
            Decision::Stay(Some(r)) if r.contains("riding it out")
        ));
        // 31 minutes out is past the 30-minute grace: switch.
        let o = obs(
            "a",
            &[("a", known_at(96.0, NOW + 31 * 60)), ("b", known(0.0))],
        );
        assert_eq!(switch_to(decide(&o, &mut m, &cfg(), NOW, false)), "b");
    }

    #[test]
    fn gap_blocks_then_allows() {
        let o = obs("a", &[("a", known(96.0)), ("b", known(0.0))]);
        let mut m = Mem {
            last_switch_at: Some(NOW - 100),
            ..Default::default()
        };
        assert!(matches!(
            decide(&o, &mut m, &cfg(), NOW, false),
            Decision::Stay(Some(_))
        ));
        m.last_switch_at = Some(NOW - 901);
        assert_eq!(switch_to(decide(&o, &mut m, &cfg(), NOW, false)), "b");
    }

    #[test]
    fn waits_for_idle_then_gives_up_waiting() {
        let o = obs("a", &[("a", known(96.0)), ("b", known(0.0))]);
        let mut m = Mem::default();
        assert!(matches!(
            decide(&o, &mut m, &cfg(), NOW, true),
            Decision::Stay(Some(r)) if r.contains("idle")
        ));
        assert_eq!(m.waiting_since, Some(NOW));
        // Still busy 4 minutes later: keep waiting.
        assert!(matches!(
            decide(&o, &mut m, &cfg(), NOW + 240, true),
            Decision::Stay(Some(_))
        ));
        // Sessions go idle: switch immediately.
        assert_eq!(switch_to(decide(&o, &mut m, &cfg(), NOW + 250, false)), "b");
        assert_eq!(m.waiting_since, None);
        // Or, busy past the cap: switch anyway.
        let mut m = Mem {
            waiting_since: Some(NOW - 301),
            ..Default::default()
        };
        assert_eq!(switch_to(decide(&o, &mut m, &cfg(), NOW, true)), "b");
        // prefer_idle off: busy is ignored.
        let mut c = cfg();
        c.prefer_idle = false;
        let mut m = Mem::default();
        assert_eq!(switch_to(decide(&o, &mut m, &c, NOW, true)), "b");
    }

    #[test]
    fn expired_token_is_last_resort_only() {
        let expired = |last: Option<i64>| Entry {
            last_active_at: last,
            ..Entry::new(
                Reading::Unknown {
                    reason: usage::TOKEN_EXPIRED.into(),
                },
                0,
            )
        };
        let o = obs(
            "a",
            &[("a", known(96.0)), ("b", expired(None)), ("c", known(50.0))],
        );
        let mut m = Mem::default();
        assert_eq!(switch_to(decide(&o, &mut m, &cfg(), NOW, false)), "c");
        let o = obs(
            "a",
            &[
                ("a", known(96.0)),
                ("b", expired(Some(500))),
                ("c", expired(Some(100))),
            ],
        );
        assert_eq!(switch_to(decide(&o, &mut m, &cfg(), NOW, false)), "c");
        let o = obs("a", &[("a", known(96.0)), ("b", unknown())]);
        assert!(matches!(
            decide(&o, &mut m, &cfg(), NOW, false),
            Decision::Stay(Some(_))
        ));
    }

    #[test]
    fn single_account_or_unknown_current_stays() {
        let o = obs("a", &[("a", known(99.0))]);
        let mut m = Mem::default();
        assert!(matches!(
            decide(&o, &mut m, &cfg(), NOW, false),
            Decision::Stay(Some(_))
        ));
        let o = obs("a", &[("a", unknown()), ("b", known(0.0))]);
        assert_eq!(decide(&o, &mut m, &cfg(), NOW, false), Decision::Stay(None));
        let none = Observation::default();
        assert!(matches!(
            decide(&none, &mut m, &cfg(), NOW, false),
            Decision::Stay(Some(_))
        ));
    }
}
