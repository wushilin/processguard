//! processguard: a cron-driven process guard.
//!
//! Invoked periodically (e.g. `* * * * * /home/usera/apps/app1/guard`), it reads
//! `guard.toml` next to itself, and starts the configured process if it is not running
//! (or stops it if it has been disabled).

use std::env;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde::Deserialize;

mod rotate;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    app_name: String,
    commands: Commands,
    #[serde(default)]
    options: Options,
    #[serde(default)]
    logging: Logging,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Commands {
    /// Launches the process. Run detached, nohup-style, and not waited for, so
    /// it may simply run the app in the foreground. Exempt from the timeout.
    start: String,
    /// Exit 0 = running, exit 1 = not running, anything else = error (no action).
    status: String,
    /// Optional. Exit 0 = enabled, anything else = disabled.
    enabled: Option<String>,
    /// Optional. Run when disabled but still running.
    stop: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Options {
    /// Max seconds `enabled`, `status` and `stop` may run before being killed.
    timeout_secs: u64,
    /// Guard's own log file.
    log: String,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            timeout_secs: 30,
            log: "guard.log".to_string(),
        }
    }
}

/// How long the start command is watched for an immediate failure.
const START_GRACE: Duration = Duration::from_secs(1);

/// Where the app's output goes. Nothing is collected unless a file is named.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Logging {
    /// File for the start command's stdout. Default: /dev/null.
    stdout: Option<String>,
    /// File for the start command's stderr; may equal `stdout`. Default: /dev/null.
    stderr: Option<String>,
    /// Rotated generations kept per file (`<file>.1` .. `<file>.<max_keep>`).
    max_keep: u32,
    /// Generations beyond this one are gzipped (`<file>.<n>.gz`).
    compress_after: u32,
    /// A file reaching this size is rotated. Bytes, or a string like "15MiB".
    #[serde(deserialize_with = "deserialize_size")]
    max_size: u64,
}

impl Default for Logging {
    fn default() -> Self {
        Logging {
            stdout: None,
            stderr: None,
            max_keep: 10,
            compress_after: 3,
            max_size: 10 << 20,
        }
    }
}

fn parse_size(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let digits = text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len());
    let number: u64 = text[..digits]
        .parse()
        .map_err(|_| format!("invalid size `{text}`"))?;
    let unit: u64 = match text[digits..].trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kib" => 1 << 10,
        "m" | "mib" => 1 << 20,
        "g" | "gib" => 1 << 30,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        _ => return Err(format!("invalid size `{text}`")),
    };
    number
        .checked_mul(unit)
        .ok_or_else(|| format!("size `{text}` is too large"))
}

fn deserialize_size<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Size {
        Bytes(u64),
        Text(String),
    }
    match Size::deserialize(d)? {
        Size::Bytes(n) => Ok(n),
        Size::Text(s) => parse_size(&s).map_err(serde::de::Error::custom),
    }
}

struct Guard {
    config: Config,
    dir: PathBuf,
}

