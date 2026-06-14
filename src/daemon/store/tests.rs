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
    let second = store.reserve_process(Some("missing"), &command, dir.path(), false, &[], &[])?;
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
