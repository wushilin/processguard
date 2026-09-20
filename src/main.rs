//! A cron-driven, convention-based process supervisor.

use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

mod rotate;
mod suspend;

const FORCE_KILL_WAIT: Duration = Duration::from_secs(5);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    global: GlobalConfig,
    #[serde(rename = "app")]
    app_overrides: Vec<AppOverride>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct GlobalConfig {
    base_dir: PathBuf,
    startup_validation_secs: u64,
    stop_grace_secs: u64,
    timeout_secs: u64,
    guard_log: PathBuf,
    log: LogSettings,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            base_dir: PathBuf::from("apps"),
            startup_validation_secs: 10,
            stop_grace_secs: 30,
            timeout_secs: 30,
            guard_log: PathBuf::from("guard.log"),
            log: LogSettings::default(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LogSettings {
    stdout: PathBuf,
    stderr: PathBuf,
    #[serde(deserialize_with = "deserialize_size")]
    max_size: u64,
    max_keep: u32,
    compress_after: u32,
    compression: rotate::CompressionMethod,
    rotation: rotate::RotationMethod,
}

impl Default for LogSettings {
    fn default() -> Self {
        Self {
            stdout: PathBuf::from("logs/stdout.log"),
            stderr: PathBuf::from("logs/stderr.log"),
            max_size: 15 << 20,
            max_keep: 10,
            compress_after: 3,
            compression: rotate::CompressionMethod::Gzip,
            rotation: rotate::RotationMethod::CopyTruncate,
        }
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LogOverrides {
    stdout: Option<PathBuf>,
    stderr: Option<PathBuf>,
    #[serde(deserialize_with = "deserialize_optional_size")]
    max_size: Option<u64>,
    max_keep: Option<u32>,
    compress_after: Option<u32>,
    compression: Option<rotate::CompressionMethod>,
    rotation: Option<rotate::RotationMethod>,
}

impl LogOverrides {
    fn apply_to(&self, settings: &mut LogSettings) {
        if let Some(value) = &self.stdout {
            settings.stdout = value.clone();
        }
        if let Some(value) = &self.stderr {
            settings.stderr = value.clone();
        }
        if let Some(value) = self.max_size {
            settings.max_size = value;
        }
        if let Some(value) = self.max_keep {
            settings.max_keep = value;
        }
        if let Some(value) = self.compress_after {
            settings.compress_after = value;
        }
        if let Some(value) = self.compression {
            settings.compression = value;
        }
        if let Some(value) = self.rotation {
            settings.rotation = value;
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct AppOverride {
    name: String,
    #[serde(default = "default_true")]
    managed: bool,
    start: Option<String>,
    pid_file: Option<String>,
    disable_file: Option<String>,
    stop: Option<String>,
    signal_file: Option<String>,
    healthcheck: Option<String>,
    startup_validation_secs: Option<u64>,
    stop_grace_secs: Option<u64>,
    timeout_secs: Option<u64>,
    #[serde(default)]
    log: LogOverrides,
}

fn default_true() -> bool {
    true
}

#[derive(Clone)]
struct AppSpec {
    name: String,
    dir: PathBuf,
    managed: bool,
    start: String,
    pid_file: String,
    disable_file: String,
    stop: String,
    signal_file: String,
    healthcheck: Option<String>,
    startup_validation: Duration,
    stop_grace: Duration,
    command_timeout: Duration,
    log_override: LogOverrides,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommandOutcome {
    Success,
    Negative,
    Unexpected(i32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
enum ExpectedExitCode {
    Success = 0,
    Negative = 1,
}

impl CommandOutcome {
    fn from_status(status: ExitStatus) -> Result<Self, String> {
        let code = status
            .code()
            .ok_or_else(|| format!("command was killed by signal {:?}", status.signal()))?;
        if code == ExpectedExitCode::Success as i32 {
            Ok(Self::Success)
        } else if code == ExpectedExitCode::Negative as i32 {
            Ok(Self::Negative)
        } else {
            Ok(Self::Unexpected(code))
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessState {
    Stopped,
    Running,
    OrphanedProcessGroup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProbeResult {
    Exists,
    Missing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AppCheckResult {
    Idle,
    Started,
    Stopped,
    Restarted,
    Quarantined,
    SkippedInvalid,
    SkippedUnmanaged,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SupervisionResult {
    Success,
    Failure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StopSignal {
    Terminate,
    Interrupt,
    Hangup,
    Quit,
    Kill,
}

impl StopSignal {
    fn parse(text: &str) -> Result<Self, String> {
        let normalized = text.trim().to_ascii_uppercase();
        match normalized.trim_start_matches("SIG") {
            "TERM" => Ok(Self::Terminate),
            "INT" => Ok(Self::Interrupt),
            "HUP" => Ok(Self::Hangup),
            "QUIT" => Ok(Self::Quit),
            "KILL" => Ok(Self::Kill),
            value => Err(format!("unsupported stop signal {value}")),
        }
    }

    fn as_raw(self) -> libc::c_int {
        match self {
            Self::Terminate => libc::SIGTERM,
            Self::Interrupt => libc::SIGINT,
            Self::Hangup => libc::SIGHUP,
            Self::Quit => libc::SIGQUIT,
            Self::Kill => libc::SIGKILL,
        }
    }

    fn is_forceful(self) -> bool {
        self == Self::Kill
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PidRecord {
    pid: libc::pid_t,
    process_group: libc::pid_t,
}

#[derive(Serialize)]
struct InvalidRecord<'a> {
    detected_at: String,
    reason: &'a str,
    detail: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    observed_for_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<libc::pid_t>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signal: Option<i32>,
}

struct Supervisor<'a> {
    global: &'a GlobalConfig,
    app: &'a AppSpec,
    guard_log: &'a Path,
}

impl Supervisor<'_> {
    fn path(&self, configured: &str) -> PathBuf {
        resolve_from(&self.app.dir, Path::new(configured))
    }

    fn log(&self, message: &str) {
        write_log(self.guard_log, &self.app.name, message);
    }

    fn invalid_path(&self) -> PathBuf {
        self.app.dir.join("invalid")
    }

    fn load_log_settings(&self) -> Result<LogSettings, String> {
        let mut settings = self.global.log.clone();
        self.app.log_override.apply_to(&mut settings);
        let path = self.app.dir.join("log.conf");
        match fs::read_to_string(&path) {
            Ok(text) => {
                let overrides: LogOverrides =
                    toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
                overrides.apply_to(&mut settings);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
        Ok(settings)
    }

    fn validate_app_files(&self) -> Result<(), String> {
        let start = self.path(&self.app.start);
        let metadata = fs::metadata(&start)
            .map_err(|e| format!("cannot inspect start program {}: {e}", start.display()))?;
        if !metadata.is_file() {
            return Err(format!("start program {} is not a file", start.display()));
        }
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!(
                "start program {} is not executable",
                start.display()
            ));
        }
        Ok(())
    }

    fn shell_command(&self, command: &str) -> Command {
        let mut path = self.app.dir.as_os_str().to_os_string();
        if let Some(existing) = env::var_os("PATH") {
            path.push(":");
            path.push(existing);
        }
        let mut child = Command::new("/bin/sh");
        child
            .arg("-c")
            .arg(command)
            .current_dir(&self.app.dir)
            .env("PATH", path)
            .env("GUARD_APP_NAME", &self.app.name)
            .stdin(Stdio::null());
        child
    }

    fn prepare_session(command: &mut Command) {
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::signal(libc::SIGHUP, libc::SIG_IGN);
                Ok(())
            });
        }
    }

    fn exec(&self, command: &str) -> Result<CommandOutcome, String> {
        let mut process = self.shell_command(command);
        process.stdout(Stdio::null()).stderr(Stdio::null());
        Self::prepare_session(&mut process);
        let mut child = process
            .spawn()
            .map_err(|e| format!("cannot run {command}: {e}"))?;
        match wait(&mut child, self.app.command_timeout)
            .map_err(|e| format!("waiting for {command}: {e}"))?
        {
            Some(status) => CommandOutcome::from_status(status),
            None => {
                let _ = signal_process_group(child.id() as libc::pid_t, libc::SIGKILL);
                let _ = child.wait();
                Err(format!(
                    "{command} timed out after {}s; killed",
                    self.app.command_timeout.as_secs()
                ))
            }
        }
    }

    fn pid_record(&self) -> Result<Option<PidRecord>, String> {
        let path = self.path(&self.app.pid_file);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        if let Ok(pid) = text.trim().parse::<libc::pid_t>()
            && pid > 0
        {
            return Ok(Some(PidRecord {
                pid,
                process_group: pid,
            }));
        }
        let record: PidRecord = toml::from_str(&text)
            .map_err(|e| format!("invalid pid file {}: {e}", path.display()))?;
        if record.pid <= 0 || record.process_group <= 0 {
            return Err(format!("invalid pid values in {}", path.display()));
        }
        Ok(Some(record))
    }

    fn process_state(&self, record: PidRecord) -> Result<ProcessState, String> {
        let leader = probe_process(record.pid)?;
        let group = probe_process(-record.process_group)?;
        match (leader, group) {
            (ProbeResult::Missing, ProbeResult::Missing) => Ok(ProcessState::Stopped),
            (ProbeResult::Exists, ProbeResult::Exists) => Ok(ProcessState::Running),
            (ProbeResult::Missing, ProbeResult::Exists) => Ok(ProcessState::OrphanedProcessGroup),
            (ProbeResult::Exists, ProbeResult::Missing) => Err(format!(
                "pid {} exists outside expected process group {}",
                record.pid, record.process_group
            )),
        }
    }

    fn current_state(&self) -> Result<(ProcessState, Option<PidRecord>), String> {
        let Some(record) = self.pid_record()? else {
            return Ok((ProcessState::Stopped, None));
        };
        Ok((self.process_state(record)?, Some(record)))
    }

    fn write_pid_record(&self, record: PidRecord) -> Result<(), String> {
        let text = toml::to_string(&record).map_err(|e| e.to_string())?;
        atomic_write(&self.path(&self.app.pid_file), text.as_bytes())
    }

    fn remove_pid_file(&self) -> Result<(), String> {
        let path = self.path(&self.app.pid_file);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
        }
    }

    fn quarantine(
        &self,
        reason: &str,
        detail: &str,
        observed_for: Option<Duration>,
        pid: Option<libc::pid_t>,
        status: Option<ExitStatus>,
    ) -> AppCheckResult {
        let path = self.invalid_path();
        match path.try_exists() {
            Ok(true) => return AppCheckResult::SkippedInvalid,
            Ok(false) => {}
            Err(e) => {
                self.log(&format!("error: cannot check {}: {e}", path.display()));
                return AppCheckResult::Failed;
            }
        }
        let record = InvalidRecord {
            detected_at: chrono::Local::now().to_rfc3339(),
            reason,
            detail,
            observed_for_ms: observed_for
                .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)),
            pid,
            exit_code: status.and_then(|value| value.code()),
            signal: status.and_then(|value| value.signal()),
        };
        let text = match toml::to_string_pretty(&record) {
            Ok(text) => text,
            Err(e) => {
                self.log(&format!("error: cannot encode invalid marker: {e}"));
                return AppCheckResult::Failed;
            }
        };
        if let Err(e) = atomic_write(&path, text.as_bytes()) {
            self.log(&format!("error: {e}"));
            return AppCheckResult::Failed;
        }
        self.log(&format!("marked invalid: {detail}"));
        AppCheckResult::Quarantined
    }

    fn output_files(&self, settings: &LogSettings) -> Result<(Stdio, Stdio), String> {
        let stdout_path = resolve_from(&self.app.dir, &settings.stdout);
        let stderr_path = resolve_from(&self.app.dir, &settings.stderr);
        ensure_parent(&stdout_path)?;
        ensure_parent(&stderr_path)?;
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stdout_path)
            .map_err(|e| format!("cannot open {}: {e}", stdout_path.display()))?;
        let stderr = if stdout_path == stderr_path {
            stdout.try_clone().map_err(|e| e.to_string())?
        } else {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&stderr_path)
                .map_err(|e| format!("cannot open {}: {e}", stderr_path.display()))?
        };
        Ok((Stdio::from(stdout), Stdio::from(stderr)))
    }

    fn start(&self, settings: &LogSettings) -> AppCheckResult {
        let program = self.path(&self.app.start);
        let (stdout, stderr) = match self.output_files(settings) {
            Ok(output) => output,
            Err(e) => return self.quarantine("log_open_failed", &e, None, None, None),
        };
        let mut command = Command::new(&program);
        command
            .current_dir(&self.app.dir)
            .env("GUARD_APP_NAME", &self.app.name)
            .env("GUARD_PID_FILE", self.path(&self.app.pid_file))
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        Self::prepare_session(&mut command);
        let started_at = Instant::now();
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                return self.quarantine(
                    "startup_spawn_failed",
                    &format!("cannot start {}: {e}", program.display()),
                    None,
                    None,
                    None,
                );
            }
        };
        let pid = child.id() as libc::pid_t;
        let record = PidRecord {
            pid,
            process_group: pid,
        };
        if let Err(e) = self.write_pid_record(record) {
            let _ = signal_process_group(pid, libc::SIGKILL);
            let _ = child.wait();
            return self.quarantine("pid_file_write_failed", &e, None, Some(pid), None);
        }
        match wait(&mut child, self.app.startup_validation) {
            Ok(None) => {
                self.log(&format!(
                    "started {} (pid {pid}); survived {}s validation",
                    program.display(),
                    self.app.startup_validation.as_secs()
                ));
                AppCheckResult::Started
            }
            Ok(Some(status)) => {
                let _ = signal_process_group(pid, libc::SIGKILL);
                let _ = self.remove_pid_file();
                self.quarantine(
                    "startup_exited_early",
                    &format!(
                        "{} exited before startup validation completed ({status})",
                        program.display()
                    ),
                    Some(started_at.elapsed()),
                    Some(pid),
                    Some(status),
                )
            }
            Err(e) => {
                let _ = signal_process_group(pid, libc::SIGKILL);
                let _ = child.wait();
                let _ = self.remove_pid_file();
                self.quarantine(
                    "startup_observation_failed",
                    &format!("cannot observe {}: {e}", program.display()),
                    Some(started_at.elapsed()),
                    Some(pid),
                    None,
                )
            }
        }
    }

    fn configured_stop_signal(&self) -> Result<StopSignal, String> {
        let path = self.path(&self.app.signal_file);
        match fs::read_to_string(&path) {
            Ok(text) => StopSignal::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StopSignal::Terminate),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    fn all_processes_gone(&self, record: PidRecord) -> Result<bool, String> {
        Ok(probe_process(record.pid)? == ProbeResult::Missing
            && probe_process(-record.process_group)? == ProbeResult::Missing)
    }

    fn wait_until_gone(&self, record: PidRecord, timeout: Duration) -> Result<bool, String> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| "stop timeout is too large".to_string())?;
        loop {
            if self.all_processes_gone(record)? {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            sleep(PROCESS_POLL_INTERVAL);
        }
    }

    fn stop(&self, record: PidRecord, reason: &str) -> Result<AppCheckResult, String> {
        let stop_path = self.path(&self.app.stop);
        let stop_exists = stop_path
            .try_exists()
            .map_err(|e| format!("cannot check {}: {e}", stop_path.display()))?;
        let mut force_kill_sent = false;
        let mut stop_command_succeeded = false;
        if stop_exists {
            match self.exec(&self.app.stop) {
                Ok(CommandOutcome::Success) => stop_command_succeeded = true,
                Ok(outcome) => self.log(&format!(
                    "warning: stop command returned {outcome:?}; continuing shutdown"
                )),
                Err(e) => self.log(&format!(
                    "warning: stop command failed: {e}; continuing shutdown"
                )),
            }
        }
        if !stop_exists || !stop_command_succeeded {
            let signal = self.configured_stop_signal()?;
            if signal.is_forceful() {
                signal_process_group(record.process_group, signal.as_raw())?;
                force_kill_sent = true;
            } else {
                signal_process(record.pid, signal.as_raw())?;
            }
        }

        let graceful_wait = if force_kill_sent {
            FORCE_KILL_WAIT
        } else {
            self.app.stop_grace
        };
        if !self.wait_until_gone(record, graceful_wait)? {
            if force_kill_sent {
                return Err(format!(
                    "process group {} survived SIGKILL",
                    record.process_group
                ));
            }
            self.log(&format!(
                "{reason}; process group {} survived {}s; sending SIGKILL",
                record.process_group,
                self.app.stop_grace.as_secs()
            ));
            signal_process_group(record.process_group, libc::SIGKILL)?;
            if !self.wait_until_gone(record, FORCE_KILL_WAIT)? {
                return Err(format!(
                    "process group {} survived SIGKILL",
                    record.process_group
                ));
            }
        }
        self.remove_pid_file()?;
        self.log(&format!("{reason}; stopped pid {}", record.pid));
        Ok(AppCheckResult::Stopped)
    }

    fn disabled(&self) -> Result<bool, String> {
        self.path(&self.app.disable_file)
            .try_exists()
            .map_err(|e| format!("cannot check disable file: {e}"))
    }

    fn check(&self) -> AppCheckResult {
        if !self.app.managed {
            return AppCheckResult::SkippedUnmanaged;
        }
        match self.invalid_path().try_exists() {
            Ok(true) => return AppCheckResult::SkippedInvalid,
            Ok(false) => {}
            Err(e) => {
                self.log(&format!("error: cannot check invalid marker: {e}"));
                return AppCheckResult::Failed;
            }
        }
        if let Err(e) = self.validate_app_files() {
            return self.quarantine("invalid_start_program", &e, None, None, None);
        }
        let settings = match self.load_log_settings() {
            Ok(settings) => settings,
            Err(e) => return self.quarantine("invalid_log_configuration", &e, None, None, None),
        };
        let (state, record) = match self.current_state() {
            Ok(state) => state,
            Err(e) => return self.quarantine("invalid_process_state", &e, None, None, None),
        };
        let disabled = match self.disabled() {
            Ok(disabled) => disabled,
            Err(e) => {
                self.log(&format!("error: {e}"));
                return AppCheckResult::Failed;
            }
        };

        if disabled {
            return match (state, record) {
                (ProcessState::Stopped, _) => {
                    if let Err(e) = self.remove_pid_file() {
                        self.log(&format!("error: {e}"));
                        AppCheckResult::Failed
                    } else {
                        AppCheckResult::Idle
                    }
                }
                (_, Some(record)) => match self.stop(record, "disabled") {
                    Ok(result) => result,
                    Err(e) => {
                        self.log(&format!("error: {e}"));
                        AppCheckResult::Failed
                    }
                },
                (_, None) => AppCheckResult::Failed,
            };
        }

        match (state, record) {
            (ProcessState::Stopped, _) => {
                if let Err(e) = self.remove_pid_file() {
                    self.log(&format!("error: {e}"));
                    AppCheckResult::Failed
                } else {
                    self.start(&settings)
                }
            }
            (ProcessState::OrphanedProcessGroup, Some(record)) => {
                let _ = signal_process_group(record.process_group, libc::SIGKILL);
                let _ = self.remove_pid_file();
                self.quarantine(
                    "orphaned_process_group",
                    "application leader exited while child processes remained",
                    None,
                    Some(record.pid),
                    None,
                )
            }
            (ProcessState::Running, Some(record)) => {
                let Some(healthcheck) = &self.app.healthcheck else {
                    return AppCheckResult::Idle;
                };
                match self.exec(healthcheck) {
                    Ok(CommandOutcome::Success) => AppCheckResult::Idle,
                    Ok(CommandOutcome::Negative) => match self.stop(record, "unhealthy") {
                        Ok(_) => match self.start(&settings) {
                            AppCheckResult::Started => AppCheckResult::Restarted,
                            result => result,
                        },
                        Err(e) => {
                            self.log(&format!("error: {e}"));
                            AppCheckResult::Failed
                        }
                    },
                    Ok(CommandOutcome::Unexpected(code)) => {
                        self.log(&format!(
                            "error: healthcheck exited with {code}; not restarting"
                        ));
                        AppCheckResult::Failed
                    }
                    Err(e) => {
                        self.log(&format!("error: healthcheck: {e}"));
                        AppCheckResult::Failed
                    }
                }
            }
            _ => AppCheckResult::Failed,
        }
    }

    fn suspended_path(&self) -> PathBuf {
        self.app.dir.join("suspended")
    }

    /// The writers to stop while truncating: the running app's process group.
    fn suspend_target(&self, settings: &LogSettings) -> Option<suspend::Suspend> {
        if settings.rotation != rotate::RotationMethod::Suspend {
            return None;
        }
        match self.current_state() {
            Ok((ProcessState::Running, Some(record))) => Some(suspend::Suspend {
                process_group: record.process_group,
                marker: self.suspended_path(),
            }),
            _ => None,
        }
    }

    /// Must run under the rotation lock, which the caller holds.
    fn rotate_logs(&self) {
        // A guard that died mid-rotation may have left the app stopped.
        match suspend::recover(&self.suspended_path()) {
            Ok(Some(group)) => self.log(&format!(
                "resumed process group {group} left suspended by an interrupted rotation"
            )),
            Ok(None) => {}
            Err(e) => self.log(&format!("error: checking suspended marker: {e}")),
        }
        if !self.app.managed {
            return;
        }
        match self.invalid_path().try_exists() {
            Ok(true) => return,
            Ok(false) => {}
            Err(e) => return self.log(&format!("error: checking invalid marker: {e}")),
        }
        let settings = match self.load_log_settings() {
            Ok(settings) => settings,
            Err(e) => return self.log(&format!("error: rotating logs: {e}")),
        };
        let stdout = resolve_from(&self.app.dir, &settings.stdout);
        let stderr = resolve_from(&self.app.dir, &settings.stderr);
        let mut files = vec![stdout];
        if stderr != files[0] {
            files.push(stderr);
        }
        let policy = rotate::Policy {
            max_size: settings.max_size,
            max_keep: settings.max_keep,
            compress_after: settings.compress_after,
            compression: settings.compression,
        };
        let suspend = self.suspend_target(&settings);
        for file in files {
            match rotate::rotate(&file, &policy, suspend.as_ref()) {
                Ok(Some(rotated)) => {
                    let how = match rotated.truncation {
                        rotate::Truncation::Plain => String::new(),
                        rotate::Truncation::Suspended(pause) => {
                            format!(", app suspended for {:.2}ms", pause.as_secs_f64() * 1e3)
                        }
                        rotate::Truncation::SuspendFailed => {
                            ", could not suspend the app: truncated without it".to_string()
                        }
                    };
                    self.log(&format!(
                        "rotated {} ({} bytes{how})",
                        file.display(),
                        rotated.size
                    ));
                }
                Ok(None) => {}
                Err(e) => self.log(&format!("error: rotating {}: {e}", file.display())),
            }
        }
    }
}

fn parse_size(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let digits = text
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(text.len());
    let number: u64 = text[..digits]
        .parse()
        .map_err(|_| format!("invalid size {text}"))?;
    let unit: u64 = match text[digits..].trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kib" => 1 << 10,
        "m" | "mib" => 1 << 20,
        "g" | "gib" => 1 << 30,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        _ => return Err(format!("invalid size {text}")),
    };
    number
        .checked_mul(unit)
        .ok_or_else(|| format!("size {text} is too large"))
}

fn deserialize_size<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Size {
        Bytes(u64),
        Text(String),
    }
    match Size::deserialize(deserializer)? {
        Size::Bytes(value) => Ok(value),
        Size::Text(value) => parse_size(&value).map_err(serde::de::Error::custom),
    }
}

fn deserialize_optional_size<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Size {
        Bytes(u64),
        Text(String),
    }
    Option::<Size>::deserialize(deserializer)?.map_or(Ok(None), |size| match size {
        Size::Bytes(value) => Ok(Some(value)),
        Size::Text(value) => parse_size(&value)
            .map(Some)
            .map_err(serde::de::Error::custom),
    })
}

fn resolve_from(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn ensure_parent(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    Ok(())
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), String> {
    ensure_parent(path)?;
    let mut temporary_name: OsString = path.as_os_str().to_os_string();
    temporary_name.push(format!(".tmp.{}", std::process::id()));
    let temporary = PathBuf::from(temporary_name);
    let mut file = File::create(&temporary)
        .map_err(|e| format!("cannot create {}: {e}", temporary.display()))?;
    file.write_all(contents)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("cannot write {}: {e}", temporary.display()))?;
    fs::rename(&temporary, path).map_err(|e| {
        format!(
            "cannot rename {} to {}: {e}",
            temporary.display(),
            path.display()
        )
    })
}

fn wait(child: &mut Child, timeout: Duration) -> std::io::Result<Option<ExitStatus>> {
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "timeout is too large")
    })?;
    let mut pause = Duration::from_millis(1);
    loop {
        match child.try_wait()? {
            Some(status) => return Ok(Some(status)),
            None if Instant::now() >= deadline => return Ok(None),
            None => sleep(pause),
        }
        pause = (pause * 2).min(Duration::from_millis(50));
    }
}

