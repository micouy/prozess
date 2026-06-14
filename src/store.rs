use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use rusqlite_migration::{M, Migrations};

use crate::protocol::{
    OutputChunk, OutputStream, ProcessDetails, ProcessEnvSummary, ProcessStatus, ProcessSummary,
    RunSpec,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeoutSpec {
    pub duration_ms: u64,
    pub deadline_ms: i64,
}

#[derive(Debug, Clone)]
pub struct StoreConfig {
    pub database_path: PathBuf,
}

#[derive(Debug)]
pub struct Store {
    database_path: PathBuf,
}

impl Store {
    pub fn open(config: StoreConfig) -> Result<Self> {
        if let Some(parent) = config.database_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create state directory {}", parent.display())
            })?;
        }

        let mut connection = Connection::open(&config.database_path).with_context(|| {
            format!("failed to open database {}", config.database_path.display())
        })?;
        connection
            .pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))
            .context("failed to enable WAL")?;
        configure_connection(&connection)?;
        migrations()
            .to_latest(&mut connection)
            .context("failed to migrate database")?;

        Ok(Self {
            database_path: config.database_path,
        })
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub fn insert_process(
        &self,
        name: Option<&str>,
        command: &[String],
        cwd: &Path,
        pid: u32,
        pgid: u32,
        inherit_env: bool,
        env_files: &[String],
        env_keys: &[String],
    ) -> Result<ProcessSummary> {
        let id = self.reserve_process(name, command, cwd, inherit_env, env_files, env_keys)?;

        self.activate_process(id, pid, pgid, None)
    }

    /// Claims the name and creates the row before anything is spawned, so
    /// a name conflict can never leave an untracked child behind. The row
    /// is `running` with no pid until `activate_process`.
    pub fn reserve_process(
        &self,
        name: Option<&str>,
        command: &[String],
        cwd: &Path,
        inherit_env: bool,
        env_files: &[String],
        env_keys: &[String],
    ) -> Result<i64> {
        let connection = self.connect()?;
        let command_json = serde_json::to_string(command).context("failed to encode command")?;
        let env_files_json =
            serde_json::to_string(env_files).context("failed to encode env files")?;
        let env_keys_json = serde_json::to_string(env_keys).context("failed to encode env keys")?;
        let cwd = cwd.display().to_string();

        let result = connection.execute(
            "
            INSERT INTO processes (
                name, command, cwd, status, inherit_env, env_files, env_keys
            )
            VALUES (?1, ?2, ?3, 'running', ?4, ?5, ?6)
            ",
            params![
                name,
                command_json,
                cwd,
                inherit_env,
                env_files_json,
                env_keys_json,
            ],
        );

        match result {
            Ok(_) => Ok(connection.last_insert_rowid()),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                let name = name.unwrap_or("?");
                anyhow::bail!(
                    "a running process named {name:?} already exists; pass --replace to take over the name"
                )
            }
            Err(error) => Err(error).context("failed to reserve process"),
        }
    }

    pub fn activate_process(
        &self,
        id: i64,
        pid: u32,
        pgid: u32,
        pid_started_at: Option<i64>,
    ) -> Result<ProcessSummary> {
        let connection = self.connect()?;

        connection
            .execute(
                "
                UPDATE processes
                SET pid = ?1, pgid = ?2, pid_started_at = ?3, started_at = CURRENT_TIMESTAMP
                WHERE id = ?4
                ",
                params![pid, pgid, pid_started_at, id],
            )
            .context("failed to activate process")?;

        self.get_process(id)
    }

    pub fn mark_spawn_failed(&self, id: i64, error_message: &str) -> Result<()> {
        let connection = self.connect()?;

        connection
            .execute(
                "
                UPDATE processes
                SET status = 'failed', error_message = ?1, finished_at = CURRENT_TIMESTAMP
                WHERE id = ?2
                ",
                params![error_message, id],
            )
            .context("failed to mark process as failed")?;

        Ok(())
    }

    /// `(id, pid, pid_started_at)` of every lost generation of `name` that
    /// still has a recorded pid, newest first.
    pub fn lost_generations(&self, name: &str) -> Result<Vec<(i64, u32, Option<i64>)>> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "
                SELECT id, pid, pid_started_at FROM processes
                WHERE name = ?1 AND status = 'lost' AND pid IS NOT NULL
                ORDER BY id DESC
                ",
            )
            .context("failed to prepare lost generations query")?;
        let generations = statement
            .query_map([name], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .context("failed to query lost generations")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to decode lost generations")?;

        Ok(generations)
    }

    pub fn find_running_by_name(&self, name: &str) -> Result<Option<i64>> {
        let connection = self.connect()?;

        connection
            .query_row(
                "SELECT id FROM processes WHERE name = ?1 AND status = 'running'",
                [name],
                |row| row.get(0),
            )
            .optional()
            .context("failed to look up running process by name")
    }

    pub fn mark_process_finished(&self, id: i64, exit_code: Option<i32>) -> Result<()> {
        let connection = self.connect()?;

        connection
            .execute(
                "
                UPDATE processes
                SET status = 'exited', exit_code = ?1, finished_at = CURRENT_TIMESTAMP
                WHERE id = ?2 AND status = 'running'
                ",
                params![exit_code, id],
            )
            .context("failed to mark process finished")?;

        Ok(())
    }

    pub fn mark_process_killed(&self, id: i64) -> Result<()> {
        let connection = self.connect()?;

        connection
            .execute(
                "
                UPDATE processes
                SET status = 'killed', finished_at = CURRENT_TIMESTAMP
                WHERE id = ?1
                ",
                [id],
            )
            .context("failed to mark process killed")?;

        Ok(())
    }

    pub fn mark_process_timed_out(&self, id: i64) -> Result<()> {
        let connection = self.connect()?;

        connection
            .execute(
                "
                UPDATE processes
                SET status = 'timed_out', finished_at = CURRENT_TIMESTAMP
                WHERE id = ?1 AND status = 'running'
                ",
                [id],
            )
            .context("failed to mark process timed out")?;

        Ok(())
    }

    pub fn mark_running_processes_lost(&self) -> Result<usize> {
        let connection = self.connect()?;
        let changed = connection
            .execute(
                "
                UPDATE processes
                SET status = 'lost', finished_at = CURRENT_TIMESTAMP
                WHERE status = 'running'
                ",
                [],
            )
            .context("failed to mark running processes lost")?;

        Ok(changed)
    }

    pub fn set_timeout(&self, id: i64, timeout: Option<TimeoutSpec>) -> Result<()> {
        let connection = self.connect()?;
        let timeout_ms = sql_timeout_ms(timeout.map(|timeout| timeout.duration_ms))?;
        let timeout_at_ms = timeout.map(|timeout| timeout.deadline_ms);

        connection
            .execute(
                "
                UPDATE processes
                SET timeout_ms = ?1, timeout_at_ms = ?2
                WHERE id = ?3
                ",
                params![timeout_ms, timeout_at_ms, id],
            )
            .context("failed to update process timeout")?;

        Ok(())
    }

    pub fn get_process(&self, id: i64) -> Result<ProcessSummary> {
        let connection = self.connect()?;

        connection
            .query_row(
                "SELECT id, name, status, pid, pgid, exit_code, command, error_message, timeout_ms, timeout_at_ms, inherit_env, env_files, env_keys FROM processes WHERE id = ?1",
                [id],
                process_summary_from_row,
            )
            .context("failed to get process")
    }

    /// `live_only` restricts the query to running and lost rows, so the
    /// finished history is never loaded.
    pub fn list_processes(&self, live_only: bool) -> Result<Vec<ProcessSummary>> {
        let connection = self.connect()?;
        let sql = if live_only {
            "
            SELECT id, name, status, pid, pgid, exit_code, command, error_message, timeout_ms, timeout_at_ms, inherit_env, env_files, env_keys
            FROM processes
            WHERE status IN ('running', 'lost')
            ORDER BY id DESC
            "
        } else {
            "
            SELECT id, name, status, pid, pgid, exit_code, command, error_message, timeout_ms, timeout_at_ms, inherit_env, env_files, env_keys
            FROM processes
            ORDER BY id DESC
            "
        };
        let mut statement = connection
            .prepare(sql)
            .context("failed to prepare process list query")?;

        let processes = statement
            .query_map([], process_summary_from_row)
            .context("failed to list processes")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to read process list")?;

        Ok(processes)
    }

    pub fn get_process_details(&self, id: i64) -> Result<ProcessDetails> {
        let connection = self.connect()?;

        connection
            .query_row(
                "
                SELECT id, name, status, pid, pgid, exit_code, command, error_message, timeout_ms, timeout_at_ms, cwd, inherit_env, env_files, env_keys
                FROM processes
                WHERE id = ?1
                ",
                [id],
                process_details_from_row,
            )
            .with_context(|| format!("failed to get process {id}"))
    }

    pub fn restart_spec(&self, id: i64) -> Result<RunSpec> {
        let details = self.get_process_details(id)?;
        if !details.env.env_keys.is_empty() {
            anyhow::bail!(
                "cannot restart process {id}: inline env values were not stored; use env files or config for restartable processes"
            );
        }

        Ok(RunSpec {
            name: details.name,
            replace: false,
            timeout_ms: details.timeout_ms,
            command: details.command,
            cwd: details.cwd,
            inherit_env: details.env.inherit_env,
            env_files: details.env.env_files,
            env: Vec::new(),
        })
    }

    pub fn resolve_process_id(&self, selector: &crate::protocol::ProcessSelector) -> Result<i64> {
        match selector {
            crate::protocol::ProcessSelector::Id(id) => Ok(*id),
            crate::protocol::ProcessSelector::Name(name) => {
                let connection = self.connect()?;
                connection
                    .query_row(
                        "
                        SELECT id
                        FROM processes
                        WHERE name = ?1
                        ORDER BY CASE status WHEN 'running' THEN 0 ELSE 1 END, id DESC
                        LIMIT 1
                        ",
                        [name],
                        |row| row.get(0),
                    )
                    .with_context(|| format!("failed to resolve process name {name:?}"))
            }
        }
    }

    pub fn insert_output_chunk(
        &self,
        process_id: i64,
        stream: OutputStream,
        chunk: &[u8],
    ) -> Result<()> {
        let connection = self.connect()?;
        let created_at_ms = now_ms()?;

        connection
            .execute(
                "
                INSERT INTO process_output (process_id, stream, chunk, created_at_ms)
                VALUES (?1, ?2, ?3, ?4)
                ",
                params![process_id, stream_name(stream), chunk, created_at_ms],
            )
            .context("failed to insert output chunk")?;

        Ok(())
    }

    /// Returns matching chunks and the cursor to resume from (valid even
    /// when nothing matched). `process_id: None` reads across every
    /// process. `tail_lines` overrides `after_id`: only the last N lines
    /// of the window are read (0 = none).
    pub fn read_output(
        &self,
        process_id: Option<i64>,
        stream: OutputStream,
        after_id: Option<i64>,
        since_ms: Option<i64>,
        until_ms: Option<i64>,
        tail_lines: Option<u64>,
    ) -> Result<(Vec<OutputChunk>, i64)> {
        let connection = self.connect()?;
        // One snapshot for both the seek and the read, so chunks landing
        // in between cannot widen "the last N lines".
        let connection = connection
            .unchecked_transaction()
            .context("failed to begin read transaction")?;
        let (start, trim_lines) = match tail_lines {
            Some(tail) => seek_tail(&connection, process_id, stream, since_ms, until_ms, tail)?,
            None => (after_id.unwrap_or(0), 0),
        };
        let (sql, stream_filter) = match stream {
            OutputStream::All => (
                "
                SELECT id, process_id, stream, chunk
                FROM process_output
                WHERE (?1 IS NULL OR process_id = ?1)
                    AND id > ?2
                    AND (?3 IS NULL OR created_at_ms >= ?3)
                    AND (?4 IS NULL OR created_at_ms <= ?4)
                ORDER BY id ASC
                ",
                None,
            ),
            OutputStream::Stdout | OutputStream::Stderr => (
                "
                SELECT id, process_id, stream, chunk
                FROM process_output
                WHERE (?1 IS NULL OR process_id = ?1)
                    AND id > ?2
                    AND (?3 IS NULL OR created_at_ms >= ?3)
                    AND (?4 IS NULL OR created_at_ms <= ?4)
                    AND stream = ?5
                ORDER BY id ASC
                ",
                Some(stream_name(stream)),
            ),
        };
        let mut statement = connection
            .prepare(sql)
            .context("failed to prepare output query")?;

        let mut chunks = if let Some(stream_filter) = stream_filter {
            statement
                .query_map(
                    params![process_id, start, since_ms, until_ms, stream_filter],
                    output_chunk_from_row,
                )
                .context("failed to read output")?
                .collect::<rusqlite::Result<Vec<_>>>()
        } else {
            statement
                .query_map(
                    params![process_id, start, since_ms, until_ms],
                    output_chunk_from_row,
                )
                .context("failed to read output")?
                .collect::<rusqlite::Result<Vec<_>>>()
        }
        .context("failed to decode output")?;

        if trim_lines > 0 && !chunks.is_empty() {
            chunks[0].data = trim_leading_lines(&chunks[0].data, trim_lines);
            if chunks[0].data.is_empty() {
                chunks.remove(0);
            }
        }
        let resume_after_id = chunks.last().map(|chunk| chunk.id).unwrap_or(start);

        Ok((chunks, resume_after_id))
    }

    /// The pid identity token recorded at spawn time, if any. Compare with
    /// `pid_identity::current_token` before trusting that the stored pid
    /// still refers to the same process.
    pub fn pid_identity(&self, id: i64) -> Result<Option<i64>> {
        let connection = self.connect()?;

        connection
            .query_row(
                "SELECT pid_started_at FROM processes WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .with_context(|| format!("failed to read pid identity for process {id}"))
    }

    fn connect(&self) -> Result<Connection> {
        let connection = Connection::open(&self.database_path)
            .with_context(|| format!("failed to open database {}", self.database_path.display()))?;
        configure_connection(&connection)?;

        Ok(connection)
    }
}

