use std::thread;
use std::time::{Duration, Instant};

use baukfs::storage::{
    BusyTimeout, CreateOutcome, DatabaseState, DriverStep, StorageError, WorkspaceStore, inspect,
};

use crate::child::{Creator, Holder};
use crate::support::{TestDir, block_on, workspace};

const PARALLEL_CREATORS: usize = 4;
/// Generous so that parallel test load cannot turn a lock wait into `Busy`;
/// every child is still bounded by the harness deadline.
const CREATOR_WAIT: Duration = Duration::from_secs(10);
/// How long a holder keeps the lock after every creator has started.
const HOLD_AFTER_START: Duration = Duration::from_millis(250);

fn state(path: &std::path::Path) -> DatabaseState {
    block_on(inspect(path, BusyTimeout::IMMEDIATE)).expect("inspect")
}

fn start_together(creators: &mut [Creator]) {
    for creator in creators.iter_mut() {
        creator.go();
    }
}

fn results(creators: Vec<Creator>) -> Vec<String> {
    let mut lines: Vec<String> = creators.into_iter().map(Creator::result).collect();
    lines.sort();
    lines
}

fn assert_converged(lines: &[String], id: &str) {
    let created = format!("created {:?} {id}", CreateOutcome::Created);
    let joined = [
        format!("created {:?} {id}", CreateOutcome::AlreadyInitialized),
        format!("created {:?} {id}", CreateOutcome::InitializedConcurrently),
    ];
    assert_eq!(
        lines.iter().filter(|line| **line == created).count(),
        1,
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .all(|line| *line == created || joined.contains(line)),
        "{lines:?}"
    );
}

/// Returns the winning identity after checking that every loser reports a
/// mismatch against it.
fn assert_single_winner(lines: &[String], ids: &[String]) -> String {
    let winners: Vec<&str> = lines
        .iter()
        .filter_map(|line| line.strip_prefix(&format!("created {:?} ", CreateOutcome::Created)))
        .collect();
    let [winner] = winners.as_slice() else {
        panic!("expected exactly one winner: {lines:?}");
    };
    let mut expected: Vec<String> = ids
        .iter()
        .map(|id| {
            if id == winner {
                format!("created {:?} {id}", CreateOutcome::Created)
            } else {
                format!("mismatch {id} {winner}")
            }
        })
        .collect();
    expected.sort();
    assert_eq!(lines, expected);
    (*winner).to_owned()
}

#[test]
fn parallel_creators_with_one_identity_converge_on_one_database() {
    let dir = TestDir::new("parallel-same");
    let path = dir.database();
    let mut creators: Vec<Creator> = (0..PARALLEL_CREATORS)
        .map(|_| Creator::spawn(&path, "ws-shared", CREATOR_WAIT))
        .collect();
    start_together(&mut creators);
    let lines = results(creators);
    println!("parallel same-identity creators: {lines:?}");
    assert_converged(&lines, "ws-shared");
    assert_eq!(state(&path), DatabaseState::Current(workspace("ws-shared")));
}

#[test]
fn parallel_creators_with_different_identities_have_one_winner() {
    let dir = TestDir::new("parallel-different");
    let path = dir.database();
    let ids: Vec<String> = (0..PARALLEL_CREATORS).map(|n| format!("ws-{n}")).collect();
    let mut creators: Vec<Creator> = ids
        .iter()
        .map(|id| Creator::spawn(&path, id, CREATOR_WAIT))
        .collect();
    start_together(&mut creators);
    let lines = results(creators);
    println!("parallel conflicting creators: {lines:?}");
    let winner = assert_single_winner(&lines, &ids);
    assert_eq!(state(&path), DatabaseState::Current(workspace(&winner)));
}

