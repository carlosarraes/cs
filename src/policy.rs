//! Manual `switch next` selection.
use crate::usage::{self, Observation, Reading};
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
        .filter_map(|(alias, e)| {
            e.reading
                .known()
                .filter(|u| u.seven_day.as_ref().is_none_or(|w| w.utilization < 100.))
                .map(|u| (alias, u.five_hour.as_ref().map_or(0., |w| w.utilization)))
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{Entry, Reading, Usage};

    #[test]
    fn next_does_not_choose_weekly_exhausted_accounts() {
        let mut observation = Observation::default();
        for (alias, five) in [
            ("ultra", None),
            (
                "limited",
                Some(crate::usage::Window {
                    utilization: 0.,
                    resets_at: None,
                }),
            ),
        ] {
            observation.accounts.insert(
                alias.into(),
                Entry::new(
                    Reading::Known(Usage {
                        five_hour: five,
                        seven_day: Some(crate::usage::Window {
                            utilization: 100.,
                            resets_at: None,
                        }),
                    }),
                    1000,
                ),
            );
        }
        assert_eq!(pick_target(&observation, 90, None), None);
    }

    #[test]
    fn next_accepts_an_account_without_five_hour_limit() {
        let mut observation = Observation::default();
        observation.accounts.insert(
            "ultra".into(),
            Entry::new(
                Reading::Known(Usage {
                    five_hour: None,
                    seven_day: None,
                }),
                1000,
            ),
        );
        assert_eq!(pick_target(&observation, 90, None), Some("ultra"));
        assert_eq!(pick_target(&observation, 90, Some("ultra")), None);
    }
}