impl Clone for Store {
    fn clone(&self) -> Self {
        Self {
            database_path: self.database_path.clone(),
        }
    }
}

fn parse_status(status: &str) -> ProcessStatus {
    match status {
        "running" => ProcessStatus::Running,
        "exited" => ProcessStatus::Exited,
        "failed" => ProcessStatus::Failed,
        "killed" => ProcessStatus::Killed,
        "timed_out" => ProcessStatus::TimedOut,
        "lost" => ProcessStatus::Lost,
        _ => ProcessStatus::Failed,
    }
}

fn parse_stream(stream: &str) -> OutputStream {
    match stream {
        "stdout" => OutputStream::Stdout,
        "stderr" => OutputStream::Stderr,
        _ => OutputStream::All,
    }
}

fn stream_name(stream: OutputStream) -> &'static str {
    match stream {
        OutputStream::All => "all",
        OutputStream::Stdout => "stdout",
        OutputStream::Stderr => "stderr",
    }
}

/// Returns `(start, trim_lines)`: the `id > start` position covering the
/// last `tail_lines` lines of the window, and how many surplus leading
/// lines of the boundary chunk to slice off.
fn seek_tail(
    connection: &Connection,
    process_id: Option<i64>,
    stream: OutputStream,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    tail_lines: u64,
) -> Result<(i64, u64)> {
    let (sql, stream_filter) = match stream {
        OutputStream::All => (
            "
            SELECT id, chunk
            FROM process_output
            WHERE (?1 IS NULL OR process_id = ?1)
                AND (?2 IS NULL OR created_at_ms >= ?2)
                AND (?3 IS NULL OR created_at_ms <= ?3)
            ORDER BY id DESC
            ",
            None,
        ),
        OutputStream::Stdout | OutputStream::Stderr => (
            "
            SELECT id, chunk
            FROM process_output
            WHERE (?1 IS NULL OR process_id = ?1)
                AND (?2 IS NULL OR created_at_ms >= ?2)
                AND (?3 IS NULL OR created_at_ms <= ?3)
                AND stream = ?4
            ORDER BY id DESC
            ",
            Some(stream_name(stream)),
        ),
    };
    let mut statement = connection
        .prepare(sql)
        .context("failed to prepare tail seek query")?;
    let map_row = |row: &rusqlite::Row<'_>| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?));
    let rows = if let Some(stream_filter) = stream_filter {
        statement.query_map(
            params![process_id, since_ms, until_ms, stream_filter],
            map_row,
        )
    } else {
        statement.query_map(params![process_id, since_ms, until_ms], map_row)
    }
    .context("failed to seek output tail")?;

    let mut lines: u64 = 0;
    let mut newest = true;

    for row in rows {
        let (id, chunk) = row.context("failed to decode tail seek row")?;

        if tail_lines == 0 {
            return Ok((id, 0));
        }

        lines += chunk.iter().filter(|byte| **byte == b'\n').count() as u64;
        if newest && !chunk.ends_with(b"\n") && !chunk.is_empty() {
            // A trailing partial line still counts as a line.
            lines += 1;
        }
        newest = false;

        // Strictly greater: a cut at `lines == tail_lines` would land on
        // this chunk's start, which may be the middle of a line continued
        // from an older chunk. Overshooting by one line keeps the cut just
        // after a newline.
        if lines > tail_lines {
            return Ok((id - 1, lines - tail_lines));
        }
    }

    // Fewer stored lines than requested: read from the beginning.
    Ok((0, 0))
}

