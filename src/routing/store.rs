use super::{
    assignments::{Assignments, Binding, Lease},
    policy,
    sessions::Process,
};
use crate::{
    config::Routing,
    state::{self, Account, State},
    usage::{now, Entry},
    util,
};
use anyhow::{anyhow, bail, Context, Result};
use std::{collections::BTreeMap, path::PathBuf};

pub const AFFINITY_SECS: i64 = 3600;
#[derive(Clone)]
pub struct Store {
    pub path: PathBuf,
}
impl Store {
    pub fn default_path() -> Result<Self> {
        Ok(Self {
            path: state::state_path()?.with_file_name("routing.json"),
        })
    }
    pub fn update<T>(&self, f: impl FnOnce(&mut Assignments) -> Result<T>) -> Result<T> {
        let _lock = state::lock_at(&self.path.with_extension("lock"), true)?;
        let mut data: Assignments = match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("reading session assignments")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Assignments::default(),
            Err(e) => return Err(e.into()),
        };
        data.leases.retain(|_, l| l.process.alive());
        let roots: std::collections::BTreeSet<_> = data
            .leases
            .values()
            .filter_map(|l| l.root.clone())
            .collect();
        data.bindings.retain(|root, b| {
            roots.contains(root)
                || b.in_flight > 0
                || now().saturating_sub(b.last_request) < AFFINITY_SECS
        });
        let result = f(&mut data)?;
        util::atomic_write(&self.path, &serde_json::to_vec(&data)?)?;
        Ok(result)
    }
    pub fn register(&self) -> Result<String> {
        let token = uuid::Uuid::new_v4().to_string();
        let process = Process::current()?;
        self.update(|s| {
            s.leases.insert(
                token.clone(),
                Lease {
                    process,
                    root: None,
                },
            );
            Ok(())
        })?;
        Ok(token)
    }
    pub fn unregister(&self, token: &str) -> Result<()> {
        self.update(|s| {
            s.leases.remove(token);
            Ok(())
        })
    }
    pub fn authenticated(&self, token: &str) -> Result<()> {
        self.update(|s| {
            if !s.leases.contains_key(token) {
                bail!("unknown or closed managed session");
            }
            Ok(())
        })
    }
    pub fn reset_requests(&self) -> Result<()> {
        self.update(|s| {
            for b in s.bindings.values_mut() {
                b.in_flight = 0;
            }
            Ok(())
        })
    }
    pub fn acquire(
        &self,
        token: &str,
        root: &str,
        accounts: &State,
        entries: &BTreeMap<String, Entry>,
        cfg: &Routing,
        time: i64,
    ) -> Result<(Account, String, RequestGuard)> {
        let (account, alias) = self.update(|s| {
            let lease = s.leases.get_mut(token)
                .ok_or_else(|| anyhow!("unknown or closed managed session"))?;
            lease.root = Some(root.to_string());
            let retained = s.retained(root, time, AFFINITY_SECS).cloned();
            let alias = if let Some(binding) = retained {
                let account = accounts.claude().accounts.get(&binding.alias)
                    .ok_or_else(|| anyhow!("assigned account was removed; no automatic failover"))?;
                if account.email != binding.email {
                    bail!("assigned account identity changed; no automatic failover");
                }
                binding.alias
            } else {
                let mut counts = s.counts();
                if let Some(old) = s.bindings.get(root) {
                    if let Some(count) = counts.get_mut(&old.alias) {
                        *count = count.saturating_sub(1);
                    }
                }
                let eligible = entries.iter()
                    .filter(|(alias, _)| accounts.claude().accounts.get(*alias)
                        .is_some_and(|account| token_valid(account, time)))
                    .map(|(alias, entry)| (alias.clone(), entry.clone()))
                    .collect();
                policy::select(&eligible, &counts, time, cfg.five_hour_ceiling,
                    cfg.weekly_ceiling, cfg.max_usage_age_secs as i64)
                    .ok_or_else(|| anyhow!("no account has fresh usage and quota headroom; run cs usage --live for usage and reset times"))?
                    .to_string()
            };
            let account = accounts.claude().accounts.get(&alias)
                .ok_or_else(|| anyhow!("account disappeared"))?.clone();
            if !token_valid(&account, time) {
                bail!("{alias}: token expired or unavailable; run cs refresh after signing in; no automatic failover");
            }
            let exhausted = entries.get(&alias).and_then(|entry| entry.reading.known())
                .is_some_and(|usage| usage.five_hour.utilization >= 100.0
                    || usage.seven_day.as_ref().is_some_and(|window| window.utilization >= 100.0));
            if exhausted {
                bail!("{alias}: quota exhausted; no automatic failover (see cs usage)");
            }
            if s.retained(root, time, AFFINITY_SECS).is_none() {
                s.bindings.insert(root.into(), Binding {
                    alias: alias.clone(), email: account.email.clone(),
                    last_request: time, in_flight: 0,
                });
                crate::daemon::log("assign", &format!("{root} -> {alias} (live counts, quota headroom, reset timing)"));
            }
            let binding = s.bindings.get_mut(root).expect("binding inserted");
            binding.last_request = time;
            binding.in_flight += 1;
            Ok((account, alias))
        })?;
        Ok((
            account,
            alias,
            RequestGuard {
                store: self.clone(),
                root: root.into(),
            },
        ))
    }
}
pub fn token_valid(account: &Account, time: i64) -> bool {
    let o = &account.data["claudeAiOauth"];
    o["accessToken"].as_str().is_some_and(|s| !s.is_empty())
        && o["expiresAt"]
            .as_i64()
            .is_some_and(|t| t > time.saturating_mul(1000))
}
pub struct RequestGuard {
    store: Store,
    root: String,
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        let _ = self.store.update(|s| {
            if let Some(b) = s.bindings.get_mut(&self.root) {
                b.in_flight = b.in_flight.saturating_sub(1);
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{Reading, Usage, Window};
    fn accounts() -> State {
        let mut s = State::default();
        for alias in ["a", "b"] {
            s.claude_mut().accounts.insert(alias.into(),Account{email:format!("{alias}@x"),data:serde_json::json!({"claudeAiOauth":{"accessToken":alias,"expiresAt":(now()+7200)*1000}})});
        }
        s
    }
    fn entries() -> BTreeMap<String, Entry> {
        ["a", "b"]
            .into_iter()
            .map(|a| {
                (
                    a.into(),
                    Entry::new(
                        Reading::Known(Usage {
                            five_hour: Window {
                                utilization: 0.,
                                resets_at: None,
                            },
                            seven_day: None,
                        }),
                        now(),
                    ),
                )
            })
            .collect()
    }
    #[test]
    fn interactive_conversation_change_keeps_old_affinity_without_old_live_count() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            path: dir.path().join("routing.json"),
        };
        let token = store.register().unwrap();
        let (_, old, guard) = store
            .acquire(
                &token,
                "old",
                &accounts(),
                &entries(),
                &Routing::default(),
                now(),
            )
            .unwrap();
        drop(guard);
        let (_, _, guard) = store
            .acquire(
                &token,
                "new",
                &accounts(),
                &entries(),
                &Routing::default(),
                now(),
            )
            .unwrap();
        drop(guard);
        assert_eq!(
            store
                .update(|s| Ok(s.counts().values().sum::<usize>()))
                .unwrap(),
            1
        );
        let (_, resumed, _guard) = store
            .acquire(
                &token,
                "old",
                &accounts(),
                &entries(),
                &Routing::default(),
                now(),
            )
            .unwrap();
        assert_eq!(resumed, old);
    }

    #[test]
    fn concurrent_launches_reserve_different_accounts_and_resume_stays() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            path: dir.path().join("routing.json"),
        };
        let tokens = [store.register().unwrap(), store.register().unwrap()];
        let threads: Vec<_> = tokens
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let s = store.clone();
                let t = t.clone();
                std::thread::spawn(move || {
                    let (_, alias, guard) = s
                        .acquire(
                            &t,
                            &format!("root{i}"),
                            &accounts(),
                            &entries(),
                            &Routing::default(),
                            now(),
                        )
                        .unwrap();
                    drop(guard);
                    alias
                })
            })
            .collect();
        let aliases: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_ne!(aliases[0], aliases[1]);
        let (_, alias, guard) = store
            .acquire(
                &tokens[0],
                "root0",
                &accounts(),
                &BTreeMap::new(),
                &Routing::default(),
                now(),
            )
            .unwrap();
        assert_eq!(alias, aliases[0]);
        drop(guard);
        store.unregister(&tokens[0]).unwrap();
        let resumed = store.register().unwrap();
        let (_, alias, _guard) = store
            .acquire(
                &resumed,
                "root0",
                &accounts(),
                &entries(),
                &Routing::default(),
                now(),
            )
            .unwrap();
        assert_eq!(alias, aliases[0]);
    }
    #[test]
    fn account_replacement_and_expired_auth_do_not_fail_over() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            path: dir.path().join("routing.json"),
        };
        let token = store.register().unwrap();
        let (_, alias, guard) = store
            .acquire(
                &token,
                "root",
                &accounts(),
                &entries(),
                &Routing::default(),
                now(),
            )
            .unwrap();
        drop(guard);
        let mut changed = accounts();
        changed.claude_mut().accounts.get_mut(&alias).unwrap().email = "changed@x".into();
        assert!(store
            .acquire(
                &token,
                "root",
                &changed,
                &entries(),
                &Routing::default(),
                now()
            )
            .is_err());
        let mut expired = accounts();
        expired.claude_mut().accounts.get_mut(&alias).unwrap().data["claudeAiOauth"]["expiresAt"] =
            0.into();
        assert!(store
            .acquire(
                &token,
                "root",
                &expired,
                &entries(),
                &Routing::default(),
                now()
            )
            .is_err());
    }
    #[test]
    fn restart_releases_inflight_and_dead_leases_do_not_count() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            path: dir.path().join("routing.json"),
        };
        let token = store.register().unwrap();
        let (_, _, guard) = store
            .acquire(
                &token,
                "root",
                &accounts(),
                &entries(),
                &Routing::default(),
                now(),
            )
            .unwrap();
        store.reset_requests().unwrap();
        assert_eq!(
            store.update(|s| Ok(s.bindings["root"].in_flight)).unwrap(),
            0
        );
        drop(guard);
        store
            .update(|s| {
                s.leases.get_mut(&token).unwrap().process.start.push('x');
                Ok(())
            })
            .unwrap();
        assert!(store.update(|s| Ok(s.counts())).unwrap().is_empty());
        assert!(store.authenticated(&token).is_err());
    }
}