/// While the holder keeps the write lock on the empty database, no creator
/// can initialize, so all of them pass the read-only precheck before any
/// commits. The outcome invariants hold however the waiters are scheduled
/// after release.
fn creators_queued_behind_a_held_lock(name: &str, ids: &[&str]) -> (TestDir, Vec<String>) {
    let dir = TestDir::new(name);
    let path = dir.database();
    let holder = Holder::spawn(&path, None);
    let mut creators: Vec<Creator> = ids
        .iter()
        .map(|id| Creator::spawn(&path, id, CREATOR_WAIT))
        .collect();
    start_together(&mut creators);
    thread::sleep(HOLD_AFTER_START);
    for creator in &mut creators {
        assert_eq!(
            creator.has_replied(),
            None,
            "creator finished while the lock was held"
        );
    }
    holder.release();
    let lines = results(creators);
    (dir, lines)
}

#[test]
fn creators_queued_behind_a_held_lock_converge_for_one_identity() {
    let (dir, lines) = creators_queued_behind_a_held_lock("queued-same", &["ws-a", "ws-a"]);
    println!("queued same-identity creators: {lines:?}");
    assert_converged(&lines, "ws-a");
    assert_eq!(
        state(&dir.database()),
        DatabaseState::Current(workspace("ws-a"))
    );
}

#[test]
fn creators_queued_behind_a_held_lock_produce_one_winner_and_a_mismatch() {
    let ids = ["ws-a", "ws-b"];
    let (dir, lines) = creators_queued_behind_a_held_lock("queued-different", &ids);
    println!("queued conflicting creators: {lines:?}");
    let winner = assert_single_winner(&lines, &ids.map(str::to_owned));
    assert_eq!(
        state(&dir.database()),
        DatabaseState::Current(workspace(&winner))
    );
}

#[test]
fn inspection_while_another_process_writes_sees_only_committed_state() {
    let dir = TestDir::new("inspect-during-write");
    let path = dir.database();
    let id = workspace("ws-a");
    drop(block_on(WorkspaceStore::create(&path, &id, BusyTimeout::IMMEDIATE)).expect("create"));
    let holder = Holder::spawn(
        &path,
        Some("UPDATE baukfs_metadata SET schema_version = schema_version + 1"),
    );

    let started = Instant::now();
    let inspected = state(&path);
    let inspect_elapsed = started.elapsed();
    assert_eq!(inspected, DatabaseState::Current(id.clone()));
    let store = block_on(WorkspaceStore::open_existing(
        &path,
        &id,
        BusyTimeout::IMMEDIATE,
    ))
    .expect("open existing during the write");
    let (_, outcome) = block_on(WorkspaceStore::create(&path, &id, BusyTimeout::IMMEDIATE))
        .expect("create of an initialized database takes no write lock");
    assert_eq!(outcome, CreateOutcome::AlreadyInitialized);
    println!("inspect during held write transaction: {inspect_elapsed:?}");

    holder.release();
    assert_eq!(
        block_on(store.read_state()).expect("state after release"),
        DatabaseState::Current(id.clone())
    );
    assert_eq!(state(&path), DatabaseState::Current(id));
}

#[test]
fn create_against_a_held_write_lock_reports_typed_contention() {
    let dir = TestDir::new("create-busy");
    let path = dir.database();
    let id = workspace("ws-a");
    let holder = Holder::spawn(&path, None);

    for wait in [Duration::ZERO, Duration::from_millis(300)] {
        let started = Instant::now();
        let result = block_on(WorkspaceStore::create(&path, &id, BusyTimeout::new(wait)));
        let elapsed = started.elapsed();
        match &result {
            Err(StorageError::Driver(error)) => {
                assert_eq!(error.step(), DriverStep::Begin, "{error}");
                assert!(error.kind().is_busy(), "{error}");
            }
            other => panic!("expected lock contention, got {other:?}"),
        }
        assert!(
            elapsed >= wait,
            "gave up after {elapsed:?} with a {wait:?} busy timeout"
        );
        println!("create with {wait:?} busy timeout against a held lock: busy after {elapsed:?}");
        assert_eq!(state(&path), DatabaseState::Uninitialized);
    }

    holder.release();
    let (_, outcome) =
        block_on(WorkspaceStore::create(&path, &id, BusyTimeout::IMMEDIATE)).expect("create");
    assert_eq!(outcome, CreateOutcome::Created);
}
