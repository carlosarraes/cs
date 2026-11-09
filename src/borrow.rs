use crate::{routing::store::Store, state, usage};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{ffi::OsString, process::Command};

#[derive(Serialize, Deserialize)]
struct Grant {
    alias: String,
    email: String,
    lease_id: String,
    expires_at: i64,
    access_token: String,
}
impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grant")
            .field("access_token", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}
pub fn lend() -> Result<()> {
    let saved = state::State::load()?;
    if saved.claude().accounts.values().all(state::Account::is_key) {
        bail!("no saved subscription accounts");
    }
    for (alias, account) in &saved.claude().accounts {
        if !account.is_key() {
            crate::commands::renew_native_account(alias, &account.email)
                .map_err(|_| anyhow::anyhow!("owner credential renewal failed"))?;
        }
    }
    let saved = state::State::load()?;
    let mut observation = usage::Observation::load()?.unwrap_or_default();
    let cfg = crate::config::load()?.routing;
    let eligible = observation
        .accounts
        .iter()
        .filter(|(alias, _)| {
            saved
                .claude()
                .accounts
                .get(*alias)
                .is_some_and(|a| crate::routing::store::token_valid(a, usage::now()))
        })
        .map(|(alias, entry)| (alias.clone(), entry.clone()))
        .collect();
    if crate::routing::policy::select(
        &eligible,
        &Default::default(),
        usage::now(),
        cfg.five_hour_ceiling,
        cfg.weekly_ceiling,
        cfg.max_usage_age_secs as i64,
    )
    .is_none()
    {
        let mut provider = saved.claude().clone();
        provider.current = None;
        observation = usage::observe(&provider, Some(&observation), usage::now(), 60);
        observation.save()?;
    }
    let store = Store::default_path()?;
    let lease_id = store.register()?;
    let result = (|| -> Result<()> {
        let (account, alias) = store.reserve_native(
            &lease_id,
            &uuid::Uuid::new_v4().to_string(),
            None,
            &saved,
            &observation.accounts,
            &cfg,
        )?;
        let oauth = &account.data["claudeAiOauth"];
        let expires_at = oauth["expiresAt"]
            .as_i64()
            .map(|v| v / 1000)
            .unwrap_or(usage::now() + 600);
        let access_token = oauth["accessToken"]
            .as_str()
            .context("missing access token")?
            .to_owned();
        store.update(|s| {
            s.leases
                .get_mut(&lease_id)
                .context("missing lease")?
                .expires_at = Some(expires_at);
            Ok(())
        })?;
        eprintln!("cs: lend -> {alias} ({})", account.email);
        println!(
            "{}",
            serde_json::to_string(&Grant {
                alias,
                email: account.email,
                lease_id: lease_id.clone(),
                expires_at,
                access_token
            })?
        );
        Ok(())
    })();
    if result.is_err() {
        store.unregister(&lease_id)?;
    }
    result
}
pub fn release(id: &str) -> Result<()> {
    Store::default_path()?.unregister(id)
}
fn ssh(host: &str, remote: &str) -> Result<std::process::Output> {
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._@-".contains(&b))
        || host.starts_with('-')
    {
        bail!("invalid borrow host");
    }
    Command::new(std::env::var_os("CS_SSH").unwrap_or_else(|| "ssh".into()))
        .args(["-o", "BatchMode=yes", "-o", "ControlPath=none", host])
        .arg(format!(
            "export PATH=\"$HOME/.local/bin:$PATH\"; exec cs {remote}"
        ))
        .output()
        .context("running ssh")
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
    let host = host.or_else(|| std::env::var("CS_BORROW").ok());
    let Some(host) = host else {
        return if run {
            crate::launch::run(remaining, requested)
        } else {
            crate::launch::t3_claude(remaining, pick)
        };
    };
    if requested.is_some() {
        bail!("--borrow conflicts with --account");
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
    if pick {
        println!("{} ({})", lease.grant.alias, lease.grant.email);
        return Ok(());
    }
    let profile = std::env::temp_dir().join(format!("cs-borrow-{}", uuid::Uuid::new_v4()));
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(&profile)?;
    let result =
        (|| -> Result<std::process::ExitStatus> {
            if run && remaining.first().is_some_and(|a| a == "claude") {
                remaining.remove(0);
            }
            let mut command = crate::native::command(&profile, remaining)?;
            command
                .env("CLAUDE_CODE_OAUTH_TOKEN", &lease.grant.access_token)
                .env_remove("CS_NATIVE_LEASE")
                .env_remove("CS_NATIVE_GENERATION");
            eprintln!(
                "cs: {} --borrow {} -> {} ({})",
                if run { "run" } else { "t3-claude" },
                lease.host,
                lease.grant.alias,
                lease.grant.email
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
