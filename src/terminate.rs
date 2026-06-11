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
            Ok(()) | Err(Errno::ESRCH) => {}
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
        Ok(()) | Err(Errno::ESRCH) => {}
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
        if kill(group, None) == Err(Errno::ESRCH) {
            return true;
        }

        if tokio::time::Instant::now() >= deadline {
            return false;
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
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
}
