use crate::{routing::store::Store, state, usage};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    ffi::OsString,
    io::Read,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Renew the lent account when its access token has less than this left.
const LEND_RENEW_MARGIN_MS: i64 = 30 * 60 * 1000;
/// Never hand out a token that expires sooner than this.
const MIN_LEND_SECS: i64 = 300;

#[derive(Serialize, Deserialize)]
pub struct Grant {
    pub alias: String,
    pub email: String,
    #[serde(default)]
    pub lease_id: String,
    pub expires_at: i64,
    pub access_token: String,
    /// Non-secret OAuth metadata (scopes, subscription type) Claude needs next to the token.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub oauth_meta: Value,
    /// The account's non-secret `oauthAccount` identity.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub identity: Value,
}
impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grant")
            .field("access_token", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InventoryEntry {
    pub alias: String,
    pub email: String,
    pub expires_at: i64,
    #[serde(default)]
    pub usage: Option<usage::Usage>,
    #[serde(default)]
    pub usage_fetched_at: Option<i64>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Inventory {
    pub accounts: Vec<InventoryEntry>,
}

fn oauth_expires_secs(account: &state::Account) -> i64 {
    account.data["claudeAiOauth"]["expiresAt"]
        .as_i64()
        .map_or(0, |ms| ms / 1000)
}

fn lendable(account: &state::Account) -> bool {
    let oauth = &account.data["claudeAiOauth"];
    !account.is_key()
        && ["accessToken", "refreshToken"]
            .iter()
            .any(|key| oauth[key].as_str().is_some_and(|s| !s.is_empty()))
}

fn inventory() -> Result<()> {
    let saved = state::State::load()?;
    let observation = usage::Observation::load()?.unwrap_or_default();
    let accounts: Vec<_> = saved
        .claude()
        .accounts
        .iter()
        .filter(|(_, account)| lendable(account))
        .map(|(alias, account)| {
            let entry = observation.accounts.get(alias);
            InventoryEntry {
                alias: alias.clone(),
                email: account.email.clone(),
                expires_at: oauth_expires_secs(account),
                usage: entry.and_then(|e| e.reading.known().cloned()),
                usage_fetched_at: entry.and_then(|e| e.fetched_at),
            }
        })
        .collect();
    if accounts.is_empty() {
        bail!("no saved subscription accounts");
    }
    println!("{}", serde_json::to_string(&Inventory { accounts })?);
    Ok(())
}

/// Renew one account if it is near expiry and describe it as a grant. Renewal trouble is only
/// fatal when the stored token cannot be handed out anyway.
fn grant_for(
    alias: &str,
    lease: Option<(&Store, &usage::Observation, &crate::config::Routing)>,
) -> Result<Grant> {
    let saved = state::State::load()?;
    let account = saved
        .claude()
        .accounts
        .get(alias)
        .with_context(|| format!("unknown account {alias}"))?;
    if !lendable(account) {
        bail!("{alias}: not a subscription account that can be lent");
    }
    if let Err(error) =
        crate::commands::renew_native_account_within(alias, &account.email, LEND_RENEW_MARGIN_MS)
    {
        eprintln!("cs: lend: {alias}: credential renewal unavailable: {error}");
    }
    let saved = state::State::load()?;
    let mut account = saved
        .claude()
        .accounts
        .get(alias)
        .with_context(|| format!("{alias}: account removed"))?
        .clone();
    let expires_at = oauth_expires_secs(&account);
    let access_token = account.data["claudeAiOauth"]["accessToken"]
        .as_str()
        .filter(|token| !token.is_empty())
        .map(str::to_owned);
    let Some(access_token) = access_token.filter(|_| expires_at > usage::now() + MIN_LEND_SECS)
    else {
        bail!("{alias}: access token expired and could not be renewed");
    };
    let mut lease_id = String::new();
    if let Some((store, observation, cfg)) = lease {
        lease_id = store.register()?;
        let reserved = store.reserve_native(
            &lease_id,
            &uuid::Uuid::new_v4().to_string(),
            Some(alias),
            &saved,
            &observation.accounts,
            cfg,
        );
        let updated = reserved.and_then(|_| {
            store.update(|s| {
                s.leases
                    .get_mut(&lease_id)
                    .context("missing lease")?
                    .expires_at = Some(expires_at);
                Ok(())
            })
        });
        if let Err(error) = updated {
            store.unregister(&lease_id)?;
            return Err(error);
        }
    }
    let mut meta = account.data["claudeAiOauth"].take();
    if let Some(object) = meta.as_object_mut() {
        for key in ["accessToken", "refreshToken", "expiresAt"] {
            object.remove(key);
        }
    }
    Ok(Grant {
        alias: alias.to_owned(),
        email: account.email.clone(),
        lease_id,
        expires_at,
        access_token,
        oauth_meta: meta,
        identity: account.data["oauthAccount"].clone(),
    })
}

fn select_lend_alias(
    saved: &state::State,
    observation: &usage::Observation,
    store: &Store,
    cfg: &crate::config::Routing,
    tried: &BTreeSet<String>,
) -> Result<Option<String>> {
    let eligible = observation
        .accounts
        .iter()
        .filter(|(alias, _)| {
            !tried.contains(*alias) && saved.claude().accounts.get(*alias).is_some_and(lendable)
        })
        .map(|(alias, entry)| (alias.clone(), entry.clone()))
        .collect();
    let counts = store.update(|s| Ok(s.counts()))?;
    Ok(crate::routing::policy::select(
        &eligible,
        &counts,
        usage::now(),
        cfg.five_hour_ceiling,
        cfg.weekly_ceiling,
        cfg.max_usage_age_secs as i64,
    )
    .map(str::to_owned))
}

/// Hand out one account's access token. Only the handed-out account is ever renewed.
pub fn lend(alias: Option<&str>, want_inventory: bool, no_lease: bool) -> Result<()> {
    if want_inventory {
        return inventory();
    }
    let saved = state::State::load()?;
    if saved.claude().accounts.values().all(state::Account::is_key) {
        bail!("no saved subscription accounts");
    }
    let cfg = crate::config::load()?.routing;
    let store = Store::default_path()?;
    let mut observation = usage::Observation::load()?.unwrap_or_default();
    let grant = if let Some(alias) = alias {
        grant_for(alias, (!no_lease).then_some((&store, &observation, &cfg)))?
    } else {
        let mut tried = BTreeSet::new();
        let mut observed = false;
        loop {
            let choice = select_lend_alias(&saved, &observation, &store, &cfg, &tried)?;
            let Some(choice) = choice else {
                if tried.is_empty() && !observed {
                    observed = true;
                    let mut provider = saved.claude().clone();
                    provider.current = None;
                    observation = usage::observe(&provider, Some(&observation), usage::now(), 60);
                    observation.save()?;
                    continue;
                }
                bail!("no account has fresh usage and quota headroom; run cs usage --live");
            };
            match grant_for(&choice, (!no_lease).then_some((&store, &observation, &cfg))) {
                Ok(grant) => break grant,
                Err(error) => {
                    eprintln!("cs: lend: skipping {choice}: {error}");
                    tried.insert(choice);
                }
            }
        }
    };
    eprintln!("cs: lend -> {} ({})", grant.alias, grant.email);
    println!("{}", serde_json::to_string(&grant)?);
    Ok(())
}
pub fn release(id: &str) -> Result<()> {
    Store::default_path()?.unregister(id)
}

pub fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && !host.starts_with(['-', '.'])
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._@-".contains(&b))
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// The jump host from `--via`, else `CS_BORROW_JUMP`; empty means direct.
pub fn jump_host(via: Option<&str>) -> Option<String> {
    via.map(str::to_owned)
        .or_else(|| std::env::var("CS_BORROW_JUMP").ok())
        .filter(|v| !v.is_empty())
}

/// `ssh HOST cs REMOTE`, or the allowed hop `ssh VIA ssh HOST cs REMOTE` when a jump is set.
fn ssh_command(host: &str, via: Option<&str>, remote: &str) -> Result<Command> {
    if !valid_host(host) || via.is_some_and(|v| !valid_host(v)) {
        bail!("invalid borrow host");
    }
    let script = format!("export PATH=\"$HOME/.local/bin:$PATH\"; exec cs {remote}");
    let mut command = Command::new(std::env::var_os("CS_SSH").unwrap_or_else(|| "ssh".into()));
    command.args(["-o", "BatchMode=yes", "-o", "ControlPath=none"]);
    match via {
        None => command.arg(host).arg(script),
        Some(via) => command.arg(via).arg(format!(
            "ssh -o BatchMode=yes -o ControlPath=none {host} {}",
            shell_quote(&script)
        )),
    };
    Ok(command)
}

fn ssh(host: &str, remote: &str) -> Result<std::process::Output> {
    ssh_command(host, jump_host(None).as_deref(), remote)?
        .output()
        .context("running ssh")
}

/// Run an ssh command with a hard deadline. The child dies with this process, and is killed
/// when the deadline passes, so a hung hop cannot outlive a t3 probe.
pub fn ssh_timeout(
    host: &str,
    via: Option<&str>,
    remote: &str,
    deadline: Instant,
) -> Result<Vec<u8>> {
    let mut command = ssh_command(host, via, remote)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: prctl is async-signal-safe and only sets this child's own attribute.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
    }
    let mut child = command.spawn().context("running ssh")?;
    let mut stdout = child.stdout.take().context("ssh output")?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = (&mut stdout).take(1 << 20).read_to_end(&mut buffer);
        let _ = sender.send(buffer);
    });
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("ssh to {host} timed out");
        }
        std::thread::sleep(Duration::from_millis(15));
    };
    let output = receiver
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or_default();
    if !status.success() {
        bail!("remote command failed on {host}");
    }
    Ok(output)
}

