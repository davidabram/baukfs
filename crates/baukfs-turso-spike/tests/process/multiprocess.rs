use std::thread;
use std::time::{Duration, Instant};

use crate::support::{
    BUSY_TIMEOUT_MS, INTERRUPT_HEAD, INTERRUPT_HEX, INTERRUPT_KEY, Mode, NO_BUSY_TIMEOUT,
    OTHER_HEAD, OTHER_HEX, OTHER_KEY, PRIOR_HEAD, PRIOR_HEX, PRIOR_KEY, Session, TestDb, TestDir,
    elapsed_field, interrupt_pair_uncommitted, millis, parse_err, presence_line,
};

const FAST_FAILURE_LIMIT_MS: f64 = 500.0;
const RELEASE_AFTER: Duration = Duration::from_millis(250);
const SCHEDULING_TOLERANCE_MS: f64 = 2000.0;

fn initialized(name: &str) -> (TestDir, TestDb) {
    let dir = TestDir::new(name);
    let db = dir.database(Mode::Multiprocess);
    db.initialize();
    (dir, db)
}

fn interrupt_visible() -> String {
    presence_line(Some(INTERRUPT_HEX), Some(INTERRUPT_KEY))
}

fn interrupt_absent() -> String {
    presence_line(None, None)
}

fn other_visible() -> String {
    presence_line(Some(OTHER_HEX), Some(OTHER_KEY))
}

fn seed_prior(db: &TestDb) {
    let mut seeder = db.session(NO_BUSY_TIMEOUT);
    let reply = seeder.write_pair(PRIOR_KEY, PRIOR_HEAD, PRIOR_HEX);
    assert!(reply.starts_with("ok write-pair "), "{reply}");
    seeder.close();
}

fn write_other(session: &mut Session) -> String {
    session.write_pair(OTHER_KEY, OTHER_HEAD, OTHER_HEX)
}

#[test]
fn durability_settings_in_multiprocess_mode() {
    let dir = TestDir::new("multiprocess-settings");
    let db = dir.database(Mode::Multiprocess);
    assert_eq!(
        db.run_ok("settings", NO_BUSY_TIMEOUT),
        ["journal_mode=wal synchronous=2"]
    );
}

#[test]
fn two_processes_keep_one_database_open() {
    let (_dir, db) = initialized("multiprocess-open-twice");
    let mut first = db.session(NO_BUSY_TIMEOUT);
    let mut second = db.session(NO_BUSY_TIMEOUT);
    assert_eq!(first.get(INTERRUPT_KEY, INTERRUPT_HEAD), interrupt_absent());
    assert_eq!(
        second.get(INTERRUPT_KEY, INTERRUPT_HEAD),
        interrupt_absent()
    );
    let reply = write_other(&mut first);
    assert!(reply.starts_with("ok write-pair "), "{reply}");
    assert_eq!(second.get(OTHER_KEY, OTHER_HEAD), other_visible());
    first.assert_running();
    second.assert_running();
    first.close();
    second.close();
}

#[test]
fn reader_sees_prior_state_during_uncommitted_write_then_both_records_after_commit() {
    let (_dir, db) = initialized("multiprocess-isolation");
    seed_prior(&db);
    let mut reader = db.session(NO_BUSY_TIMEOUT);
    let mut writer = interrupt_pair_uncommitted(&db, NO_BUSY_TIMEOUT);

    let during = reader.timed(&format!("get {INTERRUPT_KEY} {INTERRUPT_HEAD}"));
    assert_eq!(during.line, interrupt_absent());
    assert_eq!(
        reader.get(PRIOR_KEY, PRIOR_HEAD),
        presence_line(Some(PRIOR_HEX), Some(PRIOR_KEY))
    );

    writer.expect_ok("commit");
    assert_eq!(
        reader.get(INTERRUPT_KEY, INTERRUPT_HEAD),
        interrupt_visible()
    );
    println!(
        "timing multiprocess reader during uncommitted write: {:.1} ms round trip",
        during.parent_ms
    );
    reader.close();
    writer.close();
}

#[test]
fn competing_writer_without_busy_timeout_fails_fast() {
    let (_dir, db) = initialized("multiprocess-busy-unset");
    let mut holder = interrupt_pair_uncommitted(&db, NO_BUSY_TIMEOUT);
    let mut competitor = db.session(NO_BUSY_TIMEOUT);

    let started = Instant::now();
    let reply = write_other(&mut competitor);
    let parent_ms = millis(started.elapsed());
    let error = parse_err(&reply);
    assert!(error.is_busy_family(), "{reply}");
    assert_eq!(error.step, "begin", "{reply}");
    assert_eq!(error.autocommit, "true", "{reply}");
    assert!(error.elapsed_ms < FAST_FAILURE_LIMIT_MS, "{reply}");
    println!(
        "timing multiprocess busy unset: {} after {:.1} ms in child, {parent_ms:.1} ms round trip; message {:?}",
        error.kind, error.elapsed_ms, error.message
    );

    holder.expect_ok("rollback");
    assert_eq!(
        competitor.get(OTHER_KEY, OTHER_HEAD),
        presence_line(None, None)
    );
    holder.close();
    competitor.close();
}

