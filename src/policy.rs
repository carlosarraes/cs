//! Pure auto-switch policy: no I/O, no clock — everything comes in as arguments
//! so it can be tested exhaustively.
//!
//! Rule: when the current account's 5h usage crosses a multiple of `step`
//! (15 → 30 → 45 …) since we last (re)baselined, switch to the least-used other
//! account that is below `ceiling`. Re-baseline whenever the current alias
//! changes (manual `cs switch`) or its 5h window resets.

use crate::config::AutoSwitcher;
use crate::usage::{self, Observation, Reading};

/// Daemon memory carried between ticks.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Mem {
    pub baseline_alias: Option<String>,
    /// Epoch seconds of the 5h reset the baseline was taken in.
    pub baseline_reset: i64,
    pub baseline_bucket: u32,
    pub last_switch_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Stay(Option<String>),
    Switch { to: String, reason: String },
}

/// Which multiple of `step` `util` has reached: `bucket(29.9, 15) == 1`.
pub fn bucket(util: f64, step: u8) -> u32 {
    (util.max(0.0) / f64::from(step.max(1))).floor() as u32
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

pub fn decide(obs: &Observation, mem: &mut Mem, cfg: &AutoSwitcher, now: i64) -> Decision {
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
    let reset = reading
        .five_hour
        .resets_at
        .as_deref()
        .and_then(usage::parse_rfc3339)
        .unwrap_or(0);
    let b = bucket(util, cfg.step);

    // Resets jitter by sub-second amounts between polls; only a real new window counts.
    let window_changed = (reset - mem.baseline_reset).abs() > 60;
    if mem.baseline_alias.as_deref() != Some(current) || window_changed {
        *mem = Mem {
            baseline_alias: Some(current.to_string()),
            baseline_reset: reset,
            baseline_bucket: b,
            last_switch_at: mem.last_switch_at,
        };
        return Decision::Stay(Some(format!("baseline {current} at {util:.0}%")));
    }
    if b <= mem.baseline_bucket {
        return Decision::Stay(None);
    }
    if let Some(last) = mem.last_switch_at {
        let remaining = last + cfg.min_switch_gap_secs as i64 - now;
        if remaining > 0 {
            // Stable text (no countdown) so the daemon can log it once, not every tick.
            return Decision::Stay(Some(format!(
                "{current} crossed {}% but the last switch was under {}s ago — waiting",
                b * u32::from(cfg.step),
                cfg.min_switch_gap_secs
            )));
        }
    }
    match pick_target(obs, cfg.ceiling, Some(current)) {
        Some(to) => {
            let to_util = obs
                .reading(to)
                .and_then(Reading::known)
                .map(|u| u.five_hour.utilization)
                .unwrap_or(0.0);
            Decision::Switch {
                to: to.to_string(),
                reason: format!("{current} {util:.0}% · {to} {to_util:.0}%"),
            }
        }
        None => match pick_expired_fallback(obs, Some(current)) {
            Some(to) => Decision::Switch {
                to: to.to_string(),
                reason: format!("{current} {util:.0}% · {to} token expired, usage unknown"),
            },
            None => Decision::Stay(Some(format!(
                "{current} crossed {}% but no other account is known and below {}%",
                b * u32::from(cfg.step),
                cfg.ceiling
            ))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{Entry, Usage, Window};

    const RESET_A: &str = "2026-08-23T19:50:00.188677+00:00";
    const RESET_A_JITTER: &str = "2026-08-23T19:50:00.900000+00:00";
    const RESET_B: &str = "2026-08-24T00:50:00.000000+00:00";

    fn known(util: f64, reset: &str) -> Entry {
        Entry::new(
            Reading::Known(Usage {
                five_hour: Window {
                    utilization: util,
                    resets_at: Some(reset.into()),
                },
                seven_day: None,
            }),
            0,
        )
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
        AutoSwitcher::default()
    }

    /// A Mem already baselined on `alias` in window RESET_A at `util`.
    fn baselined(alias: &str, util: f64) -> Mem {
        Mem {
            baseline_alias: Some(alias.into()),
            baseline_reset: usage::parse_rfc3339(RESET_A).unwrap(),
            baseline_bucket: bucket(util, 15),
            last_switch_at: None,
        }
    }

    #[test]
    fn bucket_is_floor_of_step_multiples() {
        assert_eq!(bucket(0.0, 15), 0);
        assert_eq!(bucket(14.9, 15), 0);
        assert_eq!(bucket(15.0, 15), 1);
        assert_eq!(bucket(29.9, 15), 1);
        assert_eq!(bucket(45.0, 15), 3);
        assert_eq!(bucket(-1.0, 15), 0);
    }

    #[test]
    fn first_tick_sets_baseline_and_stays() {
        let o = obs(
            "a",
            &[("a", known(40.0, RESET_A)), ("b", known(10.0, RESET_A))],
        );
        let mut m = Mem::default();
        assert!(matches!(
            decide(&o, &mut m, &cfg(), 1000),
            Decision::Stay(Some(_))
        ));
        assert_eq!(m.baseline_alias.as_deref(), Some("a"));
        assert_eq!(m.baseline_bucket, 2);
    }

    #[test]
    fn switches_on_crossing_to_least_used() {
        let o = obs(
            "a",
            &[
                ("a", known(15.0, RESET_A)),
                ("b", known(12.0, RESET_A)),
                ("c", known(3.0, RESET_A)),
            ],
        );
        let mut m = baselined("a", 5.0);
        match decide(&o, &mut m, &cfg(), 1000) {
            Decision::Switch { to, .. } => assert_eq!(to, "c"),
            d => panic!("expected switch, got {d:?}"),
        }
    }

    #[test]
    fn stays_below_crossing() {
        let o = obs(
            "a",
            &[("a", known(14.9, RESET_A)), ("b", known(0.0, RESET_A))],
        );
        let mut m = baselined("a", 5.0);
        assert_eq!(decide(&o, &mut m, &cfg(), 1000), Decision::Stay(None));
    }

    #[test]
    fn rebaselines_when_alias_changes_underneath() {
        // User ran `cs switch b` manually; b is already at 31% — no spurious switch.
        let o = obs(
            "b",
            &[("a", known(15.0, RESET_A)), ("b", known(31.0, RESET_A))],
        );
        let mut m = baselined("a", 5.0);
        assert!(matches!(
            decide(&o, &mut m, &cfg(), 1000),
            Decision::Stay(Some(_))
        ));
        assert_eq!(m.baseline_alias.as_deref(), Some("b"));
        assert_eq!(m.baseline_bucket, 2);
    }

    #[test]
    fn rebaselines_on_window_reset_but_not_on_jitter() {
        let mut m = baselined("a", 75.0);
        // Jitter: same window, sub-second difference — still counts as crossing.
        let o = obs(
            "a",
            &[
                ("a", known(90.0, RESET_A_JITTER)),
                ("b", known(0.0, RESET_A)),
            ],
        );
        assert!(matches!(
            decide(&o, &mut m, &cfg(), 1000),
            Decision::Switch { .. }
        ));
        // Real reset: usage dropped to 2% in a new window — baseline, don't bounce.
        let o = obs(
            "a",
            &[("a", known(2.0, RESET_B)), ("b", known(0.0, RESET_A))],
        );
        assert!(matches!(
            decide(&o, &mut m, &cfg(), 1000),
            Decision::Stay(Some(_))
        ));
        assert_eq!(m.baseline_bucket, 0);
        assert_eq!(m.baseline_reset, usage::parse_rfc3339(RESET_B).unwrap());
    }

    #[test]
    fn gap_blocks_then_allows() {
        let o = obs(
            "a",
            &[("a", known(15.0, RESET_A)), ("b", known(0.0, RESET_A))],
        );
        let mut m = baselined("a", 5.0);
        m.last_switch_at = Some(1000);
        let c = cfg(); // gap 300
        assert!(matches!(
            decide(&o, &mut m, &c, 1200),
            Decision::Stay(Some(_))
        ));
        assert!(matches!(
            decide(&o, &mut m, &c, 1300),
            Decision::Switch { .. }
        ));
    }

    #[test]
    fn ceiling_excludes_and_unknown_is_never_picked() {
        let o = obs(
            "a",
            &[
                ("a", known(15.0, RESET_A)),
                ("b", known(95.0, RESET_A)),
                ("c", unknown()),
            ],
        );
        assert_eq!(pick_target(&o, 90, Some("a")), None);
        let mut m = baselined("a", 5.0);
        assert!(matches!(
            decide(&o, &mut m, &cfg(), 1000),
            Decision::Stay(Some(_))
        ));
        // Baseline untouched so we retry as soon as something becomes eligible.
        assert_eq!(m.baseline_bucket, 0);
        let o = obs(
            "a",
            &[("a", known(15.0, RESET_A)), ("b", known(89.0, RESET_A))],
        );
        assert_eq!(pick_target(&o, 90, Some("a")), Some("b"));
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
        // A Known account below the ceiling still wins over an expired one.
        let o = obs(
            "a",
            &[
                ("a", known(15.0, RESET_A)),
                ("b", expired(None)),
                ("c", known(50.0, RESET_A)),
            ],
        );
        let mut m = baselined("a", 5.0);
        assert!(matches!(
            decide(&o, &mut m, &cfg(), 1000),
            Decision::Switch { to, .. } if to == "c"
        ));
        // Only expired ones left: switch to the least recently active.
        let o = obs(
            "a",
            &[
                ("a", known(15.0, RESET_A)),
                ("b", expired(Some(500))),
                ("c", expired(Some(100))),
            ],
        );
        let mut m = baselined("a", 5.0);
        assert!(matches!(
            decide(&o, &mut m, &cfg(), 1000),
            Decision::Switch { to, .. } if to == "c"
        ));
        // A 429/network Unknown is NOT eligible.
        let o = obs("a", &[("a", known(15.0, RESET_A)), ("b", unknown())]);
        let mut m = baselined("a", 5.0);
        assert!(matches!(
            decide(&o, &mut m, &cfg(), 1000),
            Decision::Stay(Some(_))
        ));
    }

    #[test]
    fn single_account_or_unknown_current_stays() {
        let o = obs("a", &[("a", known(60.0, RESET_A))]);
        let mut m = baselined("a", 5.0);
        assert!(matches!(
            decide(&o, &mut m, &cfg(), 1000),
            Decision::Stay(Some(_))
        ));
        let o = obs("a", &[("a", unknown()), ("b", known(0.0, RESET_A))]);
        assert_eq!(decide(&o, &mut m, &cfg(), 1000), Decision::Stay(None));
        let none = Observation::default();
        assert!(matches!(
            decide(&none, &mut m, &cfg(), 1000),
            Decision::Stay(Some(_))
        ));
    }
}