fn trim_leading_lines(data: &[u8], lines: u64) -> Vec<u8> {
    let mut start = 0;

    for _ in 0..lines {
        match data[start..].iter().position(|byte| *byte == b'\n') {
            Some(newline) => start += newline + 1,
            None => return Vec::new(),
        }
    }

    data[start..].to_vec()
}

fn output_chunk_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OutputChunk> {
    Ok(OutputChunk {
        id: row.get(0)?,
        process_id: row.get(1)?,
        stream: parse_stream(row.get::<_, String>(2)?.as_str()),
        data: row.get(3)?,
    })
}

fn process_summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProcessSummary> {
    let command: String = row.get(6)?;

    Ok(ProcessSummary {
        id: row.get(0)?,
        name: row.get(1)?,
        status: parse_status(row.get::<_, String>(2)?.as_str()),
        pid: row.get(3)?,
        pgid: row.get(4)?,
        exit_code: row.get(5)?,
        error_message: row.get(7)?,
        timeout_ms: timeout_ms_from_row(row, 8)?,
        timeout_at_ms: row.get(9)?,
        ports_unavailable: false,
        ports: Vec::new(),
        command: serde_json::from_str(&command).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        env: env_summary_from_row(row, 10)?,
    })
}

fn process_details_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProcessDetails> {
    let command: String = row.get(6)?;

    Ok(ProcessDetails {
        id: row.get(0)?,
        name: row.get(1)?,
        status: parse_status(row.get::<_, String>(2)?.as_str()),
        pid: row.get(3)?,
        pgid: row.get(4)?,
        exit_code: row.get(5)?,
        error_message: row.get(7)?,
        timeout_ms: timeout_ms_from_row(row, 8)?,
        timeout_at_ms: row.get(9)?,
        command: serde_json::from_str(&command).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        cwd: row.get(10)?,
        env: env_summary_from_row(row, 11)?,
    })
}

