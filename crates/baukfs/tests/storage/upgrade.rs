use std::fs;
use std::path::Path;

use baukfs::storage::{
    BusyTimeout, CURRENT_SCHEMA_VERSION, DatabaseState, DriverStep, InsertOutcome, StorageError,
    Unrecognized, UpgradeOutcome, WorkspaceStore, inspect,
};
use turso::Value;

use crate::support::{
    TestDir, block_on, create_version_1, file_bytes, metadata_rows, object_names, raw_connection,
    raw_execute, raw_values, schema_cookie, workspace,
};

const WAIT: BusyTimeout = BusyTimeout::IMMEDIATE;

const VERSION_2_OBJECTS: [&str; 3] = [
    "baukfs_content",
    "baukfs_content_immutable",
    "baukfs_content_insert_once",
];

fn state(path: &Path) -> DatabaseState {
    block_on(inspect(path, WAIT)).expect("inspect")
}

fn upgrade(path: &Path, id: &str) -> Result<(WorkspaceStore, UpgradeOutcome), StorageError> {
    block_on(WorkspaceStore::upgrade(path, &workspace(id), WAIT))
}

fn version_1_state(id: &str) -> DatabaseState {
    DatabaseState::UpgradeRequired {
        found: 1,
        workspace_id: workspace(id),
    }
}

/// Everything an operation that must not migrate could change.
#[derive(Debug, PartialEq)]
struct Unchanged {
    database: Option<Vec<u8>>,
    wal: Option<Vec<u8>>,
    objects: Vec<String>,
    metadata: Vec<Vec<Value>>,
}

fn unchanged(path: &Path) -> Unchanged {
    Unchanged {
        database: file_bytes(path, ""),
        wal: file_bytes(path, "-wal"),
        objects: object_names(path),
        metadata: metadata_rows(path),
    }
}

#[test]
fn version_1_databases_are_reported_and_only_the_upgrade_migrates_them() {
    let dir = TestDir::new("upgrade-required");
    let path = dir.database();
    create_version_1(&path, "ws-a");
    let before = unchanged(&path);
    let cookie = schema_cookie(&path);

    assert_eq!(state(&path), version_1_state("ws-a"));
    let opened = block_on(WorkspaceStore::open_existing(
        &path,
        &workspace("ws-a"),
        WAIT,
    ));
    assert!(
        matches!(opened, Err(StorageError::UpgradeRequired { found: 1 })),
        "{opened:?}"
    );
    let created = block_on(WorkspaceStore::create(&path, &workspace("ws-a"), WAIT));
    assert!(
        matches!(created, Err(StorageError::UpgradeRequired { found: 1 })),
        "{created:?}"
    );
    let other = block_on(WorkspaceStore::open_existing(
        &path,
        &workspace("ws-b"),
        WAIT,
    ));
    assert!(
        matches!(&other, Err(StorageError::WorkspaceMismatch { requested, stored })
            if requested.as_str() == "ws-b" && stored.as_str() == "ws-a"),
        "{other:?}"
    );

    assert_eq!(unchanged(&path), before);
    assert_eq!(schema_cookie(&path), cookie);
}

#[test]
fn upgrading_version_1_preserves_identity_and_enables_content() {
    let dir = TestDir::new("upgrade-success");
    let path = dir.database();
    create_version_1(&path, "ws-a");

    let (mut store, outcome) = upgrade(&path, "ws-a").expect("upgrade");
    assert_eq!(outcome, UpgradeOutcome::Upgraded { from: 1 });
    assert_eq!(store.workspace_id().as_str(), "ws-a");
    assert_eq!(
        block_on(store.read_state()).expect("state"),
        DatabaseState::Current(workspace("ws-a"))
    );
    let (id, inserted) = block_on(store.insert_content(b"after upgrade")).expect("insert");
    assert_eq!(inserted, InsertOutcome::Inserted);
    drop(store);

    assert_eq!(
        metadata_rows(&path),
        [[
            Value::Integer(1),
            Value::Integer(CURRENT_SCHEMA_VERSION),
            Value::Text("ws-a".to_owned())
        ]]
    );
    let objects = object_names(&path);
    assert!(
        VERSION_2_OBJECTS
            .iter()
            .all(|name| objects.iter().any(|o| o == name)),
        "{objects:?}"
    );
    let rejected = block_on(
        raw_connection(&path).execute("UPDATE baukfs_metadata SET workspace_id = 'ws-b'", ()),
    );
    assert!(rejected.is_err(), "{rejected:?}");

    let cookie = schema_cookie(&path);
    let (again, outcome) = upgrade(&path, "ws-a").expect("second upgrade");
    assert_eq!(outcome, UpgradeOutcome::AlreadyCurrent);
    assert_eq!(
        block_on(again.read_content(&id)).expect("read"),
        Some(b"after upgrade".to_vec())
    );
    drop(again);
    let reopened = block_on(WorkspaceStore::open_existing(
        &path,
        &workspace("ws-a"),
        WAIT,
    ))
    .expect("open existing after upgrade");
    drop(reopened);
    assert_eq!(schema_cookie(&path), cookie);
    assert_eq!(state(&path), DatabaseState::Current(workspace("ws-a")));
}

