use crate::session_control::{Controls, Session, Target};
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
        let mut command = crate::native::command(&profile, args.clone())?;
        command.env("CS_NATIVE_LEASE", &token);
        use std::io::IsTerminal;
        let unsupported = if !std::io::stdin().is_terminal() {
            Some("headless sessions cannot be switched while running".into())
        } else {
            crate::session_control::resume_args(&args, &root)
                .err()
                .map(|e| e.to_string())
        };
        Controls::default_path()?.update(|sessions| {
            sessions.insert(
                token.clone(),
                Session {
                    generation: String::new(),
                    owner: Process::current()?,
                    child: None,
                    root: root.clone(),
                    alias: alias.clone(),
                    email: account.email.clone(),
                    busy: true,
                    agents: Default::default(),
                    unsupported,
                    request: None,
                    switching: false,
                    starting: None,
                    error: None,
                    cwd: std::env::current_dir()?,
                },
            );
            Ok(())
        })?;
        eprintln!(
            "cs: {root} -> {alias} ({}) — native claude.ai login",
            account.email
        );
        let status = supervise(
            command,
            &store,
            &token,
            &alias,
            &account.email,
            &args,
            &source,
        )?;
        if !status.success() {
            let actual_alias = Controls::default_path()?
                .update(|s| Ok(s.get(&token).map(|c| c.alias.clone()).unwrap_or(alias)))?;
            bail!("{actual_alias}: Claude exited with {status}; no account fallback");
        }
        Ok(())
    })();
    let cleanup = store.unregister(&token);
    let controls_cleanup = Controls::default_path()?.update(|sessions| {
        sessions.remove(&token);
        Ok(())
    });
    result.and(cleanup).and(controls_cleanup)
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

struct MovePreparation {
    target: Target,
    root: String,
    old_alias: String,
    old_email: String,
    next: Option<crate::native::NativeCommand>,
    rollback: Option<crate::native::NativeCommand>,
    stage: MoveStage,
    since: std::time::Instant,
}
#[derive(PartialEq)]
enum MoveStage {
    Stopping,
    Starting,
    Restoring,
}

fn spawn_managed(
    command: &mut std::process::Command,
    store: &Store,
    controls: &Controls,
    token: &str,
) -> Result<RunningChild> {
    controls.update(|sessions| {
        let session = sessions.get_mut(token).context("missing session control")?;
        session.generation = uuid::Uuid::new_v4().to_string();
        command.env("CS_NATIVE_GENERATION", &session.generation);
        // Hooks acquire this lock too, so they cannot observe an unregistered child.
        let mut child = RunningChild(command.spawn().context("launching Claude")?);
        let Some(process) = Process::read(child.0.id()) else {
            if child.0.try_wait()?.is_some() {
                return Ok(child);
            }
            bail!("could not register the running Claude process");
        };
        session.child = Some(process.clone());
        store.update(|s| {
            s.leases
                .get_mut(token)
                .context("missing launch lease")?
                .process = if session.switching {
                session.owner.clone()
            } else {
                process
            };
            Ok(())
        })?;
        Ok(child)
    })
}

fn checked_destination(name: &str) -> Result<(String, state::Account)> {
    let saved = state::State::load()?;
    let (alias, account) = resolve_account(&saved, name)?;
    if let Err(error) = crate::commands::renew_native_account(alias, &account.email) {
        eprintln!("cs: {alias}: credential renewal unavailable: {error}");
    }
    let saved = state::State::load()?;
    let (alias, account) = resolve_account(&saved, name)?;
    if !crate::routing::store::token_valid(account, crate::usage::now()) {
        bail!(
            "{alias}: destination credentials are expired or unavailable; sessions were not moved"
        );
    }
    let observation = crate::usage::Observation::load()?.unwrap_or_default();
    let usage = match observation
        .accounts
        .get(alias)
        .and_then(|e| fresh_pinned_usage(e, crate::usage::now()))
    {
        Some(usage) => usage,
        None => crate::usage::fetch(
            account.data["claudeAiOauth"]["accessToken"]
                .as_str()
                .unwrap(),
        )
        .with_context(|| format!("{alias}: cannot verify destination quota"))?,
    };
    validate_pin(alias, account, &usage, crate::usage::now())?;
    Ok((alias.into(), account.clone()))
}

