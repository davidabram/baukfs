use std::io::{self, BufRead};
use std::process::Command;
use std::time::Duration;

use super::super::*;
use super::{TestDir, block_on, read_write, workspace};
use crate::{ContentId, EntryKind, Manifest, ManifestEntry, RevisionId, SessionId, WorkspacePath};

use std::io::{BufReader, Read};
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

const PROTOCOL_PREFIX: &str = "history-child> ";

struct CrashChild(Child);

impl CrashChild {
    fn at_checkpoint(mut command: Command) -> Self {
        let mut child = Self(
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn history child"),
        );
        let stdout = child.0.stdout.take().unwrap();
        let mut stderr = child.0.stderr.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        let (error_sender, errors) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                if line.contains(PROTOCOL_PREFIX) {
                    let _ = sender.send(line);
                }
            }
        });
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.read_to_end(&mut bytes);
            let _ = error_sender.send(bytes);
        });
        match receiver.recv_timeout(Duration::from_secs(30)) {
            Ok(line) if line.ends_with("checkpoint") => child,
            other => {
                child.terminate();
                let diagnostics = errors
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap_or_default();
                panic!(
                    "history child checkpoint: {other:?}; stderr: {}",
                    String::from_utf8_lossy(&diagnostics)
                );
            }
        }
    }

    fn terminate(&mut self) {
        let _ = self.0.kill();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.0.try_wait().expect("reap history child") {
                Some(_) => return,
                None if Instant::now() < deadline => thread::sleep(Duration::from_millis(2)),
                None => panic!("history child did not exit after termination"),
            }
        }
    }
}

impl Drop for CrashChild {
    fn drop(&mut self) {
        self.terminate();
    }
}

const ROLE: &str = "BAUKFS_HISTORY_TEST_ROLE";
const POINT: &str = "BAUKFS_HISTORY_TEST_POINT";
const PATH: &str = "BAUKFS_HISTORY_TEST_PATH";

pub(crate) fn before_commit() {
    if std::env::var(POINT).as_deref() == Ok("before") {
        report_and_wait();
    }
}

fn report_and_wait() {
    println!("{PROTOCOL_PREFIX}checkpoint");
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .expect("wait for parent");
    panic!("parent should terminate the process");
}

fn manifest() -> Manifest {
    Manifest::new(Vec::from([ManifestEntry::new(
        WorkspacePath::new(b"f".to_vec()).unwrap(),
        EntryKind::RegularFile {
            content: ContentId::of(b"new"),
            executable: false,
        },
    )]))
    .unwrap()
}

#[test]
fn process_child() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    let path = std::path::PathBuf::from(std::env::var_os(PATH).unwrap());
    let mut store = block_on(WorkspaceStore::open_existing(
        &path,
        &workspace("ws"),
        BusyTimeout::new(Duration::from_secs(10)),
    ))
    .unwrap();
    if role == "initialize" {
        block_on(store.initialize_history(&SessionId::new("s").unwrap(), &manifest())).unwrap();
    } else {
        let current = block_on(store.read_workspace()).unwrap().unwrap();
        block_on(store.advance_head(current.head(), &manifest())).unwrap();
    }
    report_and_wait();
}

#[test]
fn termination_before_commit_and_after_acknowledged_commit() {
    for role in ["initialize", "advance"] {
        for point in ["before", "after"] {
            let dir = TestDir::new("history-kill");
            let path = dir.database();
            let mut store = block_on(WorkspaceStore::create(
                &path,
                &workspace("ws"),
                BusyTimeout::new(Duration::from_secs(10)),
            ))
            .unwrap()
            .0;
            block_on(store.insert_content(b"new")).unwrap();
            let old = if role == "advance" {
                Some(
                    block_on(store.initialize_history(
                        &SessionId::new("s").unwrap(),
                        &Manifest::new(Vec::from([])).unwrap(),
                    ))
                    .unwrap()
                    .0,
                )
            } else {
                None
            };
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "storage::tests::history::process_child",
                    "--exact",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(ROLE, role)
                .env(POINT, point)
                .env(PATH, &path);
            let child = CrashChild::at_checkpoint(command);
            if point == "before" {
                assert_eq!(block_on(store.read_workspace()).unwrap(), old);
                assert_eq!(
                    block_on(store.read_manifest(&manifest().id().unwrap())).unwrap(),
                    None
                );
            }
            drop(child);
            drop(store);
            let mut store = block_on(WorkspaceStore::open_existing(
                &path,
                &workspace("ws"),
                BusyTimeout::new(Duration::from_secs(10)),
            ))
            .unwrap();
            let committed = block_on(store.read_workspace()).unwrap();
            if point == "before" {
                assert_eq!(committed, old);
                assert_eq!(
                    block_on(store.read_manifest(&manifest().id().unwrap())).unwrap(),
                    None
                );
            } else {
                let committed = committed.unwrap();
                assert_eq!(
                    block_on(store.read_manifest(&manifest().id().unwrap())).unwrap(),
                    Some(manifest())
                );
                let revision = block_on(store.read_revision(committed.head()))
                    .unwrap()
                    .unwrap();
                assert_eq!(revision.parent(), old.as_ref().map(crate::Workspace::head));
                assert_eq!(revision.manifest(), &manifest().id().unwrap());
            }
        }
    }
}