// rusqlite 0.40 dropped u64 To/FromSql; timeouts are stored as i64.
fn sql_timeout_ms(timeout_ms: Option<u64>) -> Result<Option<i64>> {
    timeout_ms
        .map(i64::try_from)
        .transpose()
        .context("timeout does not fit in i64")
}

fn timeout_ms_from_row(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<u64>> {
    let value: Option<i64> = row.get(index)?;

    Ok(value.and_then(|value| u64::try_from(value).ok()))
}

fn env_summary_from_row(
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> rusqlite::Result<ProcessEnvSummary> {
    let env_files: String = row.get(offset + 1)?;
    let env_keys: String = row.get(offset + 2)?;

    Ok(ProcessEnvSummary {
        inherit_env: row.get(offset)?,
        env_files: serde_json::from_str(&env_files).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                offset + 1,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        env_keys: serde_json::from_str(&env_keys).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                offset + 2,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
    })
}

// `foreign_keys` is per-connection, so this must run for every connection,
// not just the one that migrates.
fn configure_connection(connection: &Connection) -> Result<()> {
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .context("failed to enable foreign keys")
}

fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        // Baseline. Idempotent (IF NOT EXISTS + guarded column adds)
        // because databases from before schema versioning sit at
        // user_version 0 with any historical subset of this schema.
        M::up_with_hook("", |transaction: &Transaction| {
            baseline(transaction)?;
            Ok(())
        }),
        M::up("ALTER TABLE processes ADD COLUMN pid_started_at INTEGER;"),
        // Databases predating the index may hold duplicate running names
        // (spawn used to check after spawning); keep the newest.
        M::up(
            "
            UPDATE processes SET status = 'lost', finished_at = CURRENT_TIMESTAMP
            WHERE status = 'running' AND name IS NOT NULL AND id NOT IN (
                SELECT MAX(id) FROM processes
                WHERE status = 'running' AND name IS NOT NULL
                GROUP BY name
            );
            CREATE UNIQUE INDEX idx_processes_running_name
            ON processes(name) WHERE status = 'running';
            ",
        ),
    ])
}

