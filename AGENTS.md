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
- Keep entries short but solid: state the decision and the reason. Detail
  is welcome when the decision is technical (definitions, platform
  behavior, accepted limitations).
- Headers are topics, not headlines: "Definition of a dead process
  group", never "Dead means no member can run again". The decision is
  stated in the body, not performed in the title.

## Comments and docstrings

Default to no comment. The bar is surprise: comment only what a competent
reader would get wrong without it.

- Explain *why*, or a non-obvious contract — never narrate what the code
  does. If a comment restates the name or the next line, delete it.
- Prefer making code self-explanatory (better name, smaller function) over
  explaining it.
- Doc comments only where a caller could plausibly misuse the API without
  them: units, sentinel values ("0 means from now"), invariants, behavior
  on empty/edge input. A name that says everything needs no doc.
- Match the surroundings: don't comment one field, variant, or argument
  when its siblings are bare — unless that one is genuinely the surprising
  one.
- One line is almost always enough. Multi-paragraph docs belong in
  DESIGN.md, not on functions.
- In tests, comment only non-obvious setup or why an assertion expects a
  strange-looking value.

## Verification

- `cargo test` runs unit tests (in `src/`) and integration tests
  (`tests/daemon_lifecycle.rs`, exercises the real binary).
- `cargo clippy` and `cargo fmt --check` before finishing.
