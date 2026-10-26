//! Proxy logging and service installation.
use crate::{
    config::{self, DEFAULT_TOML},
    state, util,
};
use anyhow::{anyhow, Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::{thread, time::Duration};
const LOG_ROTATE_BYTES: u64 = 1024 * 1024;

pub fn log_path() -> Result<PathBuf> {
    Ok(state::state_path()?.with_file_name("daemon.log"))
}

/// Writes each line to stdout and appends it to `daemon.log` (mode 600),
/// rotating to `daemon.log.1` past 1 MB. Lines are stamped `[HH:MM]` local time.
pub struct Logger {
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

pub fn log(kind: &str, msg: &str) {
    if let Ok(mut logger) = Logger::open() {
        logger.log(kind, msg);
    }
}
pub fn log_block(kind: &str, lines: &[String]) {
    if let Ok(mut logger) = Logger::open() {
        logger.block(kind, lines);
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
         Description=cs — Claude session router\n\
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
