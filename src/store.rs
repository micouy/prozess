use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use crate::protocol::{ProcessStatus, ProcessSummary};

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

    pub fn insert_process(
        &self,
        command: &[String],
        cwd: &Path,
        pid: u32,
    ) -> Result<ProcessSummary> {
        let connection = self.connect()?;
        let command_json = serde_json::to_string(command).context("failed to encode command")?;
        let cwd = cwd.display().to_string();

        connection
            .execute(
                "
                INSERT INTO processes (command, cwd, status, pid, started_at)
                VALUES (?1, ?2, 'running', ?3, CURRENT_TIMESTAMP)
                ",
                params![command_json, cwd, pid],
            )
            .context("failed to insert process")?;

        Ok(ProcessSummary {
            id: connection.last_insert_rowid(),
            status: ProcessStatus::Running,
            exit_code: None,
            command: command.to_vec(),
        })
    }

    pub fn mark_process_finished(&self, id: i64, exit_code: Option<i32>) -> Result<()> {
        let connection = self.connect()?;

        connection
            .execute(
                "
                UPDATE processes
                SET status = 'exited', exit_code = ?1, finished_at = CURRENT_TIMESTAMP
                WHERE id = ?2
                ",
                params![exit_code, id],
            )
            .context("failed to mark process finished")?;

        Ok(())
    }

    #[cfg(test)]
    pub fn get_process(&self, id: i64) -> Result<ProcessSummary> {
        let connection = self.connect()?;

        connection
            .query_row(
                "SELECT id, status, exit_code, command FROM processes WHERE id = ?1",
                [id],
                |row| {
                    let command: String = row.get(3)?;
                    Ok(ProcessSummary {
                        id: row.get(0)?,
                        status: parse_status(row.get::<_, String>(1)?.as_str()),
                        exit_code: row.get(2)?,
                        command: serde_json::from_str(&command).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                3,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?,
                    })
                },
            )
            .context("failed to get process")
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

#[cfg(test)]
fn parse_status(status: &str) -> ProcessStatus {
    match status {
        "running" => ProcessStatus::Running,
        "exited" => ProcessStatus::Exited,
        "failed" => ProcessStatus::Failed,
        "killed" => ProcessStatus::Killed,
        _ => ProcessStatus::Failed,
    }
}

fn migrate(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS processes (
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

            CREATE TABLE IF NOT EXISTS process_output (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                process_id INTEGER NOT NULL REFERENCES processes(id) ON DELETE CASCADE,
                stream TEXT NOT NULL CHECK (stream IN ('stdout', 'stderr')),
                chunk BLOB NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            ",
        )
        .context("failed to initialize database schema")?;

    Ok(())
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

        let process = store.insert_process(&command, dir.path(), 1234)?;
        assert_eq!(process.id, 1);
        assert_eq!(process.status, ProcessStatus::Running);
        assert_eq!(process.exit_code, None);
        assert_eq!(process.command, command);

        store.mark_process_finished(process.id, Some(0))?;
        let process = store.get_process(process.id)?;
        assert_eq!(process.status, ProcessStatus::Exited);
        assert_eq!(process.exit_code, Some(0));

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
