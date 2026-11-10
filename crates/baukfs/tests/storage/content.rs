use std::collections::HashSet;
use std::path::Path;

use baukfs::ContentId;
use baukfs::storage::{
    BusyTimeout, InsertOutcome, IntegrityViolation, StorageError, WorkspaceStore,
};
use turso::Value;

use crate::support::{
    TestDir, block_on, content_rows, payload, raw_connection, raw_values, schema_cookie, workspace,
};

const WAIT: BusyTimeout = BusyTimeout::IMMEDIATE;

/// Recreated after tampering so the database still classifies as current.
const IMMUTABLE_TRIGGER: &str = "CREATE TRIGGER baukfs_content_immutable
    BEFORE UPDATE ON baukfs_content
    BEGIN
        SELECT RAISE(ABORT, 'baukfs content is immutable');
    END";

fn create(path: &Path, id: &str) -> WorkspaceStore {
    block_on(WorkspaceStore::create(path, &workspace(id), WAIT))
        .expect("create")
        .0
}

fn insert(store: &mut WorkspaceStore, bytes: &[u8]) -> (ContentId, InsertOutcome) {
    block_on(store.insert_content(bytes)).expect("insert content")
}

fn read(store: &WorkspaceStore, id: &ContentId) -> Option<Vec<u8>> {
    block_on(store.read_content(id)).expect("read content")
}

