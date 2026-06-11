# Design Decisions

This file records design decisions for `pz`. Every PR that adds, changes, or
invalidates a decision must update this file in the same PR.

## Table of Contents

- [Architecture](#architecture)
  - [The daemon owns processes; the CLI is one client](#the-daemon-owns-processes-the-cli-is-one-client)
  - [SQLite is the registry and the log store](#sqlite-is-the-registry-and-the-log-store)
  - [Schema changes are numbered migrations](#schema-changes-are-numbered-migrations)
  - [IPC is one request, one response per connection](#ipc-is-one-request-one-response-per-connection)
  - [Runtime directory is keyed by uid](#runtime-directory-is-keyed-by-uid)
- [Process Lifecycle](#process-lifecycle)
  - [Commands are spawned directly, in their own process group](#commands-are-spawned-directly-in-their-own-process-group)
  - [Empty environment by default; env values are never stored](#empty-environment-by-default-env-values-are-never-stored)
  - [A name has at most one live generation](#a-name-has-at-most-one-live-generation)
  - [The daemon never kills processes at startup; lost stays lost](#the-daemon-never-kills-processes-at-startup-lost-stays-lost)
  - [Pid liveness checks require an identity token](#pid-liveness-checks-require-an-identity-token)
- [Logs](#logs)
  - [Logs are ordered by a global cursor](#logs-are-ordered-by-a-global-cursor)
  - [Tail and follow are daemon-side queries](#tail-and-follow-are-daemon-side-queries)
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

### Schema changes are numbered migrations

Schema evolution uses `rusqlite_migration` (SQLite `user_version`): an
ordered list of steps, each applied exactly once, in a transaction, on
store open. Migration 1 is an idempotent baseline that also absorbs
databases from before versioning (any historical schema subset at
user_version 0). New schema changes append a numbered step — never edit an
existing one, and never add ad-hoc schema patches outside the list.
PRAGMAs are connection setup, not migrations: `foreign_keys` is
per-connection and `journal_mode` cannot change inside a transaction.

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
`--inherit-env`, `--env-file`, or `--env`, or defines default env vars in
the `[env]` section of the config file (`~/.config/pz.toml`). The
registry stores env *keys* and env-file *paths*, never values, so secrets
do not land in SQLite.

Reproducible sources are resolved by the daemon at spawn time — env files
are re-read and the config file's `[env]` defaults are re-applied on
every spawn, including restarts, so editing them affects the next
(re)start. Only inline `--env` values are irreproducible; they alone make
a process unrestartable.

### A name has at most one live generation

`pz run --name X` errors if a live process already holds the name —
whether that generation is `running`, or `lost` but still alive (checked
via the pid identity token). The silent alternative is two real processes
competing for the same ports with one invisible to `pz ps`; that is harder
to debug than any error. Enforcement is atomic in the daemon: the name is
reserved as a row *before* the child is spawned, backed by a partial
unique index on running names, so a conflict can never leak an untracked
child. Failed and dead generations do not block the name.

### The daemon never kills processes at startup; lost stays lost

When the daemon boots and finds registry rows from a previous daemon
generation, it marks them `lost` and leaves them alone — even if they are
still alive. Killing user processes because the *daemon* restarted is never
acceptable. `lost` is a permanent honest status: the daemon cannot
retroactively guarantee log completeness or exit-code capture for a process
it stopped watching, so it does not pretend to.

### Pid liveness checks require an identity token

Any decision based on "is pid N still alive" (lost-but-alive name
conflicts, replace-style reaping, adoption) must guard against pid reuse.
At spawn time the daemon records the process start time as an identity
token; later liveness checks compare it. A bare `kill(pid, 0)` is not
sufficient evidence: acting on a recycled pid can kill an unrelated
process. Rows recorded before tokens existed degrade to a bare existence
check.

## Logs

### Logs are ordered by a global cursor

Output chunks share one autoincrement id across all processes. This single
global cursor gives: per-process ordering, a coherent merged timeline across
processes, and resumable reads (`after_id`). New log features should be
expressed as queries against this cursor rather than new storage.

### Tail and follow are daemon-side queries

"Last N lines" and "start following from now" are answered entirely by the
daemon, not by fetching history and slicing client-side: one read request
takes an optional `tail_lines` and returns exactly the requested lines
(time window applied first, boundary chunk sliced on a line) plus the
cursor to resume from — meaningful even when no data is returned, so
followers never infer positions. There is deliberately no separate
"seek" request: every caller of a position query immediately reads from
it, so position-finding lives inside the read. Flags compose: `-f --tail
N` replays the last N lines then follows; `-f --tail 0` follows from now.
Flags that are accepted must work in every mode — silently ignoring a
parsed flag is a bug; combinations that cannot work (`-f --until`) are
rejected.

## CLI Behavior

### Broken pipe is a silent success

When a consumer closes the pipe early (`pz logs | head`), `pz` exits 0
quietly, matching standard Unix tool behavior. EPIPE on stdout is not an
error.
