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
    #[cfg(test)]
    pub fn register_pinned(&self, alias: &str, email: &str) -> Result<String> {
        let token = uuid::Uuid::new_v4().to_string();
        let root = format!("pinned-{}", uuid::Uuid::new_v4());
        let process = Process::current()?;
        self.update(|s| {
            s.leases.insert(
                token.clone(),
                Lease {
                    process,
                    root: Some(root.clone()),
                },
            );
            s.bindings.insert(
                root,
                Binding {
                    alias: alias.into(),
                    email: email.into(),
                    last_request: now(),
                    in_flight: 0,
                },
            );
            Ok(())
        })?;
        Ok(token)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn reserve_native(
        &self,
        token: &str,
        root: &str,
        forced: Option<&str>,
        accounts: &State,
        entries: &BTreeMap<String, Entry>,
        cfg: &Routing,
    ) -> Result<(Account, String)> {
        self.update(|s| {
            if !s.leases.contains_key(token){bail!("unknown launch lease");}
            if s.leases.iter().any(|(key,lease)|key!=token && lease.root.as_deref()==Some(root)) {
                bail!("conversation {root} is already running; close its existing process before resuming it");
            }
            let retained=s.retained(root,now(),AFFINITY_SECS);
            if forced.is_none() && retained.is_some_and(|binding| accounts.claude().accounts.get(&binding.alias).is_none_or(|a|a.email!=binding.email)) {
                bail!("assigned account identity changed or was removed; no fallback");
            }
            let retained=retained.filter(|binding| !quota_exhausted(entries.get(&binding.alias)));
            let alias=if let Some(alias)=forced {alias.to_string()}
            else if let Some(binding)=retained {
                binding.alias.clone()
            } else {
                let eligible=entries.iter().filter(|(alias,_)|accounts.claude().accounts.get(*alias).is_some_and(|a|token_valid(a,now()))).map(|(alias,entry)|(alias.clone(),entry.clone())).collect();
                policy::select(&eligible,&s.counts(),now(),cfg.five_hour_ceiling,cfg.weekly_ceiling,cfg.max_usage_age_secs as i64)
                    .context("no account has fresh usage and quota headroom; run cs usage --live")?.to_string()
            };
            let account=accounts.claude().accounts.get(&alias).context("unknown selected account")?.clone();
            if !token_valid(&account,now()){bail!("{alias}: token expired or unavailable; no fallback");}
            if quota_exhausted(entries.get(&alias)){bail!("{alias}: quota exhausted; no fallback");}
            let last_request=s.bindings.get(root).filter(|b|b.alias==alias && b.email==account.email).map_or(0,|b|b.last_request);
            s.bindings.insert(root.into(),Binding{alias:alias.clone(),email:account.email.clone(),last_request,in_flight:0});
            s.leases.get_mut(token).unwrap().root=Some(root.into());
            Ok((account,alias))
        })
    }

    pub fn native_event(&self, token: &str, root: &str, event: &str, time: i64) -> Result<()> {
        self.update(|s| {
            let previous = s
                .leases
                .get(token)
                .and_then(|l| l.root.clone())
                .context("unknown native session")?;
            let binding = s
                .bindings
                .get(&previous)
                .cloned()
                .context("missing native assignment")?;
            if previous != root {
                if s.retained(root,time,AFFINITY_SECS).is_some_and(|target|target.alias!=binding.alias || target.email!=binding.email) {
                    bail!("this conversation is assigned to another account; exit and resume it with cs run -- --resume {root}");
                }
                if s.leases
                    .iter()
                    .any(|(key, l)| key != token && l.root.as_deref() == Some(root))
                {
                    bail!("conversation already running");
                }
                s.bindings.insert(
                    root.into(),
                    Binding {
                        last_request: s.bindings.get(root).map_or(0,|b|b.last_request),
                        in_flight: 0,
                        ..binding
                    },
                );
                s.leases.get_mut(token).unwrap().root = Some(root.into());
            }
            if event == "UserPromptSubmit" {
                s.bindings
                    .get_mut(root)
                    .context("missing conversation binding")?
                    .last_request = time;
            }
            Ok(())
        })
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
        self.acquire_inner(token, root, accounts, entries, cfg, time, true)
    }
    #[allow(clippy::too_many_arguments)]
    fn acquire_inner(
        &self,
        token: &str,
        root: &str,
        accounts: &State,
        entries: &BTreeMap<String, Entry>,
        cfg: &Routing,
        time: i64,
        log: bool,
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
                if log {crate::daemon::log("assign", &format!("{root} -> {alias} (live counts, quota headroom, reset timing)"));}
            }
            let binding = s.bindings.get_mut(root).expect("binding inserted");
            if log {binding.last_request = time;}
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
fn quota_exhausted(entry: Option<&Entry>) -> bool {
    entry.and_then(|e| e.reading.known()).is_some_and(|usage| {
        usage.five_hour.utilization >= 100.
            || usage
                .seven_day
                .as_ref()
                .is_some_and(|window| window.utilization >= 100.)
    })
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
    fn native_selection_counts_resume_affinity_and_prompt_only_activity() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            path: dir.path().join("routing.json"),
        };
        let first = store.register().unwrap();
        let second = store.register().unwrap();
        let (_, a) = store
            .reserve_native(
                &first,
                "first",
                None,
                &accounts(),
                &entries(),
                &Routing::default(),
            )
            .unwrap();
        let (_, b) = store
            .reserve_native(
                &second,
                "second",
                None,
                &accounts(),
                &entries(),
                &Routing::default(),
            )
            .unwrap();
        assert_ne!(a, b);
        let prompt = now() - 1800;
        store
            .native_event(&first, "first", "UserPromptSubmit", prompt)
            .unwrap();
        store.native_event(&first, "first", "Stop", now()).unwrap();
        assert_eq!(
            store
                .update(|s| Ok(s.bindings["first"].last_request))
                .unwrap(),
            prompt
        );
        assert!(store
            .native_event(&second, "first", "SessionStart", now())
            .is_err());
        store.unregister(&first).unwrap();
        let resumed = store.register().unwrap();
        let (_, alias) = store
            .reserve_native(
                &resumed,
                "first",
                None,
                &accounts(),
                &entries(),
                &Routing::default(),
            )
            .unwrap();
        assert_eq!(alias, a);
        assert_eq!(
            store
                .update(|s| Ok(s.bindings["first"].last_request))
                .unwrap(),
            prompt
        );
        let duplicate = store.register().unwrap();
        assert!(store
            .reserve_native(
                &duplicate,
                "first",
                Some(&b),
                &accounts(),
                &entries(),
                &Routing::default()
            )
            .is_err());
        store.unregister(&resumed).unwrap();
        store.unregister(&second).unwrap();
        store.unregister(&duplicate).unwrap();
        assert!(store.update(|s| Ok(s.counts())).unwrap().is_empty());
    }

    #[test]
    fn automatic_resume_rebalances_at_either_quota_limit_but_explicit_pins_do_not() {
        for weekly in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = Store {
                path: dir.path().join("routing.json"),
            };
            let old = store.register().unwrap();
            store
                .reserve_native(
                    &old,
                    "pm",
                    Some("a"),
                    &accounts(),
                    &entries(),
                    &Routing::default(),
                )
                .unwrap();
            store
                .native_event(&old, "pm", "UserPromptSubmit", now())
                .unwrap();
            store.unregister(&old).unwrap();
            let mut usage = entries();
            let crate::usage::Reading::Known(value) = &mut usage.get_mut("a").unwrap().reading
            else {
                panic!("expected usage")
            };
            if weekly {
                value.seven_day = Some(crate::usage::Window {
                    utilization: 100.,
                    resets_at: None,
                });
            } else {
                value.five_hour.utilization = 100.;
            }
            let resumed = store.register().unwrap();
            assert!(store
                .reserve_native(
                    &resumed,
                    "pm",
                    Some("a"),
                    &accounts(),
                    &usage,
                    &Routing::default()
                )
                .is_err());
            assert_eq!(
                store
                    .reserve_native(
                        &resumed,
                        "pm",
                        None,
                        &accounts(),
                        &usage,
                        &Routing::default()
                    )
                    .unwrap()
                    .1,
                "b"
            );
            assert_eq!(
                store.update(|s| Ok(s.counts())).unwrap(),
                BTreeMap::from([("b".into(), 1)])
            );
        }
    }

    #[test]
    fn native_expired_affinity_rebalances_and_optional_pin_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            path: dir.path().join("routing.json"),
        };
        let first = store.register().unwrap();
        store
            .reserve_native(
                &first,
                "root",
                Some("a"),
                &accounts(),
                &entries(),
                &Routing::default(),
            )
            .unwrap();
        store
            .native_event(&first, "root", "UserPromptSubmit", now() - AFFINITY_SECS)
            .unwrap();
        store.unregister(&first).unwrap();
        let load = store.register().unwrap();
        store
            .reserve_native(
                &load,
                "busy",
                Some("a"),
                &accounts(),
                &entries(),
                &Routing::default(),
            )
            .unwrap();
        let resumed = store.register().unwrap();
        assert_eq!(
            store
                .reserve_native(
                    &resumed,
                    "root",
                    None,
                    &accounts(),
                    &entries(),
                    &Routing::default()
                )
                .unwrap()
                .1,
            "b"
        );
    }

    #[test]
    fn direct_pins_count_separately_and_survive_router_restart() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            path: dir.path().join("routing.json"),
        };
        let first = store.register_pinned("work", "work@x").unwrap();
        let second = store.register_pinned("work", "work@x").unwrap();
        store.reset_requests().unwrap();
        assert_eq!(store.update(|s| Ok(s.counts()["work"])).unwrap(), 2);
        store.unregister(&first).unwrap();
        assert_eq!(store.update(|s| Ok(s.counts()["work"])).unwrap(), 1);
        store
            .update(|s| {
                s.leases.get_mut(&second).unwrap().process.start.push('x');
                Ok(())
            })
            .unwrap();
        assert!(store.update(|s| Ok(s.counts())).unwrap().is_empty());
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
