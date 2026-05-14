use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

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

        let connection = Connection::open(&config.database_path).with_context(|| {
            format!("failed to open database {}", config.database_path.display())
        })?;

        let store = Self {
            database_path: config.database_path,
        };

        migrate(&connection)?;

        Ok(store)
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    #[cfg(test)]
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
        self.insert_process_with_timeout(
            name,
            command,
            cwd,
            pid,
            pgid,
            inherit_env,
            env_files,
            env_keys,
            None,
        )
    }

    pub fn insert_process_with_timeout(
        &self,
        name: Option<&str>,
        command: &[String],
        cwd: &Path,
        pid: u32,
        pgid: u32,
        inherit_env: bool,
        env_files: &[String],
        env_keys: &[String],
        timeout: Option<TimeoutSpec>,
    ) -> Result<ProcessSummary> {
        let connection = self.connect()?;
        ensure_active_name_available(&connection, name)?;
        let command_json = serde_json::to_string(command).context("failed to encode command")?;
        let env_files_json =
            serde_json::to_string(env_files).context("failed to encode env files")?;
        let env_keys_json = serde_json::to_string(env_keys).context("failed to encode env keys")?;
        let cwd = cwd.display().to_string();
        let timeout_ms = timeout.map(|timeout| timeout.duration_ms);
        let timeout_at_ms = timeout.map(|timeout| timeout.deadline_ms);

        connection
            .execute(
                "
                INSERT INTO processes (
                    name, command, cwd, status, pid, pgid, inherit_env, env_files, env_keys, timeout_ms, timeout_at_ms, started_at
                )
                VALUES (?1, ?2, ?3, 'running', ?4, ?5, ?6, ?7, ?8, ?9, ?10, CURRENT_TIMESTAMP)
                ",
                params![
                    name,
                    command_json,
                    cwd,
                    pid,
                    pgid,
                    inherit_env,
                    env_files_json,
                    env_keys_json,
                    timeout_ms,
                    timeout_at_ms,
                ],
            )
            .context("failed to insert process")?;

        Ok(ProcessSummary {
            id: connection.last_insert_rowid(),
            name: name.map(str::to_owned),
            status: ProcessStatus::Running,
            pid: Some(pid),
            pgid: Some(pgid),
            exit_code: None,
            error_message: None,
            timeout_ms,
            timeout_at_ms,
            ports_unavailable: false,
            ports: Vec::new(),
            command: command.to_vec(),
            env: ProcessEnvSummary {
                inherit_env,
                env_files: env_files.to_vec(),
                env_keys: env_keys.to_vec(),
            },
        })
    }

    pub fn insert_failed_process(
        &self,
        name: Option<&str>,
        command: &[String],
        cwd: &Path,
        error_message: &str,
        inherit_env: bool,
        env_files: &[String],
        env_keys: &[String],
    ) -> Result<ProcessSummary> {
        let connection = self.connect()?;
        let command_json = serde_json::to_string(command).context("failed to encode command")?;
        let env_files_json =
            serde_json::to_string(env_files).context("failed to encode env files")?;
        let env_keys_json = serde_json::to_string(env_keys).context("failed to encode env keys")?;
        let cwd = cwd.display().to_string();

        connection
            .execute(
                "
                INSERT INTO processes (
                    command, cwd, status, error_message, inherit_env, env_files, env_keys, finished_at
                )
                VALUES (?1, ?2, 'failed', ?3, ?4, ?5, ?6, CURRENT_TIMESTAMP)
                ",
                params![command_json, cwd, error_message, inherit_env, env_files_json, env_keys_json],
            )
            .context("failed to insert failed process")?;

        Ok(ProcessSummary {
            id: connection.last_insert_rowid(),
            name: name.map(str::to_owned),
            status: ProcessStatus::Failed,
            pid: None,
            pgid: None,
            exit_code: None,
            error_message: Some(error_message.to_owned()),
            timeout_ms: None,
            timeout_at_ms: None,
            ports_unavailable: false,
            ports: Vec::new(),
            command: command.to_vec(),
            env: ProcessEnvSummary {
                inherit_env,
                env_files: env_files.to_vec(),
                env_keys: env_keys.to_vec(),
            },
        })
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
        let timeout_ms = timeout.map(|timeout| timeout.duration_ms);
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

    pub fn list_processes(&self) -> Result<Vec<ProcessSummary>> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "
                SELECT id, name, status, pid, pgid, exit_code, command, error_message, timeout_ms, timeout_at_ms, inherit_env, env_files, env_keys
                FROM processes
                ORDER BY id DESC
                ",
            )
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

    pub fn read_output(
        &self,
        process_id: i64,
        stream: OutputStream,
        after_id: Option<i64>,
        since_ms: Option<i64>,
        until_ms: Option<i64>,
    ) -> Result<Vec<OutputChunk>> {
        let connection = self.connect()?;
        let (sql, stream_filter) = match stream {
            OutputStream::All => (
                "
                SELECT id, stream, chunk
                FROM process_output
                WHERE process_id = ?1
                    AND id > ?2
                    AND (?3 IS NULL OR created_at_ms >= ?3)
                    AND (?4 IS NULL OR created_at_ms <= ?4)
                ORDER BY id ASC
                ",
                None,
            ),
            OutputStream::Stdout | OutputStream::Stderr => (
                "
                SELECT id, stream, chunk
                FROM process_output
                WHERE process_id = ?1
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

        let chunks = if let Some(stream_filter) = stream_filter {
            statement
                .query_map(
                    params![
                        process_id,
                        after_id.unwrap_or(0),
                        since_ms,
                        until_ms,
                        stream_filter
                    ],
                    output_chunk_from_row,
                )
                .context("failed to read output")?
                .collect::<rusqlite::Result<Vec<_>>>()
        } else {
            statement
                .query_map(
                    params![process_id, after_id.unwrap_or(0), since_ms, until_ms],
                    output_chunk_from_row,
                )
                .context("failed to read output")?
                .collect::<rusqlite::Result<Vec<_>>>()
        }
        .context("failed to decode output")?;

        Ok(chunks)
    }

    fn connect(&self) -> Result<Connection> {
        Connection::open(&self.database_path)
            .with_context(|| format!("failed to open database {}", self.database_path.display()))
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

fn ensure_active_name_available(connection: &Connection, name: Option<&str>) -> Result<()> {
    let Some(name) = name else {
        return Ok(());
    };

    let existing: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM processes WHERE name = ?1 AND status = 'running'",
            [name],
            |row| row.get(0),
        )
        .context("failed to check process name")?;

    if existing > 0 {
        anyhow::bail!("a running process named {name:?} already exists");
    }

    Ok(())
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

fn output_chunk_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OutputChunk> {
    Ok(OutputChunk {
        id: row.get(0)?,
        stream: parse_stream(row.get::<_, String>(1)?.as_str()),
        data: row.get(2)?,
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
        timeout_ms: row.get(8)?,
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
        timeout_ms: row.get(8)?,
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

fn migrate(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;

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
        )
        .context("failed to initialize database schema")?;

    if !column_exists(connection, "process_output", "created_at_ms")? {
        connection
            .execute(
                "ALTER TABLE process_output ADD COLUMN created_at_ms INTEGER",
                [],
            )
            .context("failed to add process_output created_at_ms column")?;
    }

    if !column_exists(connection, "processes", "name")? {
        connection
            .execute("ALTER TABLE processes ADD COLUMN name TEXT", [])
            .context("failed to add process name column")?;
    }

    if !column_exists(connection, "processes", "error_message")? {
        connection
            .execute("ALTER TABLE processes ADD COLUMN error_message TEXT", [])
            .context("failed to add process error_message column")?;
    }

    if !column_exists(connection, "processes", "pgid")? {
        connection
            .execute("ALTER TABLE processes ADD COLUMN pgid INTEGER", [])
            .context("failed to add process pgid column")?;
    }

    if !column_exists(connection, "processes", "timeout_ms")? {
        connection
            .execute("ALTER TABLE processes ADD COLUMN timeout_ms INTEGER", [])
            .context("failed to add process timeout_ms column")?;
    }

    if !column_exists(connection, "processes", "timeout_at_ms")? {
        connection
            .execute("ALTER TABLE processes ADD COLUMN timeout_at_ms INTEGER", [])
            .context("failed to add process timeout_at_ms column")?;
    }

    if !column_exists(connection, "processes", "inherit_env")? {
        connection
            .execute(
                "ALTER TABLE processes ADD COLUMN inherit_env INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .context("failed to add process inherit_env column")?;
    }

    if !column_exists(connection, "processes", "env_files")? {
        connection
            .execute(
                "ALTER TABLE processes ADD COLUMN env_files TEXT NOT NULL DEFAULT '[]'",
                [],
            )
            .context("failed to add process env_files column")?;
    }

    if !column_exists(connection, "processes", "env_keys")? {
        connection
            .execute(
                "ALTER TABLE processes ADD COLUMN env_keys TEXT NOT NULL DEFAULT '[]'",
                [],
            )
            .context("failed to add process env_keys column")?;
    }

    Ok(())
}

fn column_exists(connection: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .context("failed to inspect table schema")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .context("failed to query table schema")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read table schema")?;

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
    fn insert_failed_process_metadata() -> Result<()> {
        let dir = tempdir()?;
        let store = Store::open(StoreConfig {
            database_path: dir.path().join("pz.sqlite"),
        })?;
        let command = vec!["/missing".to_owned()];

        let process = store.insert_failed_process(
            Some("missing"),
            &command,
            dir.path(),
            "not found",
            true,
            &["/tmp/test.env".to_owned()],
            &["SECRET".to_owned()],
        )?;
        assert_eq!(process.id, 1);
        assert_eq!(process.name, Some("missing".to_owned()));
        assert_eq!(process.status, ProcessStatus::Failed);
        assert_eq!(process.pid, None);
        assert_eq!(process.exit_code, None);
        assert_eq!(process.error_message, Some("not found".to_owned()));
        assert_eq!(process.command, command);
        assert!(process.env.inherit_env);
        assert_eq!(process.env.env_files, vec!["/tmp/test.env"]);
        assert_eq!(process.env.env_keys, vec!["SECRET"]);

        let process = store.get_process(process.id)?;
        assert_eq!(process.status, ProcessStatus::Failed);
        assert_eq!(process.error_message, Some("not found".to_owned()));

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

        let processes = store.list_processes()?;
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

        let chunks = store.read_output(process.id, OutputStream::All, None, None, None)?;
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].id, 1);
        assert_eq!(chunks[1].id, 2);
        assert_eq!(chunks[0].data, b"out\n");
        assert_eq!(chunks[1].data, b"err\n");

        let chunks = store.read_output(process.id, OutputStream::Stderr, None, None, None)?;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].data, b"err\n");

        let chunks = store.read_output(process.id, OutputStream::All, Some(1), None, None)?;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].id, 2);

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
