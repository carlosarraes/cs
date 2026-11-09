#!/usr/bin/env bash
# Stress test for the cs-t3-claude shim, mimicking how t3 spawns and kills it.
#
# Run:   scripts/t3-shim-stress.sh              (uses target/release/cs or target/debug/cs)
#        CS_BIN=/path/to/cs CYCLES=200 CONCURRENCY=6 SEED=1 scripts/t3-shim-stress.sh
# Needs: python3, ps. Exit 0 only if no shim, fake claude, native-maintain or zombie process
#        is left over compared with the baseline taken before the first cycle.
#
# Everything runs in a throwaway HOME / XDG dir with two fake subscription accounts and a fake
# `claude` first on PATH (no network, no model calls). The cycles mix: --version, a stream-json
# session ended by closing stdin, SDK abort (SIGTERM, 5s, SIGKILL), immediate SIGKILL, t3's
# /bin/sh process-group watcher script (CLAUDE_GROUP_WATCH_SCRIPT) with fd3 closed, and
# kill -KILL of the whole process group. On the kill-style cycles the driver keeps the dead
# process's stdin pipe open until the check, so a Claude that outlives its shim is counted
# instead of quietly exiting on EOF. After the cycles it waits up to REAP_SECS (default 15,
# the watcher sleeps 5s before its group SIGKILL) for stragglers to disappear, then compares.
# Borrow: unless CS_STRESS_BORROW=0, half of the cycles also run with CS_BORROW=owner against a
# fake lend (a CS_SSH stub answering `lend --inventory` and `lend --alias X --no-lease` for two
# throwaway accounts). The cache is warmed once with `cs borrow refresh` before the cycles; the
# run then fails if any ssh is started afterwards (the spawn path must never use the network)
# or if the borrowed path was never exercised.
# On macOS the cs profile credentials live in the Keychain, so set CS_STRESS_ALLOW_KEYCHAIN=1
# to accept that fake items are created for the throwaway profile paths.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
if [[ -z "${CS_BIN:-}" ]]; then
    for candidate in "$root/target/release/cs" "$root/target/debug/cs"; do
        [[ -x "$candidate" ]] && CS_BIN="$candidate" && break
    done
fi
[[ -x "${CS_BIN:-}" ]] || { echo "error: build cs first or set CS_BIN" >&2; exit 2; }
if [[ "$(uname)" == Darwin && -z "${CS_STRESS_ALLOW_KEYCHAIN:-}" ]]; then
    echo "error: set CS_STRESS_ALLOW_KEYCHAIN=1 to run on macOS (see header)" >&2
    exit 2
fi
export CS_BIN CS_STRESS_BORROW="${CS_STRESS_BORROW:-1}" CYCLES="${CYCLES:-200}" CONCURRENCY="${CONCURRENCY:-6}" SEED="${SEED:-1}" REAP_SECS="${REAP_SECS:-15}"

exec python3 -I - <<'PY'
import json, os, random, shutil, signal, subprocess, sys, tempfile, threading, time
from concurrent.futures import ThreadPoolExecutor

CS = os.path.realpath(os.environ["CS_BIN"])
CYCLES, CONCURRENCY = int(os.environ["CYCLES"]), int(os.environ["CONCURRENCY"])
REAP_SECS, SEED = float(os.environ["REAP_SECS"]), int(os.environ["SEED"])
STREAM = ["--output-format", "stream-json", "--verbose", "--input-format", "stream-json"]
WATCH = ("{ trap '' TERM; read -r _ <&3; kill -TERM 0; sleep 5; kill -KILL 0; } </dev/null >/dev/null 2>&1 &\n"
         'exec "$@" 3<&-')

