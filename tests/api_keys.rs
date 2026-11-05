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
    assert!(
        f.command()
            .args(["add-key", "bad", &key()])
            .output()
            .unwrap()
            .status
            .code()
            == Some(2)
    );
}
