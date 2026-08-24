//! Session snapshots: capture the set of currently-open Claude Code sessions
//! (with their tmux placement) so they can be reopened after a reboot.
//!
//! `cs snapshot` records each live session's `sessionId`, name, cwd, and where
//! it sits in tmux (session / window / pane — resolved to durable names while
//! everything is still alive). `cs snapshot restore` rebuilds that shape in
//! tmux and types `claude --resume <id>` into each pane.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use crate::state;
use crate::usage;
use crate::util;

const KEEP: usize = 10;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TmuxLoc {
    pub session: String,
    pub window_index: u32,
    pub window_name: String,
    pub pane_index: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sess {
    pub session_id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub cwd: String,
    #[serde(default)]
    pub tmux: Option<TmuxLoc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub taken_at: i64,
    pub sessions: Vec<Sess>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    snapshots: BTreeMap<String, Snapshot>,
}

fn store_path() -> Result<PathBuf> {
    Ok(state::state_path()?.with_file_name("snapshots.json"))
}

impl Store {
    fn load() -> Result<Store> {
        let path = store_path()?;
        if !path.exists() {
            return Ok(Store::default());
        }
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
    }

    fn save(&self) -> Result<()> {
        util::atomic_write(&store_path()?, &serde_json::to_vec_pretty(self)?)
    }
}

// --- capture ------------------------------------------------------------------

/// A live entry from `~/.claude/sessions/`, already liveness-checked.
struct LiveSess {
    session_id: String,
    name: Option<String>,
    cwd: String,
    /// Raw `"2:@16.%29"` reference, resolved against tmux later.
    tmux_ref: Option<String>,
}

fn live_sessions() -> Result<Vec<LiveSess>> {
    let dir = crate::claude::config_home()?.join("sessions");
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(out),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let Some(pid) = v.get("pid").and_then(Value::as_i64) else {
            continue;
        };
        let Some(session_id) = v.get("sessionId").and_then(Value::as_str) else {
            continue;
        };
        if !crate::claude::pid_alive(pid) {
            continue;
        }
        // Guard against pid reuse where we can compare exactly (Linux).
        if let Some(rec) = v.get("procStart").and_then(Value::as_str) {
            if !proc_start_matches(pid, rec) {
                continue;
            }
        }
        out.push(LiveSess {
            session_id: session_id.to_string(),
            name: v
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from),
            cwd: v
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or("~")
                .to_string(),
            tmux_ref: v.get("tmux").and_then(Value::as_str).map(String::from),
        });
    }
    Ok(out)
}

/// Linux: Claude records `/proc/<pid>/stat` starttime verbatim — exact match.
/// Elsewhere the recorded format/timezone is unreliable, so accept.
#[cfg(target_os = "linux")]
fn proc_start_matches(pid: i64, recorded: &str) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return true;
    };
    // Field 22, counting after the ')' that ends the (possibly space-filled) comm.
    let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
        return true;
    };
    rest.split_whitespace().nth(19) == Some(recorded)
}

#[cfg(not(target_os = "linux"))]
fn proc_start_matches(_pid: i64, _recorded: &str) -> bool {
    true
}

/// `"2:@16.%29"` → the `%29` pane id used to look the pane up in tmux.
fn pane_id_of(tmux_ref: &str) -> Option<&str> {
    let pane = tmux_ref.rsplit_once('.').map(|(_, p)| p)?;
    pane.starts_with('%').then_some(pane)
}

/// Ask tmux where every pane lives, keyed by pane id.
fn tmux_pane_map() -> BTreeMap<String, TmuxLoc> {
    let mut map = BTreeMap::new();
    let out = Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}\t#{session_name}\t#{window_index}\t#{window_name}\t#{pane_index}",
        ])
        .output();
    let Ok(out) = out else {
        return map;
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if let [pane, sess, win_i, win_name, pane_i] = parts[..] {
            if let (Ok(wi), Ok(pi)) = (win_i.parse(), pane_i.parse()) {
                map.insert(
                    pane.to_string(),
                    TmuxLoc {
                        session: sess.to_string(),
                        window_index: wi,
                        window_name: win_name.to_string(),
                        pane_index: pi,
                    },
                );
            }
        }
    }
    map
}

