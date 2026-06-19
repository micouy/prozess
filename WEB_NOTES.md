# Web client — design decisions and PR split

Working notes for the `feature/web` spike. Delete before the final merge;
the durable decisions move into DESIGN.md.

## Design decisions

### D1. Web is a feature-gated module, never a separate crate
`src/web/`, behind `#[cfg(feature = "web")]`. Depends only on `client` and
`protocol` — never `daemon` internals (same boundary the CLI respects).
Keeps one published `pz` crate; `cargo install pz` unchanged.

### D2. Web is on by default, opt out to go lean
`[features] default = ["web"]`. `cargo install pz` includes the web UI;
`cargo install pz --no-default-features` builds the lean CLI with no axum.

### D3. `pz serve` is a daemon client, not a second daemon
`pz serve` connects to an already-running pz daemon over the existing
socket via `client::Client`. It does not spawn or supervise processes
itself — every action is a `protocol::Request` round-trip. Pure
presentation, consistent with "actions in the daemon, observation in the
client".

### D4. HTTP stack: axum
Tokio-native (we already use tokio), minimal deps. `tower-http` for static
file serving. Feature `web` pulls these in.

### D5. JSON API is the contract; the UI is a client of it
`/api/*` returns JSON built from `protocol` types (already `Serialize`).
The browser UI consumes the same API. This decouples frontend choices from
the backend and makes the API testable directly.

### D6. Frontend: server-served static assets + vanilla JS (no build step)
A single static `index.html` + JS bundle served by `tower-http`, polling
the JSON API. No npm/bundler in the build — keeps `cargo install` the only
toolchain needed. (Revisit if the UI outgrows it.)

### D7. Read-only first, then actions
Endpoints land in order: list/show/logs (observation) before
stop/restart/run (actions), so the risky mutating surface is separable.

### D8. Logs over the API
`GET /api/processes/:id/logs?after_id=&tail=` maps to `ReadLogs`. The UI
polls with the returned `resume_after_id` cursor — the same cursor model
the CLI's `-f` uses. No websockets in v1.

## Proposed PR split (after the spike works)

1. **Feature scaffolding**: `web` feature, `dep:axum`/`tower-http`,
   `pz serve` command (errors helpfully without the feature), empty router
   + `/api/health`. Proves the gate and the daemon-client wiring.
2. **Read API**: `/api/processes`, `/api/processes/:id`,
   `/api/processes/:id/logs`. JSON from protocol types.
3. **Static UI shell**: `index.html` + JS, process list view, served by
   tower-http.
4. **Logs view**: per-process log pane polling the cursor.
5. **Action API + UI**: stop / restart / run, with the buttons.
6. **Docs**: fold D1–D8 into DESIGN.md, README `pz serve` section, drop
   this file.
