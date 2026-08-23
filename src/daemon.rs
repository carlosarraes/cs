//! The auto-switcher loop (`cs start`), its log (`cs logs`), and the service
//! installer (`cs daemon install`).
//!
//! Each tick: reload config + state → poll every account's usage → persist the
//! observation → maybe report → ask the policy → maybe switch. A bad tick is
//! logged and the loop carries on; SIGINT/SIGTERM finish the tick and exit.

use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::commands;
use crate::config::{self, Config, DEFAULT_TOML};
use crate::policy::{self, Decision, Mem};
use crate::provider::Provider;
use crate::state::{self, State};
use crate::usage::{self, Observation, Reading};
use crate::{claude, util};

const LOG_ROTATE_BYTES: u64 = 1024 * 1024;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as libc::sighandler_t);
    }
}

pub fn log_path() -> Result<PathBuf> {
    Ok(state::state_path()?.with_file_name("daemon.log"))
}

/// Writes each line to stdout and appends it to `daemon.log` (mode 600),
/// rotating to `daemon.log.1` past 1 MB. Lines are stamped `[HH:MM]` local time.
struct Logger {
    file: Option<File>,
}

impl Logger {
    fn open() -> Result<Logger> {
        Self::open_at(&log_path()?)
    }

    fn open_at(path: &Path) -> Result<Logger> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        if fs::metadata(path)
            .map(|m| m.len() > LOG_ROTATE_BYTES)
            .unwrap_or(false)
        {
            let rotated = path.with_extension("log.1");
            fs::rename(path, rotated).ok();
        }
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        Ok(Logger { file: Some(file) })
    }

    fn stamp() -> String {
        chrono::Local::now().format("%H:%M").to_string()
    }

    fn log(&mut self, kind: &str, msg: &str) {
        self.block(kind, &[msg.to_string()]);
    }

    /// First line gets the stamp + kind; continuation lines are indented under it.
    fn block(&mut self, kind: &str, lines: &[String]) {
        let head = format!("[{}] {kind:<7}", Self::stamp());
        let pad = " ".repeat(head.len());
        let mut out = String::new();
        for (i, line) in lines.iter().enumerate() {
            out.push_str(if i == 0 { &head } else { &pad });
            out.push(' ');
            out.push_str(line);
            out.push('\n');
        }
        print!("{out}");
        io::stdout().flush().ok();
        if let Some(f) = &mut self.file {
            f.write_all(out.as_bytes()).ok();
        }
    }
}

/// Everything the loop carries from one tick to the next.
struct Loop {
    cfg: Config,
    mem: Mem,
    prev: Option<Observation>,
    last_report: i64,
    last_stay: Option<String>,
    last_config_err: Option<String>,
    last_warn: BTreeMap<&'static str, String>,
}

impl Loop {
    /// Log `msg` under `key` only when it differs from the last one logged there.
    fn warn_once(&mut self, log: &mut Logger, key: &'static str, msg: String) {
        if self.last_warn.get(key) != Some(&msg) {
            log.log("warn", &msg);
            self.last_warn.insert(key, msg);
        }
    }

    fn clear_warn(&mut self, key: &'static str) {
        self.last_warn.remove(key);
    }
}

pub fn run() -> Result<()> {
    install_signal_handlers();
    let mut log = Logger::open()?;
    let cfg = match config::load() {
        Ok(c) => c,
        Err(e) => {
            log.log("warn", &format!("{e:#} — using defaults"));
            Config::default()
        }
    };
    let prev = Observation::load().unwrap_or_default();
    let mut lp = Loop {
        mem: Mem {
            last_switch_at: prev.as_ref().and_then(|o| o.last_switch_at),
            ..Default::default()
        },
        cfg,
        prev,
        last_report: 0,
        last_stay: None,
        last_config_err: None,
        last_warn: BTreeMap::new(),
    };
    log.log(
        "start",
        &format!(
            "cs {} · {} · config {}",
            env!("CARGO_PKG_VERSION"),
            chrono::Local::now().format("%Y-%m-%d"),
            config::config_path()?.display()
        ),
    );

    while !STOP.load(Ordering::SeqCst) {
        if let Err(e) = tick(&mut lp, &mut log) {
            log.log("warn", &format!("tick failed: {e:#}"));
        }
        for _ in 0..lp.cfg.auto_switcher.poll_secs {
            if STOP.load(Ordering::SeqCst) {
                break;
            }
            thread::sleep(Duration::from_secs(1));
        }
    }
    log.log("stop", "shutting down");
    Ok(())
}

