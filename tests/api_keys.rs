#![cfg(target_os = "linux")]
use serde_json::Value;
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
};

struct Fixture(tempfile::TempDir);
impl Fixture {
    fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cs"));
        command
            .current_dir(self.0.path())
            .env("HOME", self.0.path())
            .env("XDG_DATA_HOME", self.0.path().join("data"))
            .env("XDG_CONFIG_HOME", self.0.path().join("config"))
            .env("CLAUDE_CONFIG_DIR", self.0.path().join("claude"));
        command
    }
    fn add(&self) -> std::process::Output {
        let mut child = self
            .command()
            .args(["add-key", "work-api"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        input.write_all(key().as_bytes()).unwrap();
        input.write_all(b"\n").unwrap();
        drop(input);
        child.wait_with_output().unwrap()
    }
}
fn key() -> String {
    ["sk-ant-api03-", "fixture-private-", "ABCD"].concat()
}

#[test]
fn add_list_delete_keep_secrets_out_of_output_state_and_config() {
    let f = Fixture::new();
    let output = f.add();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state = f.0.path().join("data/cs/state.json");
    let value: Value = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
    assert_eq!(
        value["providers"]["claude"]["accounts"]["work-api"]["kind"]["key_hint"],
        "sk-ant-…ABCD"
    );
    let listed = f.command().arg("list").output().unwrap();
    assert!(String::from_utf8_lossy(&listed.stdout).contains("[key]"));
    let duplicate = f.add();
    assert!(!duplicate.status.success());
    for bytes in [
        &output.stdout,
        &output.stderr,
        &listed.stdout,
        &listed.stderr,
        &duplicate.stdout,
        &duplicate.stderr,
        &fs::read(&state).unwrap(),
    ] {
        let text = String::from_utf8_lossy(bytes);
        assert!(!text.contains(&key()));
        assert!(!text.contains("fixture-private"));
    }
    let secrets = f.0.path().join("data/cs/key-credentials.json");
    assert_eq!(
        fs::metadata(&secrets).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let deleted = f.command().args(["del", "work-api"]).output().unwrap();
    assert!(deleted.status.success());
    assert!(
        !secrets.exists()
            || !fs::read_to_string(secrets)
                .unwrap()
                .contains("fixture-private")
    );
    let rejected = f
        .command()
        .args(["add-key", "bad", &key()])
        .output()
        .unwrap();
    assert_eq!(rejected.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&rejected.stderr).contains(&key()));
}

#[test]
fn usage_estimates_only_assigned_sessions_and_obeys_cache_and_live_refresh() {
    let f = Fixture::new();
    assert!(f.add().status.success());
    let source = f.0.path().join("claude/projects/project");
    fs::create_dir_all(&source).unwrap();
    let log = |input: u64| {
        serde_json::json!({"timestamp":chrono::Utc::now().to_rfc3339(),
        "sessionId":"ours","requestId":"request","version":"2.0.1",
        "message":{"id":"message","model":"unknown-model","usage":{"input_tokens":input}}})
        .to_string()
    };
    fs::write(
        source.join("session.jsonl"),
        format!(
            "{}\n{}",
            log(1_000_000),
            log(100_000_000).replace("ours", "outside")
        ),
    )
    .unwrap();
    fs::write(
        f.0.path().join("data/cs/routing.json"),
        r#"{"leases":{},"bindings":{},"key_sessions":{"ours":"work-api"}}"#,
    )
    .unwrap();
    let first = f.command().arg("usage").output().unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let out = String::from_utf8_lossy(&first.stdout);
    assert!(out.contains("work-api  [key …ABCD]  $3.00 7d  $3.00 30d  (as of "));
    assert!(out.contains("est."));
    assert!(out.contains("local  $"), "missing local total: {out}");
    assert!(!out.contains("admin"), "{out}");
    assert_eq!(
        String::from_utf8_lossy(&first.stderr)
            .matches("unknown Claude model")
            .count(),
        1
    );
    fs::write(source.join("session.jsonl"), log(2_000_000)).unwrap();
    let cached = f.command().arg("usage").output().unwrap();
    assert!(String::from_utf8_lossy(&cached.stdout).contains("$3.00 7d"));
    let live = f.command().args(["usage", "--live"]).output().unwrap();
    assert!(
        live.status.success(),
        "{}",
        String::from_utf8_lossy(&live.stderr)
    );
    assert!(String::from_utf8_lossy(&live.stdout).contains("$6.00 7d"));
}

#[test]
fn explicit_key_launch_injects_only_child_and_persists_attribution() {
    let f = Fixture::new();
    assert!(f.add().status.success());
    let bin = f.0.path().join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("claude"),r#"#!/bin/sh
test "$ANTHROPIC_API_KEY" = "$TEST_EXPECTED_KEY" || exit 71
test -z "$ANTHROPIC_AUTH_TOKEN$CLAUDE_CODE_OAUTH_TOKEN$ANTHROPIC_BASE_URL" || exit 72
case " $* " in
 *" auth status "*) printf '%s\n' '{"loggedIn":true,"authMethod":"api_key","apiProvider":"firstParty"}'; exit 0;;
esac
printf 'KEY_CHILD_OK\n'
"#).unwrap();
    fs::set_permissions(bin.join("claude"), fs::Permissions::from_mode(0o755)).unwrap();
    let output = f
        .command()
        .args(["run", "--account", "work-api", "--", "-p", "hello"])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("TEST_EXPECTED_KEY", key())
        .env("ANTHROPIC_API_KEY", "wrong-key")
        .env("ANTHROPIC_AUTH_TOKEN", "wrong-token")
        .env("CLAUDE_CODE_OAUTH_TOKEN", "wrong-oauth")
        .env("ANTHROPIC_BASE_URL", "https://invalid.example")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("KEY_CHILD_OK"));
    let registry: Value =
        serde_json::from_slice(&fs::read(f.0.path().join("data/cs/routing.json")).unwrap())
            .unwrap();
    assert_eq!(registry["key_sessions"].as_object().unwrap().len(), 1);
    assert_eq!(
        registry["key_sessions"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap(),
        "work-api"
    );
    assert!(registry["leases"].as_object().unwrap().is_empty());
    for bytes in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(bytes);
        assert!(!text.contains(&key()));
    }
    fn scan(path: &std::path::Path) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                scan(&entry.path());
            } else if entry.file_type().unwrap().is_file()
                && entry.file_name() != "key-credentials.json"
            {
                let text = fs::read_to_string(entry.path()).unwrap();
                assert!(
                    !text.contains("fixture-private"),
                    "credential leaked to {}",
                    entry.path().display()
                );
            }
        }
    }
    scan(&f.0.path().join("data"));
    assert!(f
        .command()
        .args(["switch", "work-api", "--yes"])
        .output()
        .unwrap()
        .status
        .success());
    let automatic = f
        .command()
        .args(["run", "--", "-p", "hi"])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("TEST_EXPECTED_KEY", key())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        automatic.status.success(),
        "{}",
        String::from_utf8_lossy(&automatic.stderr)
    );
    let switch = f
        .command()
        .args(["switch", "next", "--yes"])
        .output()
        .unwrap();
    assert!(!switch.status.success());
}