fn prepare_move(
    session: &Session,
    args: &[OsString],
    source: &std::path::Path,
    token: &str,
) -> Result<MovePreparation> {
    if let Some(reason) = &session.unsupported {
        bail!("{reason}");
    }
    let target = session.request.clone().context("missing switch request")?;
    let (alias, account) = checked_destination(&target.alias)?;
    if alias != target.alias || account.email != target.email {
        bail!("destination account identity changed; switch canceled");
    }
    let resumed = crate::session_control::resume_args(args, &session.root)?;
    let profile = crate::native::prepare(&alias, &account, source)?;
    crate::native::verify(&profile, &alias, &account.email)?;
    let mut next = crate::native::command(&profile, resumed.clone())?;
    next.env("CS_NATIVE_LEASE", token);
    let original_profile = crate::native::profile_path(&session.alias)?;
    crate::native::verify(&original_profile, &session.alias, &session.email)?;
    let mut rollback = crate::native::command(&original_profile, resumed)?;
    rollback.env("CS_NATIVE_LEASE", token);
    Ok(MovePreparation {
        target,
        root: session.root.clone(),
        old_alias: session.alias.clone(),
        old_email: session.email.clone(),
        next: Some(next),
        rollback: Some(rollback),
        stage: MoveStage::Stopping,
        since: std::time::Instant::now(),
    })
}

