use std::time::Instant;

use crate::support::{
    Mode, NO_BUSY_TIMEOUT, TestDb, TestDir, elapsed_field, millis, presence_line,
};

const SHARED_KEY: &str = "shared-content";
const SHARED_HEAD: &str = "shared-head";
const COPY_A_HEX: &str = "aa00ff";
const COPY_B_HEX: &str = "bb01fe";
const FAST_LIMIT_MS: f64 = 500.0;

fn copies(name: &str) -> (TestDir, TestDb, TestDir, TestDb) {
    let dir_a = TestDir::new(&format!("{name}-a"));
    let dir_b = TestDir::new(&format!("{name}-b"));
    let db_a = dir_a.database(Mode::Multiprocess);
    let db_b = dir_b.database(Mode::Multiprocess);
    db_a.initialize();
    db_b.initialize();
    (dir_a, db_a, dir_b, db_b)
}

fn shared(hex: Option<&str>) -> String {
    presence_line(hex, hex.map(|_| SHARED_KEY))
}

#[test]
fn writers_on_two_copies_proceed_concurrently_with_identical_keys() {
    let (_dir_a, db_a, _dir_b, db_b) = copies("independent-concurrent");
    let mut writer_a = db_a.session(NO_BUSY_TIMEOUT);
    let mut writer_b = db_b.session(NO_BUSY_TIMEOUT);
    writer_a.expect_ok("begin");
    writer_a.expect_ok(&format!("put {SHARED_KEY} {COPY_A_HEX}"));
    writer_a.expect_ok(&format!("head {SHARED_HEAD} {SHARED_KEY}"));

    let begin_b = writer_b.expect_ok("begin");
    assert!(elapsed_field(&begin_b) < FAST_LIMIT_MS, "{begin_b}");
    writer_b.expect_ok(&format!("put {SHARED_KEY} {COPY_B_HEX}"));
    writer_b.expect_ok(&format!("head {SHARED_HEAD} {SHARED_KEY}"));

    writer_b.expect_ok("commit");
    writer_a.expect_ok("commit");

    let mut reader_a = db_a.session(NO_BUSY_TIMEOUT);
    let mut reader_b = db_b.session(NO_BUSY_TIMEOUT);
    assert_eq!(
        reader_a.get(SHARED_KEY, SHARED_HEAD),
        shared(Some(COPY_A_HEX))
    );
    assert_eq!(
        reader_b.get(SHARED_KEY, SHARED_HEAD),
        shared(Some(COPY_B_HEX))
    );
    for session in [writer_a, writer_b, reader_a, reader_b] {
        session.close();
    }
}

#[test]
fn held_write_on_one_copy_does_not_block_writes_to_the_other() {
    let (_dir_a, db_a, _dir_b, db_b) = copies("independent-unblocked");
    let mut holder_a = db_a.session(NO_BUSY_TIMEOUT);
    holder_a.expect_ok("begin");
    holder_a.expect_ok(&format!("put {SHARED_KEY} {COPY_A_HEX}"));

    let mut writer_b = db_b.session(NO_BUSY_TIMEOUT);
    let started = Instant::now();
    let reply = writer_b.write_pair(SHARED_KEY, SHARED_HEAD, COPY_B_HEX);
    assert!(reply.starts_with("ok write-pair "), "{reply}");
    assert!(elapsed_field(&reply) < FAST_LIMIT_MS, "{reply}");
    println!(
        "timing independent copy write while the other copy holds a write: {:.1} ms in child, {:.1} ms round trip",
        elapsed_field(&reply),
        millis(started.elapsed())
    );
    holder_a.expect_ok("rollback");
    holder_a.close();
    writer_b.close();
}

#[test]
fn rollback_and_termination_on_one_copy_leave_the_other_intact() {
    let (_dir_a, db_a, _dir_b, db_b) = copies("independent-isolation");
    let mut writer_b = db_b.session(NO_BUSY_TIMEOUT);
    let reply = writer_b.write_pair(SHARED_KEY, SHARED_HEAD, COPY_B_HEX);
    assert!(reply.starts_with("ok write-pair "), "{reply}");
    writer_b.close();

    let mut rolled_back = db_a.session(NO_BUSY_TIMEOUT);
    rolled_back.expect_ok("begin");
    rolled_back.expect_ok(&format!("put {SHARED_KEY} {COPY_A_HEX}"));
    rolled_back.expect_ok(&format!("head {SHARED_HEAD} {SHARED_KEY}"));
    rolled_back.expect_ok("rollback");
    rolled_back.close();

    let mut killed = db_a.session(NO_BUSY_TIMEOUT);
    killed.expect_ok("begin");
    killed.expect_ok(&format!("put {SHARED_KEY} {COPY_A_HEX}"));
    killed.expect_ok(&format!("head {SHARED_HEAD} {SHARED_KEY}"));
    killed.kill();

    let mut reader_a = db_a.session(NO_BUSY_TIMEOUT);
    let mut reader_b = db_b.session(NO_BUSY_TIMEOUT);
    assert_eq!(reader_a.get(SHARED_KEY, SHARED_HEAD), shared(None));
    assert_eq!(
        reader_b.get(SHARED_KEY, SHARED_HEAD),
        shared(Some(COPY_B_HEX))
    );
    reader_a.close();
    reader_b.close();
}