#[test]
fn missing_parent_rolls_back_manifest_and_revision() {
    let dir = TestDir::new("missing-parent");
    let path = dir.database();
    let mut store = block_on(WorkspaceStore::create(
        &path,
        &workspace("ws"),
        BusyTimeout::IMMEDIATE,
    ))
    .unwrap()
    .0;
    let result = block_on(store.history_transaction(
        TransactionBehavior::Immediate,
        async |connection| {
            history::persist(
                connection,
                &Manifest::new(Vec::from([])).unwrap(),
                Some(RevisionId::new("missing").unwrap()),
            )
            .await
        },
    ));
    assert!(matches!(
        result,
        Err(StorageError::History(HistoryError::MissingRevision(_)))
    ));
    assert_eq!(block_on(store.read_workspace()).unwrap(), None);
    assert_eq!(
        block_on(store.read_manifest(&Manifest::new(Vec::from([])).unwrap().id().unwrap()))
            .unwrap(),
        None
    );
}

#[test]
fn uncertain_commit_and_failed_rollback_discard_connections() {
    for rollback in [false, true] {
        let dir = TestDir::new("uncertain");
        let mut store = block_on(WorkspaceStore::create(
            &dir.database(),
            &workspace("ws"),
            BusyTimeout::IMMEDIATE,
        ))
        .unwrap()
        .0;
        let result = block_on(store.history_transaction(
            TransactionBehavior::Immediate,
            async |connection| {
                connection
                    .execute("ROLLBACK", ())
                    .await
                    .map_err(|e| Abandoned::Failed(DriverError::new(DriverStep::History, e)))?;
                if rollback {
                    Err(Abandoned::History(HistoryError::NotInitialized))
                } else {
                    Ok(())
                }
            },
        ));
        if rollback {
            assert!(matches!(result, Err(StorageError::RollbackFailed { .. })));
        } else {
            assert!(matches!(result, Err(StorageError::Driver(_))));
        }
        assert!(matches!(
            block_on(store.read_workspace()),
            Err(StorageError::ConnectionDiscarded)
        ));
        assert!(matches!(
            block_on(store.insert_content(b"new")),
            Err(StorageError::ConnectionDiscarded)
        ));
        let mut reopened = block_on(WorkspaceStore::open_existing(
            &dir.database(),
            &workspace("ws"),
            BusyTimeout::IMMEDIATE,
        ))
        .unwrap();
        assert_eq!(block_on(reopened.read_workspace()).unwrap(), None);
    }
}

#[test]
fn version_2_upgrade_preserves_content_and_identity_without_initializing_history() {
    let dir = TestDir::new("v2-history");
    let path = dir.database();
    let id = workspace("v2");
    let mut connection = read_write(&path);
    let transaction =
        block_on(connection.transaction_with_behavior(TransactionBehavior::Immediate)).unwrap();
    block_on(schema::apply_steps(
        &transaction,
        schema::SCHEMA
            .bootstrap
            .iter()
            .chain(schema::SCHEMA.upgrades[0])
            .copied(),
        2,
        &id,
        DriverStep::Bootstrap,
    ))
    .unwrap();
    block_on(content::insert_under_lock(
        &transaction,
        &ContentId::of(b"old"),
        b"old",
    ))
    .unwrap();
    block_on(transaction.commit()).unwrap();
    drop(connection);
    assert!(matches!(
        block_on(inspect(&path, BusyTimeout::IMMEDIATE)).unwrap(),
        DatabaseState::UpgradeRequired { found: 2, .. }
    ));
    assert!(matches!(
        block_on(WorkspaceStore::open_existing(
            &path,
            &id,
            BusyTimeout::IMMEDIATE
        )),
        Err(StorageError::UpgradeRequired { found: 2 })
    ));
    let (mut store, outcome) =
        block_on(WorkspaceStore::upgrade(&path, &id, BusyTimeout::IMMEDIATE)).unwrap();
    assert_eq!(outcome, UpgradeOutcome::Upgraded { from: 2 });
    assert_eq!(store.workspace_id(), &id);
    assert_eq!(
        block_on(store.read_content(&ContentId::of(b"old"))).unwrap(),
        Some(b"old".to_vec())
    );
    assert_eq!(block_on(store.read_workspace()).unwrap(), None);
}