fn probe_process(target: libc::pid_t) -> Result<ProbeResult, String> {
    const PROCESS_PROBE_SIGNAL: libc::c_int = 0;
    const SYSCALL_SUCCESS: libc::c_int = 0;
    if unsafe { libc::kill(target, PROCESS_PROBE_SIGNAL) } == SYSCALL_SUCCESS {
        return Ok(ProbeResult::Exists);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(ProbeResult::Missing),
        Some(libc::EPERM) => Ok(ProbeResult::Exists),
        _ => Err(format!("cannot probe process target {target}: {error}")),
    }
}

fn signal_process(pid: libc::pid_t, signal: libc::c_int) -> Result<(), String> {
    signal_target(pid, signal)
}

fn signal_process_group(group: libc::pid_t, signal: libc::c_int) -> Result<(), String> {
    signal_target(-group, signal)
}

fn signal_target(target: libc::pid_t, signal: libc::c_int) -> Result<(), String> {
    const SYSCALL_SUCCESS: libc::c_int = 0;
    if unsafe { libc::kill(target, signal) } == SYSCALL_SUCCESS {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(format!("cannot signal process target {target}: {error}"))
    }
}

fn write_log(path: &Path, name: &str, message: &str) {
    let line = format!(
        "{} [{name}] {message}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
    let written = ensure_parent(path).and_then(|_| {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| e.to_string())
            .and_then(|mut file| file.write_all(line.as_bytes()).map_err(|e| e.to_string()))
    });
    if written.is_err() {
        eprint!("{line}");
    }
}

