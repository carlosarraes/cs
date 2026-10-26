use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub start: String,
}
impl Process {
    pub fn current() -> Result<Self> {
        Self::read(std::process::id()).context("reading process identity")
    }
    pub fn read(pid: u32) -> Option<Self> {
        process_start(pid).map(|start| Self { pid, start })
    }
    pub fn alive(&self) -> bool {
        Self::read(self.pid).as_ref() == Some(self)
    }
}
#[cfg(target_os = "linux")]
fn process_start(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<_> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    if fields.first() == Some(&"Z") {
        return None;
    }
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    Some(format!("{}:{}", boot.trim(), fields.get(19)?))
}
#[cfg(target_os = "macos")]
fn process_start(pid: u32) -> Option<String> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    // SAFETY: proc_pidinfo fills the supplied, correctly sized native structure.
    let read = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size as i32,
        )
    };
    if read != size as i32 {
        return None;
    }
    // SAFETY: the kernel wrote the full structure above.
    let info = unsafe { info.assume_init() };
    if info.pbi_status == 5 {
        return None;
    }
    Some(format!(
        "{}:{}",
        info.pbi_start_tvsec, info.pbi_start_tvusec
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_reused_and_dead_pids() {
        let mut process = Process::current().unwrap();
        assert!(process.alive());
        process.start.push('x');
        assert!(!process.alive());
        assert!(Process::read(u32::MAX).is_none());
    }
}