#[test]
fn key_launch_rejects_unattributed_existing_conversations() {
    let f = Fixture::new();
    assert!(f.add().status.success());
    let root = "1b48e828-81df-469f-801f-4c268d29d8b2";
    let source = f.0.path().join("claude/projects/other-project");
    let bin = f.0.path().join("empty-bin");
    fs::create_dir(&bin).unwrap();
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join(format!("{root}.jsonl")), "{}").unwrap();
    let output = f
        .command()
        .args(["run", "--account", "work-api", "--", "--resume", root])
        .env("PATH", &bin)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no attribution to this key"));
}

#[test]
fn interactive_resume_cannot_attribute_old_transcript_to_key() {
    let f = Fixture::new();
    assert!(f.add().status.success());
    let root = "1b48e828-81df-469f-801f-4c268d29d8b2";
    let source = f.0.path().join("claude/projects/old-project");
    fs::create_dir_all(&source).unwrap();
    fs::write(
        source.join(format!("{root}.jsonl")),
        serde_json::json!({"sessionId":root,"message":{"usage":{"input_tokens":1000}}}).to_string(),
    )
    .unwrap();
    let bin = f.0.path().join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("claude"),r#"#!/bin/sh
case " $* " in
 *" auth status "*) printf '%s\n' '{"loggedIn":true,"authMethod":"api_key","apiProvider":"firstParty"}'; exit 0;;
