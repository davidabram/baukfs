use crate::support::{TestDir, block_on, raw_connection, raw_execute, raw_values, workspace};
use baukfs::storage::{BusyTimeout, HistoryError, HistoryOutcome, StorageError, WorkspaceStore};
use baukfs::{ContentId, EntryKind, Manifest, ManifestEntry, RevisionId, SessionId, WorkspacePath};

fn session() -> SessionId {
    SessionId::new("session").unwrap()
}
fn empty() -> Manifest {
    Manifest::new(Vec::from([])).unwrap()
}
pub fn file_manifest(bytes: &[u8]) -> Manifest {
    Manifest::new(Vec::from([ManifestEntry::new(
        WorkspacePath::new(b"file".to_vec()).unwrap(),
        EntryKind::RegularFile {
            content: ContentId::of(bytes),
            executable: false,
        },
    )]))
    .unwrap()
}
fn create(dir: &TestDir) -> WorkspaceStore {
    block_on(WorkspaceStore::create(
        &dir.database(),
        &workspace("ws"),
        BusyTimeout::IMMEDIATE,
    ))
    .unwrap()
    .0
}
fn reopen(dir: &TestDir) -> WorkspaceStore {
    block_on(WorkspaceStore::open_existing(
        &dir.database(),
        &workspace("ws"),
        BusyTimeout::IMMEDIATE,
    ))
    .unwrap()
}
fn counts(dir: &TestDir) -> Vec<Vec<turso::Value>> {
    raw_values(
        &raw_connection(&dir.database()),
        "SELECT (SELECT count(*) FROM baukfs_manifests), (SELECT count(*) FROM baukfs_revisions), (SELECT count(*) FROM baukfs_workspace)",
    )
}
fn assert_counts(dir: &TestDir, manifests: i64, revisions: i64, workspaces: i64) {
    assert_eq!(
        counts(dir),
        Vec::from([Vec::from([
            turso::Value::Integer(manifests),
            turso::Value::Integer(revisions),
            turso::Value::Integer(workspaces)
        ])])
    );
}

#[test]
fn roots_reopen_lineage_dedup_and_retries() {
    let dir = TestDir::new("history-lineage");
    let mut store = create(&dir);
    assert_eq!(block_on(store.read_workspace()).unwrap(), None);
    let (root, outcome) = block_on(store.initialize_history(&session(), &empty())).unwrap();
    assert_eq!(outcome, HistoryOutcome::Initialized);
    assert_eq!(root.base(), root.head());
    let revision = block_on(store.read_revision(root.head())).unwrap().unwrap();
    assert_eq!(revision.parent(), None);
    assert_eq!(revision.manifest(), &empty().id().unwrap());
    let (same, outcome) = block_on(store.initialize_history(&session(), &empty())).unwrap();
    assert_eq!(same, root);
    assert_eq!(outcome, HistoryOutcome::AlreadyApplied);
    assert_counts(&dir, 1, 1, 1);
    drop(store);
    let mut store = reopen(&dir);
    assert_eq!(block_on(store.read_workspace()).unwrap().unwrap(), root);
    let (second, outcome) = block_on(store.advance_head(root.head(), &empty())).unwrap();
    assert_eq!(outcome, HistoryOutcome::Advanced);
    assert_ne!(second.head(), root.head());
    assert_eq!(second.base(), root.base());
    assert_eq!(second.session(), root.session());
    assert_eq!(
        block_on(store.read_revision(second.head()))
            .unwrap()
            .unwrap()
            .parent(),
        Some(root.head())
    );
    assert_eq!(
        block_on(store.advance_head(root.head(), &empty())).unwrap(),
        (second.clone(), HistoryOutcome::AlreadyApplied)
    );
    assert_counts(&dir, 1, 2, 1);
    block_on(store.insert_content(b"third")).unwrap();
    let third_manifest = file_manifest(b"third");
    let (third, _) = block_on(store.advance_head(second.head(), &third_manifest)).unwrap();
    assert_eq!(third.base(), root.base());
    assert_eq!(third.session(), root.session());
    assert_eq!(
        block_on(store.read_revision(third.head()))
            .unwrap()
            .unwrap()
            .parent(),
        Some(second.head())
    );
    assert!(matches!(
        block_on(store.advance_head(root.head(), &empty())),
        Err(StorageError::History(HistoryError::StaleHead { .. }))
    ));
    assert_counts(&dir, 2, 3, 1);
    assert_eq!(
        block_on(store.read_manifest(&third_manifest.id().unwrap())).unwrap(),
        Some(third_manifest)
    );
    let absent = RevisionId::new("opaque does not prove existence").unwrap();
    assert_eq!(block_on(store.read_revision(&absent)).unwrap(), None);
}

