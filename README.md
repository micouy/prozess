# pz

`pz` is a local process manager. It runs commands through a daemon, stores process state in SQLite, and lets you inspect, stop, restart, and attach to processes later.

The CLI is only one client. The daemon owns the processes so commands keep running if the terminal, SSH session, or agent harness exits.

## Install

```sh
cargo install pz
```

## Daemon

```sh
pz daemon start
pz daemon status
pz daemon stop
```

Use foreground mode for debugging:

```sh
pz daemon run
```

## Run Processes

```sh
pz run --name api -- cargo run
pz run --name worker --cwd /path/to/repo -- ./gradlew :app:run
```

Processes run with an empty environment by default. Add env explicitly:

```sh
pz run --env-file .env -- /usr/bin/env
pz run --env FOO=bar -- /usr/bin/env
pz run --inherit-env -- /usr/bin/env
```

## Inspect And Control

```sh
pz ps
pz show api
pz logs api -f
pz logs api --tail 50
pz logs api -f --tail 0   # follow new output only, no replay
pz stop api
pz stop api --force
pz stop api --grace 10s   # SIGTERM, wait, then SIGKILL; default grace is 5s
pz wait api
pz restart api
```

Resources and ports:

```sh
pz resources api
pz ports api
```

## Timeouts

```sh
pz run --name slow --timeout 30s -- /bin/sleep 300
pz timeout slow 5m
pz timeout slow clear
```

Timeouts are enforced by the daemon.

## Config

`pz` reads defaults from:

```text
~/.config/pz.toml
```

Example:

```toml
[run]
inherit_env = false
env_files = []

[env]
PATH = [
  "/opt/homebrew/bin",
  "/usr/local/bin",
  "/usr/bin",
  "/bin"
]
JAVA_HOME = "/path/to/java"
```

Env vars from the `[env]` section above are applied by the daemon when it
starts a process, including on `pz restart` — so config edits take effect
on the next (re)start. `pz` stores env keys and env-file paths, not env
values.

## Notes

- Commands are spawned directly, not through a shell.
- Processes are placed in their own process groups.
- `pz stop` signals the process group and returns once it is confirmed
  dead, escalating SIGTERM to SIGKILL after the grace period.
- Logs are captured after spawn and can be read or followed later.
- If the daemon dies, previously running processes are marked `lost` on daemon restart.