fn tick(lp: &mut Loop, log: &mut Logger) -> Result<()> {
    match config::load() {
        Ok(c) => {
            if c != lp.cfg {
                log.log("info", "config reloaded");
                lp.cfg = c;
            }
            lp.last_config_err = None;
        }
        Err(e) => {
            let msg = format!("{e:#} — keeping last good config");
            if lp.last_config_err.as_deref() != Some(&msg) {
                log.log("warn", &msg);
                lp.last_config_err = Some(msg);
            }
        }
    }

    let now = usage::now();
    // Read-only here; the lock is taken only around the actual switch so a manual
    // `cs switch` never waits behind our network polls.
    let state = State::load()?;
    let ps = state.provider(Provider::Claude);
    if ps.accounts.is_empty() {
        lp.warn_once(
            log,
            "accounts",
            "no Claude accounts saved — add some with `cs add`".into(),
        );
        return Ok(());
    }
    lp.clear_warn("accounts");

    let obs = usage::observe(
        ps,
        lp.prev.as_ref(),
        now,
        lp.cfg.auto_switcher.idle_poll_secs,
    );
    log_reading_transitions(lp, log, &obs);
    obs.save()?;

    if let (Some(cur), Ok(Some(live))) = (ps.current.as_deref(), claude::live_email()) {
        match ps.accounts.get(cur) {
            Some(acct) if acct.email != live => lp.warn_once(
                log,
                "identity",
                format!(
                    "live login is {live} but current alias '{cur}' is {} — \
                     `cs switch {cur}` or `cs add <alias> --current` to resync",
                    acct.email
                ),
            ),
            _ => lp.clear_warn("identity"),
        }
    }

    if lp.cfg.report.enabled && now - lp.last_report >= lp.cfg.report.interval_secs as i64 {
        let mut lines = usage::format_lines(&obs, now);
        if lp.cfg.report.verbose {
            lines.push(verbose_line(&lp.cfg));
        }
        log.block("report", &lines);
        lp.last_report = now;
    }

    if !lp.cfg.auto_switcher.enabled {
        lp.prev = Some(obs);
        return Ok(());
    }
    match policy::decide(&obs, &mut lp.mem, &lp.cfg.auto_switcher, now) {
        Decision::Stay(Some(reason)) => {
            if lp.last_stay.as_deref() != Some(&reason) {
                log.log("info", &reason);
                lp.last_stay = Some(reason);
            }
            lp.prev = Some(obs);
        }
        Decision::Stay(None) => lp.prev = Some(obs),
        Decision::Switch { to, reason } => {
            let from = obs.current.clone().unwrap_or_default();
            match locked_switch(&from, &to) {
                Ok(_) => {
                    log.log("switch", &format!("{from} → {to}  ({reason})"));
                    lp.mem.last_switch_at = Some(now);
                    lp.last_stay = None;
                    // Stamp the timers now rather than a poll later.
                    let mut after = obs.clone();
                    after.current = Some(to);
                    usage::carry_timers(Some(&obs), &mut after, now);
                    after.save()?;
                    lp.prev = Some(after);
                }
                Err(e) => {
                    log.log(
                        "warn",
                        &format!("switch {from} → {to} failed: {e:#} — will retry"),
                    );
                    lp.prev = Some(obs);
                }
            }
        }
    }
    Ok(())
}

/// Switch under the state lock, re-reading `state.json` first and bailing if the
/// current alias changed underneath us (a manual `cs switch` mid-tick).
fn locked_switch(from: &str, to: &str) -> Result<()> {
    let _lock = state::lock()?;
    let mut state = State::load()?;
    let current = state.provider(Provider::Claude).current.as_deref();
    if current != Some(from) {
        return Err(anyhow!(
            "current account changed to {} meanwhile",
            current.unwrap_or("none")
        ));
    }
    commands::perform_switch(&mut state, Provider::Claude, to).map(drop)
}

/// Log when an account's usage becomes Unknown (or changes reason), and when it
/// becomes Known again — not every tick.
fn log_reading_transitions(lp: &mut Loop, log: &mut Logger, obs: &Observation) {
    for (alias, e) in &obs.accounts {
        let before = lp.prev.as_ref().and_then(|p| p.reading(alias));
        match (&e.reading, before) {
            (Reading::Unknown { reason }, Some(Reading::Unknown { reason: r0 }))
                if reason == r0 => {}
            (Reading::Unknown { reason }, _) => {
                log.log("warn", &format!("{alias}: usage unknown — {reason}"))
            }
            (Reading::Known(_), Some(Reading::Unknown { .. })) => {
                log.log("info", &format!("{alias}: usage known again"))
            }
            _ => {}
        }
    }
}

