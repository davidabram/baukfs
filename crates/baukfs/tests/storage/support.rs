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
