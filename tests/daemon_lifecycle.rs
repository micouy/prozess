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
fn logs_tail_limits_output_lines() -> Result<()> {
    let runtime_dir = tempdir()?;
    let state_dir = tempdir()?;
    let binary = cargo_bin("pz");

    run_pz(&binary, &runtime_dir, &state_dir, &["daemon", "start"])?;
    run_pz(
        &binary,
        &runtime_dir,
        &state_dir,
        &["run", "--", "/usr/bin/printf", "one\\ntwo\\nthree\\n"],
    )?;
    run_pz(&binary, &runtime_dir, &state_dir, &["wait", "1"])?;
    let logs = run_pz(
        &binary,
        &runtime_dir,
        &state_dir,
        &["logs", "1", "--tail", "2"],
    )?;
    assert_eq!(String::from_utf8(logs.stdout)?, "two\nthree\n");
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

#[test]
fn help_guides_agents_to_managed_processes() -> Result<()> {
    let binary = cargo_bin("pz");
    let help = Command::new(&binary)
        .arg("--help")
        .output()
        .context("failed to run pz help")?;

    assert!(help.status.success());
    let stdout = String::from_utf8(help.stdout)?;
    assert!(
        stdout.contains("Agent-friendly process manager"),
        "{stdout}"
    );
    assert!(
        stdout.contains("pz run --name <name> -- <command>"),
        "{stdout}"
    );
    assert!(stdout.contains("--inherit-env"), "{stdout}");

    Ok(())
}

#[test]
fn wrong_top_level_command_prints_help() -> Result<()> {
    let binary = cargo_bin("pz");
    let output = Command::new(&binary)
        .arg("status")
        .output()
        .context("failed to run wrong pz command")?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("unrecognized subcommand"), "{stderr}");
    assert!(
        stderr.contains("Agent-friendly process manager"),
        "{stderr}"
    );

    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn resources_do_not_count_threads_as_process_memory() -> Result<()> {
    let runtime_dir = tempdir()?;
    let state_dir = tempdir()?;
    let work_dir = tempdir()?;
    let script = work_dir.path().join("threaded.py");
    let binary = cargo_bin("pz");

    std::fs::write(
        &script,
        r#"
import threading
import time

buf = bytearray(64 * 1024 * 1024)
threads = []
for _ in range(32):
    thread = threading.Thread(target=time.sleep, args=(30,), daemon=True)
    thread.start()
    threads.append(thread)

print("ready", flush=True)
time.sleep(30)
"#,
    )?;

    run_pz(&binary, &runtime_dir, &state_dir, &["daemon", "start"])?;
    run_pz(
        &binary,
        &runtime_dir,
        &state_dir,
        &[
            "run",
            "--name",
            "threaded-memory",
            "--",
            "/usr/bin/python3",
            script.to_str().context("script path should be utf-8")?,
        ],
    )?;

    for _ in 0..100 {
        let logs = run_pz(
            &binary,
            &runtime_dir,
            &state_dir,
            &["logs", "threaded-memory", "stdout"],
        )?;
        if String::from_utf8(logs.stdout)?.contains("ready") {
            break;
        }
        sleep(Duration::from_millis(50));
    }

    let resources = run_pz(
        &binary,
        &runtime_dir,
        &state_dir,
        &["resources", "threaded-memory"],
    )?;
    let resources = String::from_utf8(resources.stdout)?;
    assert!(resources.contains("status: running"), "{resources}");
    assert!(resources.contains("processes: 1"), "{resources}");
    assert_memory_below(&resources, 512.0)?;

    run_pz(
        &binary,
        &runtime_dir,
        &state_dir,
        &["stop", "threaded-memory", "--force"],
    )?;
    run_pz(&binary, &runtime_dir, &state_dir, &["daemon", "stop"])?;
    wait_for_socket_removal(runtime_dir.path().join("pz.sock"))?;

    Ok(())
}

#[cfg(target_os = "linux")]
fn assert_memory_below(resources: &str, max_mb: f64) -> Result<()> {
    let memory = resources
        .lines()
        .find_map(|line| line.strip_prefix("memory: "))
        .context("resources should include memory line")?;
    let mut parts = memory.split_whitespace();
    let value = parts
        .next()
        .context("memory should include value")?
        .parse::<f64>()?;
    let unit = parts.next().context("memory should include unit")?;
    let mb = match unit {
        "GB" => value * 1024.0,
        "MB" => value,
        "KB" => value / 1024.0,
        "B" => value / 1024.0 / 1024.0,
        other => bail!("unexpected memory unit {other:?}"),
    };

    assert!(
        mb < max_mb,
        "expected memory below {max_mb} MB, got {memory}\n{resources}"
    );

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
