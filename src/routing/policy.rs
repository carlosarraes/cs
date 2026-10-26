use crate::usage::{Entry, Window};
use std::collections::BTreeMap;

pub fn select<'a>(
    entries: &'a BTreeMap<String, Entry>,
    counts: &BTreeMap<String, usize>,
    now: i64,
    five_ceiling: f64,
    weekly_ceiling: f64,
    max_age: i64,
) -> Option<&'a str> {
    entries
        .iter()
        .filter_map(|(alias, entry)| {
            let age = now.checked_sub(entry.fetched_at?)?;
            if age < 0 || age > max_age {
                return None;
            }
            let usage = entry.reading.known()?;
            let five = rate(&usage.five_hour, now, five_ceiling, 5 * 3600)?;
            let week = match &usage.seven_day {
                Some(w) => rate(w, now, weekly_ceiling, 7 * 24 * 3600)?,
                None => 0.,
            };
            Some((alias, counts.get(alias).copied().unwrap_or(0), week, five))
        })
        .min_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| b.2.total_cmp(&a.2))
                .then_with(|| b.3.total_cmp(&a.3))
                .then_with(|| a.0.cmp(b.0))
        })
        .map(|(alias, _, _, _)| alias.as_str())
}

fn rate(window: &Window, now: i64, ceiling: f64, duration: i64) -> Option<f64> {
    if !window.utilization.is_finite() || !(0.0..ceiling).contains(&window.utilization) {
        return None;
    }
    let remaining = match &window.resets_at {
        Some(reset) => crate::usage::parse_rfc3339(reset)?.checked_sub(now)?,
        None => duration,
    };
    if remaining <= 0 {
        return None;
    }
    Some((100.0 - window.utilization) / (remaining.max(900) as f64 / 3600.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{Reading, Usage};
    fn entry(five: f64, week: f64, reset: i64) -> Entry {
        Entry::new(
            Reading::Known(Usage {
                five_hour: Window {
                    utilization: five,
                    resets_at: None,
                },
                seven_day: Some(Window {
                    utilization: week,
                    resets_at: Some(
                        chrono::DateTime::from_timestamp(reset, 0)
                            .unwrap()
                            .to_rfc3339(),
                    ),
                }),
            }),
            1000,
        )
    }
    #[test]
    fn counts_first_then_capacity_until_reset() {
        let entries = BTreeMap::from([
            ("a".into(), entry(0., 60., 1000 + 12 * 3600)),
            ("b".into(), entry(0., 10., 1000 + 144 * 3600)),
        ]);
        assert_eq!(
            select(&entries, &BTreeMap::new(), 1000, 90., 95., 300),
            Some("a")
        );
        assert_eq!(
            select(
                &entries,
                &BTreeMap::from([("a".into(), 1)]),
                1000,
                90.,
                95.,
                300
            ),
            Some("b")
        );
    }
    #[test]
    fn rejects_exhaustion_stale_and_elapsed_windows() {
        for e in [
            entry(90., 0., 2000),
            entry(0., 95., 2000),
            entry(0., 0., 999),
        ] {
            assert_eq!(
                select(
                    &BTreeMap::from([("a".into(), e)]),
                    &BTreeMap::new(),
                    1000,
                    90.,
                    95.,
                    300
                ),
                None
            );
        }
        assert_eq!(
            select(
                &BTreeMap::from([("a".into(), entry(0., 0., 2000))]),
                &BTreeMap::new(),
                1301,
                90.,
                95.,
                300
            ),
            None
        );
    }
    #[test]
    fn deterministic_ties_and_nonfinite_usage() {
        let mut es = BTreeMap::from([
            ("b".into(), entry(0., 0., 2000)),
            ("a".into(), entry(0., 0., 2000)),
        ]);
        assert_eq!(
            select(&es, &BTreeMap::new(), 1000, 90., 95., 300),
            Some("a")
        );
        es.insert("a".into(), entry(f64::NAN, 0., 2000));
        assert_eq!(
            select(&es, &BTreeMap::new(), 1000, 90., 95., 300),
            Some("b")
        );
    }
}