#[test]
fn conflicts_and_missing_content_leave_no_history() {
    let dir = TestDir::new("history-missing");
    let mut store = create(&dir);
    assert!(matches!(
        block_on(store.initialize_history(&session(), &file_manifest(b"missing"))),
        Err(StorageError::History(HistoryError::MissingContent(_)))
    ));
    assert_counts(&dir, 0, 0, 0);
    let (root, _) = block_on(store.initialize_history(&session(), &empty())).unwrap();
    assert!(matches!(
        block_on(store.initialize_history(&SessionId::new("other").unwrap(), &empty())),
        Err(StorageError::History(HistoryError::InitializationConflict))
    ));
    assert!(matches!(
        block_on(store.initialize_history(&session(), &file_manifest(b"missing"))),
        Err(StorageError::History(HistoryError::InitializationConflict))
    ));
    assert!(matches!(
        block_on(store.advance_head(root.head(), &file_manifest(b"missing"))),
        Err(StorageError::History(HistoryError::MissingContent(_)))
    ));
    assert_eq!(block_on(store.read_workspace()).unwrap(), Some(root));
    assert_counts(&dir, 1, 1, 1);
}

#[test]
fn corruption_is_rejected_on_reads_and_writes() {
    for damage in [
        "encoding",
        "manifest-id",
        "revision-id",
        "content",
        "missing-content",
        "parent",
        "missing-manifest",
    ] {
        let dir = TestDir::new(damage);
        let mut store = create(&dir);
        block_on(store.insert_content(b"bytes")).unwrap();
        let manifest = file_manifest(b"bytes");
        let (root, _) = block_on(store.initialize_history(&session(), &manifest)).unwrap();
        let (head, _) = block_on(store.advance_head(root.head(), &empty())).unwrap();
        match damage {
            "encoding" => raw_execute(
                &dir.database(),
                "DROP TRIGGER baukfs_manifests_immutable; UPDATE baukfs_manifests SET encoding = X'00'",
            ),
            "manifest-id" => {
                let connection = raw_connection(&dir.database());
                block_on(connection.execute("DROP TRIGGER baukfs_manifests_immutable", ()))
                    .unwrap();
                block_on(connection.execute(
                    "UPDATE baukfs_manifests SET encoding = ?1 WHERE digest = ?2",
                    (
                        empty().encode().unwrap(),
                        manifest.id().unwrap().digest().as_slice(),
                    ),
                ))
                .unwrap();
            }
            "revision-id" => raw_execute(
                &dir.database(),
                "DROP TRIGGER baukfs_revisions_immutable; UPDATE baukfs_revisions SET id = 'damaged' WHERE parent IS NULL",
            ),
            "content" => raw_execute(
                &dir.database(),
                "DROP TRIGGER baukfs_content_immutable; UPDATE baukfs_content SET contents = X'6261642121'",
            ),
            "missing-content" => raw_execute(&dir.database(), "DELETE FROM baukfs_content"),
            "parent" => raw_execute(
                &dir.database(),
                "DROP TRIGGER baukfs_revisions_undeletable; DELETE FROM baukfs_revisions WHERE parent IS NULL",
            ),
            "missing-manifest" => raw_execute(
                &dir.database(),
                "DROP TRIGGER baukfs_manifests_undeletable; DELETE FROM baukfs_manifests",
            ),
            _ => unreachable!(),
        }
        let error = block_on(store.read_workspace()).unwrap_err();
        match (damage, &error) {
            ("encoding", StorageError::History(HistoryError::ManifestEncoding(_)))
            | ("manifest-id", StorageError::History(HistoryError::ManifestIdentity(_)))
            | ("revision-id" | "parent", StorageError::History(HistoryError::MissingRevision(_)))
            | ("content", StorageError::Integrity(_))
            | ("missing-content", StorageError::History(HistoryError::MissingContent(_)))
            | ("missing-manifest", StorageError::History(HistoryError::MissingManifest(_))) => {}
            _ => panic!("unexpected {damage} failure: {error:?}"),
        }
        assert!(
            block_on(store.read_revision(head.head())).is_err(),
            "{damage}"
        );
        assert!(
            block_on(store.advance_head(head.head(), &empty())).is_err(),
            "{damage}"
        );
    }
}