fn baseline(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "
            CREATE TABLE IF NOT EXISTS processes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT,
                command TEXT NOT NULL,
                cwd TEXT NOT NULL,
                status TEXT NOT NULL,
                pid INTEGER,
                pgid INTEGER,
                exit_code INTEGER,
                error_message TEXT,
                timeout_ms INTEGER,
                timeout_at_ms INTEGER,
                inherit_env INTEGER NOT NULL DEFAULT 0,
                env_files TEXT NOT NULL DEFAULT '[]',
                env_keys TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                started_at TEXT,
                finished_at TEXT
            );

            CREATE TABLE IF NOT EXISTS process_output (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                process_id INTEGER NOT NULL REFERENCES processes(id) ON DELETE CASCADE,
                stream TEXT NOT NULL CHECK (stream IN ('stdout', 'stderr')),
                chunk BLOB NOT NULL,
                created_at_ms INTEGER,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            ",
    )?;

    let patches = [
        ("process_output", "created_at_ms", "INTEGER"),
        ("processes", "name", "TEXT"),
        ("processes", "error_message", "TEXT"),
        ("processes", "pgid", "INTEGER"),
        ("processes", "timeout_ms", "INTEGER"),
        ("processes", "timeout_at_ms", "INTEGER"),
        ("processes", "inherit_env", "INTEGER NOT NULL DEFAULT 0"),
        ("processes", "env_files", "TEXT NOT NULL DEFAULT '[]'"),
        ("processes", "env_keys", "TEXT NOT NULL DEFAULT '[]'"),
    ];

    for (table, column, definition) in patches {
        if !column_exists(connection, table, column)? {
            connection.execute(
                &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
                [],
            )?;
        }
    }

    Ok(())
}

fn column_exists(connection: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(columns.iter().any(|name| name == column))
}