fn next_id(existing: &BTreeMap<String, Snapshot>, base: &str) -> String {
    if !existing.contains_key(base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|id| !existing.contains_key(id))
        .expect("unbounded")
}

fn prune(store: &mut Store, keep: usize) {
    while store.snapshots.len() > keep {
        if let Some(oldest) = store
            .snapshots
            .iter()
            .min_by_key(|(_, s)| s.taken_at)
            .map(|(id, _)| id.clone())
        {
            store.snapshots.remove(&oldest);
        }
    }
}

pub fn take() -> Result<()> {
    let live = live_sessions()?;
    if live.is_empty() {
        return Err(anyhow!("no Claude Code sessions are currently running"));
    }
    let panes = tmux_pane_map();
    let sessions: Vec<Sess> = live
        .into_iter()
        .map(|l| {
            let tmux = l
                .tmux_ref
                .as_deref()
                .and_then(pane_id_of)
                .and_then(|p| panes.get(p).cloned());
            Sess {
                session_id: l.session_id,
                name: l.name,
                cwd: l.cwd,
                tmux,
            }
        })
        .collect();
    let mut store = Store::load()?;
    let id = next_id(
        &store.snapshots,
        &chrono::Local::now().format("%m%d-%H%M").to_string(),
    );
    let n = sessions.len();
    store.snapshots.insert(
        id.clone(),
        Snapshot {
            taken_at: usage::now(),
            sessions,
        },
    );
    prune(&mut store, KEEP);
    store.save()?;
    println!("Snapshot '{id}' saved ({n} session{}).", plural(n));
    println!("After a reboot: `cs snapshot restore {id}` (or just `cs snapshot restore`).");
    Ok(())
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

// --- list / del ---------------------------------------------------------------

pub fn list(id: Option<&str>) -> Result<()> {
    let store = Store::load()?;
    if store.snapshots.is_empty() {
        return Err(anyhow!("no snapshots yet — take one with `cs snapshot`"));
    }
    match id {
        None => {
            let mut rows: Vec<_> = store.snapshots.iter().collect();
            rows.sort_by_key(|(_, s)| std::cmp::Reverse(s.taken_at));
            for (id, snap) in rows {
                let when = chrono::Local
                    .timestamp_opt(snap.taken_at, 0)
                    .single()
                    .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_else(|| "?".into());
                println!(
                    "{id:<12} {when}  {} session{}",
                    snap.sessions.len(),
                    plural(snap.sessions.len())
                );
            }
        }
        Some(id) => {
            let snap = store
                .snapshots
                .get(id)
                .ok_or_else(|| anyhow!("no snapshot '{id}' (see `cs snapshot list`)"))?;
            let width = snap
                .sessions
                .iter()
                .map(|s| s.name.as_deref().unwrap_or("-").len())
                .max()
                .unwrap_or(1);
            for s in &snap.sessions {
                let loc = s
                    .tmux
                    .as_ref()
                    .map(|t| format!("tmux {}:{}({})", t.session, t.window_index, t.window_name))
                    .unwrap_or_else(|| "no tmux".into());
                println!(
                    "{:<width$}  {}  {:<24}  {}",
                    s.name.as_deref().unwrap_or("-"),
                    s.session_id,
                    loc,
                    s.cwd
                );
            }
        }
    }
    Ok(())
}

pub fn del(id: &str) -> Result<()> {
    let mut store = Store::load()?;
    if store.snapshots.remove(id).is_none() {
        return Err(anyhow!("no snapshot '{id}'"));
    }
    store.save()?;
    println!("Deleted snapshot '{id}'.");
    Ok(())
}

// --- restore ------------------------------------------------------------------

struct WindowPlan<'a> {
    name: String,
    panes: Vec<&'a Sess>,
}

struct SessionPlan<'a> {
    tmux_session: String,
    windows: Vec<WindowPlan<'a>>,
}

