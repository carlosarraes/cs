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
case " $* " in
 *" mcp "*" --settings "*) echo "error: unknown option '--settings'" >&2; exit 2;;
esac
printf '%s\n' "$CLAUDE_CONFIG_DIR" > "$TEST_ARGS.profile"
printf '%s\n' "$@" > "$TEST_ARGS"
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
            .env("TEST_ARGS", self.temp.path().join("args"))
            .env("TEST_CHILD_PID", self.temp.path().join("child.pid"))
            .env("TEST_SIGNALS", self.temp.path().join("signals"));
        cmd
    }
    fn registry(&self) -> Value {
        serde_json::from_slice(&fs::read(self.data.join("cs/routing.json")).unwrap()).unwrap()
    }
    fn registry_opt(&self) -> Option<Value> {
        fs::read(self.data.join("cs/routing.json"))
            .ok()
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
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
import json,os,sys,pathlib,time,subprocess,signal
profile=pathlib.Path(os.environ['CLAUDE_CONFIG_DIR'])
key=os.environ.get('ANTHROPIC_API_KEY','')
assert not os.environ.get('ANTHROPIC_AUTH_TOKEN') and not os.environ.get('CLAUDE_CODE_OAUTH_TOKEN') and not os.environ.get('ANTHROPIC_BASE_URL')
if key:
 assert key=='sk-ant-api03-fixture-private-ABCD'
 email='key'
else:
 email=json.loads((profile/'.claude.json').read_text())['oauthAccount']['emailAddress']
args=sys.argv[1:]
if 'auth' in args:
 print(json.dumps({'loggedIn':True,'authMethod':'api_key' if key else 'claude.ai','apiProvider':'firstParty','email':email}));sys.exit(0)
root=args[args.index('--resume')+1] if '--resume' in args else args[args.index('--session-id')+1]
test=pathlib.Path(os.environ['TEST_CONTROL_DIR'])
with (test/'launches.jsonl').open('a') as out:out.write(json.dumps({'root':root,'email':email,'pid':os.getpid(),'args':args,'cwd':os.getcwd()})+'\n')
if email=='b@x' and (test/'fail-destination').exists():sys.exit(17)
def event(name,agent=None,notification=None):
 data={'session_id':root,'hook_event_name':name,'cwd':os.getcwd()}
 if agent:data['agent_id']=agent
 if notification:data['notification_type']=notification
 r=subprocess.run([os.environ['TEST_CS_BINARY'],'native-event'],input=json.dumps(data),text=True,capture_output=True)
 if r.returncode:sys.exit(21)
 return r.stdout
if email=='a@x' and (test/'change-directory').exists():os.chdir(test/'frontend')
assert not event('SessionStart')
busy=os.environ.get('TEST_BUSY')=='yes' and email=='a@x'
structural=(test/'structural-status').exists()
def record(status):
 start=pathlib.Path('/proc/self/stat').read_text().rsplit(')',1)[1].split()[19]
 value={'pid':os.getpid(),'procStart':start,'sessionId':root,'version':'2.1.319','status':status,'statusUpdatedAt':int(time.time()*1000)}
 if (test/'legacy-status').exists():
  value.pop('status');value.pop('statusUpdatedAt');value['version']='2.0.0'
 path=profile/'sessions'/f'{os.getpid()}.json'
 pending=path.with_suffix('.tmp')
 pending.write_text(json.dumps(value));pending.replace(path)
def terminate(signum,frame):
 with (test/'terminated.jsonl').open('a') as out:out.write(json.dumps({'pid':os.getpid(),'email':email})+'\n')
 sys.exit(0)
if structural:signal.signal(signal.SIGTERM,terminate)
if not busy and not structural:event('Notification',notification='idle_prompt')
if busy:
 assert not event('UserPromptSubmit')
 if not structural:event('SubagentStart','background-agent')
if structural:record('busy' if busy else 'idle')
while True:
 if busy and structural and (test/'probe-busy').exists():
  (test/'busy-observed').touch()
 if busy and (test/'finish-response').exists():
  event('Stop');busy=False
  if structural:record('idle')
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
    let mut command = f.command();
    command.args([
        "run",
        "--account",
        account,
        "--",
        "--chrome",
        "--model",
        "sonnet",
        "--dangerously-skip-permissions",
    ]);
    if f.temp.path().join("initial-resume").exists() {
        command.args(["--resume", "11111111-1111-4111-8111-111111111111"]);
    } else if !f.temp.path().join("structural-status").exists() {
        command.arg("initial prompt");
    }
    let child = command
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
fn assert_no_daemon(f: &Fixture) {
    assert!(!f.data.join("cs/proxy.json").exists());
    assert!(!f.data.join("cs/daemon.log").exists());
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Ok(exe) = fs::read_link(entry.path().join("exe")) else {
            continue;
        };
        if exe != std::path::Path::new(env!("CARGO_BIN_EXE_cs")) {
            continue;
        }
        let cmd = fs::read(entry.path().join("cmdline")).unwrap_or_default();
        assert!(!cmd.split(|b| *b == 0).skip(1).any(|arg| arg == b"start"));
    }
}

