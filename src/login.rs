//! Keep Claude's browser login and stdin protocol intact, with masked terminal input.
use anyhow::{anyhow, Context, Result};
use std::{
    fs::File,
    io::{self, IsTerminal, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicI32, Ordering},
};

static INTERRUPTED: AtomicI32 = AtomicI32::new(0);
extern "C" fn interrupted(signal: libc::c_int) {
    INTERRUPTED.store(signal, Ordering::Relaxed);
}

struct Signals(Vec<(libc::c_int, libc::sigaction)>);
impl Signals {
    fn install() -> io::Result<Self> {
        INTERRUPTED.store(0, Ordering::Relaxed);
        let mut guard = Self(Vec::new());
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                let mut old = std::mem::zeroed();
                action.sa_sigaction = interrupted as *const () as usize;
                libc::sigemptyset(&mut action.sa_mask);
                if libc::sigaction(signal, &action, &mut old) == -1 {
                    return Err(io::Error::last_os_error());
                }
                guard.0.push((signal, old));
            }
        }
        Ok(guard)
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        for (signal, old) in &self.0 {
            unsafe {
                libc::sigaction(*signal, old, std::ptr::null_mut());
            }
        }
    }
}

struct Terminal(libc::termios);
impl Terminal {
    fn raw() -> io::Result<Self> {
        let original = terminal_settings(0)?;
        let guard = Self(original);
        let mut raw = original;
        unsafe {
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(0, libc::TCSANOW, &raw) == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(guard)
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.0);
        }
    }
}

struct LoginChild(Child);
impl Drop for LoginChild {
    fn drop(&mut self) {
        // The child has its own session. Clean up its listener and descendants
        // on relay errors or cancellation as well as normal completion.
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}

fn terminal_settings(fd: i32) -> io::Result<libc::termios> {
    unsafe {
        let mut settings = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut settings) == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(settings)
    }
}

fn resize(master: i32) {
    unsafe {
        let mut size: libc::winsize = std::mem::zeroed();
        if libc::ioctl(0, libc::TIOCGWINSZ, &mut size) == 0 {
            libc::ioctl(master, libc::TIOCSWINSZ, &size);
        }
    }
}

#[derive(Clone, Copy, Default)]
enum EscapeState {
    #[default]
    None,
    Escape,
    Csi,
    Ss3,
}

impl EscapeState {
    fn consume(&mut self, byte: u8) -> bool {
        match (*self, byte) {
            (_, 0x1b) => *self = Self::Escape,
            (Self::Escape, b'[') => *self = Self::Csi,
            (Self::Escape, b'O') => *self = Self::Ss3,
            (Self::Csi, 0x40..=0x7e) | (Self::Ss3, _) => *self = Self::None,
            (Self::Csi, _) => {}
            _ => {
                *self = Self::None;
                return false;
            }
        }
        true
    }
}

pub fn run() -> Result<()> {
    let mut command = Command::new("claude");
    command.args(["auth", "login"]);
    let status = if io::stdin().is_terminal() && io::stdout().is_terminal() {
        interactive(&mut command)?
    } else {
        eprintln!(
            "If prompted, paste the code and press Enter; pasted characters may not be shown."
        );
        command
            .status()
            .context("failed to run `claude` (is Claude Code installed and on PATH?)")?
    };
    if !status.success() {
        return Err(anyhow!("`claude auth login` did not complete"));
    }
    Ok(())
}

