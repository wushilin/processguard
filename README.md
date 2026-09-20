# processguard

A cron-driven, convention-based supervisor for hosts where a resident service
manager is unavailable.

```cron
* * * * * /usr/local/bin/processguard -c /etc/processguard/guard.toml
```

One invocation discovers applications, checks all of them concurrently, rotates
their logs, and exits. A lock prevents overlapping cron invocations.

## Drop-in layout

Every visible immediate directory under `base_dir` is an application:

```text
apps/
├── api/
│   ├── run.sh
│   ├── healthcheck.sh
│   ├── log.conf
│   └── logs/
└── worker/
    └── run.sh
```

Hidden directories are ignored, which permits atomic deployments:

```sh
mv apps/.deploying-api apps/api
```

The only required application file is executable `run.sh`. It must remain in
the foreground and ultimately `exec` the real application. Processguard starts
it in a new session and owns the PID file.

## Configuration

The minimal configuration contains no `[[app]]` entries:

```toml
[global]
base_dir = "/home/abc/processguard/apps"
```

Defaults:

```toml
[global]
startup_validation_secs = 10
stop_grace_secs = 30
timeout_secs = 30
guard_log = "guard.log"

[global.log]
stdout = "logs/stdout.log"
stderr = "logs/stderr.log"
max_size = "15MiB"
max_keep = 10
compress_after = 3
compression = "gzip"
rotation = "copytruncate"
```

An optional `[[app]]` section overrides conventions for a discovered directory;
it does not register an application:

```toml
[[app]]
name = "api"
healthcheck = "./healthcheck.sh"
startup_validation_secs = 20

[app.log]
max_keep = 5

[[app]]
name = "manually-managed"
managed = false
```

An override without a matching directory is an error. Duplicate names, path
components as names, and symlinks resolving outside `base_dir` are rejected.

Configuration precedence is:

```text
built-in conventions
  < [global] and [global.log]
  < matching [[app]] and [app.log]
  < <app-directory>/log.conf
```

See [guard.toml.example](guard.toml.example) and
[log.conf.example](log.conf.example).

## Application conventions

For `<base_dir>/api`, processguard uses:

| Purpose | Default |
|---|---|
| Start program | `./run.sh` |
| PID record | `./app.pid` |
| Disable marker | `./disabled` |
| Invalid marker | `./invalid` |
| Suspended marker (transient) | `./suspended` |
| Optional stop program | `./stop.sh` |
| Optional stop signal | `./signal` |
| Optional log settings | `./log.conf` |
| Standard output | `./logs/stdout.log` |
| Standard error | `./logs/stderr.log` |

The PID record is TOML and contains both the leader PID and process-group ID.
Legacy files containing one numeric PID are also accepted.

## Startup quarantine

After spawning `run.sh`, processguard writes `app.pid` atomically and observes
the child for `startup_validation_secs`. Any exit during that window, including
a successful exit, means the foreground/exec contract was broken.

The process group is killed, the PID file is removed, and an `invalid` TOML
marker records the timestamp, reason, PID, observation duration, and exit
status. While `invalid` exists, the application is not inspected, launched,
stopped, health-checked, or rotated. Remove the marker after fixing the app.

Creating an invalid marker makes that invocation fail. Subsequent invocations
treat the quarantined application as intentionally skipped.

## Stopping

When `disabled` exists, or a configured health check reports unhealthy:

1. If `stop.sh` exists, processguard runs it.
2. Otherwise it reads `signal`; missing `signal` defaults to `TERM`.
3. `TERM`, `INT`, `HUP`, and `QUIT` are sent to the leader PID.
4. Processguard waits up to `stop_grace_secs` for both the leader and process
   group to disappear.
5. Survivors receive `SIGKILL` as an entire process group.

If `stop.sh` fails or times out, processguard also uses the configured signal
before beginning the grace period.

Setting `signal` to `KILL` skips the grace period and immediately kills the
entire process group. Signal names may optionally include the `SIG` prefix.

If the leader exits while its process group remains during an ordinary check,
the remaining group is killed and the application is quarantined as an
orphaned process group.

## Health checks

Health checks are opt-in through an app override:

```toml
[[app]]
name = "api"
healthcheck = "./healthcheck.sh"
```

Exit 0 means healthy, exit 1 means unhealthy, and other outcomes are errors. An
unhealthy app follows the normal stop procedure and then undergoes the complete
startup-validation procedure again.

## Concurrency and logs

Lifecycle checks use one scoped worker per discovered app. A ten-second startup
validation therefore costs roughly ten seconds for any number of apps started
in the same pass, rather than ten seconds per app.

Rotation runs after workers join and remains serial under its own lock to avoid
saturating storage with concurrent compression. Compression can be `gzip` or
`none`.

### Lossless rotation

The app keeps its log open, so the live file is copied to `<file>.1` and then
truncated in place. With the default `rotation = "copytruncate"`, output written
in the instant between the end of the copy and the truncate is lost: nothing
measurable for a quiet app, but about one `write()` per rotation for an app
that logs flat out.

`rotation = "suspend"` closes that window. The bulk of the file is copied while
the app runs; then the app's whole process group is stopped with `SIGSTOP`, the
remainder is copied, the file is truncated, and the group is resumed with
`SIGCONT`. A stopped process cannot write, and output still buffered inside the
app is frozen with it and lands in the fresh file afterwards, so nothing is
lost. The pause covers only the final copy and the truncate: typically around a
millisecond, and logged with every rotation.

```toml
[global.log]            # or [app.log], or <app>/log.conf
rotation = "suspend"
```

- Only the app's process group is stopped. A process that holds the log open
  from outside the group (one that called `setsid`) is not covered.
- If the group cannot be confirmed stopped within a second, it is resumed and
  the file is rotated as with `copytruncate`; the guard log says so.
- While the app is stopped, a `suspended` marker names its process group. If the
  guard dies before resuming it, the next run resumes the group and removes the
  marker, so an app is never left frozen for longer than a cron interval.
- `SIGSTOP`/`SIGCONT` make some blocking system calls return `EINTR`. Language
  runtimes retry these transparently; hence this is opt-in rather than default.
- Supported on Linux, FreeBSD and macOS; elsewhere it behaves as `copytruncate`.

## Building

```sh
cargo build --release
cp target/release/processguard /usr/local/bin/processguard
```

On FreeBSD systems without `cc`/`ld`, `./build-freebsd.sh` uses the bundled
`rust-lld` through `freebsd-link.sh`.