#[test]
fn competing_writer_waits_for_a_release_within_its_busy_timeout() {
    let (_dir, db) = initialized("multiprocess-busy-release");
    let mut holder = interrupt_pair_uncommitted(&db, NO_BUSY_TIMEOUT);
    let mut competitor = db.session(BUSY_TIMEOUT_MS);

    let started = competitor.start_write_pair(OTHER_KEY, OTHER_HEAD, OTHER_HEX);
    thread::sleep(RELEASE_AFTER);
    assert_eq!(
        competitor.has_replied(),
        None,
        "the competing writer finished while the lock was held"
    );
    let holder_commit = holder.timed("commit");
    let reply = competitor.next_line();
    let parent_ms = millis(started.elapsed());
    println!(
        "timing multiprocess busy release: holder commit {:?} ({:.1} ms round trip), competitor {reply}",
        holder_commit.line, holder_commit.parent_ms
    );
    assert!(
        holder_commit.line.starts_with("ok commit "),
        "{}",
        holder_commit.line
    );
    assert!(reply.starts_with("ok write-pair "), "{reply}");
    let child_ms = elapsed_field(&reply);
    assert!(child_ms >= millis(RELEASE_AFTER), "{reply}");
    assert!(
        child_ms < millis(Duration::from_millis(BUSY_TIMEOUT_MS)),
        "{reply}"
    );
    println!(
        "timing multiprocess busy release: lock released after {} ms, competitor committed after {child_ms:.1} ms in child, {parent_ms:.1} ms from parent",
        RELEASE_AFTER.as_millis()
    );

    assert_eq!(
        competitor.get(INTERRUPT_KEY, INTERRUPT_HEAD),
        interrupt_visible()
    );
    assert_eq!(competitor.get(OTHER_KEY, OTHER_HEAD), other_visible());
    holder.close();
    competitor.close();
}

#[test]
fn competing_writer_exhausts_its_busy_timeout_without_partial_writes_then_recovers() {
    let (_dir, db) = initialized("multiprocess-busy-exhausted");
    let mut holder = interrupt_pair_uncommitted(&db, NO_BUSY_TIMEOUT);
    let mut competitor = db.session(BUSY_TIMEOUT_MS);

    let started = Instant::now();
    let reply = write_other(&mut competitor);
    let parent_ms = millis(started.elapsed());
    let error = parse_err(&reply);
    assert!(error.is_busy_family(), "{reply}");
    assert_eq!(error.step, "begin", "{reply}");
    assert_eq!(error.autocommit, "true", "{reply}");
    let budget_ms = millis(Duration::from_millis(BUSY_TIMEOUT_MS));
    assert!(error.elapsed_ms >= budget_ms, "{reply}");
    assert!(
        error.elapsed_ms < budget_ms + SCHEDULING_TOLERANCE_MS,
        "{reply}"
    );
    println!(
        "timing multiprocess busy exhausted: {} after {:.1} ms in child, {parent_ms:.1} ms round trip (busy timeout {BUSY_TIMEOUT_MS} ms)",
        error.kind, error.elapsed_ms
    );

    holder.assert_running();
    assert_eq!(
        competitor.get(OTHER_KEY, OTHER_HEAD),
        presence_line(None, None)
    );
    assert_eq!(
        competitor.get(INTERRUPT_KEY, INTERRUPT_HEAD),
        interrupt_absent()
    );

    holder.expect_ok("rollback");
    let retry = write_other(&mut competitor);
    assert!(retry.starts_with("ok write-pair "), "{retry}");
    holder.expect_ok("begin");
    holder.expect_ok(&format!("put {INTERRUPT_KEY} {INTERRUPT_HEX}"));
    holder.expect_ok(&format!("head {INTERRUPT_HEAD} {INTERRUPT_KEY}"));
    holder.expect_ok("commit");
    assert_eq!(competitor.get(OTHER_KEY, OTHER_HEAD), other_visible());
    assert_eq!(
        competitor.get(INTERRUPT_KEY, INTERRUPT_HEAD),
        interrupt_visible()
    );
    holder.close();
    competitor.close();
}

#[test]
fn termination_before_commit_exposes_neither_record() {
    let (_dir, db) = initialized("multiprocess-kill-before-commit");
    let mut survivor = db.session(BUSY_TIMEOUT_MS);
    let writer = interrupt_pair_uncommitted(&db, NO_BUSY_TIMEOUT);
    assert_eq!(
        survivor.get(INTERRUPT_KEY, INTERRUPT_HEAD),
        interrupt_absent()
    );
    writer.kill();

    assert_eq!(
        survivor.get(INTERRUPT_KEY, INTERRUPT_HEAD),
        interrupt_absent()
    );
    let started = Instant::now();
    let reply = write_other(&mut survivor);
    assert!(reply.starts_with("ok write-pair "), "{reply}");
    println!(
        "timing multiprocess write after writer killed mid-transaction: {:.1} ms in child, {:.1} ms round trip",
        elapsed_field(&reply),
        millis(started.elapsed())
    );
    survivor.close();

    assert_eq!(
        db.run_ok("inspect", NO_BUSY_TIMEOUT),
        ["content=absent head=absent"]
    );
    let lines = db.run_ok("inspect-existing", NO_BUSY_TIMEOUT);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("current ") && lines[0].ends_with(" content=absent head=absent"),
        "{lines:?}"
    );
}

#[test]
fn termination_after_commit_preserves_both_records() {
    let (_dir, db) = initialized("multiprocess-kill-after-commit");
    let mut survivor = db.session(BUSY_TIMEOUT_MS);
    let mut writer = interrupt_pair_uncommitted(&db, NO_BUSY_TIMEOUT);
    writer.expect_ok("commit");
    writer.kill();

    assert_eq!(
        survivor.get(INTERRUPT_KEY, INTERRUPT_HEAD),
        interrupt_visible()
    );
    let reply = write_other(&mut survivor);
    assert!(reply.starts_with("ok write-pair "), "{reply}");
    survivor.close();

    assert_eq!(
        db.run_ok("inspect", NO_BUSY_TIMEOUT),
        ["content=present head=present"]
    );
    let mut fresh = db.session(NO_BUSY_TIMEOUT);
    assert_eq!(fresh.get(OTHER_KEY, OTHER_HEAD), other_visible());
    fresh.close();
}
