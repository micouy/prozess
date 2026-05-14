use std::{process::Command, thread::sleep, time::Duration};

use anyhow::{Context, Result, bail};
use assert_cmd::cargo::cargo_bin;
use tempfile::tempdir;

#[test]
fn daemon_start_runs_in_background() -> Result<()> {
    let runtime_dir = tempdir()?;
    let state_dir = tempdir()?;
    let binary = cargo_bin("pz");

    let start = Command::new(&binary)
        .args(["daemon", "start"])
        .env("PZ_RUNTIME_DIR", runtime_dir.path())
        .env("PZ_STATE_DIR", state_dir.path())
        .output()
        .context("failed to run daemon start")?;

    assert!(
        start.status.success(),
        "daemon start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );

    let stdout = String::from_utf8(start.stdout)?;
    assert!(stdout.contains("pz daemon started"), "{stdout}");

    let status = Command::new(&binary)
        .args(["daemon", "status"])
        .env("PZ_RUNTIME_DIR", runtime_dir.path())
        .env("PZ_STATE_DIR", state_dir.path())
        .output()
        .context("failed to run daemon status")?;

    assert!(
        status.status.success(),
        "daemon status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8(status.stdout)?;
    assert!(stdout.contains("pz daemon running"), "{stdout}");

    let status = Command::new(&binary)
        .args(["status"])
        .env("PZ_RUNTIME_DIR", runtime_dir.path())
        .env("PZ_STATE_DIR", state_dir.path())
        .output()
        .context("failed to run pz status")?;

    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8(status.stdout)?;
    assert!(stdout.contains("pz daemon: running"), "{stdout}");
    assert!(stdout.contains("processes:"), "{stdout}");

    let stop = Command::new(&binary)
        .args(["daemon", "stop"])
        .env("PZ_RUNTIME_DIR", runtime_dir.path())
        .env("PZ_STATE_DIR", state_dir.path())
        .output()
        .context("failed to run daemon stop")?;

    assert!(
        stop.status.success(),
        "daemon stop failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );

    wait_for_socket_removal(runtime_dir.path().join("pz.sock"))?;

    Ok(())
}

#[test]
fn logs_follow_prints_output_and_exits() -> Result<()> {
    let runtime_dir = tempdir()?;
    let state_dir = tempdir()?;
    let binary = cargo_bin("pz");

    run_pz(&binary, &runtime_dir, &state_dir, &["daemon", "start"])?;
    run_pz(
        &binary,
        &runtime_dir,
        &state_dir,
        &["run", "--", "/bin/echo", "followed"],
    )?;
    let logs = run_pz(&binary, &runtime_dir, &state_dir, &["logs", "1", "-f"])?;
    assert_eq!(String::from_utf8(logs.stdout)?, "followed\n");
    run_pz(&binary, &runtime_dir, &state_dir, &["daemon", "stop"])?;

    wait_for_socket_removal(runtime_dir.path().join("pz.sock"))?;

    Ok(())
}

#[test]
fn wait_exits_with_process_status() -> Result<()> {
    let runtime_dir = tempdir()?;
    let state_dir = tempdir()?;
    let binary = cargo_bin("pz");

    run_pz(&binary, &runtime_dir, &state_dir, &["daemon", "start"])?;
    run_pz(
        &binary,
        &runtime_dir,
        &state_dir,
        &["run", "--name", "wait-false", "--", "/usr/bin/env", "false"],
    )?;

    let wait = Command::new(&binary)
        .args(["wait", "wait-false"])
        .env("PZ_RUNTIME_DIR", runtime_dir.path())
        .env("PZ_STATE_DIR", state_dir.path())
        .output()
        .context("failed to run pz wait")?;
    assert_eq!(wait.status.code(), Some(1));

    run_pz(&binary, &runtime_dir, &state_dir, &["daemon", "stop"])?;
    wait_for_socket_removal(runtime_dir.path().join("pz.sock"))?;

    Ok(())
}

fn run_pz(
    binary: &std::path::Path,
    runtime_dir: &tempfile::TempDir,
    state_dir: &tempfile::TempDir,
    args: &[&str],
) -> Result<std::process::Output> {
    let output = Command::new(binary)
        .args(args)
        .env("PZ_RUNTIME_DIR", runtime_dir.path())
        .env("PZ_STATE_DIR", state_dir.path())
        .output()
        .with_context(|| format!("failed to run pz {}", args.join(" ")))?;

    assert!(
        output.status.success(),
        "pz {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );

    Ok(output)
}

fn wait_for_socket_removal(socket: std::path::PathBuf) -> Result<()> {
    for _ in 0..100 {
        if !socket.exists() {
            return Ok(());
        }

        sleep(Duration::from_millis(10));
    }

    bail!("daemon socket still exists at {}", socket.display())
}
