use std::thread;
use std::time::{Duration, Instant};

use baukfs::storage::{
    BusyTimeout, CreateOutcome, DatabaseState, DriverStep, StorageError, WorkspaceStore, inspect,
};

use baukfs::ContentId;
use baukfs::storage::{InsertOutcome, UpgradeOutcome};

use crate::child::{Holder, SHARED_PAYLOADS, Starter, shared_payload};
use crate::support::{TestDir, block_on, content_rows, create_version_1, metadata_rows, workspace};

const PARALLEL_CREATORS: usize = 4;
/// Generous so that parallel test load cannot turn a lock wait into `Busy`;
/// every child is still bounded by the harness deadline.
const CREATOR_WAIT: Duration = Duration::from_secs(10);
/// How long a holder keeps the lock after every creator has started.
const HOLD_AFTER_START: Duration = Duration::from_millis(250);

fn state(path: &std::path::Path) -> DatabaseState {
    block_on(inspect(path, BusyTimeout::IMMEDIATE)).expect("inspect")
}

fn start_together(creators: &mut [Starter]) {
    for creator in creators.iter_mut() {
        creator.go();
    }
}

fn results(creators: Vec<Starter>) -> Vec<String> {
    let mut lines: Vec<String> = creators.into_iter().map(Starter::result).collect();
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
    let mut creators: Vec<Starter> = (0..PARALLEL_CREATORS)
        .map(|_| Starter::create(&path, "ws-shared", CREATOR_WAIT))
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
    let mut creators: Vec<Starter> = ids
        .iter()
        .map(|id| Starter::create(&path, id, CREATOR_WAIT))
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
    let mut creators: Vec<Starter> = ids
        .iter()
        .map(|id| Starter::create(&path, id, CREATOR_WAIT))
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

const PARALLEL_OBSERVERS: usize = 2;

fn assert_upgrades_converged(lines: &[String], id: &str) {
    let upgraded = format!("upgraded {:?} {id}", UpgradeOutcome::Upgraded { from: 1 });
    let joined = [
        format!("upgraded {:?} {id}", UpgradeOutcome::AlreadyCurrent),
        format!("upgraded {:?} {id}", UpgradeOutcome::UpgradedConcurrently),
    ];
    assert_eq!(
        lines.iter().filter(|line| **line == upgraded).count(),
        1,
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .all(|line| *line == upgraded || joined.contains(line)),
        "{lines:?}"
    );
}

/// Every state an inspector saw while upgrades ran must be the old or the
/// new version, never a mix of the two read as an incomplete schema.
fn assert_observations_coherent(lines: &[String], id: &str) {
    let old = format!(
        "{:?}",
        DatabaseState::UpgradeRequired {
            found: 1,
            workspace_id: workspace(id),
        }
    );
    let new = format!("{:?}", DatabaseState::Current(workspace(id)));
    for line in lines {
        let Some(states) = line.strip_prefix("observed ") else {
            panic!("observer failed: {line}");
        };
        let states: Vec<&str> = states.split(" | ").collect();
        assert!(
            states == [new.as_str()] || states == [old.as_str(), new.as_str()],
            "{states:?}"
        );
    }
}

fn assert_upgraded(path: &std::path::Path, id: &str) {
    assert_eq!(state(path), DatabaseState::Current(workspace(id)));
    assert_eq!(
        metadata_rows(path),
        [[
            turso::Value::Integer(1),
            turso::Value::Integer(baukfs::storage::CURRENT_SCHEMA_VERSION),
            turso::Value::Text(id.to_owned())
        ]]
    );
}

#[test]
fn parallel_upgraders_converge_and_inspectors_never_see_a_mixed_schema() {
    let dir = TestDir::new("parallel-upgrade");
    let path = dir.database();
    create_version_1(&path, "ws-a");
    let mut upgraders: Vec<Starter> = (0..PARALLEL_CREATORS)
        .map(|_| Starter::upgrade(&path, "ws-a", CREATOR_WAIT))
        .collect();
    let mut observers: Vec<Starter> = (0..PARALLEL_OBSERVERS)
        .map(|_| Starter::observe(&path, CREATOR_WAIT))
        .collect();
    start_together(&mut observers);
    start_together(&mut upgraders);

    let lines = results(upgraders);
    let observations = results(observers);
    println!("parallel upgraders: {lines:?}");
    println!("observers: {observations:?}");
    assert_upgrades_converged(&lines, "ws-a");
    assert_observations_coherent(&observations, "ws-a");
    assert_upgraded(&path, "ws-a");
}

#[test]
fn upgraders_queued_behind_a_held_lock_upgrade_exactly_once() {
    let dir = TestDir::new("queued-upgrade");
    let path = dir.database();
    create_version_1(&path, "ws-a");
    let holder = Holder::spawn(&path, None);
    let mut observers: Vec<Starter> = (0..PARALLEL_OBSERVERS)
        .map(|_| Starter::observe(&path, CREATOR_WAIT))
        .collect();
    let mut upgraders: Vec<Starter> = (0..PARALLEL_CREATORS)
        .map(|_| Starter::upgrade(&path, "ws-a", CREATOR_WAIT))
        .collect();
    start_together(&mut observers);
    start_together(&mut upgraders);
    thread::sleep(HOLD_AFTER_START);
    for child in upgraders.iter_mut().chain(&mut observers) {
        assert_eq!(
            child.has_replied(),
            None,
            "a child finished while the lock was held"
        );
    }
    holder.release();

    let lines = results(upgraders);
    let observations = results(observers);
    println!("queued upgraders: {lines:?}");
    println!("observers: {observations:?}");
    assert_upgrades_converged(&lines, "ws-a");
    assert_observations_coherent(&observations, "ws-a");
    for line in &observations {
        assert!(line.contains("UpgradeRequired"), "{line}");
    }
    assert_upgraded(&path, "ws-a");
}

#[test]
fn parallel_inserters_store_each_payload_once_without_overwriting() {
    let dir = TestDir::new("parallel-insert");
    let path = dir.database();
    drop(
        block_on(WorkspaceStore::create(
            &path,
            &workspace("ws-a"),
            BusyTimeout::IMMEDIATE,
        ))
        .expect("create"),
    );
    let mut inserters: Vec<Starter> = (0..PARALLEL_CREATORS)
        .map(|_| Starter::insert(&path, "ws-a", CREATOR_WAIT))
        .collect();
    start_together(&mut inserters);
    let lines = results(inserters);

    let payloads: Vec<Vec<u8>> = (0..SHARED_PAYLOADS).map(shared_payload).collect();
    let ids: Vec<ContentId> = payloads.iter().map(|bytes| ContentId::of(bytes)).collect();
    let mut inserted = vec![0_usize; payloads.len()];
    for line in &lines {
        let Some(outcomes) = line.strip_prefix("inserted ") else {
            panic!("inserter failed: {line}");
        };
        let outcomes: Vec<&str> = outcomes.split(',').collect();
        assert_eq!(outcomes.len(), payloads.len(), "{line}");
        for ((outcome, id), count) in outcomes.iter().zip(&ids).zip(&mut inserted) {
            if *outcome == format!("{:?}:{id}", InsertOutcome::Inserted) {
                *count += 1;
            } else {
                assert_eq!(
                    *outcome,
                    format!("{:?}:{id}", InsertOutcome::AlreadyPresent)
                );
            }
        }
    }
    println!("inserts per payload across {PARALLEL_CREATORS} processes: {inserted:?}");
    assert!(inserted.iter().all(|count| *count == 1), "{inserted:?}");

    let mut expected: Vec<(Vec<u8>, i64, Vec<u8>)> = ids
        .iter()
        .zip(payloads)
        .map(|(id, bytes)| {
            let length = i64::try_from(bytes.len()).expect("small payload");
            (id.digest().to_vec(), length, bytes)
        })
        .collect();
    expected.sort();
    assert_eq!(content_rows(&path), expected);
}