esac
printf '%s' '{"session_id":"1b48e828-81df-469f-801f-4c268d29d8b2","hook_event_name":"SessionStart"}' | "$TEST_CS" native-event
"#).unwrap();
    fs::set_permissions(bin.join("claude"), fs::Permissions::from_mode(0o755)).unwrap();
    let output = f
        .command()
        .args(["run", "--account", "work-api", "--", "-p", "hi"])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("TEST_CS", env!("CARGO_BIN_EXE_cs"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("\"continue\":false"));
    let registry: Value =
        serde_json::from_slice(&fs::read(f.0.path().join("data/cs/routing.json")).unwrap())
            .unwrap();
    assert!(registry["key_sessions"].get(root).is_none());
}

#[test]
fn plain_key_switch_clears_only_shared_oauth_and_reports_key_mode() {
    let f = Fixture::new();
    assert!(f.add().status.success());
    let source = f.0.path().join("claude");
    fs::create_dir_all(&source).unwrap();
    fs::write(
        source.join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"fixture-oauth"},"mcpOAuth":{"keep":true}}"#,
    )
    .unwrap();
    fs::write(
        source.join(".claude.json"),
        r#"{"oauthAccount":{"emailAddress":"old@x"},"keep":true}"#,
    )
    .unwrap();
    let output = f
        .command()
        .args(["switch", "work-api", "--yes"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state: Value =
        serde_json::from_slice(&fs::read(f.0.path().join("data/cs/state.json")).unwrap()).unwrap();
    assert_eq!(state["providers"]["claude"]["current"], "work-api");
    let credentials: Value =
        serde_json::from_slice(&fs::read(source.join(".credentials.json")).unwrap()).unwrap();
    assert!(credentials["claudeAiOauth"].is_null());
    assert_eq!(credentials["mcpOAuth"]["keep"], true);
    let config: Value =
        serde_json::from_slice(&fs::read(source.join(".claude.json")).unwrap()).unwrap();
    assert!(config["oauthAccount"].is_null());
    assert_eq!(config["keep"], true);
    let who = f.command().arg("whoami").output().unwrap();
    let listed = f.command().arg("list").output().unwrap();
    assert!(String::from_utf8_lossy(&who.stdout).contains("work-api [key]"));
    assert!(!String::from_utf8_lossy(&who.stdout).contains("old@x"));
    assert!(String::from_utf8_lossy(&listed.stdout).starts_with("* work-api"));
    for bytes in [
        &output.stdout,
        &output.stderr,
        &who.stdout,
        &who.stderr,
        &listed.stdout,
    ] {
        assert!(!String::from_utf8_lossy(bytes).contains(&key()));
    }
}

#[test]
fn next_ignores_keys_even_with_cached_quota_in_a_mixed_account_set() {
    let f = Fixture::new();
    assert!(f.add().status.success());
    let path = f.0.path().join("data/cs/state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state["providers"]["claude"]["current"] = serde_json::json!("work-api");
    state["providers"]["claude"]["accounts"]["subscription"] = serde_json::json!({"email":"sub@x", "data":{"claudeAiOauth":{"accessToken":"expired-fixture","expiresAt":1},"oauthAccount":{"emailAddress":"sub@x"}}});
    fs::write(&path, state.to_string()).unwrap();
    fs::write(f.0.path().join("data/cs/usage.json"), serde_json::json!({"observed_at":chrono::Utc::now().timestamp(),"accounts":{"work-api":{"reading":{"status":"known","five_hour":{"utilization":0}},"fetched_at":chrono::Utc::now().timestamp()}}}).to_string()).unwrap();
    let output = f
        .command()
        .args(["switch", "next", "--yes"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(state["providers"]["claude"]["current"], "subscription");
    assert_eq!(state["providers"]["claude"]["previous"], "work-api");
    let shared: Value =
        serde_json::from_slice(&fs::read(f.0.path().join("claude/.credentials.json")).unwrap())
            .unwrap();
    assert_eq!(shared["claudeAiOauth"]["accessToken"], "expired-fixture");
}
