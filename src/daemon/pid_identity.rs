//! Pid identity tokens.
//!
//! A pid alone is not a process identity: pids are recycled, and acting on
//! a recycled pid can target an unrelated process. Every decision based on
//! "is pid N still alive" must therefore compare an identity token captured
//! at spawn time (see DESIGN.md: "Pid liveness checks require an identity
//! token"). The token is the process start time in seconds since the Unix
//! epoch: a recycled pid gets a different start time.

use sysinfo::{Pid, ProcessesToUpdate, System};

/// Returns the identity token for `pid`, or `None` if no such process is
/// currently alive.
pub fn current_token(pid: u32) -> Option<i64> {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[Pid::from_u32(pid)]), true);

    let process = system.process(Pid::from_u32(pid))?;

    i64::try_from(process.start_time()).ok()
}

/// Returns whether `pid` is alive *and* is the same process the token was
/// captured from. A `None` token (rows recorded before tokens existed)
/// degrades to a bare existence check.
///
/// Resolution is one second: a pid recycled within the same second as the
/// original spawn is indistinguishable. Acceptable for guarding kills; not
/// a security boundary.
// TODO: consumed by lost-but-alive name conflicts and `run --replace`;
// remove the allow once those land.
#[allow(dead_code)]
pub fn is_alive(pid: u32, token: Option<i64>) -> bool {
    match (current_token(pid), token) {
        (None, _) => false,
        (Some(current), Some(expected)) => current == expected,
        (Some(_), None) => true,
    }
}

#[cfg(test)]
mod tests;
