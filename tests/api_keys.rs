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
    fn add(&self, admin: bool) -> std::process::Output {
        let mut command = self.command();
        command.args(["add-key", "work-api"]);
        if admin {
            command.arg("--admin");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        input.write_all(key().as_bytes()).unwrap();
        if admin {
            input
                .write_all(format!("\n{}", admin_key()).as_bytes())
                .unwrap();
        }
        input.write_all(b"\n").unwrap();
        drop(input);
        child.wait_with_output().unwrap()
    }
}
fn key() -> String {
    ["sk-ant-api03-", "fixture-private-", "ABCD"].concat()
}
fn admin_key() -> String {
    ["sk-ant-admin01-", "fixture-private-", "WXYZ"].concat()
}

#[test]
fn add_list_delete_keep_both_secrets_out_of_output_state_and_config() {
    let f = Fixture::new();
    let output = f.add(true);
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
    let duplicate = f.add(false);
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
        assert!(!text.contains(&admin_key()));
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
    assert!(f.add(false).status.success());
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
    assert!(String::from_utf8_lossy(&first.stdout).contains("$3.00 7d  $3.00 30d  (as of "));
    assert!(String::from_utf8_lossy(&first.stdout).contains("est."));
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
    assert!(f.add(true).status.success());
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
        assert!(!text.contains(&key()) && !text.contains(&admin_key()));
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
    let automatic = f
        .command()
        .args(["run", "--", "-p", "hi"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!automatic.status.success());
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
    assert!(f.add(false).status.success());
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
    assert!(f.add(false).status.success());
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
