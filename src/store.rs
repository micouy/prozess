use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;

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

    fn table_exists(connection: &Connection, table: &str) -> Result<bool> {
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        )?;

        Ok(count == 1)
    }
}
