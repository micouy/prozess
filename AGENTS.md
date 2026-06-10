# pz — agent notes

`pz` is a local process manager: a daemon supervises processes, state and
logs live in SQLite, the CLI talks to the daemon over a Unix socket. See
README.md for usage.

## Design decisions

Design decisions are recorded in DESIGN.md.

- Before designing or changing behavior, read DESIGN.md and stay consistent
  with it.
- **Every PR must update DESIGN.md** if it adds, changes, or invalidates a
  design decision. A PR that changes behavior without touching DESIGN.md
  should explain why no decision changed.
- Keep entries short but solid: state the decision and the reason, not the
  implementation details.

## Verification

- `cargo test` runs unit tests (in `src/`) and integration tests
  (`tests/daemon_lifecycle.rs`, exercises the real binary).
- `cargo clippy` and `cargo fmt --check` before finishing.
