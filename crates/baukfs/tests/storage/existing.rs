use std::fs;
use std::path::Path;

use baukfs::storage::{
    BusyTimeout, CreateOutcome, DatabaseState, StorageError, Unrecognized, WorkspaceStore, inspect,
};
use turso::Value;

use crate::support::{
    TestDir, block_on, file_bytes, metadata_rows, raw_connection, raw_execute, raw_values,
    schema_cookie, workspace,
};

const WAIT: BusyTimeout = BusyTimeout::IMMEDIATE;
const SIDECARS: [&str; 2] = ["-wal", "-tshm"];

fn state(path: &Path) -> DatabaseState {
    block_on(inspect(path, WAIT)).expect("inspect")
}

fn open_existing(path: &Path, id: &str) -> Result<WorkspaceStore, StorageError> {
    block_on(WorkspaceStore::open_existing(path, &workspace(id), WAIT))
}

fn create(path: &Path, id: &str) -> Result<(WorkspaceStore, CreateOutcome), StorageError> {
    block_on(WorkspaceStore::create(path, &workspace(id), WAIT))
}

/// Database and WAL contents. The `-tshm` coordination file is shared memory
/// that readers legitimately update, so only its presence is compared.
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    entries: Vec<String>,
    database: Option<Vec<u8>>,
    wal: Option<Vec<u8>>,
}

fn snapshot(dir: &TestDir, path: &Path) -> Snapshot {
    Snapshot {
        entries: dir.entries(),
        database: file_bytes(path, ""),
        wal: file_bytes(path, "-wal"),
    }
}

fn database_and_sidecars(path: &Path) -> Vec<String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("UTF-8 file name");
    [""].iter()
        .chain(&SIDECARS)
        .map(|suffix| format!("{name}{suffix}"))
        .collect()
}

#[test]
fn existing_only_operations_on_a_missing_database_leave_the_directory_unchanged() {
    let dir = TestDir::new("missing");
    let path = dir.database();
    let nested = dir.path().join("absent").join("workspace.db");

    assert_eq!(state(&path), DatabaseState::Missing);
    assert!(matches!(
        open_existing(&path, "ws-a"),
        Err(StorageError::Missing)
    ));
    assert_eq!(state(&nested), DatabaseState::Missing);
    assert!(matches!(
        open_existing(&nested, "ws-a"),
        Err(StorageError::Missing)
    ));

    assert!(dir.entries().is_empty(), "{:?}", dir.entries());
}

#[test]
fn inspecting_a_current_database_changes_nothing() {
    let dir = TestDir::new("inspect-current");
    let path = dir.database();
    drop(create(&path, "ws-a").expect("create"));

    let before = snapshot(&dir, &path);
    assert!(before.database.is_some());
    for _ in 0..2 {
        assert_eq!(state(&path), DatabaseState::Current(workspace("ws-a")));
    }
    assert_eq!(snapshot(&dir, &path), before);
}

#[test]
fn opening_a_current_database_rewrites_nothing() {
    let dir = TestDir::new("open-current");
    let path = dir.database();
    drop(create(&path, "ws-a").expect("create"));
    let cookie = schema_cookie(&path);
    let metadata = metadata_rows(&path);

    let store = open_existing(&path, "ws-a").expect("open existing");
    assert_eq!(store.workspace_id().as_str(), "ws-a");
    drop(store);

    assert_eq!(schema_cookie(&path), cookie);
    assert_eq!(metadata_rows(&path), metadata);
    let allowed = database_and_sidecars(&path);
    assert!(
        dir.entries().iter().all(|name| allowed.contains(name)),
        "{:?}",
        dir.entries()
    );
}

