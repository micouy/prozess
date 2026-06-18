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
    assert!(!group_alive(pgid));

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
