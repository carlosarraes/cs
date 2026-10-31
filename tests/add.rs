#![cfg(target_os = "linux")]
use serde_json::{json, Value};
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

struct Fixture {
    temp: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        for dir in ["data/cs", "claude", "bin"] {
            fs::create_dir_all(temp.path().join(dir)).unwrap();
        }
        let f = Self { temp };
        let account = |email: &str| json!({"email":email,"data":{"claudeAiOauth":{"test_marker":email},"oauthAccount":{"emailAddress":email}}});
        f.write("data/cs/state.json", &json!({"providers":{"claude":{"current":"old","previous":"previous","accounts":{"old":account("old@example.test"),"previous":account("previous@example.test")}}}}));
        f.live("old@example.test");
        fs::write(f.temp.path().join("bin/claude"), r#"#!/usr/bin/env python3
import json, os, pathlib, sys
if sys.argv[1:] != ['auth', 'login']: sys.exit(70)
p = pathlib.Path(os.environ['CLAUDE_CONFIG_DIR'])
(p / '.credentials.json').write_text(json.dumps({'claudeAiOauth': {'test_marker': 'new@example.test'}, 'unrelated':True}))
(p / '.claude.json').write_text(json.dumps({'oauthAccount': {'emailAddress':'new@example.test'}, 'unrelated':True}))
"#).unwrap();
        fs::set_permissions(
            f.temp.path().join("bin/claude"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        f
    }
    fn write(&self, path: &str, value: &Value) {
        fs::write(self.temp.path().join(path), value.to_string()).unwrap();
    }
    fn read(&self, path: &str) -> Value {
        serde_json::from_slice(&fs::read(self.temp.path().join(path)).unwrap()).unwrap()
    }
    fn live(&self, email: &str) {
        self.write(
            "claude/.credentials.json",
            &json!({"claudeAiOauth":{"test_marker":email,"refreshed":true},"unrelated":true}),
        );
        self.write(
            "claude/.claude.json",
            &json!({"oauthAccount":{"emailAddress":email},"unrelated":true}),
        );
    }
    fn add(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_cs"))
            .arg("add")
            .args(args)
            .env("XDG_DATA_HOME", self.temp.path().join("data"))
            .env("CLAUDE_CONFIG_DIR", self.temp.path().join("claude"))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.temp.path().join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .output()
            .unwrap()
    }
}

#[test]
fn add_saves_and_restores_outgoing_login_and_selection() {
    for flags in [
        vec!["new"],
        vec!["previous", "--force"],
        vec!["new", "--current"],
        vec!["previous", "--current", "--force"],
    ] {
        let f = Fixture::new();
        let current = flags.contains(&"--current");
        let before = if current {
            json!({"claudeAiOauth": f.read("data/cs/state.json")["providers"]["claude"]["accounts"]["old"]["data"]["claudeAiOauth"]})
        } else {
            f.read("claude/.credentials.json")
        };
        if current {
            f.live("new@example.test");
        }
        let output = f.add(&flags);
        assert!(output.status.success());
        let state = f.read("data/cs/state.json");
        let ps = &state["providers"]["claude"];
        assert_eq!(ps["current"], "old");
        assert_eq!(ps["previous"], "previous");
        assert_eq!(ps["accounts"][flags[0]]["email"], "new@example.test");
        let live = f.read("claude/.credentials.json");
        assert!(live["claudeAiOauth"] == before["claudeAiOauth"]);
        assert!(live["unrelated"] == true);
        assert_eq!(
            f.read("claude/.claude.json")["oauthAccount"]["emailAddress"],
            "old@example.test"
        );
        if !current {
            assert!(ps["accounts"]["old"]["data"]["claudeAiOauth"] == before["claudeAiOauth"]);
        }
        let message = String::from_utf8(output.stdout).unwrap();
        assert!(message.contains("not active") && message.contains("cs switch"));
    }
}

#[test]
fn switch_flag_activates_with_force_and_current() {
    for flags in [
        vec!["new", "--switch"],
        vec!["previous", "--force", "--switch"],
        vec!["new", "--current", "--switch"],
    ] {
        let f = Fixture::new();
        if flags.contains(&"--current") {
            f.live("new@example.test");
        }
        let output = f.add(&flags);
        assert!(output.status.success());
        let state = f.read("data/cs/state.json");
        let ps = &state["providers"]["claude"];
        assert_eq!(ps["current"], flags[0]);
        assert_eq!(ps["previous"], "old");
        assert_eq!(
            f.read("claude/.claude.json")["oauthAccount"]["emailAddress"],
            "new@example.test"
        );
        assert!(String::from_utf8(output.stdout)
            .unwrap()
            .contains("and set it active."));
    }
}

#[test]
fn no_current_or_unrestorable_account_keeps_new_login_active() {
    for reason in ["none", "missing", "malformed", "overwritten"] {
        let f = Fixture::new();
        let mut state = f.read("data/cs/state.json");
        let ps = &mut state["providers"]["claude"];
        match reason {
            "none" => ps["current"] = Value::Null,
            "missing" => ps["current"] = json!("missing"),
            "malformed" => {
                ps["accounts"]["old"]["data"] = json!({});
                f.live("drifted@example.test");
            }
            _ => {}
        }
        f.write("data/cs/state.json", &state);
        let flags = if reason == "overwritten" {
            vec!["old", "--force"]
        } else {
            vec!["new"]
        };
        let output = f.add(&flags);
        assert!(output.status.success());
        assert_eq!(
            f.read("data/cs/state.json")["providers"]["claude"]["current"],
            flags[0]
        );
        assert_eq!(
            f.read("claude/.claude.json")["oauthAccount"]["emailAddress"],
            "new@example.test"
        );
        assert!(String::from_utf8(output.stdout)
            .unwrap()
            .contains("set it active"));
    }
}

#[test]
fn unsuccessful_login_restores_outgoing_account_without_adding_alias() {
    for switch in [false, true] {
        let f = Fixture::new();
        let fake = f.temp.path().join("bin/claude");
        let mut script = fs::read_to_string(&fake).unwrap();
        script.push_str("\nsys.exit(7)\n");
        fs::write(fake, script).unwrap();
        let before = f.read("claude/.credentials.json");
        let output = f.add(if switch {
            &["new", "--switch"]
        } else {
            &["new"]
        });
        assert!(!output.status.success());
        let state = f.read("data/cs/state.json");
        let ps = &state["providers"]["claude"];
        assert_eq!(ps["current"], "old");
        assert_eq!(ps["previous"], "previous");
        assert!(ps["accounts"].get("new").is_none());
        assert!(f.read("claude/.credentials.json")["claudeAiOauth"] == before["claudeAiOauth"]);
        assert_eq!(
            f.read("claude/.claude.json")["oauthAccount"]["emailAddress"],
            "old@example.test"
        );
    }
}