tmp = tempfile.mkdtemp(prefix="cs-t3-stress-")
bin_dir = os.path.join(tmp, "bin")
os.makedirs(bin_dir)
shim = os.path.join(bin_dir, "cs-t3-claude")
os.symlink(CS, shim)
fake = os.path.join(bin_dir, "claude")
with open(fake, "w") as f:
    f.write('#!/bin/sh\n'
            'case "$1" in --version) echo "2.1.0 (Claude Code)"; exit 0;; esac\n'
            'printf \'{"type":"system","subtype":"init"}\\n\'\n'
            'while IFS= read -r _; do :; done\n')
os.chmod(fake, 0o755)

data, source = os.path.join(tmp, "data"), os.path.join(tmp, "claude")
for d in (os.path.join(data, "cs"), source, os.path.join(tmp, "home")):
    os.makedirs(d, exist_ok=True)

BORROW = os.environ["CS_STRESS_BORROW"] != "0"
ssh_log = os.path.join(tmp, "ssh.log")
fake_ssh = os.path.join(bin_dir, "fake-ssh")
if BORROW:
    exp = int(time.time()) + 7200
    replies = {"inv": {"accounts": [{"alias": a, "email": f"{a}@x", "expires_at": exp, "usage": None} for a in ("ba", "bb")]}}
    for a in ("ba", "bb"):
        replies[a] = {"alias": a, "email": f"{a}@x", "lease_id": "", "expires_at": exp, "access_token": f"{a}-access",
                      "oauth_meta": {"scopes": ["user:inference"], "subscriptionType": "max"},
                      "identity": {"emailAddress": f"{a}@x"}}
    for name, reply in replies.items():
        with open(os.path.join(tmp, f"reply-{name}.json"), "w") as f:
            json.dump(reply, f)
    with open(fake_ssh, "w") as f:
        f.write(f"""#!/bin/sh
echo "$*" >> '{ssh_log}'
eval "cmd=\\${{$#}}"
case "$cmd" in
  *'lend --inventory'*) cat '{tmp}/reply-inv.json';;
  *"lend --alias 'ba' "*) cat '{tmp}/reply-ba.json';;
  *"lend --alias 'bb' "*) cat '{tmp}/reply-bb.json';;
  *) exit 1;;
esac
""")
    os.chmod(fake_ssh, 0o755)

def write_usage():
    now = int(time.time())
    reading = {"reading": {"status": "known", "five_hour": {"utilization": 0, "resets_at": None}}, "fetched_at": now}
    path = os.path.join(data, "cs", "usage.json")
    with open(path + ".new", "w") as f:
        json.dump({"observed_at": now, "accounts": {"a": reading, "b": reading}}, f)
    os.replace(path + ".new", path)

expiry = (int(time.time()) + 7200) * 1000
accounts = {}
for alias in ("a", "b"):
    email = f"{alias}@x"
    accounts[alias] = {"email": email, "data": {
        "claudeAiOauth": {"accessToken": f"{alias}-access", "refreshToken": f"{alias}-refresh", "expiresAt": expiry},
        "oauthAccount": {"emailAddress": email}}}
with open(os.path.join(data, "cs", "state.json"), "w") as f:
    json.dump({"providers": {"claude": {"current": "a", "accounts": accounts}}}, f)
with open(os.path.join(source, ".claude.json"), "w") as f:
    json.dump({"hasCompletedOnboarding": True}, f)
write_usage()

env = {k: v for k, v in os.environ.items()
       if not k.startswith(("ANTHROPIC_", "CLAUDE_CODE_", "CS_")) and k != "CLAUDE_CONFIG_DIR"}
env.update(HOME=os.path.join(tmp, "home"), XDG_DATA_HOME=data, XDG_CONFIG_HOME=os.path.join(tmp, "config"),
           CLAUDE_CONFIG_DIR=source, CS_NATIVE_SOURCE_CONFIG=source,
           PATH=bin_dir + os.pathsep + os.environ.get("PATH", ""))

borrow_env = dict(env, CS_BORROW="owner", CS_SSH=fake_ssh)

def pick_env(rng):
    return borrow_env if BORROW and rng.random() < 0.5 else env