pub fn read_secret(prompt: &str) -> Result<String> {
    let terminal = io::stdin().is_terminal();
    let _signals = if terminal {
        Some(Signals::install()?)
    } else {
        None
    };
    let _terminal = if terminal {
        Some(Terminal::raw()?)
    } else {
        None
    };
    let mut output = io::stderr().lock();
    output.write_all(prompt.as_bytes())?;
    output.flush()?;
    let mut line = Vec::new();
    let mut escape = EscapeState::default();
    loop {
        if terminal && INTERRUPTED.load(Ordering::Relaxed) != 0 {
            return Err(anyhow!("key input interrupted"));
        }
        // Read exactly one byte so a second prompt can consume the same piped stdin.
        let mut byte = [0];
        let n = unsafe { libc::read(0, byte.as_mut_ptr().cast(), 1) };
        if n < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if n == 0 {
            break;
        }
        let byte = byte[0];
        if byte != 3 && escape.consume(byte) {
            continue;
        }
        match byte {
            3 | 4 => return Err(anyhow!("key input cancelled")),
            b'\r' | b'\n' => break,
            8 | 127 if !line.is_empty() => {
                line.pop();
                if terminal {
                    output.write_all(b"\x08 \x08")?;
                }
            }
            32..=126 => {
                if line.len() >= 512 {
                    return Err(anyhow!("key input too long"));
                }
                line.push(byte);
                if terminal {
                    output.write_all(b"*")?;
                }
            }
            _ => {}
        }
        output.flush()?;
    }
    output.write_all(if terminal { b"\r\n" } else { b"\n" })?;
    String::from_utf8(line).context("invalid key input")
}

fn interactive(command: &mut Command) -> Result<ExitStatus> {
    let _signals = Signals::install().context("installing login signal handlers")?;
    let mut settings = terminal_settings(0)?;
    settings.c_lflag &= !(libc::ECHO | libc::ECHONL);
    let (mut master_fd, mut slave_fd) = (-1, -1);
    unsafe {
        if libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(settings),
            std::ptr::null_mut(),
        ) == -1
        {
            return Err(io::Error::last_os_error()).context("opening login terminal");
        }
    }
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    for fd in [master_fd, slave_fd] {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(io::Error::last_os_error())
                .context("protecting login terminal descriptors");
        }
    }
    resize(master_fd);
    command
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let _terminal = Terminal::raw().context("setting login terminal mode")?;
    let mut child = LoginChild(
        command
            .spawn()
            .context("failed to run `claude` (is Claude Code installed and on PATH?)")?,
    );
    // Command retains its configured Stdio handles after spawn. Close the
    // parent's slave copies so the master can report EOF when Claude exits.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut output = io::stdout().lock();
    let mut line = Vec::new();
    let mut escape = EscapeState::default();
    loop {
        if INTERRUPTED.load(Ordering::Relaxed) != 0 {
            return Err(anyhow!("login interrupted"));
        }
        resize(master.as_raw_fd());
        let mut fds = [
            libc::pollfd {
                fd: master_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, 100) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if fds[0].revents != 0 {
            let mut bytes = [0; 4096];
            match master.read(&mut bytes) {
                Ok(0) => return Ok(child.0.wait()?),
                Ok(n) => {
                    output.write_all(&bytes[..n])?;
                    output.flush()?;
                }
                Err(e) if e.raw_os_error() == Some(libc::EIO) => return Ok(child.0.wait()?),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        } else if let Some(status) = child.0.try_wait()? {
            return Ok(status);
        }
        if fds[1].revents != 0 {
            let mut bytes = [0; 512];
            // Rust's Stdin buffers reads beyond this chunk. Poll must observe
            // the same unbuffered descriptor we read, including large pastes.
            let n = unsafe { libc::read(0, bytes.as_mut_ptr().cast(), bytes.len()) };
            if n < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            let n = n as usize;
            if n == 0 {
                return Err(anyhow!("login input closed"));
            }
            for &byte in &bytes[..n] {
                if byte != 3 && escape.consume(byte) {
                    continue;
                }
                match byte {
                    3 => return Err(anyhow!("login cancelled")),
                    4 if line.is_empty() => master.write_all(&[4])?,
                    b'\r' | b'\n' => {
                        // Send only the edited line. Claude's input-only readline
                        // does not echo it; the slave also starts with echo off.
                        master.write_all(&line)?;
                        master.write_all(b"\n")?;
                        line.clear();
                        output.write_all(b"\r\n")?;
                    }
                    8 | 127 if !line.is_empty() => {
                        while line.pop().is_some_and(|b| b & 0xc0 == 0x80) {}
                        output.write_all(b"\x08 \x08")?;
                    }
                    32..=126 | 128..=255 => {
                        if line.len() >= 16384 {
                            return Err(anyhow!("login input too long"));
                        }
                        line.push(byte);
                        if byte & 0xc0 != 0x80 {
                            output.write_all(b"*")?;
                        }
                    }
                    _ => {}
                }
            }
            output.flush()?;
        }
    }
}