fn cases() -> [(&'static str, Vec<u8>); 5] {
    [
        ("empty", Vec::new()),
        ("embedded NUL", b"before\0after\0".to_vec()),
        ("non-UTF-8", [0xff, 0xfe, 0x80, b'a', 0xc3, 0x28].to_vec()),
        ("all byte values", (0..=u8::MAX).collect()),
        ("8 MiB", payload(7, 8 << 20)),
    ]
}

/// Simulates damage that bypasses the SQL protections: the immutability
/// trigger is dropped, the record changed, and the trigger restored.
fn tamper(path: &Path, id: &ContentId, assignment: &str) {
    let connection = raw_connection(path);
    block_on(connection.execute("BEGIN IMMEDIATE", ())).expect("begin");
    block_on(connection.execute("DROP TRIGGER baukfs_content_immutable", ())).expect("drop");
    let changed = block_on(connection.execute(
        format!("UPDATE baukfs_content SET {assignment} WHERE digest = ?1"),
        (id.digest().as_slice(),),
    ))
    .expect("tamper");
    assert_eq!(changed, 1);
    block_on(connection.execute(IMMUTABLE_TRIGGER, ())).expect("restore trigger");
    block_on(connection.execute("COMMIT", ())).expect("commit");
}

fn expect_integrity(
    result: Result<impl std::fmt::Debug, StorageError>,
    id: &ContentId,
) -> IntegrityViolation {
    match result {
        Err(StorageError::Integrity(error)) => {
            assert_eq!(error.id(), id);
            error.violation().clone()
        }
        other => panic!("expected an integrity error, got {other:?}"),
    }
}

#[test]
fn bytes_round_trip_exactly_under_their_blake3_identity() {
    let dir = TestDir::new("content-round-trip");
    let path = dir.database();
    let mut store = create(&path, "ws-a");
    let mut ids = HashSet::new();

    for (name, bytes) in cases() {
        let (id, outcome) = insert(&mut store, &bytes);
        assert_eq!(outcome, InsertOutcome::Inserted, "{name}");
        assert_eq!(id, ContentId::of(&bytes), "{name}");
        assert_eq!(id.digest(), blake3::hash(&bytes).as_bytes(), "{name}");
        assert_eq!(
            read(&store, &id).as_deref(),
            Some(bytes.as_slice()),
            "{name}"
        );
        assert!(
            ids.insert(id),
            "{name}: identifier collides with another case"
        );
    }

    let rows = content_rows(&path);
    assert_eq!(rows.len(), cases().len());
    for (digest, length, contents) in rows {
        assert_eq!(usize::try_from(length).ok(), Some(contents.len()));
        assert_eq!(digest, blake3::hash(&contents).as_bytes());
    }
}

#[test]
fn repeated_inserts_keep_one_record_and_one_identifier() {
    let dir = TestDir::new("content-repeat");
    let path = dir.database();
    let mut store = create(&path, "ws-a");
    let mut second = block_on(WorkspaceStore::open_existing(
        &path,
        &workspace("ws-a"),
        WAIT,
    ))
    .expect("second handle");
    let bytes = b"same bytes".to_vec();

    let (id, first) = insert(&mut store, &bytes);
    assert_eq!(first, InsertOutcome::Inserted);
    assert_eq!(
        insert(&mut store, &bytes),
        (id, InsertOutcome::AlreadyPresent)
    );
    assert_eq!(
        insert(&mut second, &bytes),
        (id, InsertOutcome::AlreadyPresent)
    );
    assert_eq!(
        insert(&mut store, &bytes),
        (id, InsertOutcome::AlreadyPresent)
    );
    assert_eq!(content_rows(&path), [(id.digest().to_vec(), 10, bytes)]);
}

#[test]
fn missing_content_is_none_not_an_error() {
    let dir = TestDir::new("content-missing");
    let path = dir.database();
    let mut store = create(&path, "ws-a");
    assert_eq!(read(&store, &ContentId::of(b"never stored")), None);
    insert(&mut store, b"stored");
    assert_eq!(read(&store, &ContentId::of(b"never stored")), None);
}

#[test]
fn stored_bytes_persist_across_reopen_and_reads_change_nothing() {
    let dir = TestDir::new("content-reopen");
    let path = dir.database();
    let mut store = create(&path, "ws-a");
    let stored: Vec<(ContentId, Vec<u8>)> = cases()
        .into_iter()
        .map(|(_, bytes)| (insert(&mut store, &bytes).0, bytes))
        .collect();
    drop(store);
    let cookie = schema_cookie(&path);
    let rows = content_rows(&path);

    let reopened = block_on(WorkspaceStore::open_existing(
        &path,
        &workspace("ws-a"),
        WAIT,
    ))
    .expect("reopen");
    for (id, bytes) in &stored {
        assert_eq!(read(&reopened, id).as_ref(), Some(bytes));
    }
    drop(reopened);
    assert_eq!(schema_cookie(&path), cookie);
    assert_eq!(content_rows(&path), rows);
}

#[test]
fn independent_copies_store_and_read_content_independently() {
    let dir = TestDir::new("content-copies");
    let path_a = dir.path().join("copy-a.db");
    let path_b = dir.path().join("copy-b.db");
    let mut copy_a = create(&path_a, "ws-a");
    let mut copy_b = create(&path_b, "ws-b");

    let (only_a, _) = insert(&mut copy_a, b"only in a");
    let (only_b, _) = insert(&mut copy_b, b"only in b");
    assert_eq!(insert(&mut copy_a, b"in both").1, InsertOutcome::Inserted);
    assert_eq!(insert(&mut copy_b, b"in both").1, InsertOutcome::Inserted);

    assert_eq!(read(&copy_a, &only_a).as_deref(), Some(&b"only in a"[..]));
    assert_eq!(read(&copy_a, &only_b), None);
    assert_eq!(read(&copy_b, &only_b).as_deref(), Some(&b"only in b"[..]));
    assert_eq!(read(&copy_b, &only_a), None);
    assert_eq!(content_rows(&path_a).len(), 2);
    assert_eq!(content_rows(&path_b).len(), 2);
}

#[test]
fn sql_constraints_keep_content_records_immutable_and_well_formed() {
    let dir = TestDir::new("content-constraints");
    let path = dir.database();
    let mut store = create(&path, "ws-a");
    let (id, _) = insert(&mut store, b"original");
    let digest = id.digest().as_slice();
    let connection = raw_connection(&path);

    let attempts: [(&str, Vec<Value>); 7] = [
        (
            "UPDATE baukfs_content SET contents = ?1",
            Vec::from([Value::Blob(b"replaced".to_vec())]),
        ),
        ("UPDATE baukfs_content SET byte_length = 1", Vec::new()),
        (
            "INSERT OR REPLACE INTO baukfs_content (digest, byte_length, contents) \
             VALUES (?1, 8, ?2)",
            Vec::from([
                Value::Blob(digest.to_vec()),
                Value::Blob(b"replaced".to_vec()),
            ]),
        ),
        (
            "INSERT INTO baukfs_content (digest, byte_length, contents) VALUES (?1, 8, ?2)",
            Vec::from([
                Value::Blob(digest.to_vec()),
                Value::Blob(b"replaced".to_vec()),
            ]),
        ),
        (
            "INSERT INTO baukfs_content (digest, byte_length, contents) VALUES (?1, 0, X'')",
            Vec::from([Value::Blob(vec![0; 31])]),
        ),
        (
            "INSERT INTO baukfs_content (digest, byte_length, contents) VALUES (?1, -1, X'')",
            Vec::from([Value::Blob(vec![1; 32])]),
        ),
        (
            "INSERT INTO baukfs_content (digest, byte_length, contents) VALUES (?1, 0, NULL)",
            Vec::from([Value::Blob(vec![2; 32])]),
        ),
    ];
    for (sql, params) in attempts {
        let result = block_on(connection.execute(sql, params));
        assert!(
            matches!(
                result,
                Err(turso::Error::Error(_) | turso::Error::Constraint(_))
            ),
            "{sql}: {result:?}"
        );
    }
    drop(connection);
    assert_eq!(read(&store, &id).as_deref(), Some(&b"original"[..]));
    assert_eq!(content_rows(&path).len(), 1);
}

#[test]
fn tampered_bytes_are_reported_and_never_returned_or_overwritten() {
    let dir = TestDir::new("content-tampered-bytes");
    let path = dir.database();
    let mut store = create(&path, "ws-a");
    let (id, _) = insert(&mut store, b"original");
    tamper(&path, &id, "contents = X'6f726967696e414c'");
    let tampered = b"originAL";

    let violation = expect_integrity(block_on(store.read_content(&id)), &id);
    assert_eq!(
        violation,
        IntegrityViolation::DigestMismatch {
            computed: ContentId::of(tampered)
        }
    );
    let violation = expect_integrity(block_on(store.insert_content(b"original")), &id);
    assert!(
        matches!(violation, IntegrityViolation::DigestMismatch { .. }),
        "{violation:?}"
    );
    assert_eq!(
        content_rows(&path),
        [(id.digest().to_vec(), 8, tampered.to_vec())]
    );
}

#[test]
fn a_tampered_length_is_reported_and_never_returned() {
    let dir = TestDir::new("content-tampered-length");
    let path = dir.database();
    let mut store = create(&path, "ws-a");
    let (id, _) = insert(&mut store, b"original");
    tamper(&path, &id, "byte_length = 9");

    let expected = IntegrityViolation::LengthMismatch {
        recorded: 9,
        stored: 8,
    };
    assert_eq!(
        expect_integrity(block_on(store.read_content(&id)), &id),
        expected
    );
    assert_eq!(
        expect_integrity(block_on(store.insert_content(b"original")), &id),
        expected
    );
    let reader = crate::support::raw_reader(&path);
    assert_eq!(
        raw_values(
            &reader,
            "SELECT byte_length, length(contents) FROM baukfs_content"
        ),
        [[Value::Integer(9), Value::Integer(8)]]
    );
}