/// Group sessions back into tmux-session → window → panes, in their original
/// order. Sessions that weren't in tmux land in a `restored` session, one
/// window each, named after the Claude session name (or its id prefix).
fn plan(sessions: &[Sess]) -> Vec<SessionPlan<'_>> {
    // (tmux session, window index) → panes sorted by pane index.
    type Windows<'a> = BTreeMap<(u32, String), Vec<(u32, &'a Sess)>>;
    let mut grouped: BTreeMap<String, Windows> = BTreeMap::new();
    let mut loose: Vec<&Sess> = Vec::new();
    for s in sessions {
        match &s.tmux {
            Some(t) => grouped
                .entry(t.session.clone())
                .or_default()
                .entry((t.window_index, t.window_name.clone()))
                .or_default()
                .push((t.pane_index, s)),
            None => loose.push(s),
        }
    }
    let mut plans: Vec<SessionPlan> = grouped
        .into_iter()
        .map(|(tmux_session, windows)| SessionPlan {
            tmux_session,
            windows: windows
                .into_iter()
                .map(|((_, name), mut panes)| {
                    panes.sort_by_key(|(pi, _)| *pi);
                    WindowPlan {
                        name,
                        panes: panes.into_iter().map(|(_, s)| s).collect(),
                    }
                })
                .collect(),
        })
        .collect();
    if !loose.is_empty() {
        plans.push(SessionPlan {
            tmux_session: "restored".into(),
            windows: loose
                .iter()
                .map(|s| WindowPlan {
                    name: window_name_for(s),
                    panes: vec![*s],
                })
                .collect(),
        });
    }
    plans
}

fn window_name_for(s: &Sess) -> String {
    s.name
        .clone()
        .unwrap_or_else(|| s.session_id.chars().take(8).collect())
}

