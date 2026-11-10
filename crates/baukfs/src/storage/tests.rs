use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::process;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use std::thread;

use super::*;

/// Polls until ready. The driver may return `Pending` without waking, so a
/// spinning poll is the simplest correct executor for these tests.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        thread::yield_now();
    }
}

struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "baukfs-storage-unit-{}-{unique}-{name}",
            process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn database(&self) -> PathBuf {
        self.0.join("workspace.db")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn workspace(value: &str) -> WorkspaceId {
    WorkspaceId::new(value).expect("valid workspace id")
}

fn read_write(path: &Path) -> Connection {
    block_on(open(path, Access::ReadWrite, BusyTimeout::IMMEDIATE)).expect("open read-write")
}

fn state(path: &Path) -> DatabaseState {
    block_on(inspect(path, BusyTimeout::IMMEDIATE)).expect("inspect")
}

#[test]
fn injected_bootstrap_failure_rolls_back_schema_and_metadata_together() {
    let dir = TestDir::new("injected-failure");
    let path = dir.database();
    let mut connection = read_write(&path);
    let mut failing = BOOTSTRAP.to_vec();
    failing.push(BootstrapStep::Execute(
        "INSERT INTO baukfs_metadata (singleton, schema_version, workspace_id) \
         VALUES (1, 1, 'second')",
    ));

    let result = block_on(initialize(&mut connection, &workspace("ws-a"), &failing));

    let Err(StorageError::Driver(error)) = result else {
        panic!("expected a driver failure, got {result:?}");
    };
    assert_eq!(error.step(), DriverStep::Bootstrap);
    // Turso 0.8.3 reports RAISE(ABORT) from a trigger as its generic error variant.
    assert_eq!(error.kind(), DriverErrorKind::Other, "{error}");
    drop(connection);
    assert_eq!(state(&path), DatabaseState::Uninitialized);

    let mut connection = read_write(&path);
    let outcome = block_on(initialize(&mut connection, &workspace("ws-a"), &BOOTSTRAP));
    assert_eq!(
        outcome.expect("initialize after rollback"),
        CreateOutcome::Created
    );
}

#[test]
fn initializer_rechecks_state_after_taking_the_write_lock() {
    let dir = TestDir::new("recheck");
    let path = dir.database();
    let mut winner = read_write(&path);
    let mut same = read_write(&path);
    let mut other = read_write(&path);
    assert_eq!(state(&path), DatabaseState::Uninitialized);

    let won = block_on(initialize(&mut winner, &workspace("ws-a"), &BOOTSTRAP));
    assert_eq!(won.expect("winner"), CreateOutcome::Created);

    let converged = block_on(initialize(&mut same, &workspace("ws-a"), &BOOTSTRAP));
    assert_eq!(
        converged.expect("same identity"),
        CreateOutcome::InitializedConcurrently
    );

    let conflicting = block_on(initialize(&mut other, &workspace("ws-b"), &BOOTSTRAP));
    match conflicting {
        Err(StorageError::WorkspaceMismatch { requested, stored }) => {
            assert_eq!(requested.as_str(), "ws-b");
            assert_eq!(stored.as_str(), "ws-a");
        }
        unexpected => panic!("expected a workspace mismatch, got {unexpected:?}"),
    }
    assert_eq!(state(&path), DatabaseState::Current(workspace("ws-a")));
}