def table():
    out = subprocess.run(["ps", "-axo", "pid=,ppid=,stat=,command="], capture_output=True, text=True).stdout
    rows = []
    for line in out.splitlines():
        parts = line.split(None, 3)
        if len(parts) >= 3:
            rows.append((int(parts[0]), parts[2], parts[3] if len(parts) > 3 else ""))
    return rows

def counts():
    me = os.getpid()
    c = dict(shim=0, claude=0, maintain=0, zombie=0, ssh=0)
    for pid, stat, cmd in table():
        if stat.startswith("Z"):
            c["zombie"] += 1
        if pid == me or " ps " in f" {cmd} " or cmd.startswith("ps "):
            continue
        if fake_ssh in cmd:
            c["ssh"] += 1
        elif f"{fake}" in cmd:
            c["claude"] += 1
        elif "native-maintain" in cmd:
            c["maintain"] += 1
        elif "cs-t3-claude" in cmd or " t3-claude" in cmd:
            c["shim"] += 1
    return c

def ssh_calls():
    try:
        with open(ssh_log) as f:
            return len(f.read().splitlines())
    except FileNotFoundError:
        return 0

if BORROW:
    warm = subprocess.run([CS, "borrow", "refresh", "--from", "owner"], env=borrow_env, cwd=tmp,
                          capture_output=True, text=True, timeout=60)
    if warm.returncode != 0 or "-access" in warm.stdout + warm.stderr:
        print("FAIL: borrow refresh warm-up:", warm.returncode, warm.stderr.strip()[:200])
        sys.exit(1)
warm_ssh_calls = ssh_calls()

before = counts()
load_before = os.getloadavg()[0]
peak = dict(load=load_before, procs=0)
stop = threading.Event()

def monitor():
    last_usage = time.time()
    while not stop.wait(0.5):
        peak["load"] = max(peak["load"], os.getloadavg()[0])
        now = counts()
        peak["procs"] = max(peak["procs"], now["shim"] + now["claude"] + now["maintain"])
        if time.time() - last_usage > 60:
            write_usage()
            last_usage = time.time()

errors = []
held = []

def hold(p):
    held.append(p)

def fail(kind, message):
    errors.append(f"{kind}: {message}")

def spawn(argv, e, **kw):
    return subprocess.Popen(argv, env=e, cwd=tmp, **kw)

def shim_argv(rng, args):
    return [shim, *args] if rng.random() < 0.7 else [CS, "t3-claude", *args]

def wait_exit(proc, seconds):
    try:
        return proc.wait(seconds)
    except subprocess.TimeoutExpired:
        return None

def first_line(proc, kind):
    line = proc.stdout.readline()
    if not line:
        fail(kind, f"no output before exit (rc={proc.poll()})")
    return line

def cycle_version(rng):
    out = subprocess.run(shim_argv(rng, ["--version"]), env=pick_env(rng), cwd=tmp, stdin=subprocess.DEVNULL,
                         capture_output=True, text=True, timeout=30)
    if out.returncode != 0 or "Claude Code" not in out.stdout:
        fail("version", f"rc={out.returncode} stderr={out.stderr.strip()[:200]}")

