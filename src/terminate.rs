//! Confirmed process-group termination: TERM, grace, KILL, verified dead.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use nix::{
    errno::Errno,
    sys::signal::{Signal, kill},
    unistd::Pid,
};

use crate::protocol::StopSignal;

// TODO: consumed by confirmed stop/restart and timeout escalation;
// remove the allows once those land.
#[allow(dead_code)]
pub const DEFAULT_GRACE: Duration = Duration::from_secs(5);
/// How long SIGKILL gets before we give up; only exceeded by unkillable
/// (e.g. uninterruptible-sleep) processes.
const KILL_DEADLINE: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Kills the process group and returns only once every member is confirmed
/// gone. `force` skips straight to SIGKILL. Returns the signal that did it.
#[allow(dead_code)]
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

        if wait_until_dead(group, grace).await {
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

    if wait_until_dead(group, KILL_DEADLINE).await {
        return Ok(StopSignal::Kill);
    }

    bail!("process group {pgid} did not exit after SIGKILL")
}

async fn wait_until_dead(group: Pid, budget: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + budget;

    loop {
        if !group_alive(group) {
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
fn group_alive(group: Pid) -> bool {
    if kill(group, None).is_err() {
        return false;
    }

    let pgid = group.as_raw().unsigned_abs().to_string();
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
fn group_alive(group: Pid) -> bool {
    !matches!(kill(group, None), Err(Errno::ESRCH | Errno::EPERM))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn spawn_group(program: &str, args: &[&str]) -> Result<(tokio::process::Child, u32)> {
        let mut command = tokio::process::Command::new(program);
        command.args(args).process_group(0);
        let child = command.spawn()?;
        let pid = child.id().context("child has no pid")?;

        Ok((child, pid))
    }

    #[tokio::test]
    async fn polite_group_dies_on_term() -> Result<()> {
        let (mut child, pgid) = spawn_group("/bin/sleep", &["30"]).await?;

        let (signal, status) = tokio::join!(
            kill_group_confirmed(pgid, false, Duration::from_secs(5)),
            child.wait(),
        );
        assert!(matches!(signal?, StopSignal::Term));
        assert!(!status?.success());

        Ok(())
    }

    #[tokio::test]
    async fn stubborn_group_is_escalated_to_kill() -> Result<()> {
        // The shell ignores TERM and respawns its sleep; only KILL ends it.
        // The ready file avoids signaling before the trap is installed.
        let dir = tempfile::tempdir()?;
        let ready = dir.path().join("ready");
        let script = format!(
            "trap '' TERM; touch {}; while :; do sleep 0.1; done",
            ready.display()
        );
        let (mut child, pgid) = spawn_group("/bin/sh", &["-c", &script]).await?;
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let (signal, status) = tokio::join!(
            kill_group_confirmed(pgid, false, Duration::from_millis(300)),
            child.wait(),
        );
        assert!(matches!(signal?, StopSignal::Kill));
        assert!(!status?.success());

        Ok(())
    }

    #[tokio::test]
    async fn force_skips_straight_to_kill() -> Result<()> {
        let (mut child, pgid) = spawn_group("/bin/sleep", &["30"]).await?;

        let (signal, status) = tokio::join!(
            kill_group_confirmed(pgid, true, Duration::from_secs(5)),
            child.wait(),
        );
        assert!(matches!(signal?, StopSignal::Kill));
        assert!(!status?.success());

        Ok(())
    }

    #[tokio::test]
    async fn dead_group_reports_term_immediately() -> Result<()> {
        let (mut child, pgid) = spawn_group("/usr/bin/true", &[]).await?;
        child.wait().await?;

        let signal = kill_group_confirmed(pgid, false, Duration::from_secs(5)).await?;
        assert!(matches!(signal, StopSignal::Term));

        Ok(())
    }

    #[tokio::test]
    async fn surviving_grandchild_is_escalated_with_the_group() -> Result<()> {
        // The parent dies politely on TERM, but its TERM-ignoring
        // grandchild keeps the group alive; escalation must end it too.
        let dir = tempfile::tempdir()?;
        let ready = dir.path().join("ready");
        let script = format!(
            "(trap '' TERM; touch {}; while :; do sleep 0.1; done) & sleep 30",
            ready.display()
        );
        let (mut child, pgid) = spawn_group("/bin/sh", &["-c", &script]).await?;
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let (signal, _) = tokio::join!(
            kill_group_confirmed(pgid, false, Duration::from_millis(300)),
            child.wait(),
        );
        assert!(matches!(signal?, StopSignal::Kill));
        // The grandchild may linger as a zombie until init reaps it; use
        // the same liveness definition as the primitive.
        assert!(!group_alive(Pid::from_raw(-(pgid as i32))));

        Ok(())
    }

    #[tokio::test]
    async fn unreaped_zombie_group_counts_as_dead() -> Result<()> {
        // macOS reports EPERM, not ESRCH, for a zombie-only group.
        let (child, pgid) = spawn_group("/usr/bin/true", &[]).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;

        let signal = kill_group_confirmed(pgid, false, Duration::from_millis(300)).await?;
        assert!(matches!(signal, StopSignal::Term));
        drop(child);

        Ok(())
    }

    #[tokio::test]
    async fn concurrent_kills_both_succeed() -> Result<()> {
        let (mut child, pgid) = spawn_group("/bin/sleep", &["30"]).await?;

        let (first, second, _) = tokio::join!(
            kill_group_confirmed(pgid, false, Duration::from_secs(2)),
            kill_group_confirmed(pgid, true, Duration::from_secs(2)),
            child.wait(),
        );
        first?;
        second?;

        Ok(())
    }
}