fn structural_switch(plain: bool, busy: bool, resume: bool) {
    let f = interactive_fixture();
    fs::write(f.temp.path().join("structural-status"), "").unwrap();
    if resume {
        fs::write(f.temp.path().join("initial-resume"), "").unwrap();
    }
    let _running = start_interactive(&f, "a", busy);
    wait_until(&f, || {
        controls(&f).as_object().unwrap().values().any(|s| {
            s["child"]["pid"]
                .as_u64()
                .is_some_and(|pid| f.source.join(format!("sessions/{pid}.json")).exists())
        })
    });
    let before = controls(&f)
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .clone();
    assert_eq!(before["busy"], true);
    assert_no_daemon(&f);
    let args = if plain {
        vec!["switch", "b", "--yes"]
    } else {
        vec!["switch", "b", "--only", "a"]
    };
    let output = f.command().args(args).output().unwrap();
    assert!(output.status.success(), "switch failed");
    if busy {
        assert!(before["last_prompt_at_ms"].as_i64().unwrap() > 0);
        let output = f.command().arg("sessions").output().unwrap();
        assert!(String::from_utf8_lossy(&output.stdout).contains("waiting: Claude busy"));
        fs::write(f.temp.path().join("probe-busy"), "").unwrap();
        wait_until(&f, || f.temp.path().join("busy-observed").exists());
        assert_eq!(
            controls(&f).as_object().unwrap().values().next().unwrap()["child"],
            before["child"]
        );
        assert!(!f.temp.path().join("terminated.jsonl").exists());
        fs::write(f.temp.path().join("finish-response"), "").unwrap();
    }
    wait_until(&f, || {
        controls(&f)
            .as_object()
            .unwrap()
            .values()
            .any(|s| s["alias"] == "b" && s["switching"] == false)
    });
    if !busy {
        assert!(String::from_utf8_lossy(&output.stdout).contains("switching now (Claude is idle)"));
    }
    let state: Value =
        serde_json::from_slice(&fs::read(f.data.join("cs/state.json")).unwrap()).unwrap();
    assert_eq!(
        state["providers"]["claude"]["current"],
        if plain { "b" } else { "a" }
    );
    let terminated: Value = serde_json::from_str(
        fs::read_to_string(f.temp.path().join("terminated.jsonl"))
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(terminated["pid"], before["child"]["pid"]);
    let launches: Vec<Value> = fs::read_to_string(f.temp.path().join("launches.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(launches.len(), 2);
    assert_eq!(launches[1]["email"], "b@x");
    assert_eq!(launches[1]["root"], before["root"]);
    assert!(launches[1]["args"]
        .as_array()
        .unwrap()
        .iter()
        .any(|arg| arg == "--resume"));
    assert_no_daemon(&f);
}

#[test]
fn targeted_switch_uses_live_idle_record_without_notification_or_daemon() {
    structural_switch(false, false, false);
}

#[test]
fn plain_switch_uses_live_idle_record_without_notification_or_daemon() {
    structural_switch(true, false, false);
}

#[test]
fn resumed_idle_session_switches_without_prompt_or_notification() {
    structural_switch(false, false, true);
}

#[test]
fn queued_session_explains_missing_idle_signal_from_older_claude() {
    let f = interactive_fixture();
    fs::write(f.temp.path().join("structural-status"), "").unwrap();
    fs::write(f.temp.path().join("legacy-status"), "").unwrap();
    let _running = start_interactive(&f, "a", false);
    wait_until(&f, || {
        controls(&f).as_object().unwrap().values().any(|s| {
            s["child"]["pid"]
                .as_u64()
                .is_some_and(|pid| f.source.join(format!("sessions/{pid}.json")).exists())
        })
    });
    let output = f
        .command()
        .args(["switch", "b", "--only", "a"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("waiting for Claude to become idle"));
    let output = f.command().arg("sessions").output().unwrap();
    assert!(String::from_utf8_lossy(&output.stdout).contains("waiting: no idle signal from Claude"));
    assert!(!f.temp.path().join("terminated.jsonl").exists());
    assert_no_daemon(&f);
}

#[test]
fn live_busy_record_waits_for_idle_without_notification_or_daemon() {
    structural_switch(false, true, false);
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
    let status = f.command().arg("sessions").output().unwrap();
    assert!(String::from_utf8_lossy(&status.stdout).contains("waiting: background agents"));
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

#[test]
fn shell_directory_changes_do_not_block_targeted_resume_in_launch_project() {
    let f = interactive_fixture();
    fs::create_dir(f.temp.path().join("frontend")).unwrap();
    fs::write(f.temp.path().join("change-directory"), "").unwrap();
    let _one = start_interactive(&f, "a", false);
    wait_until(&f, || {
        controls(&f)
            .as_object()
            .unwrap()
            .values()
            .any(|v| v["busy"] == false)
    });
    let original = controls(&f);
    let (token, session) = original.as_object().unwrap().iter().next().unwrap();
    assert!(
        session["unsupported"].is_null(),
        "{}",
        session["unsupported"]
    );
    let path = f.data.join("cs/session-controls.json");
    let mut legacy = controls(&f);
    legacy[token]["unsupported"] =
        json!("conversation changed working directory; restart it manually on the target account");
    fs::write(path, legacy.to_string()).unwrap();
    let output = f
        .command()
        .args([
            "switch",
            "b",
            "--session",
            session["root"].as_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    wait_until(&f, || controls(&f)[token]["alias"] == "b");
    let launches = fs::read_to_string(f.temp.path().join("launches.jsonl")).unwrap();
    let resumed: Value = launches
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|v| v["email"] == "b@x")
        .unwrap();
    assert_eq!(resumed["root"], session["root"]);
    assert_eq!(
        PathBuf::from(resumed["cwd"].as_str().unwrap()),
        fs::canonicalize(f.temp.path()).unwrap()
    );
}

#[test]
fn utility_commands_use_pinned_identity_without_quota_or_conversation() {
    for command in [vec!["mcp", "list"], vec!["--version"], vec!["--help"]] {
        let f = Fixture::new();
        fs::write(f.data.join("cs/usage.json"),json!({"observed_at":chrono::Utc::now().timestamp(),"accounts":{"a":{"reading":{"status":"known","five_hour":{"utilization":0,"resets_at":null},"seven_day":{"utilization":100,"resets_at":null}},"fetched_at":chrono::Utc::now().timestamp()}}}).to_string()).unwrap();
        let before = fs::read(f.source.join(".credentials.json")).unwrap();
        let output = f
            .command()
            .args(["run", "--account", "a", "--"])
            .args(&command)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let args = fs::read_to_string(f.temp.path().join("args")).unwrap();
        assert!(
            !args.contains("--session-id") && !args.contains("--resume"),
            "{args}"
        );
        assert!(args.lines().any(|arg| arg == command[0]));
        assert!(!f.data.join("cs/routing.json").exists());
        assert_eq!(
            fs::read(f.source.join(".credentials.json")).unwrap(),
            before
        );
    }
}

#[test]
fn prompts_do_not_bypass_quota_by_containing_utility_names() {
    for args in [vec!["-p", "mcp"], vec!["config"], vec!["-p", "--version"]] {
        let f = Fixture::new();
        fs::write(f.data.join("cs/usage.json"),json!({"observed_at":chrono::Utc::now().timestamp(),"accounts":{"a":{"reading":{"status":"known","five_hour":{"utilization":0,"resets_at":null},"seven_day":{"utilization":100,"resets_at":null}},"fetched_at":chrono::Utc::now().timestamp()}}}).to_string()).unwrap();
        let output = f
            .command()
            .args(["run", "--account", "a", "--"])
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("weekly quota exhausted"));
        assert!(!f.temp.path().join("child.pid").exists());
    }
}

#[test]
fn utility_commands_ignore_missing_usage_but_reject_missing_accounts() {
    let f = Fixture::new();
    fs::remove_file(f.data.join("cs/usage.json")).unwrap();
    let output = f
        .command()
        .args(["run", "--", "mcp", "list"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_file(f.temp.path().join("child.pid")).unwrap();
    let output = f
        .command()
        .args(["run", "--account", "missing", "--", "mcp", "list"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown account"));
    assert!(!f.temp.path().join("child.pid").exists());
}

#[test]
fn utility_commands_refuse_expired_credentials_without_fallback() {
    let f = Fixture::new();
    let path = f.data.join("cs/state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state["providers"]["claude"]["accounts"]["a"]["data"]["claudeAiOauth"] =
        json!({"accessToken":"expired-secret","expiresAt":1});
    fs::write(path, state.to_string()).unwrap();
    fs::write(
        f.source.join(".credentials.json"),
        json!({"claudeAiOauth":{"accessToken":"expired-secret","expiresAt":1}}).to_string(),
    )
    .unwrap();
    let output = f
        .command()
        .args(["run", "--account", "a", "--", "mcp", "list"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("expired-secret"));
    assert!(!f.temp.path().join("child.pid").exists());
}

#[test]
fn plain_switch_moves_all_managed_aliases_and_updates_shared_selection() {
    let f = interactive_fixture();
    let _one = start_interactive(&f, "a", false);
    let _two = start_interactive(&f, "c", false);
    let _already = start_interactive(&f, "b", false);
    wait_until(&f, || {
        controls(&f).as_object().unwrap().len() == 3
            && controls(&f)
                .as_object()
                .unwrap()
                .values()
                .all(|s| s["busy"] == false)
    });
    let before = controls(&f);
    let already = before
        .as_object()
        .unwrap()
        .values()
        .find(|s| s["alias"] == "b")
        .unwrap()["child"]
        .clone();
    let output = f.command().args(["switch", "b", "--yes"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .matches("Queued")
            .count(),
        2
    );
    wait_until(&f, || {
        controls(&f)
            .as_object()
            .unwrap()
            .values()
            .all(|s| s["alias"] == "b" && s["switching"] == false)
    });
    let state: Value =
        serde_json::from_slice(&fs::read(f.data.join("cs/state.json")).unwrap()).unwrap();
    assert_eq!(state["providers"]["claude"]["current"], "b");
    assert_eq!(state["providers"]["claude"]["previous"], "a");
    let shared: Value =
        serde_json::from_slice(&fs::read(f.source.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(shared["oauthAccount"]["emailAddress"], "b@x");
    for (token, old) in before.as_object().unwrap() {
        let after = controls(&f);
        assert_eq!(after[token]["root"], old["root"]);
        if old["alias"] == "b" {
            assert_eq!(after[token]["child"], already);
        }
    }
}

#[test]
fn only_moves_source_alias_and_leaves_shared_login_unchanged() {
    let f = interactive_fixture();
    let _one = start_interactive(&f, "a", true);
    let _two = start_interactive(&f, "c", false);
    wait_until(&f, || {
        controls(&f).as_object().unwrap().len() == 2
            && controls(&f)
                .as_object()
                .unwrap()
                .values()
                .all(|s| s["child"].is_object())
    });
    let output = f
        .command()
        .args(["switch", "b", "--only", "a"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after = controls(&f);
    for s in after.as_object().unwrap().values() {
        if s["alias"] == "a" {
            assert_eq!(s["request"]["alias"], "b");
        } else {
            assert!(s["request"].is_null());
        }
    }
    let state: Value =
        serde_json::from_slice(&fs::read(f.data.join("cs/state.json")).unwrap()).unwrap();
    assert_eq!(state["providers"]["claude"]["current"], "a");
}

#[test]
fn failed_shared_restore_does_not_flip_selection_or_queue_moves() {
    let f = interactive_fixture();
    let _one = start_interactive(&f, "a", true);
    wait_until(&f, || {
        controls(&f)
            .as_object()
            .unwrap()
            .values()
            .any(|s| s["child"].is_object())
    });
    fs::remove_file(f.source.join(".credentials.json")).unwrap();
    fs::create_dir(f.source.join(".credentials.json")).unwrap();
    let output = f.command().args(["switch", "b", "--yes"]).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("switching to 'b'"));
    let state: Value =
        serde_json::from_slice(&fs::read(f.data.join("cs/state.json")).unwrap()).unwrap();
    assert_eq!(state["providers"]["claude"]["current"], "a");
    assert!(controls(&f)
        .as_object()
        .unwrap()
        .values()
        .all(|s| s["request"].is_null()));
}

#[test]
fn plain_switch_to_key_resumes_managed_conversations_and_attributes_only_new_spend() {
    let f = interactive_fixture();
    let mut add = f
        .command()
        .args(["add-key", "limit"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    add.stdin
        .take()
        .unwrap()
        .write_all(b"sk-ant-api03-fixture-private-ABCD\n")
        .unwrap();
    assert!(add.wait().unwrap().success());
    let _one = start_interactive(&f, "a", false);
    let _two = start_interactive(&f, "c", false);
    wait_until(&f, || {
        controls(&f).as_object().unwrap().len() == 2
            && controls(&f)
                .as_object()
                .unwrap()
                .values()
                .all(|s| s["busy"] == false)
    });
    let before = controls(&f);
    let root = before.as_object().unwrap().values().next().unwrap()["root"]
        .as_str()
        .unwrap();
    let project = f.source.join("projects/test");
    fs::create_dir_all(&project).unwrap();
    let row = |timestamp: String, id: &str| {
        json!({"timestamp": timestamp, "sessionId":root,"requestId":id,"message":{"id":id,"model":"claude-sonnet-4-5","usage":{"input_tokens":1000000}}}).to_string()
    };
    let old = row(
        (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339(),
        "old",
    );
    let log = project.join(format!("{root}.jsonl"));
    fs::write(&log, &old).unwrap();
    let output = f
        .command()
        .args(["switch", "limit", "--yes"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .matches("Queued")
            .count(),
        2
    );
    wait_until(&f, || {
        controls(&f)
            .as_object()
            .unwrap()
            .values()
            .all(|s| s["alias"] == "limit" && s["switching"] == false)
    });
    let after = controls(&f);
    let registry = f.registry();
    for (token, s) in after.as_object().unwrap() {
        assert_eq!(s["root"], before[token]["root"]);
        assert!(s["unsupported"].as_str().unwrap().contains("API key"));
        let root = s["root"].as_str().unwrap();
        assert_eq!(registry["bindings"][root]["alias"], "limit");
        assert_eq!(registry["key_sessions"][root], "limit");
    }
    fs::write(
        log,
        format!("{}\n{}", old, row(chrono::Utc::now().to_rfc3339(), "new")),
    )
    .unwrap();
    let usage = f.command().arg("usage").output().unwrap();
    assert!(usage.status.success());
    assert!(
        String::from_utf8_lossy(&usage.stdout).contains("$3.00 7d  $3.00 30d"),
        "{}",
        String::from_utf8_lossy(&usage.stdout)
    );
    let retry = f
        .command()
        .args(["switch", "b", "--only", "limit"])
        .output()
        .unwrap();
    assert!(!retry.status.success());
    assert!(String::from_utf8_lossy(&retry.stderr).contains("API key conversations"));
    let _subscription = start_interactive(&f, "a", false);
    wait_until(&f, || {
        controls(&f)
            .as_object()
            .unwrap()
            .values()
            .any(|s| s["alias"] == "a" && s["busy"] == false)
    });
    let back = f.command().args(["switch", "b", "--yes"]).output().unwrap();
    assert!(
        back.status.success(),
        "{}",
        String::from_utf8_lossy(&back.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&back.stdout)
            .matches("Queued")
            .count(),
        1
    );
    assert!(String::from_utf8_lossy(&back.stderr).contains("API key conversations"));
    wait_until(&f, || {
        controls(&f)
            .as_object()
            .unwrap()
            .values()
            .any(|s| s["alias"] == "b" && s["switching"] == false)
    });
    let shared: Value =
        serde_json::from_slice(&fs::read(f.source.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(shared["oauthAccount"]["emailAddress"], "b@x");
    for bytes in [
        &output.stdout,
        &output.stderr,
        &usage.stdout,
        &usage.stderr,
        &retry.stderr,
        &fs::read(f.temp.path().join("supervisors.log")).unwrap(),
    ] {
        assert!(!String::from_utf8_lossy(bytes).contains("fixture-private"));
    }
}

#[test]
fn plain_switch_reapplies_current_shared_login_and_moves_other_aliases() {
    let f = interactive_fixture();
    let _one = start_interactive(&f, "c", false);
    wait_until(&f, || {
        controls(&f)
            .as_object()
            .unwrap()
            .values()
            .any(|s| s["busy"] == false)
    });
    fs::write(
        f.source.join(".claude.json"),
        json!({"oauthAccount":{"emailAddress":"drifted@x"}}).to_string(),
    )
    .unwrap();
    let output = f.command().args(["switch", "a", "--yes"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let shared: Value =
        serde_json::from_slice(&fs::read(f.source.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(shared["oauthAccount"]["emailAddress"], "a@x");
    wait_until(&f, || {
        controls(&f)
            .as_object()
            .unwrap()
            .values()
            .all(|s| s["alias"] == "a" && s["switching"] == false)
    });
    let state: Value =
        serde_json::from_slice(&fs::read(f.data.join("cs/state.json")).unwrap()).unwrap();
    assert_eq!(state["providers"]["claude"]["current"], "a");
    assert!(state["providers"]["claude"]["previous"].is_null());
}

#[test]
fn t3_passthrough_and_pick() {
    let f = Fixture::new();
    let flags = [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--input-format",
        "stream-json",
    ];
    let pick = f.command().arg("pick").output().unwrap();
    assert!(
        pick.status.success(),
        "{}",
        String::from_utf8_lossy(&pick.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&pick.stdout), "a (a@x)\n");
    assert!(!f.temp.path().join("args").exists());
    let output = f.command().arg("t3-claude").args(flags).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"FAKE_CLAUDE_OK\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("t3-claude -> a (a@x)"));
    let args = fs::read_to_string(f.temp.path().join("args")).unwrap();
    assert!(args.starts_with(&flags.join("\n")));
    let profile = fs::read_to_string(f.temp.path().join("args.profile")).unwrap();
    assert_eq!(
        profile.trim(),
        f.data.join("cs/profiles/61").to_str().unwrap()
    );
    use std::os::unix::process::CommandExt;
    let output = f
        .command()
        .arg0("cs-t3-claude")
        .args(flags)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"FAKE_CLAUDE_OK\n");
}

#[test]
fn t3_empty_accounts_preserves_current_environment_and_status() {
    let f = Fixture::new();
    fs::write(f.data.join("cs/state.json"), "{}").unwrap();
    let output = f
        .command()
        .arg("t3-claude")
        .args(["-p", "hello"])
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"FAKE_CLAUDE_OK\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("fallback"));
    assert_eq!(
        fs::read_to_string(f.temp.path().join("args")).unwrap(),
        "-p\nhello\n"
    );
    let output = f.command().arg("pick").output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"current\n");
}

#[test]
fn t3_pick_avoids_account_with_live_session() {
    let f = Fixture::new();
    let path = f.data.join("cs/state.json");
    let mut saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let mut b = saved["providers"]["claude"]["accounts"]["a"].clone();
    b["email"] = json!("b@x");
    b["data"]["oauthAccount"]["emailAddress"] = json!("b@x");
    saved["providers"]["claude"]["accounts"]["b"] = b;
    fs::write(path, saved.to_string()).unwrap();
    let path = f.data.join("cs/usage.json");
    let mut usage: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    usage["accounts"]["b"] = usage["accounts"]["a"].clone();
    fs::write(path, usage.to_string()).unwrap();
    let mut child = f
        .command()
        .arg("t3-claude")
        .arg("-p")
        .env("TEST_WAIT", "yes")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f.temp.path().join("child.pid").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = f.command().arg("pick").output().unwrap();
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    child.wait().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"b (b@x)\n");
}

fn wait_for(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn t3_shim_becomes_claude_and_leaves_no_wrapper_or_stray_files() {
    let f = Fixture::new();
    for _ in 0..2 {
        let _ = fs::remove_file(f.temp.path().join("child.pid"));
        let mut child = f
            .command()
            .arg("t3-claude")
            .args(["--output-format", "stream-json", "--verbose"])
            .env("TEST_WAIT", "yes")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_for(&f.temp.path().join("child.pid"));
        let claude_pid = fs::read_to_string(f.temp.path().join("child.pid")).unwrap();
        assert_eq!(claude_pid.trim(), child.id().to_string());
        let leases = f.registry()["leases"].as_object().unwrap().clone();
        assert!(leases
            .values()
            .any(|lease| lease["process"]["pid"] == child.id()));
        unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
        child.wait().unwrap();
    }
    let profile = f.data.join("cs/profiles/61");
    let launch_files: Vec<_> = fs::read_dir(&profile)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("settings") && name != "settings.json")
        .collect();
    assert_eq!(launch_files, ["t3-launch-settings.json"]);
    let args = fs::read_to_string(f.temp.path().join("args")).unwrap();
    assert!(args.contains(profile.join("t3-launch-settings.json").to_str().unwrap()));
}

#[test]
fn t3_shim_passes_version_and_session_args_through_the_same_exec() {
    let f = Fixture::new();
    let output = f
        .command()
        .args(["t3-claude", "--version"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let args = fs::read_to_string(f.temp.path().join("args")).unwrap();
    assert_eq!(args.lines().next(), Some("--version"));
}

#[test]
fn t3_shim_execs_when_cached_usage_is_stale_or_missing() {
    let stale = chrono::Utc::now().timestamp() - 301;
    for usage in [
        None,
        Some(
            json!({"observed_at":stale,"accounts":{"a":{"reading":{"status":"known","five_hour":{"utilization":10.0,"resets_at":null}},"fetched_at":stale}}}),
        ),
    ] {
        let f = Fixture::new();
        let path = f.data.join("cs/usage.json");
        match &usage {
            None => fs::remove_file(&path).unwrap(),
            Some(value) => fs::write(&path, value.to_string()).unwrap(),
        }
        let output = f
            .command()
            .args(["t3-claude", "--version"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(f.temp.path().join("args"))
                .unwrap()
                .lines()
                .next(),
            Some("--version")
        );
    }
}

#[test]
fn t3_shim_does_not_launch_when_fresh_usage_is_exhausted() {
    let f = Fixture::new();
    fs::write(f.data.join("cs/usage.json"),json!({"observed_at":chrono::Utc::now().timestamp(),"accounts":{"a":{"reading":{"status":"known","five_hour":{"utilization":0,"resets_at":null},"seven_day":{"utilization":100,"resets_at":null}},"fetched_at":chrono::Utc::now().timestamp()}}}).to_string()).unwrap();
    let output = f
        .command()
        .args(["t3-claude", "--version"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("fresh usage") || err.contains("quota exhausted"),
        "{err}"
    );
    assert!(!f.temp.path().join("args").exists());
}

#[test]
fn t3_shim_keeps_a_valid_profile_token_and_imports_a_fresher_one() {
    let f = Fixture::new();
    assert!(f
        .command()
        .args(["t3-claude", "--version"])
        .output()
        .unwrap()
        .status
        .success());
    let profile = f.data.join("cs/profiles/61");
    let creds = profile.join(".credentials.json");
    let fresher = (chrono::Utc::now().timestamp() + 20000) * 1000;
    let rotated = json!({"claudeAiOauth":{"accessToken":"profile-access","refreshToken":"profile-refresh","expiresAt":fresher}});
    fs::write(&creds, rotated.to_string()).unwrap();
    assert!(f
        .command()
        .args(["t3-claude", "--version"])
        .output()
        .unwrap()
        .status
        .success());
    let kept: Value = serde_json::from_slice(&fs::read(&creds).unwrap()).unwrap();
    assert_eq!(kept, rotated);
    let state: Value =
        serde_json::from_slice(&fs::read(f.data.join("cs/state.json")).unwrap()).unwrap();
    let saved = &state["providers"]["claude"]["accounts"]["a"]["data"]["claudeAiOauth"];
    assert_eq!(saved["accessToken"], "profile-access");
    assert_eq!(saved["refreshToken"], "profile-refresh");

    let stale = json!({"claudeAiOauth":{"accessToken":"old","expiresAt":(chrono::Utc::now().timestamp() + 60) * 1000}});
    fs::write(&creds, stale.to_string()).unwrap();
    assert!(f
        .command()
        .args(["t3-claude", "--version"])
        .output()
        .unwrap()
        .status
        .success());
    let replaced: Value = serde_json::from_slice(&fs::read(&creds).unwrap()).unwrap();
    assert_eq!(replaced["claudeAiOauth"]["accessToken"], "profile-access");
    assert!(replaced["claudeAiOauth"].get("refreshToken").is_none());
}

#[test]
fn borrowed_launches_release_and_keep_grants_out_of_storage_and_errors() {
    for exit in [0, 7, 143] {
        let f = Fixture::new();
        let ssh = f.bin.join("ssh");
        fs::write(&ssh, r#"#!/bin/sh
printf '%s\n' "$@" >> "$TEST_SIGNALS"
case "$*" in
 *lend-release*) exit 0;;
esac
printf '%s\n' '{"alias":"owner","email":"owner@x","lease_id":"same-lease","expires_at":9999999999,"access_token":"test-access-aaa"}'
"#).unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            f.bin.join("claude"),
            format!(
                r#"#!/bin/sh
[ "$CLAUDE_CODE_OAUTH_TOKEN" = test-access-aaa ] || exit 81
printf '%s\n' "$@" > "$TEST_ARGS"
echo CHILD
exit {exit}
"#
            ),
        )
        .unwrap();
        let before = fs::read(f.data.join("cs/state.json")).unwrap();
        let output = f
            .command()
            .env("CS_SSH", &ssh)
            .args([
                "run",
                "--borrow=owner",
                "--",
                "-p",
                "--output-format",
                "stream-json",
            ])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(exit));
        assert_eq!(output.stdout, b"CHILD\n");
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(!err.contains("test-access-aaa") && !err.contains("test-refresh-bbb"));
        assert!(err.contains("--borrow owner -> owner (owner@x)"));
        assert_eq!(before, fs::read(f.data.join("cs/state.json")).unwrap());
        let args = fs::read_to_string(f.temp.path().join("args")).unwrap();
        assert!(!args.contains("--borrow"));
        assert!(args.contains("stream-json"));
        let ssh_args = fs::read_to_string(f.temp.path().join("signals")).unwrap();
        assert!(ssh_args.contains("BatchMode=yes") && ssh_args.contains("ControlPath=none"));
        assert!(ssh_args.contains("lend-release 'same-lease'"));
    }
}

#[test]
fn lend_is_durable_until_release() {
    let f = Fixture::new();
    let output = f.command().arg("lend").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let grant: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(grant.get("refresh_token").is_none());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(!err.contains("saved-access") && !err.contains("saved-refresh"));
    let id = grant["lease_id"].as_str().unwrap();
    assert!(f.command().arg("pick").output().unwrap().status.success());
    assert!(f.registry()["leases"].get(id).is_some());
    assert!(f
        .command()
        .args(["lend-release", id])
        .status()
        .unwrap()
        .success());
    assert!(f.registry()["leases"].get(id).is_none());
}

#[test]
fn borrowed_run_releases_after_sigterm() {
    let f = Fixture::new();
    let ssh = f.bin.join("ssh");
    fs::write(&ssh, r#"#!/bin/sh
printf '%s\n' "$@" >> "$TEST_SIGNALS"
case "$*" in *lend-release*) exit 0;; esac
echo '{"alias":"owner","email":"owner@x","lease_id":"signal-lease","expires_at":9999999999,"access_token":"test-access-aaa"}'
"#).unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        f.bin.join("claude"),
        r#"#!/bin/sh
echo ready > "$TEST_CHILD_PID"
while :; do sleep 0.1; done
"#,
    )
    .unwrap();
    let mut command = f.command();
    command
        .env("CS_SSH", &ssh)
        .args(["run", "--borrow", "owner", "--"]);
    let mut child = command
        .args(["-p", "--output-format", "stream-json"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f.temp.path().join("child.pid").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(143));
    assert!(fs::read_to_string(f.temp.path().join("signals"))
        .unwrap()
        .contains("lend-release 'signal-lease'"));
}

#[test]
fn lend_without_saved_accounts_has_no_grant() {
    let f = Fixture::new();
    fs::write(f.data.join("cs/state.json"), "{}").unwrap();
    let output = f.command().arg("lend").output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("saved-refresh"));
}

// ---- borrow: lender, cache, t3 exec path -------------------------------------------------

/// An owner machine with two saved subscription accounts, `oa` and `ob`. `ob` has no refresh
/// token, so renewing it can only fail, and it is the one routing prefers (lower usage).
fn owner_fixture(ob_expires_in: i64) -> Fixture {
    let o = Fixture::new();
    let now = chrono::Utc::now().timestamp();
    let account = |alias: &str, expires_in: i64, refresh: bool| {
        let mut oauth = json!({"accessToken":format!("{alias}-access"),"expiresAt":(now + expires_in) * 1000,"scopes":["user:inference"],"subscriptionType":"max"});
        if refresh {
            oauth["refreshToken"] = json!(format!("{alias}-refresh"));
        }
        json!({"email":format!("{alias}@x"),"data":{"claudeAiOauth":oauth,"oauthAccount":{"emailAddress":format!("{alias}@x"),"accountUuid":format!("{alias}-uuid")}}})
    };
    let state = json!({"providers":{"claude":{"current":"oa","accounts":{
        "oa":account("oa", 7200, true),"ob":account("ob", ob_expires_in, false)}}}});
    fs::write(o.data.join("cs/state.json"), state.to_string()).unwrap();
    let reading = |five: f64| json!({"reading":{"status":"known","five_hour":{"utilization":five,"resets_at":null}},"fetched_at":now});
    fs::write(
        o.data.join("cs/usage.json"),
        json!({"observed_at":now,"accounts":{"oa":reading(50.0),"ob":reading(1.0)}}).to_string(),
    )
    .unwrap();
    o
}

fn script(path: &std::path::Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A CS_SSH stub that runs the remote command against the owner fixture and logs each call.
fn owner_ssh(borrower: &Fixture, owner: &Fixture) -> PathBuf {
    let cs_dir = borrower.temp.path().join("owner-bin");
    fs::create_dir_all(&cs_dir).unwrap();
    let _ = std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_cs"), cs_dir.join("cs"));
    let ssh = borrower.temp.path().join("ssh-owner");
    script(
        &ssh,
        &format!(
            r#"echo "$*" >> "{log}"
eval "cmd=\${{$#}}"
exec env -u CS_BORROW -u CS_SSH -u CS_BORROW_JUMP HOME="{home}" XDG_DATA_HOME="{data}" XDG_CONFIG_HOME="{cfg}" CLAUDE_CONFIG_DIR="{src}" CS_NATIVE_SOURCE_CONFIG="{src}" PATH="{bin}:/usr/bin:/bin" sh -c "$cmd" 2>> "{log}.err"
"#,
            log = ssh_log(borrower).display(),
            home = owner.temp.path().display(),
            data = owner.data.display(),
            cfg = owner.temp.path().join("config").display(),
            src = owner.source.display(),
            bin = cs_dir.display(),
        ),
    );
    ssh
}

fn ssh_log(f: &Fixture) -> PathBuf {
    f.temp.path().join("ssh.log")
}

fn ssh_calls(f: &Fixture, needle: &str) -> usize {
    fs::read_to_string(ssh_log(f))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(needle))
        .count()
}

/// A CS_SSH stub that must not be needed: it logs, then fails or hangs.
fn broken_ssh(f: &Fixture, hang: bool) -> PathBuf {
    let ssh = f.temp.path().join("ssh-broken");
    script(
        &ssh,
        &format!(
            "echo \"$*\" >> \"{}\"\n{}\n",
            ssh_log(f).display(),
            if hang { "exec sleep 30" } else { "exit 1" }
        ),
    );
    ssh
}

fn borrow_cache(f: &Fixture) -> PathBuf {
    f.data.join("cs/borrow/owner.json")
}

fn refresh_cache(f: &Fixture, ssh: &std::path::Path) {
    let output = f
        .command()
        .env("CS_SSH", ssh)
        .args(["borrow", "refresh", "--from", "owner"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn lend_renews_only_the_selected_alias_and_ignores_unrenewable_others() {
    let o = owner_fixture(-3600);
    let before = fs::read(o.data.join("cs/state.json")).unwrap();
    let output = o
        .command()
        .args(["lend", "--alias", "oa", "--no-lease"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{err}");
    let grant: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(grant["alias"], "oa");
    assert_eq!(grant["email"], "oa@x");
    assert!(grant.get("refresh_token").is_none() && grant.get("refreshToken").is_none());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("oa-refresh"));
    assert!(!err.contains("oa-access") && !err.contains("oa-refresh"));
    assert_eq!(before, fs::read(o.data.join("cs/state.json")).unwrap());
    assert!(o.registry_opt().is_none_or(|r| r["leases"]
        .as_object()
        .is_none_or(|leases| leases.is_empty())));

    let output = o
        .command()
        .args(["lend", "--alias", "ob", "--no-lease"])
        .output()
        .unwrap();
    assert!(!output.status.success() && output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("ob-access"));
}

#[test]
fn bare_lend_skips_the_preferred_account_when_only_it_cannot_be_lent() {
    let o = owner_fixture(-3600);
    let output = o.command().arg("lend").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let grant: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(grant["alias"], "oa");
    assert!(!grant["lease_id"].as_str().unwrap().is_empty());
}

#[test]
fn lend_inventory_lists_aliases_and_usage_without_credentials() {
    let o = owner_fixture(3600);
    let output = o.command().args(["lend", "--inventory"]).output().unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    for secret in ["access", "refresh", "oa-uuid"] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    let inventory: Value = serde_json::from_str(&text).unwrap();
    let accounts = inventory["accounts"].as_array().unwrap();
    let aliases: Vec<_> = accounts
        .iter()
        .map(|a| a["alias"].as_str().unwrap())
        .collect();
    assert_eq!(aliases, ["oa", "ob"]);
    assert_eq!(accounts[1]["email"], "ob@x");
    assert!(accounts[1]["expires_at"].as_i64().unwrap() > chrono::Utc::now().timestamp());
    assert_eq!(accounts[1]["usage"]["five_hour"]["utilization"], 1.0);
}

#[test]
fn borrow_refresh_caches_access_only_grants_with_private_modes() {
    let o = owner_fixture(7200);
    let f = Fixture::new();
    let ssh = owner_ssh(&f, &o);
    let output = f
        .command()
        .env("CS_SSH", &ssh)
        .args(["borrow", "refresh", "--from", "owner"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{stderr} / {:?}",
        fs::read_to_string(format!("{}.err", ssh_log(&f).display()))
    );
    assert!(stdout.contains("oa") && stdout.contains("ob"));
    for text in [stdout.as_ref(), stderr.as_ref()] {
        assert!(!text.contains("-access") && !text.contains("-refresh"));
    }
    let cache = borrow_cache(&f);
    let mode = |p: &std::path::Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&cache), 0o600);
    assert_eq!(mode(cache.parent().unwrap()), 0o700);
    let text = fs::read_to_string(&cache).unwrap();
    assert!(text.contains("oa-access") && text.contains("ob-access"));
    assert!(!text.to_lowercase().contains("refresh"), "{text}");
    assert_eq!(ssh_calls(&f, "--inventory"), 1);
    assert_eq!(ssh_calls(&f, "lend --alias"), 2);
    assert!(o.registry_opt().is_none_or(|r| r["leases"]
        .as_object()
        .is_none_or(|leases| leases.is_empty())));

    refresh_cache(&f, &ssh);
    assert_eq!(
        ssh_calls(&f, "lend --alias"),
        2,
        "warm tokens are not re-lent"
    );
}

#[test]
fn borrow_refresh_relends_only_aliases_that_are_missing_or_near_expiry() {
    let o = owner_fixture(7200);
    let f = Fixture::new();
    let ssh = owner_ssh(&f, &o);
    refresh_cache(&f, &ssh);
    let mut cache: Value = serde_json::from_slice(&fs::read(borrow_cache(&f)).unwrap()).unwrap();
    cache["entries"]["ob"]["expires_at"] = json!(chrono::Utc::now().timestamp() + 60);
    fs::write(borrow_cache(&f), cache.to_string()).unwrap();
    refresh_cache(&f, &ssh);
    assert_eq!(ssh_calls(&f, "lend --alias 'ob'"), 2);
    assert_eq!(ssh_calls(&f, "lend --alias 'oa'"), 1);
}

#[test]
fn borrow_refresh_nests_the_jump_host_and_validates_hosts() {
    let f = Fixture::new();
    let ssh = broken_ssh(&f, false);
    for (args, env) in [(vec!["--via", "hop"], None), (vec![], Some("hop"))] {
        let _ = fs::remove_file(ssh_log(&f));
        let mut command = f.command();
        command
            .env("CS_SSH", &ssh)
            .args(["borrow", "refresh", "--from", "owner"])
            .args(args);
        if let Some(hop) = env {
            command.env("CS_BORROW_JUMP", hop);
        }
        assert!(!command.output().unwrap().status.success());
        let log = fs::read_to_string(ssh_log(&f)).unwrap();
        assert!(
            log.contains("ControlPath=none hop ssh -o BatchMode=yes -o ControlPath=none owner "),
            "{log}"
        );
        assert!(log.contains("--inventory"));
    }
    let _ = fs::remove_file(ssh_log(&f));
    f.command()
        .env("CS_SSH", &ssh)
        .args(["borrow", "refresh", "--from", "owner"])
        .output()
        .unwrap();
    let log = fs::read_to_string(ssh_log(&f)).unwrap();
    assert!(log.contains("ControlPath=none owner export PATH"), "{log}");
    for bad in ["-oProxyCommand=x", "a b", "", ".."] {
        let output = f
            .command()
            .env("CS_SSH", &ssh)
            .args(["borrow", "refresh", "--from", bad])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{bad:?} accepted");
    }
    assert!(!f
        .command()
        .env("CS_SSH", &ssh)
        .args([
            "borrow",
            "refresh",
            "--from",
            "owner",
            "--via",
            "-oProxyCommand=x"
        ])
        .status()
        .unwrap()
        .success());
}

fn spawn_t3(f: &Fixture, ssh: &std::path::Path, wait: bool) -> std::process::Child {
    let mut command = f.command();
    command
        .env("CS_SSH", ssh)
        .env("CS_BORROW", "owner")
        .arg("t3-claude")
        .args(["--output-format", "stream-json", "--verbose"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if wait {
        command.env("TEST_WAIT", "yes");
    }
    command.spawn().unwrap()
}

#[test]
fn warm_cache_t3_spawn_execs_claude_without_touching_ssh_and_rotates() {
    let o = owner_fixture(7200);
    let f = Fixture::new();
    refresh_cache(&f, &owner_ssh(&f, &o));
    let calls = ssh_calls(&f, "");
    let broken = broken_ssh(&f, true);
    let mut used = Vec::new();
    for _ in 0..2 {
        let _ = fs::remove_file(f.temp.path().join("child.pid"));
        let started = Instant::now();
        let mut child = spawn_t3(&f, &broken, true);
        wait_for(&f.temp.path().join("child.pid"));
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid = fs::read_to_string(f.temp.path().join("child.pid")).unwrap();
        assert_eq!(
            pid.trim(),
            child.id().to_string(),
            "Claude is the spawned pid"
        );
        let profile = PathBuf::from(
            fs::read_to_string(f.temp.path().join("args.profile"))
                .unwrap()
                .trim(),
        );
        assert!(profile.starts_with(f.data.join("cs/borrow/profiles/owner")));
        let config: Value =
            serde_json::from_slice(&fs::read(profile.join(".claude.json")).unwrap()).unwrap();
        used.push(
            config["oauthAccount"]["emailAddress"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
        let credentials = fs::read_to_string(profile.join(".credentials.json")).unwrap();
        let oauth: Value = serde_json::from_str(&credentials).unwrap();
        assert!(oauth["claudeAiOauth"]["accessToken"]
            .as_str()
            .unwrap()
            .ends_with("-access"));
        assert!(oauth["claudeAiOauth"].get("refreshToken").is_none());
        assert!(
            oauth["claudeAiOauth"]["expiresAt"].as_i64().unwrap()
                > chrono::Utc::now().timestamp_millis()
        );
        assert_eq!(oauth["claudeAiOauth"]["subscriptionType"], "max");
        assert!(f
            .registry_opt()
            .is_none_or(|r| r["leases"].as_object().is_none_or(|l| l.is_empty())));
        unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
        let mut stderr = String::new();
        use std::io::Read;
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        child.wait().unwrap();
        assert!(!stderr.contains("-access") && !stderr.contains("-refresh"));
    }
    used.sort();
    assert_eq!(
        used,
        ["oa@x", "ob@x"],
        "consecutive spawns rotate across the cache"
    );
    assert_eq!(ssh_calls(&f, ""), calls, "warm spawns never run ssh");
}

#[test]
fn cold_cache_concurrent_t3_spawns_share_one_refresh() {
    let o = owner_fixture(7200);
    let f = Fixture::new();
    let ssh = owner_ssh(&f, &o);
    let children: Vec<_> = (0..4)
        .map(|_| {
            let mut command = f.command();
            command
                .env("CS_SSH", &ssh)
                .env("CS_BORROW", "owner")
                .args(["t3-claude", "--version"])
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            command.spawn().unwrap()
        })
        .collect();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(ssh_calls(&f, "--inventory"), 1);
    assert!(borrow_cache(&f).exists());
}

#[test]
fn failing_or_hung_refresh_falls_back_to_the_local_login_and_backs_off() {
    for hang in [false, true] {
        let f = Fixture::new();
        let ssh = broken_ssh(&f, hang);
        let run = || {
            let started = Instant::now();
            let output = f
                .command()
                .env("CS_SSH", &ssh)
                .env("CS_BORROW", "owner")
                .env("CS_BORROW_TIMEOUT_SECS", "1")
                .args(["t3-claude", "--version"])
                .output()
                .unwrap();
            (output, started.elapsed())
        };
        let (output, took) = run();
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{err}");
        assert!(took < Duration::from_secs(10), "{took:?}");
        assert!(
            err.contains("borrow") && err.contains("falling back"),
            "{err}"
        );
        assert_eq!(
            fs::read_to_string(f.temp.path().join("args"))
                .unwrap()
                .lines()
                .next(),
            Some("--version")
        );
        let profile = fs::read_to_string(f.temp.path().join("args.profile")).unwrap();
        assert!(
            profile.trim().ends_with("/profiles/61"),
            "local account profile: {profile}"
        );
        assert_eq!(ssh_calls(&f, "--inventory"), 1);
        let (output, _) = run();
        assert!(output.status.success());
        assert_eq!(
            ssh_calls(&f, "--inventory"),
            1,
            "recent failure suppresses retries"
        );
    }
}

#[test]
fn borrow_pick_names_a_cached_account_and_reports_bad_responses_safely() {
    let o = owner_fixture(7200);
    let f = Fixture::new();
    refresh_cache(&f, &owner_ssh(&f, &o));
    let output = f
        .command()
        .env("CS_SSH", broken_ssh(&f, false))
        .args(["pick", "--borrow", "owner"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let line = String::from_utf8(output.stdout).unwrap();
    assert!(line == "oa (oa@x)\n" || line == "ob (ob@x)\n", "{line}");

    let f = Fixture::new();
    let bad = f.temp.path().join("ssh-bad");
    script(&bad, "echo 'test-access-aaa not json'\n");
    let output = f
        .command()
        .env("CS_SSH", &bad)
        .args(["borrow", "refresh", "--from", "owner"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("test-access-aaa"));
    assert!(!borrow_cache(&f).exists());
}
