use crate::harness::Finished;
use crate::support::interrupt_pair_uncommitted;
use crate::support::{Mode, NO_BUSY_TIMEOUT, TestDb, TestDir};

fn assert_second_process_cannot_open(db: &TestDb) {
    let finished = db.run("inspect", NO_BUSY_TIMEOUT);
    assert_eq!(finished.status.code(), Some(1), "{finished:?}");
    assert!(finished.lines.is_empty(), "{finished:?}");
    assert!(
        finished
            .stderr
            .contains("error: Error(\"Locking error: Failed locking file")
            && finished
                .stderr
                .contains("File is locked by another process"),
        "unexpected concurrent open failure:\n{}",
        finished.stderr
    );
}

#[test]
fn binary_contents_survive_a_fresh_process() {
    let dir = TestDir::new("default-roundtrip");
    let db = dir.database(Mode::Default);
    assert_eq!(
        db.run_ok("roundtrip-write", NO_BUSY_TIMEOUT),
        ["wrote 6 cases"]
    );
    assert_eq!(
        db.run_ok("roundtrip-verify", NO_BUSY_TIMEOUT),
        ["verified 6 cases"]
    );
}

#[test]
fn committed_transaction_is_visible_to_a_fresh_process() {
    let dir = TestDir::new("default-commit");
    let db = dir.database(Mode::Default);
    assert_eq!(
        db.run_ok("transaction-commit", NO_BUSY_TIMEOUT),
        ["content=present head=present"]
    );
    assert_eq!(
        db.run_ok("inspect", NO_BUSY_TIMEOUT),
        ["content=present head=present"]
    );
}

#[test]
fn rolled_back_transaction_is_invisible_to_a_fresh_process() {
    let dir = TestDir::new("default-rollback");
    let db = dir.database(Mode::Default);
    assert_eq!(
        db.run_ok("transaction-rollback", NO_BUSY_TIMEOUT),
        ["content=absent head=absent"]
    );
    assert_eq!(
        db.run_ok("inspect", NO_BUSY_TIMEOUT),
        ["content=absent head=absent"]
    );
}

#[test]
fn durability_settings_in_a_fresh_process() {
    let dir = TestDir::new("default-settings");
    let db = dir.database(Mode::Default);
    assert_eq!(
        db.run_ok("settings", NO_BUSY_TIMEOUT),
        ["journal_mode=wal synchronous=2"]
    );
}

#[test]
fn termination_before_commit_exposes_neither_record() {
    let dir = TestDir::new("default-kill-before-commit");
    let db = dir.database(Mode::Default);
    db.initialize();
    let writer = interrupt_pair_uncommitted(&db, NO_BUSY_TIMEOUT);
    assert_second_process_cannot_open(&db);
    writer.kill();
    assert_eq!(
        db.run_ok("inspect", NO_BUSY_TIMEOUT),
        ["content=absent head=absent"]
    );
}

#[test]
fn termination_after_commit_exposes_both_records() {
    let dir = TestDir::new("default-kill-after-commit");
    let db = dir.database(Mode::Default);
    db.initialize();
    let mut writer = interrupt_pair_uncommitted(&db, NO_BUSY_TIMEOUT);
    writer.expect_ok("commit");
    assert_second_process_cannot_open(&db);
    writer.kill();
    assert_eq!(
        db.run_ok("inspect", NO_BUSY_TIMEOUT),
        ["content=present head=present"]
    );
}

fn assert_accepted(finished: &Finished, expected: &str) {
    assert!(finished.status.success(), "{finished:?}");
    assert_eq!(finished.lines, [expected], "{finished:?}");
}

#[test]
fn mixed_open_modes_are_not_rejected_through_the_turso_crate() {
    let dir = TestDir::new("mixed-modes");
    let multiprocess_db = dir.database_named("multiprocess.db", Mode::Multiprocess);
    multiprocess_db.initialize();
    let holder = multiprocess_db.session(NO_BUSY_TIMEOUT);
    let default_view = multiprocess_db.deliberately_in_other_mode(Mode::Default);
    assert_accepted(
        &default_view.run("inspect", NO_BUSY_TIMEOUT),
        "content=absent head=absent",
    );
    assert_accepted(
        &default_view.run("transaction-commit", NO_BUSY_TIMEOUT),
        "content=present head=present",
    );
    holder.close();

    let default_db = dir.database_named("default.db", Mode::Default);
    default_db.initialize();
    let holder = default_db.session(NO_BUSY_TIMEOUT);
    let multiprocess_view = default_db.deliberately_in_other_mode(Mode::Multiprocess);
    assert_accepted(
        &multiprocess_view.run("inspect", NO_BUSY_TIMEOUT),
        "content=absent head=absent",
    );
    assert_accepted(
        &multiprocess_view.run("transaction-commit", NO_BUSY_TIMEOUT),
        "content=present head=present",
    );
    holder.close();
}