def cycle_session(rng):
    p = spawn(shim_argv(rng, STREAM), pick_env(rng), stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    first_line(p, "session")
    p.stdin.close()
    if wait_exit(p, 10) != 0:
        fail("session", "did not exit 0 after stdin closed")
        p.kill(); p.wait()

def cycle_abort(rng):
    p = spawn(shim_argv(rng, STREAM), pick_env(rng), stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    time.sleep(rng.uniform(0, 0.15))
    p.send_signal(signal.SIGTERM)
    if wait_exit(p, 5) is None:
        p.kill(); p.wait()
    hold(p)

def cycle_kill(rng):
    p = spawn(shim_argv(rng, STREAM), pick_env(rng), stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    time.sleep(rng.uniform(0, 0.05))
    p.kill(); p.wait()
    hold(p)

def wrapper_cycle(rng, kind, kill_group_on_exit):
    r, w = os.pipe()
    def child_setup():
        os.setsid()
        os.dup2(r, 3)
        os.set_inheritable(3, True)
    p = spawn(["/bin/sh", "-c", WATCH, "t3-claude", *shim_argv(rng, STREAM)], pick_env(rng), stdin=subprocess.PIPE,
              stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, close_fds=False, preexec_fn=child_setup)
    os.close(r)
    first_line(p, kind)
    os.close(w)
    if wait_exit(p, 10) is None:
        fail(kind, "session did not stop after fd3 closed")
        p.kill(); p.wait()
    if kill_group_on_exit:
        try:
            os.killpg(p.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    p.stdin.close(); p.stdout.close()

def cycle_wrapper(rng):
    wrapper_cycle(rng, "wrapper", True)

def cycle_wrapper_watcher_only(rng):
    wrapper_cycle(rng, "wrapper-watcher", False)

def cycle_group_kill(rng):
    p = spawn(shim_argv(rng, STREAM), pick_env(rng), stdin=subprocess.PIPE, stdout=subprocess.PIPE,
              stderr=subprocess.DEVNULL, start_new_session=True)
    time.sleep(rng.uniform(0, 0.1))
    try:
        os.killpg(p.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    p.wait()
    hold(p)

CYCLES_BY_KIND = [cycle_version, cycle_session, cycle_abort, cycle_kill, cycle_wrapper,
                  cycle_wrapper_watcher_only, cycle_group_kill]

def run_one(index):
    rng = random.Random(SEED * 100003 + index)
    kind = CYCLES_BY_KIND[index % len(CYCLES_BY_KIND)]
    try:
        kind(rng)
    except Exception as error:
        fail(kind.__name__, repr(error))

started = time.time()
threading.Thread(target=monitor, daemon=True).start()
with ThreadPoolExecutor(CONCURRENCY) as pool:
    list(pool.map(run_one, range(CYCLES)))
elapsed = time.time() - started

deadline = time.time() + REAP_SECS
after = counts()
while time.time() < deadline and any(after[k] > before[k] for k in after):
    time.sleep(0.5)
    after = counts()
stop.set()
for p in held:
    p.stdin.close(); p.stdout.close()
leftover = {k: after[k] - before[k] for k in after if after[k] > before[k]}
if BORROW:
    if ssh_calls() != warm_ssh_calls:
        fail("borrow", f"ssh ran on the spawn path ({ssh_calls() - warm_ssh_calls} extra calls)")
    try:
        with open(os.path.join(data, "cs", "borrow", "owner.json")) as f:
            used = sum(1 for e in json.load(f)["entries"].values() if e.get("last_used"))
        if not used:
            fail("borrow", "no borrowed grant was ever picked")
    except (OSError, ValueError, KeyError) as error:
        fail("borrow", f"cache unreadable: {error!r}")

print(f"cycles={CYCLES} concurrency={CONCURRENCY} seed={SEED} borrow={int(BORROW)} elapsed={elapsed:.1f}s")
print(f"before: {before}")
if BORROW:
    print(f"ssh calls: warm-up={warm_ssh_calls} total={ssh_calls()}")
print(f"after:  {after}")
print(f"load1: before={load_before:.2f} peak={peak['load']:.2f} after={os.getloadavg()[0]:.2f}; "
      f"peak live shim/claude/maintain processes={peak['procs']}")
for message in errors[:20]:
    print("ERROR", message)
ok = not leftover and not errors
if not ok:
    for pid, stat, cmd in table():
        if tmp in cmd:
            print("LEFTOVER", pid, stat, cmd[:160])
    subprocess.run(["pkill", "-KILL", "-f", tmp], check=False)
shutil.rmtree(tmp, ignore_errors=True)
print("PASS: leftover counts are 0" if ok else f"FAIL: leftover={leftover} errors={len(errors)}")
sys.exit(0 if ok else 1)
PY
