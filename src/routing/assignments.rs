use super::sessions::Process;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lease {
    pub process: Process,
    pub root: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Binding {
    pub alias: String,
    pub email: String,
    pub last_request: i64,
    pub in_flight: usize,
}
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Assignments {
    pub leases: BTreeMap<String, Lease>,
    pub bindings: BTreeMap<String, Binding>,
}
impl Assignments {
    pub fn counts(&self) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        let mut roots: std::collections::BTreeSet<_> = self
            .leases
            .values()
            .filter(|l| l.process.alive())
            .filter_map(|l| l.root.as_ref())
            .collect();
        roots.extend(
            self.bindings
                .iter()
                .filter(|(_, b)| b.in_flight > 0)
                .map(|(root, _)| root),
        );
        for root in roots {
            if let Some(b) = self.bindings.get(root) {
                *counts.entry(b.alias.clone()).or_default() += 1;
            }
        }
        counts
    }
    pub fn retained(&self, root: &str, now: i64, ttl: i64) -> Option<&Binding> {
        self.bindings
            .get(root)
            .filter(|b| b.in_flight > 0 || now.saturating_sub(b.last_request) < ttl)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Assignments {
        let mut s = Assignments::default();
        s.bindings.insert(
            "root".into(),
            Binding {
                alias: "a".into(),
                email: "a@x".into(),
                last_request: 1000,
                in_flight: 0,
            },
        );
        for key in ["one", "two"] {
            s.leases.insert(
                key.into(),
                Lease {
                    process: Process::current().unwrap(),
                    root: Some("root".into()),
                },
            );
        }
        s
    }
    #[test]
    fn live_roots_are_deduplicated_but_closed_bindings_survive() {
        let mut s = fixture();
        assert_eq!(s.counts().get("a"), Some(&1));
        s.leases.clear();
        assert!(s.counts().is_empty());
        assert!(s.retained("root", 4599, 3600).is_some());
        assert!(s.retained("root", 4600, 3600).is_none());
    }
    #[test]
    fn long_requests_pin_binding_past_expiry() {
        let mut s = fixture();
        s.bindings.get_mut("root").unwrap().in_flight = 1;
        assert!(s.retained("root", 5000, 3600).is_some());
    }
}