struct RemoteLease {
    host: String,
    grant: Grant,
}
impl Drop for RemoteLease {
    fn drop(&mut self) {
        let id = self.grant.lease_id.replace('\'', "'\"'\"'");
        if !ssh(&self.host, &format!("lend-release '{id}'")).is_ok_and(|o| o.status.success()) {
            eprintln!("cs: could not release borrowed lease");
        }
    }
}
pub fn pin_from_env() -> Option<String> {
    std::env::var("CS_PIN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn dispatch(
    mut args: Vec<OsString>,
    requested: Option<&str>,
    pick: bool,
    run: bool,
) -> Result<()> {
    let mut host = None;
    let mut remaining = Vec::new();
    let mut it = args.drain(..);
    while let Some(arg) = it.next() {
        if arg == "--borrow" {
            host = Some(
                it.next()
                    .context("--borrow requires a host")?
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("invalid borrow host"))?,
            );
        } else if let Some(value) = arg.to_str().and_then(|a| a.strip_prefix("--borrow=")) {
            host = Some(value.to_owned());
        } else {
            remaining.push(arg);
        }
    }
    let pin = pin_from_env();
    let host = host.or_else(|| std::env::var("CS_BORROW").ok().filter(|h| !h.is_empty()));
    let Some(host) = host else {
        let requested = requested.or(pin.as_deref());
        return if run {
            crate::launch::run(remaining, requested)
        } else {
            crate::launch::t3_claude(remaining, pick, requested)
        };
    };
    if requested.is_some() {
        bail!("--borrow conflicts with --account");
    }
    if !run {
        return crate::borrow_cache::t3(&host, remaining, pick, pin.as_deref());
    }
    let output = ssh(&host, "lend")?;
    if !output.status.success() {
        bail!("lend failed on {host}");
    }
    let grant: Grant = serde_json::from_slice(&output.stdout)
        .map_err(|_| anyhow::anyhow!("invalid lend response from {host}"))?;
    let lease = RemoteLease { host, grant };
    if lease.grant.access_token.is_empty() || lease.grant.expires_at <= usage::now() {
        bail!("expired or empty lend grant");
    }
    let profile = std::env::temp_dir().join(format!("cs-borrow-{}", uuid::Uuid::new_v4()));
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(&profile)?;
    let result =
        (|| -> Result<std::process::ExitStatus> {
            if remaining.first().is_some_and(|a| a == "claude") {
                remaining.remove(0);
            }
            let mut command = crate::native::command(&profile, remaining)?;
            command
                .env("CLAUDE_CODE_OAUTH_TOKEN", &lease.grant.access_token)
                .env_remove("CS_NATIVE_LEASE")
                .env_remove("CS_NATIVE_GENERATION");
            eprintln!(
                "cs: run --borrow {} -> {} ({})",
                lease.host, lease.grant.alias, lease.grant.email
            );
            tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
            use tokio::signal::unix::{signal, SignalKind};
            let mut terminate = signal(SignalKind::terminate())?;
            let mut interrupt = signal(SignalKind::interrupt())?;
            let mut child = command.spawn().context("launching Claude")?;
            loop {
                if let Some(status) = child.try_wait()? { return Ok(status); }
                let sig = tokio::select! {
                    _ = terminate.recv() => libc::SIGTERM,
                    _ = interrupt.recv() => libc::SIGINT,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => continue,
                };
                unsafe { libc::kill(child.id() as i32, sig); }
            }
        })
        })();
    let _ = std::fs::remove_dir_all(profile);
    drop(lease);
    use std::os::unix::process::ExitStatusExt;
    let status = result?;
    std::process::exit(
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_restricted_and_quoting_survives_two_shells() {
        for ok in ["mac", "user@mac.local", "a_b-c.d"] {
            assert!(valid_host(ok));
        }
        for bad in ["", "-oProxyCommand=x", ".", "..", "a b", "a;b", "a$(x)"] {
            assert!(!valid_host(bad), "{bad:?}");
        }
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
    }

    #[test]
    fn grant_debug_redacts_the_token() {
        let grant = Grant {
            alias: "a".into(),
            email: "a@x".into(),
            lease_id: String::new(),
            expires_at: 1,
            access_token: "test-token".into(),
            oauth_meta: Value::Null,
            identity: Value::Null,
        };
        assert!(!format!("{grant:?}").contains("test-token"));
    }
}
