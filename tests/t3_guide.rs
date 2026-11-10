use serde_json::json;
use std::{fs, process::Command};

fn run(home: &std::path::Path) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_cs"))
        .arg("--t3")
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env_remove("T3CODE_HOME")
        .env_remove("CS_BORROW")
        .env_remove("CS_BORROW_JUMP")
        .output()
        .unwrap();
    (
        output.status.code().unwrap_or(255),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn write(path: &std::path::Path, value: &serde_json::Value) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, value.to_string()).unwrap();
}

#[test]
fn t3_guide_prints_live_instances_and_aliases_without_secrets() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    write(
        &home.join(".t3/userdata/settings.json"),
        &json!({
            "providerInstances": {
                "claudeAgent": {
                    "driver": "claudeAgent",
                    "enabled": true,
                    "config": {"binaryPath": "/secret-bin-path-should-not-appear"},
                    "environment": [{"name": "CS_BORROW", "value": "should-not-appear-env-value"}]
                },
                "claude-carlos3": {
                    "driver": "claudeAgent",
                    "enabled": true,
                    "environment": [
                        {"name": "CS_BORROW", "value": "should-not-appear-env-value"},
                        {"name": "CS_PIN", "value": "pinned-one"}
                    ]
                },
                "codex": {
                    "driver": "codex",
                    "enabled": false
                }
            }
        }),
    );
    write(
        &home.join("data/cs/state.json"),
        &json!({
            "providers": {
                "claude": {
                    "current": "local-one",
                    "accounts": {
                        "local-one": {
                            "email": "secret-local@example.test",
                            "data": {"claudeAiOauth": {"accessToken": "SECRET_LOCAL_TOKEN"}}
                        }
                    }
                }
            }
        }),
    );
    write(
        &home.join("data/cs/borrow/mac.json"),
        &json!({
            "entries": {
                "borrowed-one": {
                    "email": "secret-borrowed@example.test",
                    "access_token": "SECRET_ACCESS_TOKEN_SHOULD_NOT_LEAK",
                    "expires_at": 4_102_444_800_i64
                }
            }
        }),
    );

    let (code, stdout, stderr) = run(home);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("cs-t3-claude"), "{stdout}");
    assert!(stdout.contains("claudeAgent"), "{stdout}");
    assert!(stdout.contains("claude-carlos3"), "{stdout}");
    assert!(stdout.contains("pin=pinned-one"), "{stdout}");
    assert!(stdout.contains("codex"), "{stdout}");
    assert!(stdout.contains("disabled"), "{stdout}");
    assert!(stdout.contains("local-one"), "{stdout}");
    assert!(stdout.contains("borrowed-one"), "{stdout}");
    assert!(stdout.contains("Borrowed from mac"), "{stdout}");
    assert!(stdout.contains("t3_thread_launch"), "{stdout}");
    assert!(stdout.contains("modelSelection"), "{stdout}");
    assert!(stdout.contains("instanceId"), "{stdout}");
    assert!(
        stdout.contains("No provider instance registered for id"),
        "{stdout}"
    );
    assert!(stdout.contains("unknown account; use cs list"), "{stdout}");
    for secret in [
        "SECRET_ACCESS_TOKEN_SHOULD_NOT_LEAK",
        "SECRET_LOCAL_TOKEN",
        "secret-local@example.test",
        "secret-borrowed@example.test",
        "should-not-appear-env-value",
        "/secret-bin-path-should-not-appear",
    ] {
        assert!(!stdout.contains(secret), "stdout leaked {secret}: {stdout}");
        assert!(!stderr.contains(secret), "stderr leaked {secret}: {stderr}");
    }
}

#[test]
fn t3_guide_succeeds_when_t3_settings_are_missing() {
    let temp = tempfile::tempdir().unwrap();
    let (code, stdout, stderr) = run(temp.path());
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("No t3 settings file"), "{stdout}");
    assert!(stdout.contains("t3_thread_launch"), "{stdout}");
    assert!(
        stdout.contains("No saved or borrowed aliases found"),
        "{stdout}"
    );
}
