use crate::{routing::sessions::Process, state, util};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    path::PathBuf,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Target {
    #[serde(default)]
    pub interrupt: bool,
    pub id: String,
    pub alias: String,
    pub email: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    pub generation: String,
    pub owner: Process,
    pub child: Option<Process>,
    pub root: String,
    pub alias: String,
    pub email: String,
    pub busy: bool,
    pub agents: BTreeSet<String>,
    pub unsupported: Option<String>,
    pub request: Option<Target>,
    pub switching: bool,
    pub starting: Option<Target>,
    pub error: Option<String>,
    pub cwd: PathBuf,
}

pub struct Controls {
    pub path: PathBuf,
}
impl Controls {
    pub fn default_path() -> Result<Self> {
        Ok(Self {
            path: state::state_path()?.with_file_name("session-controls.json"),
        })
    }
    pub fn update<T>(
        &self,
        f: impl FnOnce(&mut BTreeMap<String, Session>) -> Result<T>,
    ) -> Result<T> {
        let _lock = state::lock_at(&self.path.with_extension("lock"), true)?;
        let mut sessions = match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice::<BTreeMap<String, Session>>(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        let before = serde_json::to_vec(&sessions)?;
        sessions.retain(|_, session| session.owner.alive());
        let result = f(&mut sessions)?;
        let after = serde_json::to_vec(&sessions)?;
        if before != after {
            util::atomic_write(&self.path, &after)?;
        }
        Ok(result)
    }
    pub fn queue(
        &self,
        selected: &[(String, String, String, Process)],
        target: &Target,
    ) -> Result<()> {
        self.update(|sessions| {
            for (token, root, alias, process) in selected {
                let session = sessions.get(token).with_context(|| format!("{root}: launcher does not support targeted switching; reopen it with the updated cs run first"))?;
                if session.root != *root || session.alias != *alias || session.child.as_ref() != Some(process) || !process.alive() {
                    bail!("{root}: session changed while selecting targets; retry");
                }
                if let Some(reason) = &session.unsupported { bail!("{root}: {reason}"); }
                if session.request.is_some() || session.switching { bail!("{root}: a switch is already pending"); }
            }
            for (token, _, _, _) in selected {
                let session = sessions.get_mut(token).unwrap();
                session.request = Some(target.clone());
                session.error = None;
            }
            Ok(())
        })
    }
    pub fn claim(&self, token: &str) -> Result<Option<Session>> {
        self.update(|sessions| {
            let Some(session) = sessions.get_mut(token) else {
                return Ok(None);
            };
            if session.request.is_none()
                || (!session.request.as_ref().is_some_and(|r| r.interrupt)
                    && (session.busy || !session.agents.is_empty()))
                || session.switching
            {
                return Ok(None);
            }
            session.switching = true;
            Ok(Some(session.clone()))
        })
    }
    pub fn cancel(&self, token: &str, error: String) -> Result<()> {
        self.update(|sessions| {
            if let Some(session) = sessions.get_mut(token) {
                session.request = None;
                session.switching = false;
                session.starting = None;
                session.error = Some(error);
            }
            Ok(())
        })
    }
    pub fn event(
        &self,
        token: &str,
        root: &str,
        kind: &str,
        payload: &serde_json::Value,
        generation: Option<&str>,
    ) -> Result<()> {
        self.update(|sessions| {
            let Some(session) = sessions.get_mut(token) else {
                return crate::routing::store::Store::default_path()?.native_event(token, root, kind, crate::usage::now());
            };
            if generation != Some(session.generation.as_str()) {
                bail!("event belongs to a previous Claude process");
            }
            let agent = payload["agent_id"].as_str();
            let cwd = payload["cwd"].as_str();
            if kind == "SubagentStart" {
                session.agents.insert(agent.context("missing subagent identity")?.into());
                return Ok(());
            }
            if kind == "SubagentStop" {
                // A SubagentStop hook may still ask the agent to continue.
                // Only Claude’s idle notification confirms all agents finished.
                return Ok(());
            }
            if agent.is_some() { return Ok(()); }
            if session.switching && !(kind == "SessionStart" && session.starting.is_some()) {
                bail!("account switch is in progress; submit your prompt again after the conversation resumes");
            }
            if let Some(target) = session.starting.clone().filter(|_| kind == "SessionStart") {
                if session.root != root { bail!("replacement resumed a different conversation"); }
                crate::routing::store::Store::default_path()?.update(|s| {
                    let lease = s.leases.get_mut(token).context("session lease disappeared during switch")?;
                    lease.root = Some(root.into());
                    if let Some(child) = &session.child {
                        lease.process = child.clone();
                    }
                    let last_request = s.bindings.get(root).map_or(0, |b| b.last_request);
                    s.bindings.insert(root.into(), crate::routing::assignments::Binding {
                        alias: target.alias.clone(), email: target.email.clone(), last_request, in_flight: 0,
                    });
                    Ok(())
                })?;
                session.alias = target.alias;
                session.email = target.email;
                session.starting = None;
                session.request = None;
                session.switching = false;
                session.busy = true;
                return Ok(());
            }
            crate::routing::store::Store::default_path()?.native_event(token, root, kind, crate::usage::now())?;
            if session.root != root {
                if session.request.take().is_some() { session.error = Some("switch canceled because the conversation changed".into()); }
                session.root = root.into();
            }
            if let Some(cwd) = cwd {
                if std::fs::canonicalize(cwd).ok() != std::fs::canonicalize(&session.cwd).ok() {
                    session.unsupported = Some("conversation changed working directory; restart it manually on the target account".into());
                }
            }
            match kind {
                "UserPromptSubmit" => session.busy = true,
                "StopFailure" => session.busy = false,
                "Notification" => match payload["notification_type"].as_str() {
                    Some("idle_prompt") => { session.busy = false; session.agents.clear(); }
                    Some("quota_auto_resume_fired") => session.busy = true,
                    _ => {}
                },
                _ => {}
            }
            Ok(())
        })
    }
}

pub fn resume_args(args: &[OsString], root: &str) -> Result<Vec<OsString>> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i]
            .to_str()
            .context("non-UTF-8 launch flags cannot be switched automatically")?;
        if arg == "--" {
            break;
        }
        let key = arg.split('=').next().unwrap();
        if matches!(
            key,
            "-p" | "--print" | "-w" | "--worktree" | "--tmux" | "--from-pr" | "--teleport"
        ) {
            bail!("this launch mode cannot be resumed automatically; restart it manually on the target account");
        }
        if matches!(
            key,
            "--resume" | "-r" | "--session-id" | "--resume-session-at"
        ) {
            i += if arg.contains('=') { 1 } else { 2 };
            continue;
        }
        if matches!(key, "--continue" | "-c" | "--fork-session") {
            i += 1;
            continue;
        }
        if !arg.starts_with('-') {
            i += 1;
            continue;
        }
        let values = match key {
            "--chrome" | "--no-chrome" | "--dangerously-skip-permissions" | "--allow-dangerously-skip-permissions" | "--verbose" | "--strict-mcp-config" | "--disable-slash-commands" | "--brief" | "--ax-screen-reader" => 0,
            "--model" | "--agent" | "--agents" | "--effort" | "--permission-mode" | "--name" | "-n" | "--settings" | "--setting-sources" | "--system-prompt" | "--append-system-prompt" | "--system-prompt-file" | "--append-system-prompt-file" | "--system-prompt-snapshot" | "--autocompact" | "--fallback-model" | "--debug-file" => 1,
            "--add-dir" | "--allowedTools" | "--allowed-tools" | "--disallowedTools" | "--disallowed-tools" | "--tools" | "--mcp-config" | "--plugin-dir" | "--betas" => 2,
            _ => bail!("launch flag {key} is not supported for automatic resume; restart this session manually"),
        };
        result.push(args[i].clone());
        i += 1;
        if !arg.contains('=') && values > 0 {
            result.push(
                args.get(i)
                    .context("launch flag is missing its value")?
                    .clone(),
            );
            i += 1;
            if values == 2 {
                while i < args.len() && !args[i].to_string_lossy().starts_with('-') {
                    result.push(args[i].clone());
                    i += 1;
                }
            }
        }
    }
    result.extend([OsString::from("--resume"), OsString::from(root)]);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn session(alias: &str, root: &str) -> Session {
        Session {
            generation: "test".into(),
            owner: Process::current().unwrap(),
            child: Some(Process::current().unwrap()),
            root: root.into(),
            alias: alias.into(),
            email: format!("{alias}@x"),
            busy: false,
            agents: BTreeSet::new(),
            unsupported: None,
            request: None,
            switching: false,
            starting: None,
            error: None,
            cwd: std::env::current_dir().unwrap(),
        }
    }
    fn target() -> Target {
        Target {
            interrupt: false,
            id: "request".into(),
            alias: "b".into(),
            email: "b@x".into(),
        }
    }
    #[test]
    fn batch_validation_never_queues_a_partial_move() {
        let dir = tempfile::tempdir().unwrap();
        let controls = Controls {
            path: dir.path().join("controls.json"),
        };
        controls
            .update(|s| {
                s.insert("one".into(), session("a", "root1"));
                Ok(())
            })
            .unwrap();
        let selected = vec![
            (
                "one".into(),
                "root1".into(),
                "a".into(),
                Process::current().unwrap(),
            ),
            (
                "old-launcher".into(),
                "root2".into(),
                "a".into(),
                Process::current().unwrap(),
            ),
        ];
        assert!(controls.queue(&selected, &target()).is_err());
        assert!(controls.update(|s| Ok(s["one"].request.is_none())).unwrap());
        controls.queue(&selected[..1], &target()).unwrap();
        assert_eq!(
            controls
                .claim("one")
                .unwrap()
                .unwrap()
                .request
                .unwrap()
                .alias,
            "b"
        );
    }
    #[test]
    fn busy_sessions_and_background_agents_wait_before_switching() {
        let dir = tempfile::tempdir().unwrap();
        let controls = Controls {
            path: dir.path().join("controls.json"),
        };
        let mut value = session("a", "root");
        value.busy = true;
        value.request = Some(target());
        controls
            .update(|s| {
                s.insert("one".into(), value);
                Ok(())
            })
            .unwrap();
        assert!(controls.claim("one").unwrap().is_none());
        controls
            .update(|s| {
                let s = s.get_mut("one").unwrap();
                s.busy = false;
                s.agents.insert("agent".into());
                Ok(())
            })
            .unwrap();
        assert!(controls.claim("one").unwrap().is_none());
        controls
            .update(|s| {
                s.get_mut("one").unwrap().agents.clear();
                Ok(())
            })
            .unwrap();
        assert!(controls.claim("one").unwrap().is_some());
        assert!(controls.claim("one").unwrap().is_none());
    }
    #[test]
    fn restart_preserves_flags_and_does_not_replay_initial_prompt_or_fork() {
        let args: Vec<OsString> = [
            "--chrome",
            "--model",
            "sonnet",
            "--dangerously-skip-permissions",
            "--resume",
            "old",
            "--fork-session",
            "--session-id",
            "fork",
            "initial prompt",
        ]
        .map(Into::into)
        .to_vec();
        let result = resume_args(&args, "current").unwrap();
        assert_eq!(
            result,
            [
                "--chrome",
                "--model",
                "sonnet",
                "--dangerously-skip-permissions",
                "--resume",
                "current"
            ]
            .map(OsString::from)
        );
        assert!(resume_args(&["-p".into(), "hi".into()], "root").is_err());
        assert!(resume_args(&["--unknown".into(), "value".into()], "root").is_err());
        assert!(resume_args(&["--worktree".into()], "root").is_err());
    }
}