#[test]
fn upgrade_rejects_databases_it_must_not_touch() {
    let dir = TestDir::new("upgrade-rejects");

    let mismatch = dir.path().join("mismatch.db");
    create_version_1(&mismatch, "ws-a");
    let newer = dir.path().join("newer.db");
    create_version_1(&newer, "ws-a");
    raw_execute(&newer, "UPDATE baukfs_metadata SET schema_version = 99");
    let foreign = dir.path().join("foreign.db");
    raw_execute(&foreign, "CREATE TABLE notes (body TEXT NOT NULL)");
    let incomplete = dir.path().join("incomplete.db");
    create_version_1(&incomplete, "ws-a");
    raw_execute(&incomplete, "DROP TRIGGER baukfs_metadata_undeletable");
    let current_incomplete = dir.path().join("current-incomplete.db");
    drop(
        block_on(WorkspaceStore::create(
            &current_incomplete,
            &workspace("ws-a"),
            WAIT,
        ))
        .expect("create"),
    );
    raw_execute(
        &current_incomplete,
        "DROP TRIGGER baukfs_content_insert_once",
    );
    let not_a_database = dir.path().join("text.db");
    fs::write(&not_a_database, b"plain text, not a database\n".repeat(200)).expect("write");
    let empty = dir.path().join("empty.db");
    fs::write(&empty, b"").expect("write");

    let cases: [(&Path, DatabaseState); 7] = [
        (&mismatch, version_1_state("ws-a")),
        (
            &newer,
            DatabaseState::UnsupportedSchemaVersion { found: 99 },
        ),
        (
            &foreign,
            DatabaseState::Unrecognized(Unrecognized::ForeignSchema),
        ),
        (
            &incomplete,
            DatabaseState::Unrecognized(Unrecognized::IncompleteSchema),
        ),
        (
            &current_incomplete,
            DatabaseState::Unrecognized(Unrecognized::IncompleteSchema),
        ),
        (
            &not_a_database,
            DatabaseState::Unrecognized(Unrecognized::NotADatabase),
        ),
        (&empty, DatabaseState::Uninitialized),
    ];
    for (path, expected) in cases {
        let label = path.display();
        assert_eq!(state(path), expected, "{label}");
        let database = file_bytes(path, "");
        let wal = file_bytes(path, "-wal");
        let requested = if path == mismatch { "ws-b" } else { "ws-a" };
        let result = upgrade(path, requested).map(drop);
        let rejected = match (&expected, &result) {
            (
                DatabaseState::UpgradeRequired { .. },
                Err(StorageError::WorkspaceMismatch { requested, stored }),
            ) => requested.as_str() == "ws-b" && stored.as_str() == "ws-a",
            (
                DatabaseState::UnsupportedSchemaVersion { found },
                Err(StorageError::UnsupportedSchemaVersion { found: got }),
            ) => found == got,
            (DatabaseState::Unrecognized(reason), Err(StorageError::Unrecognized(got))) => {
                reason == got
            }
            (DatabaseState::Uninitialized, Err(StorageError::Uninitialized)) => true,
            _ => false,
        };
        assert!(rejected, "{label}: {result:?}");
        assert_eq!(state(path), expected, "{label}");
        assert_eq!(file_bytes(path, ""), database, "{label}");
        assert_eq!(file_bytes(path, "-wal"), wal, "{label}");
    }

    let missing = dir.path().join("missing.db");
    let entries = dir.entries();
    assert!(matches!(
        upgrade(&missing, "ws-a"),
        Err(StorageError::Missing)
    ));
    assert_eq!(dir.entries(), entries);
}

#[test]
fn a_failing_upgrade_step_rolls_back_and_leaves_version_1_intact() {
    let dir = TestDir::new("upgrade-step-fails");
    let path = dir.database();
    create_version_1(&path, "ws-a");
    raw_execute(
        &path,
        "CREATE TABLE baukfs_content (unrelated TEXT); INSERT INTO baukfs_content VALUES ('keep');",
    );
    let before = unchanged(&path);
    assert_eq!(state(&path), version_1_state("ws-a"));

    match upgrade(&path, "ws-a") {
        Err(StorageError::Driver(error)) => {
            assert_eq!(error.step(), DriverStep::Upgrade, "{error}");
        }
        other => panic!("expected a failed upgrade step, got {other:?}"),
    }

    assert_eq!(state(&path), version_1_state("ws-a"));
    assert_eq!(object_names(&path), before.objects);
    assert_eq!(metadata_rows(&path), before.metadata);
    assert_eq!(
        raw_values(
            &crate::support::raw_reader(&path),
            "SELECT unrelated FROM baukfs_content"
        ),
        [[Value::Text("keep".to_owned())]]
    );
}
