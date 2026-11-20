//! Isolated data and Claude config dirs for tests that exercise credential renewal, plus a stub
//! for the OAuth exchange so no test can reach the real token endpoint.

use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Mutex, MutexGuard,
    },
    time::Duration,
};

static ENV: Mutex<()> = Mutex::new(());
static EXCHANGES: AtomicUsize = AtomicUsize::new(0);
static EXCHANGE_DELAY_MS: AtomicU64 = AtomicU64::new(0);
const VARS: [&str; 2] = ["XDG_DATA_HOME", "CLAUDE_CONFIG_DIR"];

pub struct TestEnv {
    pub dir: tempfile::TempDir,
    saved: Vec<(&'static str, Option<OsString>)>,
    _guard: MutexGuard<'static, ()>,
}

impl TestEnv {
    pub fn new() -> Self {
        let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let saved = VARS.map(|var| (var, std::env::var_os(var))).to_vec();
        std::env::set_var("XDG_DATA_HOME", dir.path().join("data"));
        std::env::set_var("CLAUDE_CONFIG_DIR", dir.path().join("claude"));
        EXCHANGES.store(0, Ordering::SeqCst);
        EXCHANGE_DELAY_MS.store(0, Ordering::SeqCst);
        TestEnv {
            dir,
            saved,
            _guard: guard,
        }
    }

    pub fn exchanges(&self) -> usize {
        EXCHANGES.load(Ordering::SeqCst)
    }

    pub fn slow_exchange(&self, ms: u64) {
        EXCHANGE_DELAY_MS.store(ms, Ordering::SeqCst);
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("data/cs/daemon.log")).unwrap_or_default()
    }

    pub fn profile(&self, alias: &str) -> PathBuf {
        crate::native::profile_path(alias).unwrap()
    }

    /// The shared Claude login belongs to an unrelated account, so the aliases under test own
    /// their grants outright.
    pub fn unrelated_live_login(&self) {
        let claude = self.dir.path().join("claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join(".credentials.json"),
            json!({"claudeAiOauth":{"accessToken":"live-access","refreshToken":"live-refresh","expiresAt":1}})
                .to_string(),
        )
        .unwrap();
        std::fs::write(
            claude.join(".claude.json"),
            json!({"oauthAccount":{"emailAddress":"live@example.test"}}).to_string(),
        )
        .unwrap();
    }

    /// A native profile directory for `alias`, as `cs run` / `cs t3-claude` would leave it.
    pub fn profile_for(&self, alias: &str, email: &str) {
        let profile = self.profile(alias);
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(
            profile.join(".claude.json"),
            json!({"oauthAccount":{"emailAddress":email}}).to_string(),
        )
        .unwrap();
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        for (var, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(var, value),
                None => std::env::remove_var(var),
            }
        }
    }
}

/// Stand-in for the token endpoint: rotates `access-old`/`refresh-old` to `access-new`/
/// `refresh-new` with an 8 hour lifetime, and refuses a grant that was already spent.
pub fn exchange(oauth: &Value, now_ms: i64) -> Result<Value> {
    EXCHANGES.fetch_add(1, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(
        EXCHANGE_DELAY_MS.load(Ordering::SeqCst),
    ));
    if oauth["refreshToken"] != "refresh-old" {
        bail!("token refresh failed: HTTP 400 invalid_grant");
    }
    let mut out = oauth.clone();
    out["accessToken"] = json!("access-new");
    out["refreshToken"] = json!("refresh-new");
    out["expiresAt"] = json!(now_ms + 8 * 60 * 60 * 1000);
    Ok(out)
}