#[test]
fn stored_revision_identity_is_verified_directly() {
    let dir = TestDir::new("revision-id");
    let mut store = create(&dir);
    block_on(store.initialize_history(&session(), &empty())).unwrap();
    raw_execute(
        &dir.database(),
        "DROP TRIGGER baukfs_revisions_immutable; UPDATE baukfs_revisions SET id = 'wrong'",
    );
    assert!(matches!(
        block_on(store.read_revision(&RevisionId::new("wrong").unwrap())),
        Err(StorageError::History(HistoryError::RevisionIdentity(_)))
    ));
}

#[test]
fn failures_after_each_persistence_step_rollback_everything() {
    for (table, operation) in [
        ("baukfs_manifests", "INSERT"),
        ("baukfs_revisions", "INSERT"),
        ("baukfs_workspace", "INSERT"),
        ("baukfs_workspace", "UPDATE"),
    ] {
        let dir = TestDir::new("history-injection");
        let mut store = create(&dir);
        let root = if operation == "UPDATE" {
            Some(
                block_on(store.initialize_history(&session(), &empty()))
                    .unwrap()
                    .0,
            )
        } else {
            None
        };
        block_on(store.insert_content(b"new")).unwrap();
        raw_execute(
            &dir.database(),
            &format!(
                "CREATE TRIGGER injected BEFORE {operation} ON {table} BEGIN SELECT RAISE(ABORT, 'injected history failure'); END"
            ),
        );
        let manifest = file_manifest(b"new");
        let result = if let Some(root) = &root {
            block_on(store.advance_head(root.head(), &manifest))
        } else {
            block_on(store.initialize_history(&session(), &manifest))
        };
        assert!(matches!(result, Err(StorageError::Driver(_))), "{result:?}");
        assert_counts(
            &dir,
            i64::from(root.is_some()),
            i64::from(root.is_some()),
            i64::from(root.is_some()),
        );
        assert_eq!(block_on(store.read_workspace()).unwrap(), root);
        assert_eq!(
            block_on(store.read_content(&ContentId::of(b"new"))).unwrap(),
            Some(b"new".to_vec())
        );
        raw_execute(&dir.database(), "DROP TRIGGER injected");
        if let Some(root) = root {
            block_on(store.advance_head(root.head(), &manifest)).unwrap();
        } else {
            block_on(store.initialize_history(&session(), &manifest)).unwrap();
        }
    }
}

#[test]
fn independent_databases_have_independent_histories() {
    let a = TestDir::new("history-a");
    let b = TestDir::new("history-b");
    let mut first = create(&a);
    let mut second = create(&b);
    let (root, _) = block_on(first.initialize_history(&session(), &empty())).unwrap();
    assert_eq!(block_on(second.read_revision(root.head())).unwrap(), None);
    assert_eq!(block_on(second.read_workspace()).unwrap(), None);
    block_on(second.initialize_history(&SessionId::new("other").unwrap(), &empty())).unwrap();
    block_on(first.advance_head(root.head(), &empty())).unwrap();
    assert_counts(&a, 1, 2, 1);
    assert_counts(&b, 1, 1, 1);
}

#[test]
fn running_and_sealed_reject_advancement() {
    for status in ["running", "sealed"] {
        let dir = TestDir::new(status);
        let mut store = create(&dir);
        let (root, _) = block_on(store.initialize_history(&session(), &empty())).unwrap();
        raw_execute(
            &dir.database(),
            &format!("UPDATE baukfs_workspace SET status = '{status}'"),
        );
        let before = block_on(store.read_workspace()).unwrap().unwrap();
        assert!(matches!(
            block_on(store.advance_head(root.head(), &empty())),
            Err(StorageError::History(HistoryError::Transition(_)))
        ));
        assert_eq!(block_on(store.read_workspace()).unwrap(), Some(before));
        assert_counts(&dir, 1, 1, 1);
    }
}

