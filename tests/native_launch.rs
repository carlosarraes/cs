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

struct Interactive {
    child: std::process::Child,
    _terminal: fs::File,
}
impl Drop for Interactive {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(7);
        while self.child.try_wait().unwrap().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        let _ = self.child.wait();
    }
}
fn interactive_fixture() -> Fixture {
    let f = Fixture::new();
    let state_path = f.data.join("cs/state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    let usage_path = f.data.join("cs/usage.json");
    let mut usage: Value = serde_json::from_slice(&fs::read(&usage_path).unwrap()).unwrap();
    for alias in ["b", "c"] {
        let mut account = state["providers"]["claude"]["accounts"]["a"].clone();
        account["email"] = json!(format!("{alias}@x"));
        account["data"]["oauthAccount"]["emailAddress"] = json!(format!("{alias}@x"));
        state["providers"]["claude"]["accounts"][alias] = account;
        usage["accounts"][alias] = usage["accounts"]["a"].clone();
    }
    fs::write(state_path, state.to_string()).unwrap();
    fs::write(usage_path, usage.to_string()).unwrap();
    fs::write(f.bin.join("claude"), r#"#!/usr/bin/env python3
import json,os,sys,pathlib,time,subprocess
profile=pathlib.Path(os.environ['CLAUDE_CONFIG_DIR'])
email=json.loads((profile/'.claude.json').read_text())['oauthAccount']['emailAddress']
args=sys.argv[1:]
if 'auth' in args:
 print(json.dumps({'loggedIn':True,'authMethod':'claude.ai','apiProvider':'firstParty','email':email}));sys.exit(0)
root=args[args.index('--resume')+1] if '--resume' in args else args[args.index('--session-id')+1]
test=pathlib.Path(os.environ['TEST_CONTROL_DIR'])
with (test/'launches.jsonl').open('a') as out:out.write(json.dumps({'root':root,'email':email,'pid':os.getpid(),'args':args})+'\n')
if email=='b@x' and (test/'fail-destination').exists():sys.exit(17)
def event(name,agent=None,notification=None):
 data={'session_id':root,'hook_event_name':name,'cwd':os.getcwd()}
 if agent:data['agent_id']=agent
 if notification:data['notification_type']=notification
 r=subprocess.run([os.environ['TEST_CS_BINARY'],'native-event'],input=json.dumps(data),text=True,capture_output=True)
 if r.returncode:sys.exit(21)
 return r.stdout
assert not event('SessionStart')
busy=os.environ.get('TEST_BUSY')=='yes' and email=='a@x'
if not busy:event('Notification',notification='idle_prompt')
if busy:
 assert not event('UserPromptSubmit')
 event('SubagentStart','background-agent')
while True:
 if busy and (test/'finish-response').exists():
  event('Stop');busy=False
  (test/('stopped-'+root)).touch()
 if (test/'finish-agent').exists():
  event('SubagentStop','background-agent')
  if not busy:event('Notification',notification='idle_prompt')
 time.sleep(.05)
"#).unwrap();
    f
}
fn start_interactive(f: &Fixture, account: &str, busy: bool) -> Interactive {
    use std::os::fd::FromRawFd;
    let mut master = -1;
    let mut slave = -1;
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0
    );
    let terminal = unsafe { fs::File::from_raw_fd(master) };
    let input = unsafe { fs::File::from_raw_fd(slave) };
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(f.temp.path().join("supervisors.log"))
        .unwrap();
    let child = f
        .command()
        .args([
            "run",
            "--account",
            account,
            "--",
            "--chrome",
            "--model",
            "sonnet",
            "--dangerously-skip-permissions",
            "initial prompt",
        ])
        .env("TEST_BUSY", if busy { "yes" } else { "no" })
        .env("TEST_CONTROL_DIR", f.temp.path())
        .env("TEST_CS_BINARY", env!("CARGO_BIN_EXE_cs"))
        .stdin(input)
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .unwrap();
    Interactive {
        child,
        _terminal: terminal,
    }
}
fn controls(f: &Fixture) -> Value {
    fs::read(f.data.join("cs/session-controls.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(json!({}))
}
fn wait_until(f: &Fixture, predicate: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(12);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "timed out: {}",
            fs::read_to_string(f.temp.path().join("supervisors.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}
#[test]
fn targeted_batch_waits_for_response_and_agents_then_resumes_only_selected_sessions() {
    let f = interactive_fixture();
    let before = fs::read(f.source.join(".credentials.json")).unwrap();
    let _one = start_interactive(&f, "a", true);
    let _two = start_interactive(&f, "a", true);
    let _other = start_interactive(&f, "c", false);
    wait_until(&f, || {
        let s = controls(&f);
        s.as_object().unwrap().len() == 3
            && s.as_object()
                .unwrap()
                .values()
                .filter(|v| v["alias"] == "a")
                .all(|v| !v["agents"].as_array().unwrap().is_empty())
    });
    let original = controls(&f);
    let untouched = original
        .as_object()
        .unwrap()
        .values()
        .find(|v| v["alias"] == "c")
        .unwrap()
        .clone();
    let output = f
        .command()
        .args(["switch", "b", "--from", "a"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Queued"));
    fs::write(f.temp.path().join("finish-response"), "").unwrap();
    wait_until(&f, || {
        original
            .as_object()
            .unwrap()
            .values()
            .filter(|v| v["alias"] == "a")
            .all(|v| {
                f.temp
                    .path()
                    .join(format!("stopped-{}", v["root"].as_str().unwrap()))
                    .exists()
            })
    });
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(
        controls(&f)
            .as_object()
            .unwrap()
            .values()
            .filter(|v| v["alias"] == "a")
            .count(),
        2
    );
    fs::write(f.temp.path().join("finish-agent"), "").unwrap();
    wait_until(&f, || {
        controls(&f)
            .as_object()
            .unwrap()
            .values()
            .filter(|v| v["alias"] == "b" && v["switching"] == false)
            .count()
            == 2
    });
    let after = controls(&f);
    let other = after
        .as_object()
        .unwrap()
        .values()
        .find(|v| v["alias"] == "c")
        .unwrap();
    assert_eq!(other["child"], untouched["child"]);
    assert_eq!(
        fs::read(f.source.join(".credentials.json")).unwrap(),
        before
    );
    for (key, old) in original
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, v)| v["alias"] == "a")
    {
        assert_eq!(after[key]["root"], old["root"]);
        assert_ne!(after[key]["child"], old["child"]);
    }
    let launches = fs::read_to_string(f.temp.path().join("launches.jsonl")).unwrap();
    for line in launches.lines() {
        let v: Value = serde_json::from_str(line).unwrap();
        if v["email"] == "b@x" {
            let args = v["args"].as_array().unwrap();
            assert!(args.contains(&json!("--resume")));
            assert!(args.contains(&json!("--chrome")));
            assert!(args.contains(&json!("sonnet")));
            assert!(!args.contains(&json!("initial prompt")));
            assert!(!args.contains(&json!("--session-id")));
        }
    }
}
#[test]
fn failed_replacement_restores_original_account_and_conversation() {
    let f = interactive_fixture();
    fs::write(f.temp.path().join("fail-destination"), "").unwrap();
    let _one = start_interactive(&f, "a", false);
    wait_until(&f, || {
        controls(&f)
            .as_object()
            .unwrap()
            .values()
            .any(|v| v["busy"] == false)
    });
    let original = controls(&f)
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .clone();
    let registry_path = f.data.join("cs/routing.json");
    let mut registry = f.registry();
    let last_prompt = chrono::Utc::now().timestamp() - 30;
    registry["bindings"][original["root"].as_str().unwrap()]["last_request"] = json!(last_prompt);
    fs::write(&registry_path, registry.to_string()).unwrap();
    let output = f
        .command()
        .args([
            "switch",
            "b",
            "--session",
            original["root"].as_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    wait_until(&f, || {
        controls(&f)
            .as_object()
            .unwrap()
            .values()
            .any(|v| v["alias"] == "a" && v["switching"] == false && v["error"].is_string())
    });
    let restored = controls(&f)
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .clone();
    assert_eq!(restored["root"], original["root"]);
    assert_ne!(restored["child"], original["child"]);
    assert_eq!(
        f.registry()["bindings"][original["root"].as_str().unwrap()]["alias"],
        "a"
    );
    assert_eq!(
        f.registry()["bindings"][original["root"].as_str().unwrap()]["last_request"],
        last_prompt
    );
}

#[test]
fn exhausted_destination_leaves_source_process_and_assignment_untouched() {
    let f = interactive_fixture();
    let _one = start_interactive(&f, "a", false);
    wait_until(&f, || {
        controls(&f)
            .as_object()
            .unwrap()
            .values()
            .any(|v| v["busy"] == false)
    });
    let original = controls(&f);
    let path = f.data.join("cs/usage.json");
    let mut usage: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    usage["accounts"]["b"]["reading"]["five_hour"]["utilization"] = json!(100);
    fs::write(path, usage.to_string()).unwrap();
    let output = f
        .command()
        .args(["switch", "b", "--from", "a"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("quota exhausted"));
    assert_eq!(controls(&f), original);
    assert!(f.registry()["bindings"]
        .as_object()
        .unwrap()
        .values()
        .all(|b| b["alias"] == "a"));
}

#[test]
fn explicit_interrupt_moves_busy_session_and_rejects_stale_child_hooks() {
    let f = interactive_fixture();
    let _one = start_interactive(&f, "a", true);
    wait_until(&f, || {
        controls(&f)
            .as_object()
            .unwrap()
            .values()
            .any(|v| !v["agents"].as_array().unwrap().is_empty())
    });
    let original = controls(&f);
    let (token, old) = original.as_object().unwrap().iter().next().unwrap();
    let output = f
        .command()
        .args([
            "switch",
            "b",
            "--session",
            old["root"].as_str().unwrap(),
            "--interrupt",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    wait_until(&f, || controls(&f)[token]["alias"] == "b");
    let mut hook = f
        .command()
        .arg("native-event")
        .env("CS_NATIVE_LEASE", token)
        .env("CS_NATIVE_GENERATION", old["generation"].as_str().unwrap())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    hook.stdin
        .take()
        .unwrap()
        .write_all(
            json!({
                "session_id":old["root"], "hook_event_name":"UserPromptSubmit"
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
    let output = hook.wait_with_output().unwrap();
    assert!(String::from_utf8_lossy(&output.stdout).contains("previous Claude process"));
    assert_eq!(controls(&f)[token]["alias"], "b");
    assert_eq!(controls(&f)[token]["root"], old["root"]);
}
