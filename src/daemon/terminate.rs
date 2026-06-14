//! Confirmed process-group termination: TERM, grace, KILL, verified dead.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use nix::{
    errno::Errno,
    sys::signal::{Signal, kill},
    unistd::Pid,
};

use crate::protocol::StopSignal;

pub const DEFAULT_GRACE: Duration = Duration::from_secs(5);
/// How long SIGKILL gets before we give up; only exceeded by unkillable
/// (e.g. uninterruptible-sleep) processes.
const KILL_DEADLINE: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Kills the process group and returns only once every member is confirmed
/// gone. `force` skips straight to SIGKILL. Returns the signal that did it.
pub async fn kill_group_confirmed(pgid: u32, force: bool, grace: Duration) -> Result<StopSignal> {
    let group = Pid::from_raw(-(pgid as i32));

    if !force {
        match kill(group, Signal::SIGTERM) {
            Ok(()) | Err(Errno::ESRCH) | Err(Errno::EPERM) => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to send SIGTERM to process group {pgid}"));
            }
        }

        if wait_until_dead(pgid, grace).await {
            return Ok(StopSignal::Term);
        }
    }

    match kill(group, Signal::SIGKILL) {
        Ok(()) | Err(Errno::ESRCH) | Err(Errno::EPERM) => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to send SIGKILL to process group {pgid}"));
        }
    }

    if wait_until_dead(pgid, KILL_DEADLINE).await {
        return Ok(StopSignal::Kill);
    }

    bail!("process group {pgid} did not exit after SIGKILL")
}

async fn wait_until_dead(pgid: u32, budget: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + budget;

    loop {
        if !group_alive(pgid) {
            return true;
        }

        if tokio::time::Instant::now() >= deadline {
            return false;
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

// Zombies count as dead — exited, ports released, only the pid entry
// lingers until reaped — but the platforms report them differently:
// macOS answers EPERM for a zombie-only group (and EPERM also means a
// recycled pgid we must not signal further), while Linux happily
// "signals" zombies, so a successful kill(0) there proves nothing and
// the member states have to come from /proc.
//
// Known limit: on macOS, EPERM cannot distinguish "all zombies" from
// "all privileged" (a setuid child, e.g. via sudo), so a group we could
// never kill anyway is reported dead rather than erroring as on Linux.
#[cfg(target_os = "linux")]
pub fn group_alive(pgid: u32) -> bool {
    let group = Pid::from_raw(-(pgid as i32));
    if kill(group, None).is_err() {
        return false;
    }

    let pgid = pgid.to_string();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return true;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().filter(|name| name.parse::<u32>().is_ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // Fields after the parenthesized comm: state, ppid, pgrp, ...
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let state = fields.next();
        let _ppid = fields.next();
        let pgrp = fields.next();

        if pgrp == Some(pgid.as_str()) && state != Some("Z") {
            return true;
        }
    }

    false
}

#[cfg(not(target_os = "linux"))]
pub fn group_alive(pgid: u32) -> bool {
    let group = Pid::from_raw(-(pgid as i32));
    !matches!(kill(group, None), Err(Errno::ESRCH | Errno::EPERM))
}

#[cfg(test)]
mod tests;
