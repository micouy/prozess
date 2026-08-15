use std::{fmt, io::Write};

use anyhow::{Context, Result, bail};

use crate::client::Client;
use crate::protocol::{
    OutputChunk, OutputStream, ProcessSelector, ProcessStatus, Request, Response,
};

fn write_line<W: Write>(writer: &mut W, args: fmt::Arguments<'_>) -> Result<()> {
    Ok(writeln!(writer, "{args}")?)
}

pub async fn follow_logs(
    selector: Option<ProcessSelector>,
    stream: OutputStream,
    tail_lines: Option<usize>,
    since_ms: Option<i64>,
    all: bool,
    mut printer: OutputPrinter,
) -> Result<()> {
    let client = Client::new();
    let mut after_id = None;
    // First read only; later reads resume from the returned cursor.
    let mut tail_lines = tail_lines.map(|tail| tail as u64);
    let mut quiet_polls_after_exit = 0;

    loop {
        let (chunks, resume_after_id) = match client
            .send(Request::ReadLogs {
                selector: selector.clone(),
                stream,
                after_id,
                since_ms,
                until_ms: None,
                tail_lines: tail_lines.take(),
            })
            .await?
        {
            Response::Output {
                chunks,
                resume_after_id,
            } => (chunks, resume_after_id),
            Response::Error { message } => bail!(message),
            _ => bail!("daemon returned an unexpected logs response"),
        };
        let printed_any = !chunks.is_empty();
        printer.print(&chunks).await?;
        after_id = Some(resume_after_id);

        // Following everything has no exit condition: new processes can
        // appear at any time. Runs until interrupted.
        if !all {
            let selector = selector.clone().context("missing process selector")?;
            let is_running = match client.send(Request::ShowProcess { selector }).await? {
                Response::ProcessDetails(process) => process.status == ProcessStatus::Running,
                Response::Error { message } => bail!(message),
                _ => bail!("daemon returned an unexpected process response"),
            };

            if is_running || printed_any {
                quiet_polls_after_exit = 0;
            } else {
                quiet_polls_after_exit += 1;
            }

            if !is_running && quiet_polls_after_exit >= 3 {
                break;
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    printer.flush_partial_lines()?;

    Ok(())
}

/// Prints chunks either raw (single-process mode) or line-buffered with a
/// `name | ` prefix (all-processes mode), where buffering prevents a chunk
/// ending mid-line from gluing another process's prefix onto its tail.
pub struct OutputPrinter {
    prefixed: bool,
    labels: std::collections::HashMap<i64, String>,
    pending: std::collections::HashMap<i64, Vec<u8>>,
}

impl OutputPrinter {
    pub fn raw() -> Self {
        Self {
            prefixed: false,
            labels: Default::default(),
            pending: Default::default(),
        }
    }

    pub fn prefixed() -> Self {
        Self {
            prefixed: true,
            ..Self::raw()
        }
    }

    pub async fn print(&mut self, chunks: &[OutputChunk]) -> Result<()> {
        if !self.prefixed {
            let mut stdout = std::io::stdout().lock();
            return print_output(chunks, &mut stdout);
        }

        let mut stdout = std::io::stdout().lock();

        for chunk in chunks {
            let label = self.label(chunk.process_id).await?;
            let buffer = self.pending.entry(chunk.process_id).or_default();
            buffer.extend_from_slice(&chunk.data);

            while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
                let line = buffer.drain(..=newline).collect::<Vec<_>>();
                stdout.write_all(format!("{label} | ").as_bytes())?;
                stdout.write_all(&line)?;
            }
        }
        stdout.flush()?;

        Ok(())
    }

    async fn label(&mut self, process_id: i64) -> Result<String> {
        if let Some(label) = self.labels.get(&process_id) {
            return Ok(label.clone());
        }

        let label = match Client::new()
            .send(Request::ShowProcess {
                selector: ProcessSelector::Id(process_id),
            })
            .await?
        {
            Response::ProcessDetails(process) => {
                process.name.unwrap_or_else(|| format!("#{}", process.id))
            }
            _ => format!("#{process_id}"),
        };
        self.labels.insert(process_id, label.clone());

        Ok(label)
    }

    pub fn flush_partial_lines(&mut self) -> Result<()> {
        let mut stdout = std::io::stdout().lock();

        for (process_id, buffer) in std::mem::take(&mut self.pending) {
            if buffer.is_empty() {
                continue;
            }

            let label = self
                .labels
                .get(&process_id)
                .cloned()
                .unwrap_or_else(|| format!("#{process_id}"));
            stdout.write_all(format!("{label} | ").as_bytes())?;
            stdout.write_all(&buffer)?;
            stdout.write_all(b"\n")?;
        }
        stdout.flush()?;

        Ok(())
    }
}

pub fn print_response(response: Result<Response>) -> Result<()> {
    let mut stdout = std::io::stdout().lock();

    match response? {
        Response::DaemonStatus {
            pid,
            socket,
            database,
        } => {
            write_line(&mut stdout, format_args!("pz daemon running"))?;
            write_line(&mut stdout, format_args!("pid: {pid}"))?;
            write_line(&mut stdout, format_args!("socket: {socket}"))?;
            write_line(&mut stdout, format_args!("db: {database}"))?;
        }
        Response::DaemonStopping => write_line(&mut stdout, format_args!("pz daemon stopped"))?,
        Response::Spawned(process) => {
            write_line(&mut stdout, format_args!("spawned process {}", process.id))?;
            if let Some(name) = process.name {
                write_line(&mut stdout, format_args!("name: {name}"))?;
            }
            write_line(&mut stdout, format_args!("status: {}", process.status))?;
            if let Some(timeout_ms) = process.timeout_ms {
                write_line(
                    &mut stdout,
                    format_args!("timeout: {}", format_duration_ms(timeout_ms)),
                )?;
            }
            write_line(
                &mut stdout,
                format_args!("command: {}", process.command.join(" ")),
            )?;
        }
        Response::StoppedProcess { id, signal } => {
            write_line(&mut stdout, format_args!("stopped process {id}"))?;
            write_line(&mut stdout, format_args!("signal: {signal}"))?;
        }
        Response::TimeoutUpdated { id, timeout_ms } => {
            if let Some(timeout_ms) = timeout_ms {
                write_line(&mut stdout, format_args!("timeout set for process {id}"))?;
                write_line(
                    &mut stdout,
                    format_args!("timeout: {}", format_duration_ms(timeout_ms)),
                )?;
            } else {
                write_line(
                    &mut stdout,
                    format_args!("timeout cleared for process {id}"),
                )?;
            }
        }
        Response::WaitedProcess(process) => print_process_details(&process, &mut stdout)?,
        Response::ProcessList(processes) => print_process_list(&processes, &mut stdout)?,
        Response::ProcessDetails(process) => print_process_details(&process, &mut stdout)?,
        Response::ResourceSnapshot(snapshot) => print_resource_snapshot(&snapshot, &mut stdout)?,
        Response::PortList(ports) => print_ports(&ports, &mut stdout)?,
        Response::Output { chunks, .. } => {
            print_output(&chunks, &mut stdout)?;
        }
        Response::Error { message } => bail!(message),
    }

    Ok(())
}

fn print_ports<W: Write>(ports: &crate::protocol::PortList, stdout: &mut W) -> Result<()> {
    write_line(stdout, format_args!("id: {}", ports.process_id))?;
    if let Some(name) = &ports.name {
        write_line(stdout, format_args!("name: {name}"))?;
    }
    write_line(stdout, format_args!("status: {}", ports.status))?;

    if ports.status != ProcessStatus::Running {
        write_line(
            stdout,
            format_args!("ports: unavailable for non-running process"),
        )?;
        return Ok(());
    }

    if ports.unavailable {
        write_line(stdout, format_args!("ports: unavailable"))?;
        return Ok(());
    }

    if ports.ports.is_empty() {
        write_line(stdout, format_args!("ports: none"))?;
        return Ok(());
    }

    write_line(stdout, format_args!(""))?;
    write_line(
        stdout,
        format_args!("{:<6} {:<8} {:<22} PIDS", "PROTO", "STATE", "LOCAL"),
    )?;
    for port in &ports.ports {
        write_line(
            stdout,
            format_args!(
                "{:<6} {:<8} {:<22} {}",
                port.protocol,
                port.state,
                format!("{}:{}", port.local_addr, port.local_port),
                port.pids
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        )?;
    }
    Ok(())
}

fn print_resource_snapshot<W: Write>(
    snapshot: &crate::protocol::ResourceSnapshot,
    stdout: &mut W,
) -> Result<()> {
    write_line(stdout, format_args!("id: {}", snapshot.process_id))?;
    if let Some(name) = &snapshot.name {
        write_line(stdout, format_args!("name: {name}"))?;
    }
    write_line(stdout, format_args!("status: {}", snapshot.status))?;
    write_line(
        stdout,
        format_args!(
            "pid: {}",
            snapshot
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_owned())
        ),
    )?;
    write_line(
        stdout,
        format_args!(
            "pgid: {}",
            snapshot
                .pgid
                .map(|pgid| pgid.to_string())
                .unwrap_or_else(|| "-".to_owned())
        ),
    )?;

    if snapshot.status != ProcessStatus::Running {
        write_line(
            stdout,
            format_args!("resources: unavailable for non-running process"),
        )?;
        return Ok(());
    }

    write_line(
        stdout,
        format_args!("processes: {}", snapshot.process_count),
    )?;
    write_line(
        stdout,
        format_args!("memory: {}", format_bytes(snapshot.total_memory_bytes)),
    )?;
    write_line(
        stdout,
        format_args!("cpu: {:.1}%", snapshot.total_cpu_percent),
    )?;

    if snapshot.processes.is_empty() {
        return Ok(());
    }

    write_line(stdout, format_args!(""))?;
    write_line(
        stdout,
        format_args!("{:<8} {:<8} {:<8} {:<10} NAME", "PID", "PPID", "CPU", "MEM"),
    )?;
    for process in &snapshot.processes {
        write_line(
            stdout,
            format_args!(
                "{:<8} {:<8} {:<8} {:<10} {}",
                process.pid,
                process
                    .parent_pid
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "-".to_owned()),
                format!("{:.1}%", process.cpu_percent),
                format_bytes(process.memory_bytes),
                process.name,
            ),
        )?;
    }
    Ok(())
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;

    if bytes >= GIB {
        format!("{:.1} GB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn print_output<W: Write>(chunks: &[OutputChunk], stdout: &mut W) -> Result<()> {
    for chunk in chunks {
        stdout.write_all(&chunk.data)?;
        stdout.flush()?;
    }

    Ok(())
}

fn print_process_details<W: Write>(
    process: &crate::protocol::ProcessDetails,
    stdout: &mut W,
) -> Result<()> {
    let pid = process
        .pid
        .map(|pid| pid.to_string())
        .unwrap_or_else(|| "-".to_owned());
    let exit = process
        .exit_code
        .map(|exit| exit.to_string())
        .unwrap_or_else(|| "-".to_owned());
    let pgid = process
        .pgid
        .map(|pgid| pgid.to_string())
        .unwrap_or_else(|| "-".to_owned());

    write_line(stdout, format_args!("id: {}", process.id))?;
    if let Some(name) = &process.name {
        write_line(stdout, format_args!("name: {name}"))?;
    }
    write_line(stdout, format_args!("status: {}", process.status))?;
    write_line(stdout, format_args!("pid: {pid}"))?;
    write_line(stdout, format_args!("pgid: {pgid}"))?;
    write_line(stdout, format_args!("exit: {exit}"))?;
    write_line(
        stdout,
        format_args!("command: {}", process.command.join(" ")),
    )?;
    write_line(stdout, format_args!("cwd: {}", process.cwd))?;
    if let Some(timeout_ms) = process.timeout_ms {
        write_line(
            stdout,
            format_args!("timeout: {}", format_duration_ms(timeout_ms)),
        )?;
    }
    write_line(
        stdout,
        format_args!("inherit env: {}", process.env.inherit_env),
    )?;

    if !process.env.env_files.is_empty() {
        write_line(
            stdout,
            format_args!("env files: {}", process.env.env_files.join(", ")),
        )?;
    }

    if !process.env.env_keys.is_empty() {
        write_line(
            stdout,
            format_args!("env overrides: {}", process.env.env_keys.join(", ")),
        )?;
    }

    if let Some(error) = &process.error_message {
        write_line(stdout, format_args!("error: {error}"))?;
    }
    Ok(())
}

fn print_process_list<W: Write>(
    processes: &[crate::protocol::ProcessSummary],
    stdout: &mut W,
) -> Result<()> {
    write_line(
        stdout,
        format_args!(
            "{:<3} {:<16} {:<12} {:<12} {:<6} {:<5} COMMAND / ERROR",
            "ID", "NAME", "STATUS", "PORTS", "PID", "EXIT"
        ),
    )?;

    for process in processes {
        let pid = process
            .pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "-".to_owned());
        let exit = process
            .exit_code
            .map(|exit| exit.to_string())
            .unwrap_or_else(|| "-".to_owned());

        let command = if let Some(error) = &process.error_message {
            format!("{} ({error})", process.command.join(" "))
        } else {
            process.command.join(" ")
        };

        write_line(
            stdout,
            format_args!(
                "{:<3} {:<16} {:<12} {:<12} {:<6} {:<5} {}",
                process.id,
                process.name.as_deref().unwrap_or("-"),
                process.status,
                format_ports(process.ports_unavailable, &process.ports),
                pid,
                exit,
                command
            ),
        )?;
    }
    Ok(())
}

fn format_ports(unavailable: bool, ports: &[u16]) -> String {
    if unavailable {
        "?".to_owned()
    } else if ports.is_empty() {
        "-".to_owned()
    } else {
        ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn format_duration_ms(timeout_ms: u64) -> String {
    if timeout_ms.is_multiple_of(3_600_000) {
        format!("{}h", timeout_ms / 3_600_000)
    } else if timeout_ms.is_multiple_of(60_000) {
        format!("{}m", timeout_ms / 60_000)
    } else if timeout_ms.is_multiple_of(1_000) {
        format!("{}s", timeout_ms / 1_000)
    } else {
        format!("{timeout_ms}ms")
    }
}
