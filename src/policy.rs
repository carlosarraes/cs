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
