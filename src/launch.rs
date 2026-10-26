use crate::{
    routing::{sessions::Process, store::Store},
    state, util,
};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::{ffi::OsString, os::unix::process::CommandExt, path::PathBuf, process::Command};

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
pub fn run(args: Vec<OsString>) -> Result<()> {
    let endpoint: Endpoint = serde_json::from_slice(
        &std::fs::read(endpoint_path()?)
            .context("session router is not running; run cs start in another terminal")?,
    )?;
    if !endpoint.process.alive() {
        return Err(anyhow!("session router is not running; run cs start"));
    }
    let store = Store::default_path()?;
    let token = store.register()?;
    let base = format!("http://127.0.0.1:{}", endpoint.port);
    let check = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .get(&format!("{base}/health"))
        .set("Authorization", &format!("Bearer {token}"))
        .call();
    if check.is_err() {
        store.unregister(&token)?;
        return Err(anyhow!("session router is unavailable; run cs start"));
    }
    let mut command = Command::new("claude");
    command
        .args(args)
        .env("ANTHROPIC_BASE_URL", base)
        .env("ANTHROPIC_API_KEY", &token)
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
        .env_remove("CLAUDE_CODE_USE_BEDROCK")
        .env_remove("CLAUDE_CODE_USE_VERTEX")
        .env_remove("CLAUDE_CODE_USE_FOUNDRY");
    let error = command.exec();
    store.unregister(&token)?;
    Err(error).context("launching claude")
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
