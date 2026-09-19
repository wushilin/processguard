# processguard

A cron-driven process guard, for hosts where systemd (or another supervisor) is not an option.

```
* * * * * /home/usera/apps/app1/guard
```

Each run reads `guard.toml` next to the binary, starts the app if it is not running, stops it if it
has been disabled, rotates its logs, and exits. There is no daemon.

## Setup

```sh
cargo build --release
cp target/release/processguard /home/usera/apps/app1/guard   # or symlink one shared binary
cp guard.toml.example /home/usera/apps/app1/guard.toml
```

The config is looked up next to the path the guard was invoked by, so one binary can be symlinked
into many app directories. A config path can also be passed explicitly: `guard /path/to/guard.toml`.

## Configuration

See [guard.toml.example](guard.toml.example) for every option.

```toml
app_name = "sleeper"

[commands]
start = "run.sh"        # launch the app
status = "status.sh"    # exit 0 = running, 1 = not running
enabled = "enabled.sh"  # optional: exit 0 = enabled, otherwise disabled
stop = "stop.sh"        # optional: run when disabled but still running

[logging]               # optional: without it the app's output goes to /dev/null
stdout = "stdout.log"
stderr = "stderr.log"
max_size = "15MiB"
max_keep = 10
compress_after = 3
```

Commands run via `sh -c` in the config's directory, with that directory on `PATH`.

## Behaviour

- **start** is launched detached, nohup-style (own session, SIGHUP ignored), and not waited for,
  so the script can simply run the app in the foreground. A non-zero exit within the first second
  is logged as a failed start.
- **enabled**, **status** and **stop** must exit within `timeout_secs` (default 30), or their whole
  process tree is killed.
- **status** exiting with anything other than 0 or 1 (or timing out) is an error: nothing is
  started, because a broken status check should not cause a double start.
- A lock file makes overlapping cron runs exit immediately.
- The guard prints nothing in normal operation (no cron mail); events go to `guard.log`.

### Log rotation

Log files are checked on every run and rotated once they reach `max_size`: `stdout.log` →
`stdout.log.1` → … → `stdout.log.<max_keep>`, with generations past `compress_after` gzipped
(`stdout.log.4.gz`).

- The app keeps its log open, so the live file is copied and then truncated in place. Lines written
  in the instant between the copy and the truncate can be lost.
- Files can overshoot `max_size` by up to one cron interval of output.
- Rotation runs under its own lock, after the process check. If compression outlasts the cron
  interval, later runs keep guarding the process and skip rotation until it finishes.

## Building on FreeBSD without cc/ld (pfSense)

`./build-freebsd.sh` links with the `rust-lld` bundled in the rust package, via `freebsd-link.sh`.