fn valid_app_name(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

fn discover_apps(config: &Config, config_dir: &Path) -> Result<Vec<AppSpec>, String> {
    let configured_base = resolve_from(config_dir, &config.global.base_dir);
    let base = configured_base
        .canonicalize()
        .map_err(|e| format!("{}: {e}", configured_base.display()))?;
    if !base.is_dir() {
        return Err(format!("{} is not a directory", base.display()));
    }

    let mut overrides = HashMap::new();
    for app_override in &config.app_overrides {
        if !valid_app_name(&app_override.name) || app_override.name.starts_with('.') {
            return Err(format!("invalid app name {}", app_override.name));
        }
        if overrides
            .insert(app_override.name.clone(), app_override.clone())
            .is_some()
        {
            return Err(format!("duplicate app override {}", app_override.name));
        }
    }

    let mut discovered = Vec::new();
    let mut discovered_paths = HashMap::new();
    let entries = fs::read_dir(&base).map_err(|e| format!("{}: {e}", base.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", base.display()))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| format!("{} contains a non-UTF-8 name", base.display()))?;
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let metadata =
            fs::metadata(&path).map_err(|e| format!("cannot inspect {}: {e}", path.display()))?;
        if !metadata.is_dir() {
            continue;
        }
        let canonical = path
            .canonicalize()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if !canonical.starts_with(&base) {
            return Err(format!(
                "app directory {} resolves outside {}",
                path.display(),
                base.display()
            ));
        }
        if let Some(existing_name) = discovered_paths.insert(canonical.clone(), name.clone()) {
            return Err(format!(
                "app directories {existing_name} and {name} resolve to the same location"
            ));
        }
        let app_override = overrides.remove(&name);
        discovered.push(resolve_app(&config.global, name, canonical, app_override));
    }
    if let Some((name, _)) = overrides.into_iter().next() {
        return Err(format!("app override {name} has no matching directory"));
    }
    discovered.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(discovered)
}

fn resolve_app(
    global: &GlobalConfig,
    name: String,
    dir: PathBuf,
    app_override: Option<AppOverride>,
) -> AppSpec {
    let managed = app_override.as_ref().is_none_or(|value| value.managed);
    let string_value = |select: fn(&AppOverride) -> &Option<String>, default: &str| {
        app_override
            .as_ref()
            .and_then(|value| select(value).clone())
            .unwrap_or_else(|| default.to_string())
    };
    let duration_value = |select: fn(&AppOverride) -> Option<u64>, default: u64| {
        Duration::from_secs(app_override.as_ref().and_then(select).unwrap_or(default))
    };
    AppSpec {
        name,
        dir,
        managed,
        start: string_value(|value| &value.start, "./run.sh"),
        pid_file: string_value(|value| &value.pid_file, "./app.pid"),
        disable_file: string_value(|value| &value.disable_file, "./disabled"),
        stop: string_value(|value| &value.stop, "./stop.sh"),
        signal_file: string_value(|value| &value.signal_file, "./signal"),
        healthcheck: app_override
            .as_ref()
            .and_then(|value| value.healthcheck.clone()),
        startup_validation: duration_value(
            |value| value.startup_validation_secs,
            global.startup_validation_secs,
        ),
        stop_grace: duration_value(|value| value.stop_grace_secs, global.stop_grace_secs),
        command_timeout: duration_value(|value| value.timeout_secs, global.timeout_secs),
        log_override: app_override.map(|value| value.log).unwrap_or_default(),
    }
}

fn supervise_apps(
    global: &GlobalConfig,
    apps: &[AppSpec],
    guard_log: &Path,
) -> (SupervisionResult, Vec<AppCheckResult>) {
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = apps
            .iter()
            .map(|app| {
                scope.spawn(move || {
                    Supervisor {
                        global,
                        app,
                        guard_log,
                    }
                    .check()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| match handle.join() {
                Ok(result) => result,
                Err(_) => AppCheckResult::Failed,
            })
            .collect::<Vec<_>>()
    });
    let failed = results
        .iter()
        .any(|result| matches!(result, AppCheckResult::Failed | AppCheckResult::Quarantined));
    let overall = if failed {
        SupervisionResult::Failure
    } else {
        SupervisionResult::Success
    };
    (overall, results)
}

fn rotate_app_logs(global: &GlobalConfig, apps: &[AppSpec], guard_log: &Path, config_dir: &Path) {
    let _lock = match lock(config_dir, ".guard.rotate.lock") {
        Ok(Some(lock)) => lock,
        Ok(None) => return,
        Err(e) => return write_log(guard_log, "processguard", &format!("error: {e}")),
    };
    for app in apps {
        Supervisor {
            global,
            app,
            guard_log,
        }
        .rotate_logs();
    }
}

const USAGE: &str = "usage: processguard [-c <config.toml>]   (default: ./guard.toml)";

fn config_path() -> Result<PathBuf, String> {
    let mut path = PathBuf::from("guard.toml");
    let mut args = env::args_os().skip(1);
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("-c" | "--config") => {
                path = args.next().map(PathBuf::from).ok_or(USAGE)?;
            }
            _ => return Err(USAGE.to_string()),
        }
    }
    Ok(path)
}

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
    let setup = || -> Result<(Config, PathBuf, PathBuf, Vec<AppSpec>), String> {
        let config_path = config_path()?
            .canonicalize()
            .map_err(|e| format!("cannot resolve config: {e}"))?;
        let config_dir = config_path
            .parent()
            .ok_or_else(|| format!("{} has no parent", config_path.display()))?
            .to_path_buf();
        let text = fs::read_to_string(&config_path)
            .map_err(|e| format!("{}: {e}", config_path.display()))?;
        let config: Config =
            toml::from_str(&text).map_err(|e| format!("{}: {e}", config_path.display()))?;
        let apps = discover_apps(&config, &config_dir)?;
        let guard_log = resolve_from(&config_dir, &config.global.guard_log);
        Ok((config, config_dir, guard_log, apps))
    };
    let (config, config_dir, guard_log, apps) = match setup() {
        Ok(setup) => setup,
        Err(e) => {
            eprintln!("processguard: {e}");
            return ExitCode::from(2);
        }
    };

    let guard_lock = match lock(&config_dir, ".guard.lock") {
        Ok(Some(lock)) => lock,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            write_log(&guard_log, "processguard", &format!("error: {e}"));
            return ExitCode::FAILURE;
        }
    };
    let (result, _) = supervise_apps(&config.global, &apps, &guard_log);
    drop(guard_lock);
    rotate_app_logs(&config.global, &apps, &guard_log, &config_dir);

    match result {
        SupervisionResult::Success => ExitCode::SUCCESS,
        SupervisionResult::Failure => ExitCode::FAILURE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let path = env::temp_dir().join(format!(
                "processguard-{name}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }

        fn app(&self, name: &str, script: &str) -> PathBuf {
            let directory = self.0.join("apps").join(name);
            fs::create_dir_all(&directory).expect("create app directory");
            let run = directory.join("run.sh");
            fs::write(&run, script).expect("write run script");
            fs::set_permissions(&run, fs::Permissions::from_mode(0o755))
                .expect("make run script executable");
            directory
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn config_for(test_dir: &TestDir) -> Config {
        Config {
            global: GlobalConfig {
                base_dir: test_dir.0.join("apps"),
                startup_validation_secs: 10,
                stop_grace_secs: 0,
                timeout_secs: 2,
                guard_log: test_dir.0.join("guard.log"),
                log: LogSettings::default(),
            },
            app_overrides: Vec::new(),
        }
    }

    fn app_override(name: &str) -> AppOverride {
        AppOverride {
            name: name.to_string(),
            managed: true,
            start: None,
            pid_file: None,
            disable_file: None,
            stop: None,
            signal_file: None,
            healthcheck: None,
            startup_validation_secs: None,
            stop_grace_secs: None,
            timeout_secs: None,
            log: LogOverrides::default(),
        }
    }

    #[test]
    fn discovers_apps_without_app_sections() {
        let test_dir = TestDir::new("discovery");
        test_dir.app("worker", "#!/bin/sh\nexec sleep 5\n");
        test_dir.app("api", "#!/bin/sh\nexec sleep 5\n");
        let config = config_for(&test_dir);

        let apps = discover_apps(&config, &test_dir.0).expect("discover apps");
        assert_eq!(
            apps.iter().map(|app| app.name.as_str()).collect::<Vec<_>>(),
            vec!["api", "worker"]
        );
    }

    #[test]
    fn unmatched_override_is_rejected() {
        let test_dir = TestDir::new("unmatched");
        fs::create_dir_all(test_dir.0.join("apps")).expect("create apps");
        let mut config = config_for(&test_dir);
        config.app_overrides.push(app_override("missing"));

        assert!(discover_apps(&config, &test_dir.0).is_err());
    }

    #[test]
    fn log_conf_has_highest_precedence() {
        let test_dir = TestDir::new("logs");
        let app_dir = test_dir.app("api", "#!/bin/sh\nexec sleep 5\n");
        fs::write(
            app_dir.join("log.conf"),
            "max_keep = 2\nmax_size = \"1MiB\"\n",
        )
        .expect("write log config");
        let mut config = config_for(&test_dir);
        config.global.log.max_keep = 10;
        let mut override_config = app_override("api");
        override_config.log.max_keep = Some(5);
        config.app_overrides.push(override_config);
        let apps = discover_apps(&config, &test_dir.0).expect("discover apps");
        let supervisor = Supervisor {
            global: &config.global,
            app: &apps[0],
            guard_log: &config.global.guard_log,
        };

        let settings = supervisor.load_log_settings().expect("load logs");
        assert_eq!(settings.max_keep, 2);
        assert_eq!(settings.max_size, 1 << 20);
    }

    #[test]
    fn early_exit_creates_invalid_marker_and_future_runs_skip() {
        let test_dir = TestDir::new("invalid");
        let app_dir = test_dir.app("api", "#!/bin/sh\nexit 0\n");
        let config = config_for(&test_dir);
        let mut apps = discover_apps(&config, &test_dir.0).expect("discover apps");
        apps[0].startup_validation = Duration::from_secs(1);
        let supervisor = Supervisor {
            global: &config.global,
            app: &apps[0],
            guard_log: &config.global.guard_log,
        };

        assert_eq!(supervisor.check(), AppCheckResult::Quarantined);
        let marker = fs::read_to_string(app_dir.join("invalid")).expect("read marker");
        assert!(marker.contains("startup_exited_early"));
        assert_eq!(supervisor.check(), AppCheckResult::SkippedInvalid);
    }

    #[test]
    fn kill_is_a_supported_immediate_stop_signal() {
        assert_eq!(StopSignal::parse("KILL"), Ok(StopSignal::Kill));
        assert_eq!(StopSignal::parse("sigkill"), Ok(StopSignal::Kill));
        assert!(StopSignal::Kill.is_forceful());
    }

    #[test]
    fn example_config_parses_without_app_sections() {
        let config: Config =
            toml::from_str(include_str!("../guard.toml.example")).expect("parse example config");
        assert!(config.app_overrides.is_empty());
    }

    #[test]
    fn app_and_nested_log_overrides_parse() {
        let config: Config = toml::from_str(
            r#"
                [global]
                base_dir = "apps"

                [[app]]
                name = "api"
                healthcheck = "./healthcheck.sh"

                [app.log]
                max_keep = 4
                compression = "none"
            "#,
        )
        .expect("parse app override");

        assert_eq!(config.app_overrides.len(), 1);
        assert_eq!(config.app_overrides[0].log.max_keep, Some(4));
        assert_eq!(
            config.app_overrides[0].log.compression,
            Some(rotate::CompressionMethod::None)
        );
    }

    #[test]
    fn app_checks_run_concurrently() {
        let test_dir = TestDir::new("concurrency");
        let first = test_dir.app("first", "#!/bin/sh\nexec sleep 5\n");
        let second = test_dir.app("second", "#!/bin/sh\nexec sleep 5\n");
        let record = PidRecord {
            pid: std::process::id() as libc::pid_t,
            process_group: unsafe { libc::getpgrp() },
        };
        let pid_text = toml::to_string(&record).expect("encode pid record");
        fs::write(first.join("app.pid"), &pid_text).expect("write first pid");
        fs::write(second.join("app.pid"), &pid_text).expect("write second pid");

        let mut config = config_for(&test_dir);
        for name in ["first", "second"] {
            let mut configured = app_override(name);
            configured.healthcheck = Some("sleep 1".to_string());
            config.app_overrides.push(configured);
        }
        let apps = discover_apps(&config, &test_dir.0).expect("discover apps");
        let started = Instant::now();
        let (result, checks) = supervise_apps(&config.global, &apps, &config.global.guard_log);

        assert_eq!(result, SupervisionResult::Success);
        assert_eq!(checks, vec![AppCheckResult::Idle, AppCheckResult::Idle]);
        assert!(
            started.elapsed() < Duration::from_millis(1800),
            "two one-second checks ran serially"
        );
    }
}
