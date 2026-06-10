# Design Decisions

This file records design decisions for `pz`. Every PR that adds, changes, or
invalidates a decision must update this file in the same PR.

## Table of Contents

- [Architecture](#architecture)
  - [The daemon owns processes; the CLI is one client](#the-daemon-owns-processes-the-cli-is-one-client)
  - [SQLite is the registry and the log store](#sqlite-is-the-registry-and-the-log-store)
  - [IPC is one request, one response per connection](#ipc-is-one-request-one-response-per-connection)
  - [Runtime directory is keyed by uid](#runtime-directory-is-keyed-by-uid)
- [Process Lifecycle](#process-lifecycle)
  - [Commands are spawned directly, in their own process group](#commands-are-spawned-directly-in-their-own-process-group)
  - [Empty environment by default; env values are never stored](#empty-environment-by-default-env-values-are-never-stored)
  - [The daemon never kills processes at startup; lost stays lost](#the-daemon-never-kills-processes-at-startup-lost-stays-lost)
- [Logs](#logs)
  - [Logs are ordered by a global cursor](#logs-are-ordered-by-a-global-cursor)
- [CLI Behavior](#cli-behavior)
  - [Broken pipe is a silent success](#broken-pipe-is-a-silent-success)

## Architecture

### The daemon owns processes; the CLI is one client

Processes are spawned and supervised by a long-lived daemon, not by the CLI.
Commands keep running when the terminal, SSH session, or agent harness exits.
Anything that must be correct under concurrency (name uniqueness, spawn,
kill) is decided inside the daemon against its registry — never by
check-then-act sequences in clients.

### SQLite is the registry and the log store

Process state and captured output live in one SQLite database (WAL mode).
State survives daemon restarts: pids, pgids, commands, and logs are all
recoverable. There is no separate log file scheme; output chunks are rows.

### IPC is one request, one response per connection

The Unix-socket protocol is a single JSON request followed by a single JSON
response, then the connection closes. There is no streaming. Follow-style
commands are implemented by client polling with cursors. This keeps the
daemon's accept loop simple; if real-time push is ever needed, the protocol
gets redesigned then (framed, concurrent connections), and the polling
queries become the backfill path.

### Runtime directory is keyed by uid

The socket lives under a per-user runtime directory derived from
`$PZ_RUNTIME_DIR`, then `$XDG_RUNTIME_DIR/pz`, then a tmpdir fallback keyed
by **uid** — not `$USER`, which can be unset and yields colliding or
squattable paths like `pz-unknown`, and lets client and daemon silently
disagree about the socket path when their environments differ. Runtime
directories the daemon creates get mode 0700; pre-existing directories are
left untouched.

## Process Lifecycle

### Commands are spawned directly, in their own process group

No shell interpretation. Each process gets its own process group; all
signals target the group (`kill(-pgid, sig)`) so process trees die together.

### Empty environment by default; env values are never stored

Processes start with an empty environment unless the user passes
`--inherit-env`, `--env-file`, or `--env`. The registry stores env *keys*
and env-file *paths*, never values, so secrets do not land in SQLite.
Consequence: `restart` refuses processes started with inline `--env`,
because the values cannot be reproduced.

### The daemon never kills processes at startup; lost stays lost

When the daemon boots and finds registry rows from a previous daemon
generation, it marks them `lost` and leaves them alone — even if they are
still alive. Killing user processes because the *daemon* restarted is never
acceptable. `lost` is a permanent honest status: the daemon cannot
retroactively guarantee log completeness or exit-code capture for a process
it stopped watching, so it does not pretend to.

## Logs

### Logs are ordered by a global cursor

Output chunks share one autoincrement id across all processes. This single
global cursor gives: per-process ordering, a coherent merged timeline across
processes, and resumable reads (`after_id`). New log features should be
expressed as queries against this cursor rather than new storage.

## CLI Behavior

### Broken pipe is a silent success

When a consumer closes the pipe early (`pz logs | head`), `pz` exits 0
quietly, matching standard Unix tool behavior. EPIPE on stdout is not an
error.