fn tmux(args: &[&str]) -> Result<String> {
    let out = Command::new("tmux")
        .args(args)
        .output()
        .context("running tmux")?;
    if !out.status.success() {
        return Err(anyhow!(
            "tmux {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn restore(id: Option<&str>) -> Result<()> {
    let store = Store::load()?;
    let (id, snap) = match id {
        Some(id) => (
            id.to_string(),
            store
                .snapshots
                .get(id)
                .ok_or_else(|| anyhow!("no snapshot '{id}' (see `cs snapshot list`)"))?,
        ),
        None => store
            .snapshots
            .iter()
            .max_by_key(|(_, s)| s.taken_at)
            .map(|(id, s)| (id.clone(), s))
            .ok_or_else(|| anyhow!("no snapshots yet — take one with `cs snapshot`"))?,
    };

    // Skip sessions that are already open so we never double-resume.
    let running: Vec<String> = live_sessions()?.into_iter().map(|l| l.session_id).collect();
    let (skipped, wanted): (Vec<&Sess>, Vec<&Sess>) = snap
        .sessions
        .iter()
        .partition(|s| running.contains(&s.session_id));
    for s in &skipped {
        println!("~ {} already running — skipped", window_name_for(s));
    }
    if wanted.is_empty() {
        println!("Everything in '{id}' is already running.");
        return Ok(());
    }

    if !tmux_available() {
        println!("tmux not available — run these yourself:");
        for s in &wanted {
            println!("  cd {} && claude --resume {}", s.cwd, s.session_id);
        }
        return Ok(());
    }

    let wanted: Vec<Sess> = wanted.into_iter().cloned().collect();
    for sp in plan(&wanted) {
        let exists = Command::new("tmux")
            .args(["has-session", "-t", &format!("={}", sp.tmux_session)])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        let mut first_window = !exists;
        for w in &sp.windows {
            let cwd = &w.panes[0].cwd;
            let first_pane = if first_window {
                first_window = false;
                tmux(&[
                    "new-session",
                    "-d",
                    "-s",
                    &sp.tmux_session,
                    "-n",
                    &w.name,
                    "-c",
                    cwd,
                    "-P",
                    "-F",
                    "#{pane_id}",
                ])?
            } else {
                tmux(&[
                    "new-window",
                    "-t",
                    &format!("={}:", sp.tmux_session),
                    "-n",
                    &w.name,
                    "-c",
                    cwd,
                    "-P",
                    "-F",
                    "#{pane_id}",
                ])?
            };
            let mut pane_ids = vec![first_pane];
            for s in &w.panes[1..] {
                pane_ids.push(tmux(&[
                    "split-window",
                    "-t",
                    &pane_ids[0],
                    "-c",
                    &s.cwd,
                    "-P",
                    "-F",
                    "#{pane_id}",
                ])?);
            }
            if pane_ids.len() > 1 {
                tmux(&["select-layout", "-t", &pane_ids[0], "tiled"])?;
            }
            for (pane, s) in pane_ids.iter().zip(&w.panes) {
                tmux(&[
                    "send-keys",
                    "-t",
                    pane,
                    &format!("claude --resume {}", s.session_id),
                    "Enter",
                ])?;
                println!(
                    "* {}  →  tmux {}:{}",
                    window_name_for(s),
                    sp.tmux_session,
                    w.name
                );
            }
        }
    }
    println!(
        "Restored {} session{} from '{id}'.",
        wanted.len(),
        plural(wanted.len())
    );
    Ok(())
}

use chrono::TimeZone;

#[cfg(test)]
mod tests {
    use super::*;

    fn sess(id: &str, name: &str, tmux: Option<(&str, u32, &str, u32)>) -> Sess {
        Sess {
            session_id: id.into(),
            name: (!name.is_empty()).then(|| name.to_string()),
            cwd: "/w".into(),
            tmux: tmux.map(|(s, wi, wn, pi)| TmuxLoc {
                session: s.into(),
                window_index: wi,
                window_name: wn.into(),
                pane_index: pi,
            }),
        }
    }

    #[test]
    fn pane_id_parses_from_tmux_ref() {
        assert_eq!(pane_id_of("2:@16.%29"), Some("%29"));
        assert_eq!(pane_id_of("0:@67.%115"), Some("%115"));
        assert_eq!(pane_id_of("garbage"), None);
        assert_eq!(pane_id_of("2:@16.pane"), None);
    }

    #[test]
    fn plan_groups_sessions_windows_and_split_panes() {
        // Mirrors the real Mac layout: session 2 has a window with two panes.
        let sessions = vec![
            sess("s1", "supabase-migration", Some(("2", 16, "mig", 0))),
            sess("s2", "mondrio-cli", Some(("2", 16, "mig", 1))),
            sess("s3", "qa", Some(("2", 2, "qa", 0))),
            sess("s4", "dh-pm", Some(("0", 3, "pm", 0))),
            sess("s5", "", None),
        ];
        let plans = plan(&sessions);
        assert_eq!(plans.len(), 3);
        assert_eq!(plans[0].tmux_session, "0");
        assert_eq!(plans[1].tmux_session, "2");
        // Windows come back in index order: qa (2) before mig (16).
        assert_eq!(plans[1].windows[0].name, "qa");
        assert_eq!(plans[1].windows[1].name, "mig");
        assert_eq!(plans[1].windows[1].panes.len(), 2);
        assert_eq!(plans[1].windows[1].panes[0].session_id, "s1");
        // The non-tmux one lands in "restored", window named by id prefix.
        assert_eq!(plans[2].tmux_session, "restored");
        assert_eq!(plans[2].windows[0].name, "s5");
    }

    #[test]
    fn ids_dedupe_and_store_prunes_oldest() {
        let mut store = Store::default();
        let snap = |t| Snapshot {
            taken_at: t,
            sessions: vec![],
        };
        assert_eq!(next_id(&store.snapshots, "0823-2100"), "0823-2100");
        store.snapshots.insert("0823-2100".into(), snap(1));
        assert_eq!(next_id(&store.snapshots, "0823-2100"), "0823-2100-2");
        for i in 0..12 {
            store.snapshots.insert(format!("id{i}"), snap(10 + i));
        }
        prune(&mut store, 10);
        assert_eq!(store.snapshots.len(), 10);
        // The oldest (taken_at 1) was pruned first.
        assert!(!store.snapshots.contains_key("0823-2100"));
    }
}