impl Guard {
    fn log(&self, msg: &str) {
        let line = format!(
            "{} [{}] {}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            self.config.app_name,
            msg
        );
        let written = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.config.options.log)
            .and_then(|mut f| f.write_all(line.as_bytes()));
        if written.is_err() {
            eprint!("{line}");
        }
    }

    /// Base command: `sh -c <cmd>` in the app dir, with the app dir on PATH so
    /// that a bare `run.sh` resolves.
    fn command(&self, cmd: &str) -> Command {
        let mut path = self.dir.clone().into_os_string();
        if let Some(existing) = env::var_os("PATH") {
            path.push(":");
            path.push(existing);
        }
        let mut c = Command::new("/bin/sh");
        c.arg("-c")
            .arg(cmd)
            .current_dir(&self.dir)
            .env("PATH", path)
            .env("GUARD_APP_NAME", &self.config.app_name)
            .stdin(Stdio::null());
        c
    }

    /// Spawns a command in its own session with SIGHUP ignored (like `nohup`),
    /// so it is detached from cron and the whole tree can be killed by group.
    fn spawn(&self, cmd: &str, stdout: Stdio, stderr: Stdio) -> Result<Child, String> {
        let mut command = self.command(cmd);
        command.stdout(stdout).stderr(stderr);
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::signal(libc::SIGHUP, libc::SIG_IGN);
                Ok(())
            });
        }
        command
            .spawn()
            .map_err(|e| format!("cannot run `{cmd}`: {e}"))
    }

    /// Runs a command to completion and returns its exit code. If it does not
    /// exit within the timeout, its whole process tree is killed.
    fn exec(&self, cmd: &str) -> Result<i32, String> {
        let mut child = self.spawn(cmd, Stdio::null(), Stdio::null())?;
        let timeout = Duration::from_secs(self.config.options.timeout_secs);
        match wait(&mut child, timeout).map_err(|e| format!("waiting for `{cmd}`: {e}"))? {
            Some(status) => status
                .code()
                .ok_or_else(|| format!("`{cmd}` was killed by a signal")),
            None => {
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.wait();
                Err(format!(
                    "`{cmd}` timed out after {}s; killed",
                    self.config.options.timeout_secs
                ))
            }
        }
    }

    /// Launches the start command and leaves it running; it is the guarded
    /// process (or daemonizes it). Only watched briefly to catch instant failures.
    fn start(&self) -> Result<u32, String> {
        let cmd = &self.config.commands.start;
        let (stdout, stderr) = self.app_output()?;
        let mut child = self.spawn(cmd, stdout, stderr)?;
        match wait(&mut child, START_GRACE) {
            Ok(Some(status)) if !status.success() => {
                Err(format!("not running; `{cmd}` failed immediately ({status})"))
            }
            _ => Ok(child.id()),
        }
    }

    /// The app's stdout/stderr: the `[logging]` files opened for append (which
    /// is what lets rotation truncate them in place), or /dev/null.
    fn app_output(&self) -> Result<(Stdio, Stdio), String> {
        let open = |path: &String| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| format!("cannot open {path}: {e}"))
        };
        let logging = &self.config.logging;
        let stdout = logging.stdout.as_ref().map(open).transpose()?;
        let stderr = match (&logging.stderr, &stdout) {
            (Some(path), Some(out)) if Some(path) == logging.stdout.as_ref() => {
                Some(out.try_clone().map_err(|e| e.to_string())?)
            }
            (path, _) => path.as_ref().map(open).transpose()?,
        };
        let stdio = |f: Option<File>| f.map_or_else(Stdio::null, Stdio::from);
        Ok((stdio(stdout), stdio(stderr)))
    }

    /// Rotates the app's output files. Compression may outlast a cron interval,
    /// so this runs under its own lock: later guard runs keep guarding the
    /// process and skip rotation, letting the live file outgrow `max_size`.
    fn rotate_logs(&self) {
        let logging = &self.config.logging;
        let mut files: Vec<&String> = logging.stdout.iter().chain(&logging.stderr).collect();
        files.dedup();
        if files.is_empty() {
            return;
        }
        let _lock = match lock(&self.dir, ".guard.rotate.lock") {
            Ok(Some(f)) => f,
            Ok(None) => return,
            Err(e) => return self.log(&format!("error: {e}")),
        };
        let policy = rotate::Policy {
            max_size: logging.max_size,
            max_keep: logging.max_keep,
            compress_after: logging.compress_after,
        };
        for file in files {
            match rotate::rotate(Path::new(file), &policy) {
                Ok(Some(size)) => self.log(&format!("rotated {file} ({size} bytes)")),
                Ok(None) => {}
                Err(e) => self.log(&format!("error: rotating {file}: {e}")),
            }
        }
    }

    /// Disabled: stop the process if a stop command is configured and it is running.
    fn stop_if_running(&self) -> Result<(), String> {
        let Some(stop) = &self.config.commands.stop else {
            return Ok(());
        };
        if self.exec(&self.config.commands.status)? != 0 {
            return Ok(());
        }
        match self.exec(stop)? {
            0 => {
                self.log(&format!("disabled but running; stopped with `{stop}`"));
                Ok(())
            }
            code => Err(format!("disabled but running; `{stop}` exited with {code}")),
        }
    }

    fn run(&self) -> Result<(), String> {
        if let Some(enabled) = &self.config.commands.enabled {
            if self.exec(enabled)? != 0 {
                return self.stop_if_running();
            }
        }
        match self.exec(&self.config.commands.status)? {
            0 => Ok(()),
            1 => {
                let pid = self.start()?;
                self.log(&format!(
                    "not running; started `{}` (pid {pid})",
                    self.config.commands.start
                ));
                Ok(())
            }
            code => Err(format!(
                "status command exited with {code} (expected 0 or 1); not starting"
            )),
        }
    }
}

/// Waits up to `timeout` for the child to exit. None means still running.
fn wait(child: &mut Child, timeout: Duration) -> std::io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(status) => return Ok(Some(status)),
            None if Instant::now() >= deadline => return Ok(None),
            None => sleep(Duration::from_millis(50)),
        }
    }
}

/// Config path: first argument if given, otherwise `guard.toml` next to the
/// binary as invoked (argv[0], so per-app symlinks to one binary work).
fn config_path() -> PathBuf {
    if let Some(arg) = env::args_os().nth(1) {
        return PathBuf::from(arg);
    }
    let argv0 = env::args_os().next().map(PathBuf::from).unwrap_or_default();
    match argv0.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join("guard.toml"),
        _ => PathBuf::from("guard.toml"),
    }
}

/// Takes an exclusive lock so overlapping cron runs never do the same work
/// twice. Returns None if another guard instance holds it.
fn lock(dir: &Path, name: &str) -> Result<Option<File>, String> {
    let path = dir.join(name);
    let file = File::create(&path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(format!("cannot lock {}: {e}", path.display())),
    }
}

fn main() -> ExitCode {
    let path = config_path();
    let setup = || -> Result<Guard, String> {
        let path = path
            .canonicalize()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let config: Config = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let dir = path.parent().unwrap_or(Path::new("/")).to_path_buf();
        env::set_current_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        Ok(Guard { config, dir })
    };
    let guard = match setup() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("guard: {e}");
            return ExitCode::from(2);
        }
    };

    // Held until exit. Opened O_CLOEXEC, so the started process does not inherit it.
    let lock = match lock(&guard.dir, ".guard.lock") {
        Ok(Some(f)) => f,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            guard.log(&format!("error: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let result = guard.run();
    drop(lock);
    guard.rotate_logs();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            guard.log(&format!("error: {e}"));
            ExitCode::FAILURE
        }
    }
}