fn now_ms() -> Result<i64> {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before Unix epoch")?;

    i64::try_from(duration.as_millis()).context("current timestamp does not fit in i64")
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn migrations_are_valid() {
        assert!(migrations().validate().is_ok());
    }

    #[test]
    fn fresh_database_is_versioned() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("pz.sqlite");
        Store::open(StoreConfig {
            database_path: path.clone(),
        })?;

        let connection = Connection::open(&path)?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        assert_eq!(version, 3);

        Ok(())
    }

    #[test]
    fn legacy_database_is_patched_and_versioned() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("pz.sqlite");

        // A database from before schema versioning: user_version 0 and an
        // early schema missing later columns.
        let connection = Connection::open(&path)?;
        connection.execute_batch(
            "
            CREATE TABLE processes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                command TEXT NOT NULL,
                cwd TEXT NOT NULL,
                status TEXT NOT NULL,
                pid INTEGER,
                exit_code INTEGER,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                started_at TEXT,
                finished_at TEXT
            );

            CREATE TABLE process_output (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                process_id INTEGER NOT NULL REFERENCES processes(id) ON DELETE CASCADE,
                stream TEXT NOT NULL CHECK (stream IN ('stdout', 'stderr')),
                chunk BLOB NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );

            INSERT INTO processes (command, cwd, status, pid)
            VALUES ('[\"echo\"]', '/tmp', 'running', 42);
            ",
        )?;
        drop(connection);

        let store = Store::open(StoreConfig {
            database_path: path.clone(),
        })?;

        // Old data survives and the patched columns are usable.
        let process = store.get_process(1)?;
        assert_eq!(process.pid, Some(42));
        assert_eq!(process.name, None);

        let connection = Connection::open(&path)?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        assert_eq!(version, 3);
        assert!(column_exists(&connection, "processes", "env_keys")?);
        assert!(column_exists(&connection, "processes", "pid_started_at")?);
        assert!(column_exists(
            &connection,
            "process_output",
            "created_at_ms"
        )?);

        Ok(())
    }

    #[test]
    fn open_creates_database_and_schema() -> Result<()> {
        let dir = tempdir()?;
        let database_path = dir.path().join("pz.sqlite");
        let store = Store::open(StoreConfig {
            database_path: database_path.clone(),
        })?;

        assert_eq!(store.database_path(), database_path.as_path());
        assert!(database_path.exists());

        let connection = Connection::open(&database_path)?;
        assert!(table_exists(&connection, "processes")?);
        assert!(table_exists(&connection, "process_output")?);

        Ok(())
    }

    #[test]
    fn insert_and_finish_process_metadata() -> Result<()> {
        let dir = tempdir()?;
        let store = Store::open(StoreConfig {
            database_path: dir.path().join("pz.sqlite"),
        })?;
        let command = vec!["echo".to_owned(), "hello".to_owned()];

        let process = store.insert_process(
            Some("build"),
            &command,
            dir.path(),
            1234,
            1234,
            false,
            &[],
            &[],
        )?;
        assert_eq!(process.id, 1);
        assert_eq!(process.name, Some("build".to_owned()));
        assert_eq!(process.status, ProcessStatus::Running);
        assert_eq!(process.pid, Some(1234));
        assert_eq!(process.pgid, Some(1234));
        assert_eq!(process.exit_code, None);
        assert_eq!(process.error_message, None);
        assert_eq!(process.command, command);
        assert!(!process.env.inherit_env);
        assert!(process.env.env_files.is_empty());
        assert!(process.env.env_keys.is_empty());

        store.mark_process_finished(process.id, Some(0))?;
        let process = store.get_process(process.id)?;
        assert_eq!(process.status, ProcessStatus::Exited);
        assert_eq!(process.exit_code, Some(0));
        assert_eq!(process.error_message, None);

        Ok(())
    }

    #[test]
    fn failed_spawn_keeps_reservation_metadata() -> Result<()> {
        let dir = tempdir()?;
        let store = Store::open(StoreConfig {
            database_path: dir.path().join("pz.sqlite"),
        })?;
        let command = vec!["/missing".to_owned()];

        let id = store.reserve_process(
            Some("missing"),
            &command,
            dir.path(),
            true,
            &["/tmp/test.env".to_owned()],
            &["SECRET".to_owned()],
        )?;
        store.mark_spawn_failed(id, "not found")?;

        let process = store.get_process(id)?;
        assert_eq!(process.name, Some("missing".to_owned()));
        assert_eq!(process.status, ProcessStatus::Failed);
        assert_eq!(process.pid, None);
        assert_eq!(process.exit_code, None);
        assert_eq!(process.error_message, Some("not found".to_owned()));
        assert_eq!(process.command, command);
        assert!(process.env.inherit_env);
        assert_eq!(process.env.env_files, vec!["/tmp/test.env"]);
        assert_eq!(process.env.env_keys, vec!["SECRET"]);
        assert_eq!(
            store.resolve_process_id(&crate::protocol::ProcessSelector::Name(
                "missing".to_owned()
            ))?,
            id
        );

        // A failed generation does not block the name.
        let second =
            store.reserve_process(Some("missing"), &command, dir.path(), false, &[], &[])?;
        assert_ne!(second, id);

        Ok(())
    }

    #[test]
    fn reserve_process_rejects_duplicate_running_names() -> Result<()> {
        let dir = tempdir()?;
        let store = Store::open(StoreConfig {
            database_path: dir.path().join("pz.sqlite"),
        })?;
        let command = vec!["sleep".to_owned()];

        store.reserve_process(Some("api"), &command, dir.path(), false, &[], &[])?;
        let error = store
            .reserve_process(Some("api"), &command, dir.path(), false, &[], &[])
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("a running process named \"api\" already exists"),
            "{error}"
        );

        // Unnamed processes never conflict.
        store.reserve_process(None, &command, dir.path(), false, &[], &[])?;
        store.reserve_process(None, &command, dir.path(), false, &[], &[])?;

        Ok(())
    }

    #[test]
    fn migration_dedupes_running_names_before_indexing() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("pz.sqlite");

        // A pre-index database where the spawn-then-check bug left two
        // running rows with the same name.
        let connection = Connection::open(&path)?;
        baseline(&connection)?;
        connection.execute_batch(
            "
            INSERT INTO processes (name, command, cwd, status, pid)
            VALUES ('api', '[\"sleep\"]', '/tmp', 'running', 100);
            INSERT INTO processes (name, command, cwd, status, pid)
            VALUES ('api', '[\"sleep\"]', '/tmp', 'running', 200);
            ",
        )?;
        drop(connection);

        let store = Store::open(StoreConfig {
            database_path: path,
        })?;

        let older = store.get_process(1)?;
        let newer = store.get_process(2)?;
        assert_eq!(older.status, ProcessStatus::Lost);
        assert_eq!(newer.status, ProcessStatus::Running);

        Ok(())
    }

    #[test]
    fn lost_generations_lists_newest_first() -> Result<()> {
        let dir = tempdir()?;
        let store = Store::open(StoreConfig {
            database_path: dir.path().join("pz.sqlite"),
        })?;
        let command = vec!["sleep".to_owned()];

        let first =
            store.insert_process(Some("api"), &command, dir.path(), 100, 100, false, &[], &[])?;
        store.mark_running_processes_lost()?;
        let second =
            store.insert_process(Some("api"), &command, dir.path(), 200, 200, false, &[], &[])?;
        store.mark_running_processes_lost()?;

        let generations = store.lost_generations("api")?;
        assert_eq!(
            generations,
            vec![(second.id, 200, None), (first.id, 100, None)]
        );
        assert!(store.lost_generations("other")?.is_empty());

        Ok(())
    }

    #[test]
    fn mark_process_killed_sets_status() -> Result<()> {
        let dir = tempdir()?;
        let store = Store::open(StoreConfig {
            database_path: dir.path().join("pz.sqlite"),
        })?;
        let process = store.insert_process(
            None,
            &["sleep".to_owned()],
            dir.path(),
            1234,
            1234,
            false,
            &[],
            &[],
        )?;

        store.mark_process_killed(process.id)?;
        let process = store.get_process(process.id)?;
        assert_eq!(process.status, ProcessStatus::Killed);

        Ok(())
    }

    #[test]
    fn mark_running_processes_lost_sets_only_running_rows() -> Result<()> {
        let dir = tempdir()?;
        let store = Store::open(StoreConfig {
            database_path: dir.path().join("pz.sqlite"),
        })?;
        let running = store.insert_process(
            Some("running"),
            &["sleep".to_owned()],
            dir.path(),
            1234,
            1234,
            false,
            &[],
            &[],
        )?;
        let exited = store.insert_process(
            Some("exited"),
            &["true".to_owned()],
            dir.path(),
            1235,
            1235,
            false,
            &[],
            &[],
        )?;
        store.mark_process_finished(exited.id, Some(0))?;

        assert_eq!(store.mark_running_processes_lost()?, 1);
        assert_eq!(store.get_process(running.id)?.status, ProcessStatus::Lost);
        assert_eq!(store.get_process(exited.id)?.status, ProcessStatus::Exited);

        Ok(())
    }

    #[test]
    fn list_processes_orders_newest_first() -> Result<()> {
        let dir = tempdir()?;
        let store = Store::open(StoreConfig {
            database_path: dir.path().join("pz.sqlite"),
        })?;

        store.insert_process(
            None,
            &["first".to_owned()],
            dir.path(),
            111,
            111,
            false,
            &[],
            &[],
        )?;
        store.insert_process(
            Some("second"),
            &["second".to_owned()],
            dir.path(),
            222,
            222,
            false,
            &[],
            &[],
        )?;

        let processes = store.list_processes(false)?;
        assert_eq!(processes.len(), 2);
        assert_eq!(processes[0].id, 2);
        assert_eq!(processes[0].name, Some("second".to_owned()));
        assert_eq!(processes[0].pid, Some(222));
        assert_eq!(processes[0].pgid, Some(222));
        assert_eq!(processes[0].command, vec!["second"]);
        assert_eq!(processes[1].id, 1);

        Ok(())
    }

    #[test]
    fn resolves_named_processes_and_blocks_duplicate_running_names() -> Result<()> {
        let dir = tempdir()?;
        let store = Store::open(StoreConfig {
            database_path: dir.path().join("pz.sqlite"),
        })?;

        let first = store.insert_process(
            Some("api"),
            &["sleep".to_owned()],
            dir.path(),
            111,
            111,
            false,
            &[],
            &[],
        )?;
        assert_eq!(
            store.resolve_process_id(&crate::protocol::ProcessSelector::Name("api".to_owned()))?,
            first.id
        );

        let duplicate = store.insert_process(
            Some("api"),
            &["sleep".to_owned()],
            dir.path(),
            222,
            222,
            false,
            &[],
            &[],
        );
        assert!(duplicate.is_err());

        store.mark_process_finished(first.id, Some(0))?;
        let second = store.insert_process(
            Some("api"),
            &["sleep".to_owned()],
            dir.path(),
            333,
            333,
            false,
            &[],
            &[],
        )?;
        assert_eq!(
            store.resolve_process_id(&crate::protocol::ProcessSelector::Name("api".to_owned()))?,
            second.id
        );

        Ok(())
    }

    #[test]
    fn get_process_details_returns_full_metadata() -> Result<()> {
        let dir = tempdir()?;
        let store = Store::open(StoreConfig {
            database_path: dir.path().join("pz.sqlite"),
        })?;
        let command = vec!["echo".to_owned(), "hello".to_owned()];
        let process = store.insert_process(
            Some("details"),
            &command,
            dir.path(),
            1234,
            1234,
            true,
            &["/tmp/test.env".to_owned()],
            &["FOO".to_owned()],
        )?;

        let details = store.get_process_details(process.id)?;
        assert_eq!(details.id, process.id);
        assert_eq!(details.status, ProcessStatus::Running);
        assert_eq!(details.pid, Some(1234));
        assert_eq!(details.pgid, Some(1234));
        assert_eq!(details.exit_code, None);
        assert_eq!(details.error_message, None);
        assert_eq!(details.command, command);
        assert_eq!(details.cwd, dir.path().display().to_string());
        assert!(details.env.inherit_env);
        assert_eq!(details.env.env_files, vec!["/tmp/test.env"]);
        assert_eq!(details.env.env_keys, vec!["FOO"]);

        Ok(())
    }

    #[test]
    fn stores_and_filters_output_chunks() -> Result<()> {
        let dir = tempdir()?;
        let store = Store::open(StoreConfig {
            database_path: dir.path().join("pz.sqlite"),
        })?;
        let process = store.insert_process(
            None,
            &["echo".to_owned()],
            dir.path(),
            1234,
            1234,
            false,
            &[],
            &[],
        )?;

        store.insert_output_chunk(process.id, OutputStream::Stdout, b"out\n")?;
        store.insert_output_chunk(process.id, OutputStream::Stderr, b"err\n")?;

        let (chunks, resume) =
            store.read_output(Some(process.id), OutputStream::All, None, None, None, None)?;
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].id, 1);
        assert_eq!(chunks[1].id, 2);
        assert_eq!(chunks[0].data, b"out\n");
        assert_eq!(chunks[1].data, b"err\n");
        assert_eq!(resume, 2);

        let (chunks, _) = store.read_output(
            Some(process.id),
            OutputStream::Stderr,
            None,
            None,
            None,
            None,
        )?;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].data, b"err\n");

        let (chunks, _) = store.read_output(
            Some(process.id),
            OutputStream::All,
            Some(1),
            None,
            None,
            None,
        )?;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].id, 2);

        // Nothing new: the resume cursor holds its position.
        let (chunks, resume) = store.read_output(
            Some(process.id),
            OutputStream::All,
            Some(2),
            None,
            None,
            None,
        )?;
        assert!(chunks.is_empty());
        assert_eq!(resume, 2);

        Ok(())
    }

    #[test]
    fn read_output_tail_returns_exactly_last_lines() -> Result<()> {
        let dir = tempdir()?;
        let store = Store::open(StoreConfig {
            database_path: dir.path().join("pz.sqlite"),
        })?;
        let process = store.insert_process(
            None,
            &["echo".to_owned()],
            dir.path(),
            1234,
            1234,
            false,
            &[],
            &[],
        )?;

        let (chunks, resume) = store.read_output(
            Some(process.id),
            OutputStream::All,
            None,
            None,
            None,
            Some(0),
        )?;
        assert!(chunks.is_empty());
        assert_eq!(resume, 0);

        store.insert_output_chunk(process.id, OutputStream::Stdout, b"one\ntwo\n")?;
        store.insert_output_chunk(process.id, OutputStream::Stderr, b"err\n")?;
        store.insert_output_chunk(process.id, OutputStream::Stdout, b"three\n")?;

        // tail 0: nothing replayed, cursor past the newest chunk.
        let (chunks, resume) = store.read_output(
            Some(process.id),
            OutputStream::All,
            None,
            None,
            None,
            Some(0),
        )?;
        assert!(chunks.is_empty());
        assert_eq!(resume, 3);

        let (chunks, resume) = store.read_output(
            Some(process.id),
            OutputStream::All,
            None,
            None,
            None,
            Some(1),
        )?;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].data, b"three\n");
        assert_eq!(resume, 3);

        let (chunks, _) = store.read_output(
            Some(process.id),
            OutputStream::All,
            None,
            None,
            None,
            Some(2),
        )?;
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].data, b"err\n");
        assert_eq!(chunks[1].data, b"three\n");

        // tail 3 ends mid-chunk: the boundary chunk is sliced on the line.
        let (chunks, _) = store.read_output(
            Some(process.id),
            OutputStream::All,
            None,
            None,
            None,
            Some(3),
        )?;
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].data, b"two\n");
        assert_eq!(chunks[1].data, b"err\n");
        assert_eq!(chunks[2].data, b"three\n");

        // More lines than stored: everything, untouched.
        let (chunks, _) = store.read_output(
            Some(process.id),
            OutputStream::All,
            None,
            None,
            None,
            Some(10),
        )?;
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].data, b"one\ntwo\n");

        let (chunks, _) = store.read_output(
            Some(process.id),
            OutputStream::Stdout,
            None,
            None,
            None,
            Some(2),
        )?;
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].data, b"two\n");
        assert_eq!(chunks[1].data, b"three\n");
        let (chunks, _) = store.read_output(
            Some(process.id),
            OutputStream::Stderr,
            None,
            None,
            None,
            Some(1),
        )?;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].data, b"err\n");

        Ok(())
    }

    #[test]
    fn read_output_tail_counts_trailing_partial_line() -> Result<()> {
        let dir = tempdir()?;
        let store = Store::open(StoreConfig {
            database_path: dir.path().join("pz.sqlite"),
        })?;
        let process = store.insert_process(
            None,
            &["echo".to_owned()],
            dir.path(),
            1234,
            1234,
            false,
            &[],
            &[],
        )?;

        store.insert_output_chunk(process.id, OutputStream::Stdout, b"one\n")?;
        store.insert_output_chunk(process.id, OutputStream::Stdout, b"partial")?;

        // "partial" has no newline but counts as the last line.
        let (chunks, _) = store.read_output(
            Some(process.id),
            OutputStream::All,
            None,
            None,
            None,
            Some(1),
        )?;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].data, b"partial");

        let (chunks, _) = store.read_output(
            Some(process.id),
            OutputStream::All,
            None,
            None,
            None,
            Some(2),
        )?;
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].data, b"one\n");

        Ok(())
    }

    #[test]
    fn read_output_tail_keeps_lines_spanning_chunks_whole() -> Result<()> {
        let dir = tempdir()?;
        let store = Store::open(StoreConfig {
            database_path: dir.path().join("pz.sqlite"),
        })?;
        let process = store.insert_process(
            None,
            &["echo".to_owned()],
            dir.path(),
            1234,
            1234,
            false,
            &[],
            &[],
        )?;

        // "aaabbb" is one line captured as two chunks.
        store.insert_output_chunk(process.id, OutputStream::Stdout, b"aaa")?;
        store.insert_output_chunk(process.id, OutputStream::Stdout, b"bbb\nccc\n")?;

        let (chunks, _) = store.read_output(
            Some(process.id),
            OutputStream::All,
            None,
            None,
            None,
            Some(2),
        )?;
        let data = chunks
            .iter()
            .flat_map(|chunk| chunk.data.iter().copied())
            .collect::<Vec<_>>();
        assert_eq!(data, b"aaabbb\nccc\n");

        let (chunks, _) = store.read_output(
            Some(process.id),
            OutputStream::All,
            None,
            None,
            None,
            Some(1),
        )?;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].data, b"ccc\n");

        // A line-aligned cut between chunks must not leave an empty
        // boundary chunk behind.
        store.insert_output_chunk(process.id, OutputStream::Stdout, b"ddd\n")?;
        let (chunks, _) = store.read_output(
            Some(process.id),
            OutputStream::All,
            None,
            None,
            None,
            Some(1),
        )?;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].data, b"ddd\n");

        Ok(())
    }

    #[test]
    fn read_output_tail_respects_time_window() -> Result<()> {
        let dir = tempdir()?;
        let store = Store::open(StoreConfig {
            database_path: dir.path().join("pz.sqlite"),
        })?;
        let process = store.insert_process(
            None,
            &["echo".to_owned()],
            dir.path(),
            1234,
            1234,
            false,
            &[],
            &[],
        )?;

        store.insert_output_chunk(process.id, OutputStream::Stdout, b"one\n")?;
        store.insert_output_chunk(process.id, OutputStream::Stdout, b"two\n")?;

        // until_ms before everything: the window is empty, tail finds nothing.
        let (chunks, _) = store.read_output(
            Some(process.id),
            OutputStream::All,
            None,
            None,
            Some(0),
            Some(1),
        )?;
        assert!(chunks.is_empty());

        let now = now_ms()?;
        let (chunks, _) = store.read_output(
            Some(process.id),
            OutputStream::All,
            None,
            Some(0),
            Some(now + 1000),
            Some(1),
        )?;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].data, b"two\n");

        Ok(())
    }

    #[test]
    fn stores_pid_identity_token() -> Result<()> {
        let dir = tempdir()?;
        let store = Store::open(StoreConfig {
            database_path: dir.path().join("pz.sqlite"),
        })?;

        let id = store.reserve_process(None, &["echo".to_owned()], dir.path(), false, &[], &[])?;
        let with_token = store.activate_process(id, 1234, 1234, Some(1_700_000_000))?;
        assert_eq!(store.pid_identity(with_token.id)?, Some(1_700_000_000));

        // Rows recorded without a token read back as None.
        let without_token = store.insert_process(
            None,
            &["echo".to_owned()],
            dir.path(),
            5678,
            5678,
            false,
            &[],
            &[],
        )?;
        assert_eq!(store.pid_identity(without_token.id)?, None);

        Ok(())
    }

    fn table_exists(connection: &Connection, table: &str) -> Result<bool> {
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        )?;

        Ok(count == 1)
    }
}