#[allow(clippy::too_many_arguments)]
fn supervise(
    mut command: crate::native::NativeCommand,
    store: &Store,
    token: &str,
    alias: &str,
    email: &str,
    args: &[OsString],
    source: &std::path::Path,
) -> Result<std::process::ExitStatus> {
    use std::{
        io::IsTerminal,
        time::{Duration, Instant},
    };
    use tokio::signal::unix::{signal, SignalKind};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let mut hangup = signal(SignalKind::hangup())?;
        let controls = Controls::default_path()?;
        let mut child = spawn_managed(&mut command, store, &controls, token)?;
        let mut active_alias = alias.to_string();
        let mut active_email = email.to_string();
        let mut last_sync = Instant::now();
        let mut renewal: Option<std::process::Child> = None;
        let mut stopping: Option<Instant> = None;
        let mut terminating = false;
        let mut moving: Option<MovePreparation> = None;
        loop {
            if let Some(status) = child.0.try_wait()? {
                if terminating { return Ok(status); }
                let Some(move_) = moving.as_mut() else { return Ok(status); };
                let confirmed = controls.update(|s| Ok(s.get(token).is_some_and(|c| !c.switching && c.starting.is_none())))?;
                if move_.stage != MoveStage::Stopping && confirmed { return Ok(status); }
                let destination = if move_.stage == MoveStage::Stopping {
                    move_.stage = MoveStage::Starting;
                    command = move_.next.take().context("missing destination command")?;
                    move_.target.clone()
                } else if move_.stage == MoveStage::Starting {
                    eprintln!("cs: {}: destination failed to resume; restoring {}", move_.root, move_.old_alias);
                    move_.stage = MoveStage::Restoring;
                    command = move_.rollback.take().context("missing recovery command")?;
                    Target { interrupt: false, id: move_.target.id.clone(), alias: move_.old_alias.clone(), email: move_.old_email.clone() }
                } else {
                    bail!("conversation {} could not resume on either account; reopen it with cs run --account {} -- --resume {}", move_.root, move_.old_alias, move_.root);
                };
                controls.update(|sessions| {
                    let session = sessions.get_mut(token).context("missing switching session")?;
                    session.starting = Some(destination.clone());
                    session.switching = true;
                    session.busy = true;
                    if move_.stage == MoveStage::Restoring { session.error = Some("destination failed; restoring original account".into()); }
                    Ok(())
                })?;
                active_alias = destination.alias;
                active_email = destination.email;
                match spawn_managed(&mut command, store, &controls, token) {
                    Ok(replacement) => child = replacement,
                    Err(error) => {
                        eprintln!("cs: {}: could not start {}: {error}", move_.root, active_alias);
                        // Keep the reaped child so the next iteration enters recovery.
                        continue;
                    }
                }
                move_.since = Instant::now();
                stopping = None;
                last_sync = Instant::now();
                continue;
            }
            if let Some(move_) = moving.as_mut() {
                if move_.stage != MoveStage::Stopping {
                    let confirmed = controls.update(|s| Ok(s.get(token).is_some_and(|c| !c.switching && c.starting.is_none())))?;
                    if confirmed {
                        if move_.stage == MoveStage::Starting {
                            eprintln!("cs: {} moved: {} -> {} (same conversation resumed)", move_.root, move_.old_alias, move_.target.alias);
                        } else {
                            controls.update(|s| {
                                if let Some(c) = s.get_mut(token) { c.error = Some("destination failed; original account restored".into()); }
                                Ok(())
                            })?;
                            eprintln!("cs: {}: switch failed; conversation restored on {}", move_.root, move_.old_alias);
                        }
                        moving = None;
                    } else if move_.since.elapsed() > Duration::from_secs(60) {
                        let _ = child.0.kill();
                    }
                }
            }
            if stopping.is_some_and(|time| time.elapsed() >= Duration::from_secs(5)) { let _ = child.0.kill(); }
            if moving.is_none() && !terminating {
                if let Some(session) = controls.claim(token)? {
                    match prepare_move(&session, args, source, token) {
                        Ok(prepared) => {
                            let still_idle = controls.update(|s| {
                                if !s.get(token).is_some_and(|c| c.root == session.root && c.alias == session.alias && (prepared.target.interrupt || (!c.busy && c.agents.is_empty())) && c.child.as_ref() == session.child.as_ref()) { return Ok(false); }
                                store.update(|s| {
                                    s.leases.get_mut(token).context("missing launch lease")?.process = Process::current()?;
                                    Ok(())
                                })?;
                                unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM); }
                                Ok(true)
                            })?;
                            if still_idle {
                                eprintln!("cs: {}: resuming on {}", prepared.root, prepared.target.alias);
                                stopping = Some(Instant::now());
                                moving = Some(prepared);
                            } else {
                                controls.cancel(token, "session changed during switch preparation; retry".into())?;
                            }
                        }
                        Err(error) => {
                            eprintln!("cs: targeted switch failed; session kept on {active_alias}: {error}");
                            controls.cancel(token, error.to_string())?;
                        }
                    }
                }
            }
            if let Some(task) = renewal.as_mut() {
                if task.try_wait()?.is_some() { renewal = None; }
            }
            if last_sync.elapsed() >= Duration::from_secs(30) && renewal.is_none() && stopping.is_none() && moving.is_none() {
                use std::os::unix::process::CommandExt;
                // Finish persisting a rotating grant even if the conversation exits meanwhile.
                renewal = Some(std::process::Command::new(std::env::current_exe()?)
                    .args(["native-maintain", &active_alias, &active_email])
                    .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null())
                    .process_group(0).spawn()?);
                last_sync = Instant::now();
            }
            tokio::select! {
                _ = interrupt.recv() => {
                    // Terminal SIGINT already reaches Claude's process group.
                    if !std::io::stdin().is_terminal() { unsafe { libc::kill(child.0.id() as i32, libc::SIGINT); } }
                }
                _ = terminate.recv() => { unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM); } stopping = Some(Instant::now()); terminating = true; }
                _ = hangup.recv() => { unsafe { libc::kill(child.0.id() as i32, libc::SIGHUP); } stopping = Some(Instant::now()); terminating = true; }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
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
    if let Err(error) = Controls::default_path()?.event(
        &token,
        root,
        kind,
        &event,
        std::env::var("CS_NATIVE_GENERATION").ok().as_deref(),
    ) {
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

pub fn switch_targeted(
    destination: &str,
    from: Option<&str>,
    session: Option<&str>,
    interrupt: bool,
) -> Result<()> {
    let accounts = state::State::load()?;
    let source = from
        .map(|name| resolve_account(&accounts, name).map(|(alias, _)| alias.to_string()))
        .transpose()?;
    if let Some(root) = session {
        uuid::Uuid::parse_str(root)
            .context("--session requires a conversation UUID from cs sessions")?;
    }
    let (destination_alias, _) = resolve_account(&accounts, destination)?;
    let selected = Store::default_path()?.update(|s| {
        Ok(s.leases
            .iter()
            .filter_map(|(token, lease)| {
                let root = lease.root.as_ref()?;
                let binding = s.bindings.get(root)?;
                let matches = source
                    .as_deref()
                    .is_some_and(|alias| alias == binding.alias)
                    || session.is_some_and(|id| id == root);
                (matches && binding.alias != destination_alias).then(|| {
                    (
                        token.clone(),
                        root.clone(),
                        binding.alias.clone(),
                        lease.process.clone(),
                    )
                })
            })
            .collect::<Vec<_>>())
    })?;
    if selected.is_empty() {
        bail!("no matching live conversations need moving to {destination_alias}; use cs sessions to see current assignments");
    }
    let (alias, account) = checked_destination(destination)?;
    let target = Target {
        interrupt,
        id: uuid::Uuid::new_v4().to_string(),
        alias,
        email: account.email,
    };
    Controls::default_path()?.queue(&selected, &target)?;
    for (_, root, old, _) in selected {
        println!(
            "Queued {root}: {old} -> {} ({})",
            target.alias,
            if interrupt {
                "interrupting selected session"
            } else {
                "waiting for Claude idle notification; typically about 60 seconds after the response"
            }
        );
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
    let controls = Controls::default_path()?.update(|sessions| Ok(sessions.clone()))?;
    Store::default_path()?.update(|s| {
        if s.leases.is_empty() {
            println!("No managed Claude sessions running.");
        }
        for (token, lease) in &s.leases {
            let root = lease.root.as_deref().unwrap_or("waiting for first request");
            let account = s
                .bindings
                .get(root)
                .map(|b| b.alias.as_str())
                .unwrap_or("unassigned");
            let status = controls
                .get(token)
                .and_then(|c| {
                    c.request
                        .as_ref()
                        .map(|target| format!("  -> {} (queued)", target.alias))
                        .or_else(|| c.error.as_ref().map(|error| format!("  switch: {error}")))
                })
                .unwrap_or_default();
            println!("{}  {}  pid {}{}", root, account, lease.process.pid, status);
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