fn verbose_line(cfg: &Config) -> String {
    let a = &cfg.auto_switcher;
    format!(
        "auto-switcher {} (step {}, ceiling {}, poll {}s/{}s idle, gap {}s) · warmup {} · report every {}s",
        on_off(a.enabled),
        a.step,
        a.ceiling,
        a.poll_secs,
        a.idle_poll_secs,
        a.min_switch_gap_secs,
        on_off(cfg.warmup.enabled),
        cfg.report.interval_secs
    )
}

fn on_off(b: bool) -> &'static str {
    if b {
        "on"
    } else {
        "off"
    }
}

/// `cs logs [-f]`: print the log, optionally following it like `tail -f`.
pub fn logs(follow: bool) -> Result<()> {
    let path = log_path()?;
    if !path.exists() {
        return Err(anyhow!("no log yet at {} — run `cs start`", path.display()));
    }
    let mut file = File::open(&path)?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    io::stdout().write_all(&buf)?;
    io::stdout().flush()?;
    if !follow {
        return Ok(());
    }
    let mut pos = buf.len() as u64;
    loop {
        thread::sleep(Duration::from_millis(500));
        // Rotation replaced the file: reopen and start from the top of the new one.
        let len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if len < pos {
            file = File::open(&path)?;
            pos = 0;
        }
        if len > pos {
            file.seek(SeekFrom::Start(pos))?;
            let mut chunk = Vec::new();
            file.read_to_end(&mut chunk)?;
            io::stdout().write_all(&chunk)?;
            io::stdout().flush()?;
            pos += chunk.len() as u64;
        }
    }
}

/// `cs daemon install`: write a default config if missing and a user service
/// that runs `cs start` at login. Prints the command to enable it.
pub fn install() -> Result<()> {
    let cfg_path = config::config_path()?;
    if !cfg_path.exists() {
        util::atomic_write(&cfg_path, DEFAULT_TOML.as_bytes())?;
        fs::set_permissions(&cfg_path, fs::Permissions::from_mode(0o644))?;
        println!("Wrote default config to {}", cfg_path.display());
    } else {
        println!("Config already at {}", cfg_path.display());
    }
    let exe = std::env::current_exe().context("locating the cs binary")?;
    let exe = exe
        .to_str()
        .ok_or_else(|| anyhow!("binary path is not UTF-8"))?;
    let (path, body, enable) = if cfg!(target_os = "macos") {
        let p = util::home()?.join("Library/LaunchAgents/dev.carraes.cs.plist");
        let enable = format!("launchctl load -w {}", p.display());
        (p, launchd_plist(exe), enable)
    } else {
        let p = config::config_home()?.join("systemd/user/cs.service");
        (
            p,
            systemd_unit(exe),
            "systemctl --user daemon-reload && systemctl --user enable --now cs".to_string(),
        )
    };
    util::atomic_write(&path, body.as_bytes())?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
    println!("Wrote {}", path.display());
    println!("\nEnable it with:\n  {enable}\nThen follow along with `cs logs -f`.");
    Ok(())
}

fn systemd_unit(exe: &str) -> String {
    format!(
        "[Unit]\n\
         Description=cs — Claude account auto-switcher\n\
         After=network-online.target\n\n\
         [Service]\n\
         ExecStart={exe} start\n\
         Restart=on-failure\n\
         RestartSec=10\n\n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

fn launchd_plist(exe: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>dev.carraes.cs</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>start</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
</dict>
</plist>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_files_run_cs_start() {
        let u = systemd_unit("/usr/local/bin/cs");
        assert!(u.contains("ExecStart=/usr/local/bin/cs start"));
        assert!(u.contains("WantedBy=default.target"));
        let p = launchd_plist("/opt/cs");
        assert!(p.contains("<string>/opt/cs</string>"));
        assert!(p.contains("<string>start</string>"));
    }

    #[test]
    fn logger_appends_with_stamp_and_rotates_when_large() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        let mut log = Logger::open_at(&path).unwrap();
        log.log("info", "hello");
        log.block("report", &["a".into(), "b".into()]);
        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(
            lines[0].starts_with('[') && lines[0].contains("] info    hello"),
            "{}",
            lines[0]
        );
        assert!(lines[1].contains("report  a"));
        assert!(
            lines[2].starts_with("        ") && lines[2].ends_with(" b"),
            "{:?}",
            lines[2]
        );

        fs::write(&path, vec![b'x'; LOG_ROTATE_BYTES as usize + 1]).unwrap();
        let _ = Logger::open_at(&path).unwrap();
        assert!(dir.path().join("daemon.log.1").exists());
        assert!(fs::metadata(&path).unwrap().len() < 10);
    }
}
