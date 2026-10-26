use crate::{
    routing::{sessions::Process, store::Store},
    state, util,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{ffi::OsString, path::PathBuf};

#[derive(Serialize, Deserialize)]
pub struct Endpoint {
    pub port: u16,
    pub process: Process,
}
pub fn endpoint_path() -> Result<PathBuf> {
    Ok(state::state_path()?.with_file_name("proxy.json"))
}
pub fn write_endpoint(port: u16) -> Result<()> {
    util::atomic_write(
        &endpoint_path()?,
        &serde_json::to_vec(&Endpoint {
            port,
            process: Process::current()?,
        })?,
    )
}
pub fn run(mut args: Vec<OsString>, requested: Option<&str>) -> Result<()> {
    if args.first().is_some_and(|arg| arg == "claude") {
        args.remove(0);
    }
    if let Some(name) = requested {
        resolve_account(&state::State::load()?, name).map(|_| ())?;
    }
    let source = crate::native::source_config()?;
    let root = crate::conversation::resolve(&mut args, &source, &std::env::current_dir()?)?;
    crate::conversation::ensure_not_running(&root, &source)?;
    crate::native::maintain()?;
    let mut accounts = state::State::load()?;
    let forced = requested
        .map(|name| resolve_account(&accounts, name).map(|(alias, _)| alias.to_string()))
        .transpose()?;
    let cfg = crate::config::load()?.routing;
    let mut observation = crate::usage::Observation::load()?.unwrap_or_default();
    if let Some(alias) = &forced {
        let account = &accounts.claude().accounts[alias];
        if !crate::routing::store::token_valid(account, crate::usage::now()) {
            bail!("{alias}: token expired or unavailable; refresh this account; no fallback");
        }
        let cached = observation
            .accounts
            .get(alias)
            .and_then(|entry| fresh_pinned_usage(entry, crate::usage::now()));
        let usage = match cached {
            Some(usage) => usage,
            None => crate::usage::fetch(
                account.data["claudeAiOauth"]["accessToken"]
                    .as_str()
                    .unwrap(),
            )
            .with_context(|| format!("{alias}: cannot verify quota; no fallback"))?,
        };
        validate_pin(alias, account, &usage, crate::usage::now())?;
        observation.accounts.insert(
            alias.clone(),
            crate::usage::Entry::new(crate::usage::Reading::Known(usage), crate::usage::now()),
        );
    } else if crate::routing::policy::select(
        &observation
            .accounts
            .iter()
            .filter(|(alias, _)| {
                accounts
                    .claude()
                    .accounts
                    .get(*alias)
                    .is_some_and(|account| {
                        crate::routing::store::token_valid(account, crate::usage::now())
                    })
            })
            .map(|(alias, entry)| (alias.clone(), entry.clone()))
            .collect(),
        &Default::default(),
        crate::usage::now(),
        cfg.five_hour_ceiling,
        cfg.weekly_ceiling,
        cfg.max_usage_age_secs as i64,
    )
    .is_none()
    {
        crate::accounts::poll()?;
        accounts = state::State::load()?;
        observation = crate::usage::Observation::load()?.unwrap_or_default();
    }
    let store = Store::default_path()?;
    let token = store.register()?;
    let result = (|| -> Result<()> {
        let (account, alias) = store.reserve_native(
            &token,
            &root,
            forced.as_deref(),
            &accounts,
            &observation.accounts,
            &cfg,
        )?;
        let profile = crate::native::prepare(&alias, &account, &source)?;
        crate::native::verify(&profile, &alias, &account.email)?;
        let mut command = crate::native::command(&profile, args)?;
        command.env("CS_NATIVE_LEASE", &token);
        eprintln!(
            "cs: {root} -> {alias} ({}) — native claude.ai login",
            account.email
        );
        let status = supervise(&mut command, &store, &token, &alias, &account.email)?;
        if !status.success() {
            bail!("{alias}: Claude exited with {status}; no account fallback");
        }
        Ok(())
    })();
    let cleanup = store.unregister(&token);
    result.and(cleanup)
}

struct RunningChild(std::process::Child);
impl Drop for RunningChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn supervise(
    command: &mut std::process::Command,
    store: &Store,
    token: &str,
    alias: &str,
    email: &str,
) -> Result<std::process::ExitStatus> {
    use std::io::IsTerminal;
    use tokio::signal::unix::{signal, SignalKind};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let mut interrupt=signal(SignalKind::interrupt())?;
        let mut terminate=signal(SignalKind::terminate())?;
        let mut hangup=signal(SignalKind::hangup())?;
        let mut child=RunningChild(command.spawn().with_context(||format!("{alias}: launching Claude"))?);
        if let Some(process)=Process::read(child.0.id()) {
            store.update(|s|{s.leases.get_mut(token).context("missing launch lease")?.process=process;Ok(())})?;
        }
        let mut last_sync=std::time::Instant::now();
        let mut renewal:Option<std::process::Child>=None;
        let mut stopping:Option<std::time::Instant>=None;
        loop {
            if let Some(status)=child.0.try_wait()?{return Ok(status);}
            if stopping.is_some_and(|time|time.elapsed()>=std::time::Duration::from_secs(5)){child.0.kill()?;}
            if let Some(task) = renewal.as_mut() {
                if task.try_wait()?.is_some() { renewal = None; }
            }
            if last_sync.elapsed()>=std::time::Duration::from_secs(30) && renewal.is_none() && stopping.is_none(){
                use std::os::unix::process::CommandExt;
                // A rotating grant must be persisted even if the conversation exits mid-refresh.
                renewal=Some(std::process::Command::new(std::env::current_exe()?)
                    .args(["native-maintain", alias, email])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .process_group(0)
                    .spawn()?);
                last_sync=std::time::Instant::now();
            }
            tokio::select! {
                _=interrupt.recv()=>{
                    // Terminal SIGINT already reaches Claude's process group; do not deliver it twice.
                    if !std::io::stdin().is_terminal(){unsafe{libc::kill(child.0.id() as i32,libc::SIGINT);}}
                }
                _=terminate.recv()=>{unsafe{libc::kill(child.0.id() as i32,libc::SIGTERM);} stopping=Some(std::time::Instant::now());}
                _=hangup.recv()=>{unsafe{libc::kill(child.0.id() as i32,libc::SIGHUP);} stopping=Some(std::time::Instant::now());}
                _=tokio::time::sleep(std::time::Duration::from_millis(100))=>{}
            }
        }
    })
}

