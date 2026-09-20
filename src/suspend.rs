//! Briefly freezes an app's process group so a log can be truncated without
//! losing output.
//!
//! SIGSTOP cannot be caught or ignored, and a stopped process cannot issue
//! writes, so between "every process is stopped" and SIGCONT the log only
//! changes by our own hand. Data still buffered inside the app is frozen with
//! it and is flushed, after the resume, into the truncated file.
//!
//! A marker file records the frozen group for the duration of the pause. If
//! the guard dies before resuming, the next run finds the marker and sends
//! SIGCONT, so an app is never left frozen for longer than a cron interval.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// How long the group gets to come to a stop before we give up and resume it.
const STOP_TIMEOUT: Duration = Duration::from_secs(1);

pub struct Suspend {
    pub process_group: libc::pid_t,
    pub marker: PathBuf,
}

/// The group is stopped while this lives; dropping it resumes the group.
pub struct Frozen<'a> {
    suspend: &'a Suspend,
    since: Instant,
    old_mask: libc::sigset_t,
}

impl Suspend {
    /// Stops the group and waits until every process in it is stopped.
    /// Returns `None`, with the group running again, if that cannot be
    /// confirmed; the caller then rotates without the guarantee.
    pub fn freeze(&self) -> io::Result<Option<Frozen<'_>>> {
        if !SUPPORTED {
            return Ok(None);
        }
        fs::write(&self.marker, format!("{}\n", self.process_group))?;
        // Until the group is resumed, do not die to a polite signal.
        let old_mask = block_signals();
        let frozen = Frozen {
            suspend: self,
            since: Instant::now(),
            old_mask,
        };
        if unsafe { libc::kill(-self.process_group, libc::SIGSTOP) } != 0 {
            let error = io::Error::last_os_error();
            drop(frozen);
            // No such group: nothing is running, so nothing can write.
            return if error.raw_os_error() == Some(libc::ESRCH) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        let deadline = Instant::now() + STOP_TIMEOUT;
        let mut pause = Duration::from_micros(50);
        loop {
            match group_stopped(self.process_group) {
                Ok(true) => return Ok(Some(frozen)),
                Ok(false) if Instant::now() < deadline => {}
                Ok(false) => return Ok(None),
                Err(e) => return Err(e),
            }
            sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(10));
        }
    }
}

impl Frozen<'_> {
    pub fn elapsed(&self) -> Duration {
        self.since.elapsed()
    }
}

impl Drop for Frozen<'_> {
    fn drop(&mut self) {
        unsafe { libc::kill(-self.suspend.process_group, libc::SIGCONT) };
        let _ = fs::remove_file(&self.suspend.marker);
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &self.old_mask, std::ptr::null_mut()) };
    }
}

/// Resumes a group left frozen by a guard that died mid-rotation. Only call
/// while holding the rotation lock, so a live rotation is never resumed early.
/// Returns the resumed group, if there was one.
pub fn recover(marker: &Path) -> io::Result<Option<libc::pid_t>> {
    let text = match fs::read_to_string(marker) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let group = text.trim().parse::<libc::pid_t>().ok().filter(|g| *g > 1);
    if let Some(group) = group {
        unsafe { libc::kill(-group, libc::SIGCONT) };
    }
    fs::remove_file(marker)?;
    Ok(group)
}

fn block_signals() -> libc::sigset_t {
    unsafe {
        let mut all: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigfillset(&mut all);
        libc::pthread_sigmask(libc::SIG_BLOCK, &all, &mut old);
        old
    }
}

const SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "freebsd", target_os = "macos"));

