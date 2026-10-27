#![cfg(target_os = "linux")]
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

struct Fixture {
    temp: tempfile::TempDir,
    data: PathBuf,
    source: PathBuf,
    bin: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let source = temp.path().join("claude");
        let bin = temp.path().join("bin");
        fs::create_dir_all(data.join("cs")).unwrap();
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let expiry = (chrono::Utc::now().timestamp() + 7200) * 1000;
        let oauth =
            json!({"accessToken":"saved-access","refreshToken":"saved-refresh","expiresAt":expiry});
        fs::write(
            source.join(".credentials.json"),
            json!({"claudeAiOauth":oauth}).to_string(),
        )
        .unwrap();
        fs::write(
            source.join(".claude.json"),
            json!({"oauthAccount":{"emailAddress":"a@x"},"hasCompletedOnboarding":true})
                .to_string(),
        )
        .unwrap();
        fs::write(data.join("cs/state.json"),json!({"providers":{"claude":{"current":"a","accounts":{"a":{"email":"a@x","data":{"claudeAiOauth":oauth,"oauthAccount":{"emailAddress":"a@x"}}}}}}}).to_string()).unwrap();
        fs::write(data.join("cs/usage.json"),json!({"observed_at":chrono::Utc::now().timestamp(),"accounts":{"a":{"reading":{"status":"known","five_hour":{"utilization":0,"resets_at":null}},"fetched_at":chrono::Utc::now().timestamp()}}}).to_string()).unwrap();
        let script = r#"#!/bin/sh
if [ -n "$ANTHROPIC_API_KEY$ANTHROPIC_AUTH_TOKEN$CLAUDE_CODE_OAUTH_TOKEN$ANTHROPIC_BASE_URL" ]; then exit 71; fi
case " $* " in
 *" auth status "*) printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"a@x"}'; exit 0;;
esac
printf '%s\n' "$$" > "$TEST_CHILD_PID"
if [ "$TEST_WAIT" = yes ]; then
 trap 'echo interrupt >> "$TEST_SIGNALS"' INT
 trap 'exit 0' TERM
 while :; do sleep 0.1; done
fi
cat
printf 'FAKE_CLAUDE_OK\n'
"#;
        fs::write(bin.join("claude"), script).unwrap();
        fs::set_permissions(bin.join("claude"), fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            temp,
            data,
            source,
            bin,
        }
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cs"));
        cmd.current_dir(self.temp.path())
            .env("XDG_DATA_HOME", &self.data)
            .env("XDG_CONFIG_HOME", self.temp.path().join("config"))
            .env("CLAUDE_CONFIG_DIR", &self.source)
            .env("CS_NATIVE_SOURCE_CONFIG", &self.source)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("ANTHROPIC_API_KEY", "wrong-account-key")
            .env("CLAUDE_CODE_OAUTH_TOKEN", "wrong-account-token")
            .env("TEST_CHILD_PID", self.temp.path().join("child.pid"))
            .env("TEST_SIGNALS", self.temp.path().join("signals"));
        cmd
    }
    fn registry(&self) -> Value {
        serde_json::from_slice(&fs::read(self.data.join("cs/routing.json")).unwrap()).unwrap()
    }
}

#[test]
fn unavailable_unrelated_profile_does_not_block_a_healthy_launch() {
    let f = Fixture::new();
    let state_path = f.data.join("cs/state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    let mut other = state["providers"]["claude"]["accounts"]["a"].clone();
    other["email"] = json!("b@x");
    state["providers"]["claude"]["accounts"]["b"] = other;
    fs::write(state_path, state.to_string()).unwrap();
    let profile = f.data.join("cs/profiles/62");
    fs::create_dir_all(&profile).unwrap();
    fs::write(profile.join(".claude.json"), "unreadable profile").unwrap();
    let output = f
        .command()
        .args(["run", "--account", "a", "--", "-p", "hi"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn usage_attempts_nonshared_renewal_with_unmanaged_process_and_unreadable_login() {
    let f = Fixture::new();
    let path = f.data.join("cs/state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state["providers"]["claude"]["accounts"]["a"]["data"]["claudeAiOauth"]["expiresAt"] = json!(1);
    state["providers"]["claude"]["accounts"]["b"] = json!({
        "email":"b@x", "data":{"oauthAccount":{"emailAddress":"b@x"},
        "claudeAiOauth":{"accessToken":"expired-b","expiresAt":1}}
    });
    fs::write(path, state.to_string()).unwrap();
    fs::write(f.source.join(".credentials.json"), "unreadable credentials").unwrap();
    fs::create_dir(f.source.join("sessions")).unwrap();
    fs::write(
        f.source.join("sessions/unmanaged.json"),
        json!({"pid":std::process::id()}).to_string(),
    )
    .unwrap();
    let output = f.command().args(["usage", "--live"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("b: no refresh token saved"),
        "renewal was skipped: {stderr}"
    );
    assert!(!stderr.contains("expired-b"));
    assert!(!stderr.contains("a: no refresh token"));
}

#[test]
fn automatic_launch_preserves_stdin_uses_native_auth_and_cleans_up() {
    let f = Fixture::new();
    let before = fs::read(f.source.join(".credentials.json")).unwrap();
    let mut child = f
        .command()
        .args(["run", "--", "-p"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"PIPE_INPUT\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "PIPE_INPUT\nFAKE_CLAUDE_OK\n"
    );
    assert_eq!(
        fs::read(f.source.join(".credentials.json")).unwrap(),
        before
    );
    let oauth: Value =
        serde_json::from_slice(&fs::read(f.data.join("cs/profiles/61/.credentials.json")).unwrap())
            .unwrap();
    assert!(oauth["claudeAiOauth"].get("refreshToken").is_none());
    assert!(f.registry()["leases"].as_object().unwrap().is_empty());
}

#[test]
fn unknown_account_never_starts_claude() {
    let f = Fixture::new();
    let output = f
        .command()
        .args(["run", "--account", "bogus", "--", "-p", "hi"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("bogus: unknown account"));
    assert!(!f.temp.path().join("child.pid").exists());
}

#[test]
fn supervisor_survives_interrupt_and_reaps_on_termination() {
    let f = Fixture::new();
    let mut child = f
        .command()
        .args(["run", "--", "-p", "wait"])
        .env("TEST_WAIT", "yes")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !f.temp.path().join("child.pid").exists() {
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(20));
    }
    let pid = fs::read_to_string(f.temp.path().join("child.pid"))
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let start = Instant::now();
    while !f.temp.path().join("signals").exists() {
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(child.try_wait().unwrap().is_none());
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < Duration::from_secs(7));
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert!(f.registry()["leases"].as_object().unwrap().is_empty());
}