pub fn native_event() -> Result<()> {
    use std::io::Read;
    let Ok(token) = std::env::var("CS_NATIVE_LEASE") else {
        return Ok(());
    };
    let mut input = String::new();
    std::io::stdin()
        .take(16 * 1024 * 1024)
        .read_to_string(&mut input)?;
    let event: serde_json::Value = serde_json::from_str(&input)?;
    let root = event["session_id"]
        .as_str()
        .context("native event has no conversation ID")?;
    let kind = event["hook_event_name"]
        .as_str()
        .context("native event has no event name")?;
    if let Err(error) = Store::default_path()?.native_event(&token, root, kind, crate::usage::now())
    {
        if kind == "UserPromptSubmit" {
            println!(
                "{}",
                serde_json::json!({"decision":"block","reason":error.to_string()})
            );
            return Ok(());
        }
        println!(
            "{}",
            serde_json::json!({"continue":false,"stopReason":error.to_string()})
        );
        return Ok(());
    }
    if kind == "Stop" {
        crate::native::sync_all()?;
    }
    Ok(())
}

fn resolve_account<'a>(
    accounts: &'a state::State,
    name: &str,
) -> Result<(&'a str, &'a state::Account)> {
    let saved = &accounts.claude().accounts;
    if let Some((alias, account)) = saved.get_key_value(name) {
        return Ok((alias, account));
    }
    let mut matches = saved
        .iter()
        .filter(|(_, account)| account.email.eq_ignore_ascii_case(name));
    let Some((alias, account)) = matches.next() else {
        bail!("{name}: unknown account; use cs list to see saved aliases and emails");
    };
    if matches.next().is_some() {
        bail!("{name}: ambiguous email; choose an explicit account alias from cs list");
    }
    Ok((alias, account))
}

fn validate_pin(
    alias: &str,
    account: &state::Account,
    usage: &crate::usage::Usage,
    time: i64,
) -> Result<()> {
    if !crate::routing::store::token_valid(account, time) {
        bail!("{alias}: token expired or unavailable; sign in and refresh this saved account; no fallback");
    }
    for (label, window) in [
        ("five-hour", Some(&usage.five_hour)),
        ("weekly", usage.seven_day.as_ref()),
    ] {
        if let Some(window) = window {
            if !window.utilization.is_finite() || window.utilization < 0. {
                bail!("{alias}: invalid {label} usage; refusing to launch");
            }
            if window.utilization >= 100. {
                bail!("{alias}: {label} quota exhausted; no fallback");
            }
        }
    }
    Ok(())
}