#[test]
fn competing_processes_cannot_both_advance_the_same_head() {
    let dir = TestDir::new("history-process-race");
    let mut store = create(&dir);
    let (root, _) = block_on(store.initialize_history(&session(), &empty())).unwrap();
    for bytes in [b"candidate-a", b"candidate-b"] {
        block_on(store.insert_content(bytes)).unwrap();
    }
    let mut a = crate::child::Starter::advance(&dir.database(), false);
    let mut b = crate::child::Starter::advance(&dir.database(), true);
    a.go();
    b.go();
    let outcomes = [a.result(), b.result()];
    assert_eq!(
        outcomes.iter().filter(|s| s.as_str() == "advanced").count(),
        1,
        "{outcomes:?}"
    );
    assert_eq!(
        outcomes.iter().filter(|s| s.as_str() == "stale").count(),
        1,
        "{outcomes:?}"
    );
    let final_workspace = block_on(store.read_workspace()).unwrap().unwrap();
    assert_eq!(final_workspace.base(), root.base());
    assert_ne!(final_workspace.head(), root.head());
    assert_counts(&dir, 2, 2, 1);
}

#[test]
fn zero_row_compare_and_swap_rolls_back_new_records() {
    let dir = TestDir::new("cas-zero");
    let mut store = create(&dir);
    let (root, _) = block_on(store.initialize_history(&session(), &empty())).unwrap();
    block_on(store.insert_content(b"new")).unwrap();
    raw_execute(
        &dir.database(),
        "CREATE TRIGGER ignore_head BEFORE UPDATE OF head ON baukfs_workspace BEGIN SELECT RAISE(IGNORE); END",
    );
    assert!(matches!(
        block_on(store.advance_head(root.head(), &file_manifest(b"new"))),
        Err(StorageError::History(
            HistoryError::HeadCompareAndSwapFailed
        ))
    ));
    assert_counts(&dir, 1, 1, 1);
    assert_eq!(block_on(store.read_workspace()).unwrap(), Some(root));
}

#[test]
fn history_sql_immutability_and_repeated_content_references() {
    let dir = TestDir::new("history-immutable");
    let mut store = create(&dir);
    let content = block_on(store.insert_content(b"shared")).unwrap().0;
    let manifest = Manifest::new(
        [b"a", b"b"]
            .into_iter()
            .map(|path| {
                ManifestEntry::new(
                    WorkspacePath::new(path.to_vec()).unwrap(),
                    EntryKind::RegularFile {
                        content,
                        executable: false,
                    },
                )
            })
            .collect(),
    )
    .unwrap();
    let (root, _) = block_on(store.initialize_history(&session(), &manifest)).unwrap();
    let connection = raw_connection(&dir.database());
    for table in ["baukfs_manifests", "baukfs_revisions", "baukfs_workspace"] {
        for sql in [
            format!("DELETE FROM {table}"),
            format!("INSERT OR REPLACE INTO {table} SELECT * FROM {table}"),
        ] {
            assert!(block_on(connection.execute(&sql, ())).is_err(), "{sql}");
        }
    }
    for sql in [
        "UPDATE baukfs_manifests SET encoding = encoding",
        "UPDATE baukfs_revisions SET parent = parent",
        "UPDATE baukfs_workspace SET session = 'other'",
        "UPDATE baukfs_workspace SET base = 'other'",
    ] {
        assert!(block_on(connection.execute(sql, ())).is_err(), "{sql}");
    }
    assert_eq!(
        block_on(store.read_manifest(&manifest.id().unwrap())).unwrap(),
        Some(manifest)
    );
    assert_eq!(block_on(store.read_workspace()).unwrap(), Some(root));
}

#[test]
fn corrupt_candidate_content_cannot_initialize_or_advance_history() {
    for initialize in [false, true] {
        let dir = TestDir::new("candidate-corruption");
        let mut store = create(&dir);
        block_on(store.insert_content(b"new")).unwrap();
        let root = if initialize {
            None
        } else {
            Some(
                block_on(store.initialize_history(&session(), &empty()))
                    .unwrap()
                    .0,
            )
        };
        raw_execute(
            &dir.database(),
            "DROP TRIGGER baukfs_content_immutable; UPDATE baukfs_content SET byte_length = 99",
        );
        let result = match &root {
            None => block_on(store.initialize_history(&session(), &file_manifest(b"new"))),
            Some(root) => block_on(store.advance_head(root.head(), &file_manifest(b"new"))),
        };
        assert!(
            matches!(result, Err(StorageError::Integrity(_))),
            "{result:?}"
        );
        assert_counts(
            &dir,
            i64::from(root.is_some()),
            i64::from(root.is_some()),
            i64::from(root.is_some()),
        );
        assert_eq!(block_on(store.read_workspace()).unwrap(), root);
    }
}
