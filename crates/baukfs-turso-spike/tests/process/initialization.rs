use std::time::Instant;

use crate::support::{
    BUSY_TIMEOUT_MS, INTERRUPT_HEAD, INTERRUPT_HEX, INTERRUPT_KEY, Mode, NO_BUSY_TIMEOUT, TestDb,
    TestDir, interrupt_pair_uncommitted, millis, ok,
};

const PARALLEL_INITIALIZERS: usize = 4;

fn inspect_existing(db: &TestDb, busy_timeout_ms: u64) -> String {
    let mut lines = db.run_ok("inspect-existing", busy_timeout_ms);
    assert_eq!(lines.len(), 1, "{lines:?}");
    lines.remove(0)
}

fn schema_cookie(line: &str) -> i64 {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix("schema_cookie="))
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("no schema_cookie in {line:?}"))
}

fn outcome(line: &str) -> &str {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix("outcome="))
        .unwrap_or_else(|| panic!("no outcome in {line:?}"))
}

#[test]
fn concurrent_initializer_rechecks_readiness_after_obtaining_the_write_lock() {
    let dir = TestDir::new("init-recheck");
    let db = dir.database(Mode::Multiprocess);
    let mut first = db.session(NO_BUSY_TIMEOUT);
    assert_eq!(
        first.request("init-hold"),
        "ok init-hold recheck=uninitialized"
    );

    let mut second = ok(db.spawn("initialize", BUSY_TIMEOUT_MS));
    ok(second.expect_line("precheck uninitialized"));
    first.expect_ok("commit");
    let result = ok(second.next_line());
    assert_eq!(outcome(&result), "current-after-lock", "{result}");
    assert!(ok(second.finish_ok()).is_empty());
    first.close();

    let lines = db.run_ok("initialize", NO_BUSY_TIMEOUT);
    assert_eq!(lines[0], "precheck current", "{lines:?}");
    assert_eq!(outcome(&lines[1]), "already-current", "{lines:?}");
}

#[test]
fn parallel_initializers_apply_the_schema_exactly_once() {
    let dir = TestDir::new("init-parallel");
    let db = dir.database(Mode::Multiprocess);
    let probes: Vec<_> = (0..PARALLEL_INITIALIZERS)
        .map(|_| ok(db.spawn("initialize", BUSY_TIMEOUT_MS)))
        .collect();
    let mut outcomes: Vec<String> = probes
        .into_iter()
        .map(|probe| {
            let lines = ok(probe.finish_ok());
            assert_eq!(lines.len(), 2, "{lines:?}");
            outcome(&lines[1]).to_owned()
        })
        .collect();
    outcomes.sort();
    println!("parallel initializer outcomes: {outcomes:?}");
    assert_eq!(
        outcomes.iter().filter(|value| *value == "applied").count(),
        1,
        "{outcomes:?}"
    );
    assert!(
        outcomes
            .iter()
            .all(|value| ["applied", "already-current", "current-after-lock"]
                .contains(&value.as_str())),
        "{outcomes:?}"
    );
}

#[test]
fn inspecting_a_current_database_writes_nothing() {
    let dir = TestDir::new("inspect-no-write");
    let db = dir.database(Mode::Multiprocess);
    db.initialize();
    let mut writer = interrupt_pair_uncommitted(&db, NO_BUSY_TIMEOUT);
    writer.expect_ok("commit");
    writer.close();

    let entries_before = dir.entries();
    let database_before = db.file_bytes("");
    let wal_before = db.file_bytes("-wal");
    let first = inspect_existing(&db, NO_BUSY_TIMEOUT);
    let second = inspect_existing(&db, NO_BUSY_TIMEOUT);
    assert!(first.starts_with("current "), "{first}");
    assert!(first.ends_with(" content=present head=present"), "{first}");
    assert_eq!(schema_cookie(&first), schema_cookie(&second));
    assert_eq!(dir.entries(), entries_before);
    assert!(database_before.is_some());
    assert!(
        db.file_bytes("") == database_before,
        "database file changed"
    );
    assert!(db.file_bytes("-wal") == wal_before, "WAL file changed");
    println!(
        "inspect-no-write: files {entries_before:?}, wal bytes {:?}, schema cookie {}",
        wal_before.as_ref().map(Vec::len),
        schema_cookie(&first)
    );
}

#[test]
fn inspection_completes_while_another_process_holds_a_write_transaction() {
    let dir = TestDir::new("inspect-during-write");
    let db = dir.database(Mode::Multiprocess);
    db.initialize();
    let mut writer = interrupt_pair_uncommitted(&db, NO_BUSY_TIMEOUT);
    let started = Instant::now();
    let line = inspect_existing(&db, NO_BUSY_TIMEOUT);
    let parent_ms = millis(started.elapsed());
    assert!(line.starts_with("current "), "{line}");
    assert!(line.ends_with(" content=absent head=absent"), "{line}");
    println!(
        "timing inspect-existing during held write transaction: {parent_ms:.1} ms process lifetime"
    );
    writer.assert_running();
    writer.expect_ok("commit");
    assert_eq!(
        writer.get(INTERRUPT_KEY, INTERRUPT_HEAD),
        format!("ok content={INTERRUPT_HEX} head={INTERRUPT_KEY}")
    );
    writer.close();
}

#[test]
fn existing_only_inspection_of_a_missing_database_creates_nothing() {
    let dir = TestDir::new("inspect-missing");
    let db = dir.database(Mode::Multiprocess);
    assert_eq!(inspect_existing(&db, NO_BUSY_TIMEOUT), "missing");
    assert!(dir.entries().is_empty(), "{:?}", dir.entries());
    assert!(
        db.path()
            .parent()
            .is_some_and(|parent| parent == dir.path())
    );
}

#[test]
fn existing_only_inspection_of_an_uninitialized_database_reports_it() {
    let dir = TestDir::new("inspect-uninitialized");
    let db = dir.database(Mode::Multiprocess);
    assert_eq!(
        db.run_ok("settings", NO_BUSY_TIMEOUT),
        ["journal_mode=wal synchronous=2"]
    );
    assert_eq!(
        inspect_existing(&db, NO_BUSY_TIMEOUT),
        "schema-not-current readiness=uninitialized"
    );
}