/// True once no process (on Linux: no thread) of the group can still run.
#[cfg(target_os = "linux")]
fn group_stopped(group: libc::pid_t) -> io::Result<bool> {
    // /proc/<pid>/stat: "pid (comm) state ppid pgrp ..."; comm may contain
    // spaces and parentheses, so parse from the last ')'.
    fn state_and_group(stat: &str) -> Option<(char, libc::pid_t)> {
        let rest = &stat[stat.rfind(')')? + 1..];
        let mut fields = rest.split_ascii_whitespace();
        let state = fields.next()?.chars().next()?;
        let group = fields.nth(1)?.parse().ok()?;
        Some((state, group))
    }
    // A process vanishing mid-scan is simply gone.
    let stopped = |state: char| matches!(state, 'T' | 't' | 'Z' | 'X' | 'x');
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        match state_and_group(&stat) {
            Some((_, g)) if g != group => continue,
            None => continue,
            Some(_) => {}
        }
        let Ok(tasks) = fs::read_dir(entry.path().join("task")) else {
            continue;
        };
        for task in tasks {
            let Ok(stat) = fs::read_to_string(task?.path().join("stat")) else {
                continue;
            };
            if let Some((state, _)) = state_and_group(&stat)
                && !stopped(state)
            {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

#[cfg(target_os = "freebsd")]
fn group_stopped(group: libc::pid_t) -> io::Result<bool> {
    // One kinfo_proc per thread of every process in the group.
    let mib = [
        libc::CTL_KERN,
        libc::KERN_PROC,
        libc::KERN_PROC_PGRP | libc::KERN_PROC_INC_THREAD,
        group,
    ];
    let entry = std::mem::size_of::<libc::kinfo_proc>();
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        let mut size: libc::size_t = 0;
        let sized = unsafe {
            libc::sysctl(mib.as_ptr(), 4, std::ptr::null_mut(), &mut size, std::ptr::null(), 0)
        };
        if sized != 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ESRCH) {
                Ok(true)
            } else {
                Err(error)
            };
        }
        buffer.resize(size + 16 * entry, 0);
        let mut size = buffer.len();
        let filled = unsafe {
            libc::sysctl(
                mib.as_ptr(),
                4,
                buffer.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null(),
                0,
            )
        };
        if filled == 0 {
            buffer.truncate(size);
            break;
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ENOMEM) => continue,
            Some(libc::ESRCH) => return Ok(true),
            _ => return Err(error),
        }
    }
    for chunk in buffer.chunks_exact(entry) {
        let info: libc::kinfo_proc = unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast()) };
        if info.ki_stat != libc::SSTOP && info.ki_stat != libc::SZOMB {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(target_os = "macos")]
fn group_stopped(group: libc::pid_t) -> io::Result<bool> {
    const SSTOP: u32 = 4;
    const SZOMB: u32 = 5;
    let mut pids = vec![0 as libc::pid_t; 256];
    let count = loop {
        let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        let count = unsafe { libc::proc_listpgrppids(group, pids.as_mut_ptr().cast(), bytes) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        if (count as usize) < pids.len() {
            break count as usize;
        }
        pids.resize(pids.len() * 2, 0);
    };
    for &pid in &pids[..count] {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let got = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
        };
        // A process that vanished mid-scan is simply gone.
        if got == size && info.pbi_status != SSTOP && info.pbi_status != SZOMB {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "macos")))]
fn group_stopped(_group: libc::pid_t) -> io::Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    struct Group(std::process::Child);

    impl Group {
        fn spawn() -> Self {
            Self(
                Command::new("sleep")
                    .arg("30")
                    .process_group(0)
                    .spawn()
                    .expect("spawn sleep"),
            )
        }

        fn id(&self) -> libc::pid_t {
            self.0.id() as libc::pid_t
        }
    }

    impl Drop for Group {
        fn drop(&mut self) {
            unsafe { libc::kill(-self.id(), libc::SIGKILL) };
            let _ = self.0.wait();
        }
    }

    fn marker(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("processguard-{name}-{}", std::process::id()))
    }

    #[test]
    fn freeze_stops_the_group_and_drop_resumes_it() {
        let group = Group::spawn();
        let suspend = Suspend {
            process_group: group.id(),
            marker: marker("freeze"),
        };

        let frozen = suspend.freeze().expect("freeze").expect("group stops");
        assert!(group_stopped(group.id()).expect("state"));
        assert!(suspend.marker.exists());

        drop(frozen);
        assert!(!suspend.marker.exists());
        assert!(!group_stopped(group.id()).expect("state"));
    }

    #[test]
    fn recover_resumes_a_group_left_stopped() {
        let group = Group::spawn();
        let marker = marker("recover");
        unsafe { libc::kill(-group.id(), libc::SIGSTOP) };
        while !group_stopped(group.id()).expect("state") {
            sleep(Duration::from_millis(1));
        }
        fs::write(&marker, format!("{}\n", group.id())).expect("write marker");

        assert_eq!(recover(&marker).expect("recover"), Some(group.id()));
        assert!(!marker.exists());
        assert!(!group_stopped(group.id()).expect("state"));
        assert_eq!(recover(&marker).expect("recover"), None);
    }

    #[test]
    fn freezing_a_missing_group_is_not_an_error() {
        let group = Group::spawn();
        let id = group.id();
        drop(group);
        let suspend = Suspend {
            process_group: id,
            marker: marker("missing"),
        };

        assert!(suspend.freeze().expect("freeze").is_none());
        assert!(!suspend.marker.exists());
    }
}
