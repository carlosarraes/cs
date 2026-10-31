#![cfg(target_os = "linux")]
use std::process::Command;

#[test]
fn login_pty_masks_paste_and_restores_terminal() {
    let result = Command::new("python3")
        .arg("-c")
        .arg(r#"
import os, pathlib, pty, select, signal, subprocess, sys, tempfile, termios, time
binary = sys.argv[1]
with tempfile.TemporaryDirectory() as directory:
    root = pathlib.Path(directory)
    fake = root / 'claude'
    fake.write_text('''#!/usr/bin/env python3
import os, sys, termios, time
if sys.argv[1:] != ['auth', 'login']: sys.exit(71)
mode = os.environ['TEST_LOGIN_MODE']
a = termios.tcgetattr(0)
a[3] &= ~termios.ECHO
termios.tcsetattr(0, termios.TCSANOW, a)
sys.stdout.write('Paste code here if pro'); sys.stdout.flush()
time.sleep(.03)
sys.stdout.write('mpted > '); sys.stdout.flush()
if mode == 'browser':
    time.sleep(.1)
    sys.exit(0)
if mode == 'failure': sys.exit(7)
line = sys.stdin.readline().strip()
sys.exit(0 if line == ('w' * 600 if mode == 'long' else 'wxyz') else 72)
''')
    fake.chmod(0o755)
    env = dict(os.environ, PATH=directory + ':' + os.environ['PATH'],
               XDG_DATA_HOME=str(root / 'data'), CLAUDE_CONFIG_DIR=str(root / 'config'))
    (root / 'config').mkdir()
    (root / 'config' / '.credentials.json').write_text('{"claudeAiOauth":{}}')
    (root / 'config' / '.claude.json').write_text('{"oauthAccount":{"emailAddress":"fake@example.test"}}')
    for mode, keys in [('paste', b'wxyz\r'), ('long', b'w' * 600 + b'\r'), ('backspace', b'wxyQ\x7fz\r'),
                       ('backspace', b'wxyQ\x08z\r'),
                       ('escapes', b'\x1b[200~wxyz\x1b[201~\x1b[A\r'),
                       ('split-escapes', [b'\x1b', b'[20', b'0~', b'wxyz', b'\x1b[', b'201~', b'\x1b', b'[A', b'\x1bO', b'A', b'\x1b', b'\r']),
                       ('lone-escape', b'\x1bwxyz\r'), ('escape-cancel', b'\x1b[\x03'),
                       ('cancel', b'\x03'),
                       ('signal', None), ('sigint', None), ('sighup', None), ('sigquit', None),
                       ('eof', b'\x04'), ('browser', None), ('failure', None), ('spawn-failure', None)]:
        master, slave = pty.openpty()
        before = termios.tcgetattr(slave)
        env['TEST_LOGIN_MODE'] = mode
        if mode == 'spawn-failure':
            fake.chmod(0o644)
            env['PATH'] = directory
        process = subprocess.Popen([binary, 'add', 'fake', '--force'], env=env,
                                   stdin=slave, stdout=slave, stderr=slave,
                                   start_new_session=True)
        transcript = b''
        sent = False
        deadline = time.monotonic() + 8
        try:
            while time.monotonic() < deadline:
                ready, _, _ = select.select([master], [], [], .05)
                if ready:
                    data = os.read(master, 4096)
                    transcript += data
                if b'Paste code here if prompted > ' in transcript and not sent:
                    sent = True
                    if mode in ('signal', 'sigint', 'sighup', 'sigquit'):
                        process.send_signal({'signal':signal.SIGTERM, 'sigint':signal.SIGINT, 'sighup':signal.SIGHUP, 'sigquit':signal.SIGQUIT}[mode])
                    elif isinstance(keys, list):
                        for chunk in keys:
                            os.write(master, chunk)
                            time.sleep(.03)
                    elif keys is not None: os.write(master, keys)
                if process.poll() is not None:
                    while select.select([master], [], [], 0)[0]:
                        transcript += os.read(master, 4096)
                    break
            assert process.poll() is not None, 'login timed out'
            assert termios.tcgetattr(slave) == before, 'terminal was not restored'
            assert b'wxyz' not in transcript and b'wxyQ' not in transcript, 'input leaked'
            if mode in ('paste', 'long', 'backspace', 'escapes', 'split-escapes', 'lone-escape'):
                assert process.returncode == 0, 'fake child did not receive expected input'
                if mode == 'long':
                    assert transcript.count(b'*') == 600, 'long paste was truncated'
                    assert b'wwww' not in transcript, 'long input leaked'
                elif mode in ('paste', 'escapes', 'split-escapes', 'lone-escape'): assert transcript.count(b'*') == 4, 'incorrect mask count'
                else:
                    assert b'****\x08 \x08*' in transcript, 'backspace feedback missing'
                    assert transcript.count(b'*') == 5, 'incorrect edited mask count'
            elif mode == 'browser': assert process.returncode == 0, 'browser completion failed'
            else: assert process.returncode != 0, 'failed login was accepted'
        finally:
            if process.poll() is None: process.kill(); process.wait()
            os.close(master); os.close(slave)
"#)
        .arg(env!("CARGO_BIN_EXE_cs"))
        .output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
