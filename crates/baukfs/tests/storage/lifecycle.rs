use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use baukfs::storage::{
    BusyTimeout, CURRENT_SCHEMA_VERSION, CreateOutcome, DatabaseState, DriverErrorKind, DriverStep,
    StorageError, WorkspaceStore, inspect,
};
use turso::Value;

use crate::support::{TestDir, block_on, metadata_rows, raw_connection, workspace};

const WAIT: BusyTimeout = BusyTimeout::IMMEDIATE;

fn create(path: &Path, id: &str) -> Result<(WorkspaceStore, CreateOutcome), StorageError> {
    block_on(WorkspaceStore::create(path, &workspace(id), WAIT))
}

fn expect_mismatch(
    result: Result<impl std::fmt::Debug, StorageError>,
    requested: &str,
    stored: &str,
) {
    match result {
        Err(StorageError::WorkspaceMismatch {
            requested: got_requested,
            stored: got_stored,
        }) => {
            assert_eq!(got_requested.as_str(), requested);
            assert_eq!(got_stored.as_str(), stored);
        }
        other => panic!("expected a workspace mismatch, got {other:?}"),
    }
}

#[test]
fn create_persists_identity_and_version_across_reopen() {
    let dir = TestDir::new("create-reopen");
    let path = dir.database();

    let (store, outcome) = create(&path, "ws-a").expect("create");
    assert_eq!(outcome, CreateOutcome::Created);
    assert_eq!(store.workspace_id().as_str(), "ws-a");
    assert_eq!(store.busy_timeout(), WAIT);
    assert_eq!(
        block_on(store.read_state()).expect("state through the store"),
        DatabaseState::Current(workspace("ws-a"))
    );
    drop(store);

    assert_eq!(
        metadata_rows(&path),
        [[
            Value::Integer(1),
            Value::Integer(CURRENT_SCHEMA_VERSION),
            Value::Text("ws-a".to_owned())
        ]]
    );
    assert_eq!(
        block_on(inspect(&path, WAIT)).expect("inspect"),
        DatabaseState::Current(workspace("ws-a"))
    );
    let reopened = block_on(WorkspaceStore::open_existing(
        &path,
        &workspace("ws-a"),
        WAIT,
    ))
    .expect("reopen");
    assert_eq!(reopened.workspace_id().as_str(), "ws-a");
}

#[test]
fn create_is_idempotent_for_the_same_identity() {
    let dir = TestDir::new("create-idempotent");
    let path = dir.database();
    let (first, outcome) = create(&path, "ws-a").expect("first create");
    assert_eq!(outcome, CreateOutcome::Created);

    let (second, outcome) = create(&path, "ws-a").expect("create while open");
    assert_eq!(outcome, CreateOutcome::AlreadyInitialized);
    drop((first, second));
    let (_, outcome) = create(&path, "ws-a").expect("create after close");
    assert_eq!(outcome, CreateOutcome::AlreadyInitialized);
    assert_eq!(metadata_rows(&path).len(), 1);
}

#[test]
fn another_workspace_identity_is_rejected_and_never_stored() {
    let dir = TestDir::new("identity-mismatch");
    let path = dir.database();
    drop(create(&path, "ws-a").expect("create"));

    expect_mismatch(create(&path, "ws-b"), "ws-b", "ws-a");
    expect_mismatch(
        block_on(WorkspaceStore::open_existing(
            &path,
            &workspace("ws-b"),
            WAIT,
        )),
        "ws-b",
        "ws-a",
    );
    assert_eq!(
        block_on(inspect(&path, WAIT)).expect("inspect"),
        DatabaseState::Current(workspace("ws-a"))
    );
}

#[test]
fn metadata_constraints_block_identity_changes_outside_the_api() {
    let dir = TestDir::new("metadata-constraints");
    let path = dir.database();
    drop(create(&path, "ws-a").expect("create"));
    let connection = raw_connection(&path);
    let attempts = [
        "UPDATE baukfs_metadata SET workspace_id = 'ws-b'",
        "DELETE FROM baukfs_metadata",
        "INSERT INTO baukfs_metadata (singleton, schema_version, workspace_id) VALUES (2, 1, 'ws-b')",
        "INSERT OR REPLACE INTO baukfs_metadata (singleton, schema_version, workspace_id) \
         VALUES (1, 1, 'ws-b')",
        "UPDATE baukfs_metadata SET singleton = 2",
    ];
    for sql in attempts {
        let result = block_on(connection.execute(sql, ()));
        assert!(
            matches!(
                result,
                Err(turso::Error::Error(_) | turso::Error::Constraint(_))
            ),
            "{sql}: {result:?}"
        );
    }
    drop(connection);
    assert_eq!(
        block_on(inspect(&path, WAIT)).expect("inspect"),
        DatabaseState::Current(workspace("ws-a"))
    );
}

#[test]
fn create_does_not_create_a_missing_parent_directory() {
    let dir = TestDir::new("create-missing-parent");
    let path = dir.path().join("absent").join("workspace.db");
    match create(&path, "ws-a") {
        Err(StorageError::Driver(error)) => {
            assert_eq!(error.step(), DriverStep::Open);
            assert_eq!(error.kind(), DriverErrorKind::Io(io::ErrorKind::NotFound));
        }
        other => panic!("expected a missing-parent open failure, got {other:?}"),
    }
    assert!(dir.entries().is_empty(), "{:?}", dir.entries());
}

#[test]
fn non_utf8_paths_are_rejected_before_opening() {
    let dir = TestDir::new("non-utf8");
    let path = dir.path().join(OsStr::from_bytes(b"workspace-\xff.db"));
    let is_non_utf8 = |error: &StorageError| matches!(error, StorageError::NonUtf8Path { path: rejected } if *rejected == path);
    let created = create(&path, "ws-a").map(drop);
    assert!(created.as_ref().is_err_and(is_non_utf8), "{created:?}");
    let opened = block_on(WorkspaceStore::open_existing(
        &path,
        &workspace("ws-a"),
        WAIT,
    ))
    .map(drop);
    assert!(opened.as_ref().is_err_and(is_non_utf8), "{opened:?}");
    let inspected = block_on(inspect(&path, WAIT));
    assert!(inspected.as_ref().is_err_and(is_non_utf8), "{inspected:?}");
    assert!(dir.entries().is_empty(), "{:?}", dir.entries());
}