#[test]
fn uninitialized_databases_are_reported_and_only_creation_initializes_them() {
    let dir = TestDir::new("uninitialized");
    let empty_file = dir.path().join("empty-file.db");
    fs::write(&empty_file, b"").expect("write empty file");
    let driver_created = dir.path().join("driver-created.db");
    drop(raw_connection(&driver_created));

    for path in [&empty_file, &driver_created] {
        let before = snapshot(&dir, path);
        assert_eq!(
            state(path),
            DatabaseState::Uninitialized,
            "{}",
            path.display()
        );
        assert!(
            matches!(
                open_existing(path, "ws-a"),
                Err(StorageError::Uninitialized)
            ),
            "{}",
            path.display()
        );
        assert_eq!(snapshot(&dir, path), before, "{}", path.display());

        let (_, outcome) = create(path, "ws-a").expect("create on an empty database");
        assert_eq!(outcome, CreateOutcome::Created);
        assert_eq!(state(path), DatabaseState::Current(workspace("ws-a")));
    }
}

#[test]
fn unsupported_schema_versions_are_reported_and_never_upgraded() {
    let dir = TestDir::new("unsupported-version");
    let path = dir.database();
    drop(create(&path, "ws-a").expect("create"));
    raw_execute(&path, "UPDATE baukfs_metadata SET schema_version = 2");
    let unsupported = DatabaseState::UnsupportedSchemaVersion { found: 2 };

    let before = snapshot(&dir, &path);
    assert_eq!(state(&path), unsupported);
    assert_eq!(snapshot(&dir, &path), before);
    for result in [
        open_existing(&path, "ws-a").map(drop),
        create(&path, "ws-a").map(drop),
    ] {
        assert!(
            matches!(
                result,
                Err(StorageError::UnsupportedSchemaVersion { found: 2 })
            ),
            "{result:?}"
        );
    }
    assert_eq!(state(&path), unsupported);
    assert_eq!(
        metadata_rows(&path),
        [[
            Value::Integer(1),
            Value::Integer(2),
            Value::Text("ws-a".to_owned())
        ]]
    );
}

#[test]
fn an_unrelated_database_is_not_initialized() {
    let dir = TestDir::new("unrelated");
    let path = dir.database();
    raw_execute(
        &path,
        "CREATE TABLE notes (body TEXT NOT NULL); INSERT INTO notes (body) VALUES ('keep');",
    );
    let foreign = DatabaseState::Unrecognized(Unrecognized::ForeignSchema);

    let before = snapshot(&dir, &path);
    assert_eq!(state(&path), foreign);
    assert_eq!(snapshot(&dir, &path), before);
    for result in [
        open_existing(&path, "ws-a").map(drop),
        create(&path, "ws-a").map(drop),
    ] {
        assert!(
            matches!(
                result,
                Err(StorageError::Unrecognized(Unrecognized::ForeignSchema))
            ),
            "{result:?}"
        );
    }
    assert_eq!(state(&path), foreign);
    let connection = raw_connection(&path);
    assert_eq!(
        raw_values(&connection, "SELECT name FROM sqlite_schema"),
        [[Value::Text("notes".to_owned())]]
    );
    assert_eq!(
        raw_values(&connection, "SELECT body FROM notes"),
        [[Value::Text("keep".to_owned())]]
    );
}

#[test]
fn a_file_that_is_not_a_database_is_unrecognized_and_left_alone() {
    let dir = TestDir::new("not-a-database");
    let path = dir.database();
    let contents = b"this is a text file, not a database\n".repeat(200);
    fs::write(&path, &contents).expect("write text file");
    let not_a_database = DatabaseState::Unrecognized(Unrecognized::NotADatabase);

    assert_eq!(state(&path), not_a_database);
    for result in [
        open_existing(&path, "ws-a").map(drop),
        create(&path, "ws-a").map(drop),
    ] {
        assert!(
            matches!(
                result,
                Err(StorageError::Unrecognized(Unrecognized::NotADatabase))
            ),
            "{result:?}"
        );
    }
    assert_eq!(fs::read(&path).expect("read text file"), contents);
    assert_eq!(dir.entries(), ["workspace.db"]);
}
