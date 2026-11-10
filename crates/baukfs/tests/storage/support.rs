use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::process;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use std::thread;

use baukfs::WorkspaceId;
use turso::{Builder, Connection, Value};

/// Polls until ready. The driver may return `Pending` without waking, so a
/// spinning poll is the simplest correct executor for these tests.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        thread::yield_now();
    }
}

pub fn workspace(value: &str) -> WorkspaceId {
    WorkspaceId::new(value).expect("valid workspace id")
}

pub struct TestDir(PathBuf);

impl TestDir {
    pub fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("baukfs-storage-{}-{unique}-{name}", process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn database(&self) -> PathBuf {
        self.0.join("workspace.db")
    }

    pub fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.0)
            .expect("list test directory")
            .map(|entry| {
                entry
                    .expect("directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn file_bytes(database: &Path, suffix: &str) -> Option<Vec<u8>> {
    let mut name = database.as_os_str().to_owned();
    name.push(suffix);
    fs::read(PathBuf::from(name)).ok()
}

/// Opens a database outside the storage API, for arranging and checking
/// fixtures. Uses multiprocess WAL like every other opener of these files.
pub fn raw_connection(path: &Path) -> Connection {
    raw_open(path, false)
}

pub fn raw_reader(path: &Path) -> Connection {
    raw_open(path, true)
}

fn raw_open(path: &Path, read_only: bool) -> Connection {
    let path = path.to_str().expect("test paths are UTF-8");
    let database = block_on(
        Builder::new_local(path)
            .experimental_multiprocess_wal(true)
            .read_only(read_only)
            .build(),
    )
    .expect("raw open");
    database.connect().expect("raw connect")
}

pub fn raw_execute(path: &Path, sql: &str) {
    let connection = raw_connection(path);
    block_on(connection.execute_batch(sql)).expect("raw execute");
}

pub fn raw_values(connection: &Connection, sql: &str) -> Vec<Vec<Value>> {
    let mut rows = block_on(connection.query(sql, ())).expect("raw query");
    let mut collected = Vec::new();
    while let Some(row) = block_on(rows.next()).expect("raw row") {
        collected.push(
            (0..row.column_count())
                .map(|index| row.get_value(index).expect("raw value"))
                .collect(),
        );
    }
    collected
}

pub fn metadata_rows(path: &Path) -> Vec<Vec<Value>> {
    raw_values(
        &raw_reader(path),
        "SELECT singleton, schema_version, workspace_id FROM baukfs_metadata",
    )
}

pub fn schema_cookie(path: &Path) -> i64 {
    match raw_values(&raw_reader(path), "PRAGMA schema_version").as_slice() {
        [row] => match row.as_slice() {
            [Value::Integer(cookie)] => *cookie,
            other => panic!("unexpected schema cookie row {other:?}"),
        },
        other => panic!("unexpected schema cookie rows {other:?}"),
    }
}

/// The version-1 schema exactly as the version-1 binary created it, kept
/// here as a frozen fixture independent of the library's upgrade steps.
const VERSION_1_SCHEMA: &str = "
BEGIN IMMEDIATE;
CREATE TABLE baukfs_metadata (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    schema_version INTEGER NOT NULL CHECK (schema_version >= 1),
    workspace_id TEXT NOT NULL CHECK (length(workspace_id) > 0)
) STRICT;
CREATE TRIGGER baukfs_metadata_single_row
BEFORE INSERT ON baukfs_metadata
WHEN EXISTS (SELECT 1 FROM baukfs_metadata)
BEGIN SELECT RAISE(ABORT, 'baukfs_metadata holds exactly one row'); END;
CREATE TRIGGER baukfs_metadata_identity_immutable
BEFORE UPDATE OF workspace_id ON baukfs_metadata
WHEN NEW.workspace_id IS NOT OLD.workspace_id
BEGIN SELECT RAISE(ABORT, 'baukfs workspace identity is immutable'); END;
CREATE TRIGGER baukfs_metadata_undeletable
BEFORE DELETE ON baukfs_metadata
BEGIN SELECT RAISE(ABORT, 'baukfs_metadata row cannot be deleted'); END;
";

pub fn create_version_1(path: &Path, workspace_id: &str) {
    let connection = raw_connection(path);
    block_on(connection.execute_batch(VERSION_1_SCHEMA)).expect("version-1 schema");
    block_on(connection.execute(
        "INSERT INTO baukfs_metadata (singleton, schema_version, workspace_id) VALUES (1, 1, ?1)",
        (workspace_id,),
    ))
    .expect("version-1 metadata");
    block_on(connection.execute("COMMIT", ())).expect("commit version-1 schema");
}

pub fn object_names(path: &Path) -> Vec<String> {
    raw_values(
        &raw_reader(path),
        "SELECT name FROM sqlite_schema ORDER BY name",
    )
    .into_iter()
    .map(|row| match row.as_slice() {
        [Value::Text(name)] => name.clone(),
        other => panic!("unexpected object row {other:?}"),
    })
    .collect()
}

/// Deterministic pseudo-random bytes, distinct for distinct seeds.
pub fn payload(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect()
}

/// Every stored content record as `(digest, byte_length, contents)`.
pub fn content_rows(path: &Path) -> Vec<(Vec<u8>, i64, Vec<u8>)> {
    raw_values(
        &raw_reader(path),
        "SELECT digest, byte_length, contents FROM baukfs_content ORDER BY digest",
    )
    .into_iter()
    .map(|row| match <[Value; 3]>::try_from(row) {
        Ok(
            [
                Value::Blob(digest),
                Value::Integer(length),
                Value::Blob(contents),
            ],
        ) => (digest, length, contents),
        other => panic!("unexpected content row {other:?}"),
    })
    .collect()
}