fn fresh_pinned_usage(entry: &crate::usage::Entry, time: i64) -> Option<crate::usage::Usage> {
    let age = time.checked_sub(entry.fetched_at?)?;
    if !(0..300).contains(&age) {
        return None;
    }
    let usage = entry.reading.known()?;
    for window in std::iter::once(&usage.five_hour).chain(usage.seven_day.iter()) {
        if let Some(reset) = &window.resets_at {
            if crate::usage::parse_rfc3339(reset)? <= time {
                return None;
            }
        }
    }
    Some(usage.clone())
}

pub fn sessions() -> Result<()> {
    Store::default_path()?.update(|s| {
        if s.leases.is_empty() {
            println!("No managed Claude sessions running.");
        }
        for lease in s.leases.values() {
            let root = lease.root.as_deref().unwrap_or("waiting for first request");
            let account = s
                .bindings
                .get(root)
                .map(|b| b.alias.as_str())
                .unwrap_or("unassigned");
            println!("{}  {}  pid {}", root, account, lease.process.pid);
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{Usage, Window};

    fn accounts() -> state::State {
        let mut state = state::State::default();
        state.claude_mut().accounts.insert("work".into(), state::Account {
            email: "carlos@mondrio.io".into(),
            data: serde_json::json!({"claudeAiOauth":{"accessToken":"secret-test-token","expiresAt":2000000}}),
        });
        state
    }

    #[test]
    fn pinned_cache_requires_recent_successful_reading_before_reset() {
        let mut entry = crate::usage::Entry::new(
            crate::usage::Reading::Known(Usage {
                five_hour: Window {
                    utilization: 10.,
                    resets_at: None,
                },
                seven_day: None,
            }),
            1000,
        );
        assert!(fresh_pinned_usage(&entry, 1299).is_some());
        assert!(fresh_pinned_usage(&entry, 1300).is_none());
        assert!(fresh_pinned_usage(&entry, 999).is_none());
        if let crate::usage::Reading::Known(usage) = &mut entry.reading {
            usage.five_hour.resets_at = Some("1970-01-01T00:18:20Z".into());
        }
        assert!(fresh_pinned_usage(&entry, 1100).is_none());
        entry.reading = crate::usage::Reading::Unknown {
            reason: "token rejected".into(),
        };
        assert!(fresh_pinned_usage(&entry, 1001).is_none());
    }

    #[test]
    fn pin_resolves_alias_or_email_and_rejects_ambiguous_email() {
        let mut state = accounts();
        assert_eq!(resolve_account(&state, "work").unwrap().0, "work");
        assert_eq!(
            resolve_account(&state, "CARLOS@MONDRIO.IO").unwrap().0,
            "work"
        );
        let error = resolve_account(&state, "bogus").unwrap_err().to_string();
        assert!(error.contains("bogus") && error.contains("unknown"));
        let duplicate = state.claude().accounts["work"].clone();
        state
            .claude_mut()
            .accounts
            .insert("duplicate".into(), duplicate);
        assert!(resolve_account(&state, "carlos@mondrio.io")
            .unwrap_err()
            .to_string()
            .contains("ambiguous"));
    }

    #[test]
    fn pin_rejects_unusable_tokens_and_exhausted_quota_without_secrets() {
        let mut account = accounts().claude().accounts["work"].clone();
        let mut usage = Usage {
            five_hour: Window {
                utilization: 0.,
                resets_at: None,
            },
            seven_day: Some(Window {
                utilization: 98.,
                resets_at: None,
            }),
        };
        assert!(validate_pin("work", &account, &usage, 1000).is_ok());
        usage.seven_day.as_mut().unwrap().utilization = 100.;
        assert!(validate_pin("work", &account, &usage, 1000)
            .unwrap_err()
            .to_string()
            .contains("work: weekly quota exhausted"));
        usage.seven_day = None;
        usage.five_hour.utilization = 100.;
        assert!(validate_pin("work", &account, &usage, 1000).is_err());
        account.data["claudeAiOauth"]["expiresAt"] = 0.into();
        let error = validate_pin("work", &account, &usage, 1000)
            .unwrap_err()
            .to_string();
        assert!(error.contains("work") && error.contains("expired"));
        assert!(!error.contains("secret-test-token"));
    }
}
