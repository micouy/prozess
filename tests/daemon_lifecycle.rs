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
fn daemon_creates_private_runtime_dir() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let base = tempdir()?;
    let runtime_dir = base.path().join("nested").join("run");
    let state_dir = tempdir()?;
    let binary = cargo_bin("pz");

    let start = Command::new(&binary)
        .args(["daemon", "start"])
        .env("PZ_RUNTIME_DIR", &runtime_dir)
        .env("PZ_STATE_DIR", state_dir.path())
        .output()
        .context("failed to run daemon start")?;
    assert!(
        start.status.success(),
        "daemon start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );

    let mode = std::fs::metadata(&runtime_dir)?.permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "runtime dir should be private, got {mode:o}");

    let stop = Command::new(&binary)
        .args(["daemon", "stop"])
        .env("PZ_RUNTIME_DIR", &runtime_dir)
        .env("PZ_STATE_DIR", state_dir.path())
        .output()
        .context("failed to run daemon stop")?;
    assert!(stop.status.success());
    wait_for_socket_removal(runtime_dir.join("pz.sock"))?;

    Ok(())
}

#[test]
fn daemon_start_reports_startup_failure_cause() -> Result<()> {
    let runtime_dir = tempdir()?;
    let state_dir = tempdir()?;
    let binary = cargo_bin("pz");

    std::fs::write(state_dir.path().join("pz.sqlite"), b"not a database")?;

    let start = Command::new(&binary)
        .args(["daemon", "start"])
        .env("PZ_RUNTIME_DIR", runtime_dir.path())
        .env("PZ_STATE_DIR", state_dir.path())
        .output()
        .context("failed to run daemon start")?;

    assert!(!start.status.success());
    let stderr = String::from_utf8(start.stderr)?;
    assert!(stderr.contains("file is not a database"), "{stderr}");

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
fn logs_follow_tail_replays_only_requested_lines() -> Result<()> {
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
    wait_for_logs(&binary, &runtime_dir, &state_dir, &["logs", "1"], |logs| {
        logs == "one\ntwo\nthree\n"
    })?;

    // -f --tail 2 replays the last two lines, then exits (process is done).
    let logs = run_pz(
        &binary,
        &runtime_dir,
        &state_dir,
        &["logs", "1", "-f", "--tail", "2"],
    )?;
    assert_eq!(String::from_utf8(logs.stdout)?, "two\nthree\n");

    let logs = run_pz(
        &binary,
        &runtime_dir,
        &state_dir,
        &["logs", "1", "-f", "--tail", "0"],
    )?;
    assert_eq!(String::from_utf8(logs.stdout)?, "");

    let output = Command::new(&binary)
        .args(["logs", "1", "-f", "--until", "10s"])
        .env("PZ_RUNTIME_DIR", runtime_dir.path())
        .env("PZ_STATE_DIR", state_dir.path())
        .output()
        .context("failed to run pz logs")?;
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("cannot be used with"), "{stderr}");

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
    wait_for_logs(
        &binary,
        &runtime_dir,
        &state_dir,
        &["logs", "1", "--tail", "2"],
        |logs| logs == "two\nthree\n",
    )?;
    run_pz(&binary, &runtime_dir, &state_dir, &["daemon", "stop"])?;

    wait_for_socket_removal(runtime_dir.path().join("pz.sock"))?;

    Ok(())
}

#[test]
fn logs_exits_quietly_when_consumer_closes_pipe() -> Result<()> {
    let runtime_dir = tempdir()?;
    let state_dir = tempdir()?;
    let binary = cargo_bin("pz");

    run_pz(&binary, &runtime_dir, &state_dir, &["daemon", "start"])?;
    // Produce well over 64 KiB of output so `pz logs` always overflows the
    // pipe buffer and hits EPIPE once the consumer is gone.
    run_pz(
        &binary,
        &runtime_dir,
        &state_dir,
        &["run", "--", "/usr/bin/seq", "1", "30000"],
    )?;
    run_pz(&binary, &runtime_dir, &state_dir, &["wait", "1"])?;
    wait_for_logs(&binary, &runtime_dir, &state_dir, &["logs", "1"], |logs| {
        logs.ends_with("30000\n")
    })?;

    // Simulate `pz logs | head`: close the read end without consuming.
    let mut child = Command::new(&binary)
        .args(["logs", "1"])
        .env("PZ_RUNTIME_DIR", runtime_dir.path())
        .env("PZ_STATE_DIR", state_dir.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn pz logs")?;
    drop(child.stdout.take());
    let output = child
        .wait_with_output()
        .context("failed to wait for pz logs")?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "pz logs should exit 0 on broken pipe: {stderr}"
    );
    assert!(stderr.is_empty(), "expected no stderr, got: {stderr}");

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
    assert!(stdout.contains("daemon-backed process manager"), "{stdout}");
    assert!(stdout.contains("explicit environment control"), "{stdout}");

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
    assert!(stderr.contains("daemon-backed process manager"), "{stderr}");

    Ok(())
}

#[test]
fn wrong_subcommand_arg_prints_subcommand_help() -> Result<()> {
    let binary = cargo_bin("pz");
    let output = Command::new(&binary)
        .args(["logs", "my-app", "--stream", "stdout"])
        .output()
        .context("failed to run wrong pz logs command")?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("unexpected argument '--stream'"),
        "{stderr}"
    );
    assert!(stderr.contains("Usage: logs"), "{stderr}");
    assert!(stderr.contains("--tail <TAIL>"), "{stderr}");
    assert!(
        stderr.contains("Blocks until the process exits"),
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

    wait_for_logs(
        &binary,
        &runtime_dir,
        &state_dir,
        &["logs", "threaded-memory", "stdout"],
        |logs| logs.contains("ready"),
    )?;

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

/// Polls `pz logs` until its stdout satisfies `matches`.
///
/// `pz wait` returns on process exit, but output capture is asynchronous,
/// so logs may reach the store slightly later. Returns the matching output,
/// or bails with the last observed output after ~5s.
fn wait_for_logs(
    binary: &std::path::Path,
    runtime_dir: &tempfile::TempDir,
    state_dir: &tempfile::TempDir,
    args: &[&str],
    matches: impl Fn(&str) -> bool,
) -> Result<String> {
    let mut logs = String::new();

    for _ in 0..100 {
        let output = run_pz(binary, runtime_dir, state_dir, args)?;
        logs = String::from_utf8(output.stdout)?;
        if matches(&logs) {
            return Ok(logs);
        }

        sleep(Duration::from_millis(50));
    }

    bail!("pz {} never matched, last output: {logs:?}", args.join(" "))
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
