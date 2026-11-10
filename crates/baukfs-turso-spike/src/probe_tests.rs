use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicUsize, Ordering};

use turso::Value;

use crate::executor::block_on;
use crate::probe::{
    CONTENT_TABLE, HEAD_TABLE, ProbeError, Step, Visibility, begin_immediate, binary_cases, commit,
    connect, create_schema, durability_settings, interrupt_visibility, open, put_content, put_head,
    put_interrupt_pair, read_content, rollback,
};

const BOTH: Visibility = Visibility {
    content: true,
    head: true,
};
const NEITHER: Visibility = Visibility {
    content: false,
    head: false,
};

struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "baukfs-turso-spike-{}-{unique}-{name}",
            process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn database(&self) -> PathBuf {
        self.0.join("probe.db")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn visibility_after_reopen(path: &Path) -> Visibility {
    let database = open(path).await.expect("reopen");
    let connection = connect(&database).expect("connect after reopen");
    interrupt_visibility(&connection)
        .await
        .expect("read after reopen")
}

#[test]
fn binary_contents_round_trip_exactly() {
    let dir = TestDir::new("roundtrip");
    block_on(async {
        let database = open(&dir.database()).await.expect("open");
        let connection = connect(&database).expect("connect");
        create_schema(&connection).await.expect("schema");
        for (key, bytes) in &binary_cases() {
            put_content(&connection, key, bytes).await.expect("write");
        }
        for (key, expected) in &binary_cases() {
            let stored = read_content(&connection, key)
                .await
                .expect("read")
                .expect("row present");
            assert_eq!(&stored.bytes, expected, "{key}");
            assert_eq!(stored.sql_type, "blob", "{key}");
            assert_eq!(
                stored.sql_length,
                i64::try_from(expected.len()).expect("length fits"),
                "{key}"
            );
        }
    });
}

#[test]
fn empty_blob_is_not_null() {
    let dir = TestDir::new("empty-blob");
    block_on(async {
        let database = open(&dir.database()).await.expect("open");
        let connection = connect(&database).expect("connect");
        create_schema(&connection).await.expect("schema");
        put_content(&connection, "empty", &[]).await.expect("write");
        let mut rows = connection
            .query(
                "SELECT bytes, bytes IS NULL FROM probe_content WHERE probe_key = 'empty'",
                (),
            )
            .await
            .expect("query");
        let row = rows.next().await.expect("step").expect("row");
        assert_eq!(row.get_value(0).expect("bytes"), Value::Blob(Vec::new()));
        assert_eq!(row.get_value(1).expect("is null"), Value::Integer(0));
    });
}

#[test]
fn default_durability_settings() {
    let dir = TestDir::new("settings");
    block_on(async {
        let database = open(&dir.database()).await.expect("open");
        let connection = connect(&database).expect("connect");
        let settings = durability_settings(&connection).await.expect("settings");
        assert_eq!(settings.journal_mode, "wal");
        assert_eq!(settings.synchronous, 2);
    });
}

#[test]
fn commit_preserves_both_records() {
    let dir = TestDir::new("commit");
    block_on(async {
        {
            let database = open(&dir.database()).await.expect("open");
            let mut connection = connect(&database).expect("connect");
            create_schema(&connection).await.expect("schema");
            let transaction = begin_immediate(&mut connection).await.expect("begin");
            put_interrupt_pair(&transaction).await.expect("write pair");
            commit(transaction).await.expect("commit");
            assert_eq!(interrupt_visibility(&connection).await.expect("read"), BOTH);
        }
        assert_eq!(visibility_after_reopen(&dir.database()).await, BOTH);
    });
}

#[test]
fn rollback_preserves_neither_record() {
    let dir = TestDir::new("rollback");
    block_on(async {
        {
            let database = open(&dir.database()).await.expect("open");
            let mut connection = connect(&database).expect("connect");
            create_schema(&connection).await.expect("schema");
            let transaction = begin_immediate(&mut connection).await.expect("begin");
            put_interrupt_pair(&transaction).await.expect("write pair");
            assert_eq!(
                interrupt_visibility(&transaction)
                    .await
                    .expect("read inside"),
                BOTH
            );
            rollback(transaction).await.expect("rollback");
            assert_eq!(
                interrupt_visibility(&connection).await.expect("read"),
                NEITHER
            );
        }
        assert_eq!(visibility_after_reopen(&dir.database()).await, NEITHER);
    });
}

fn assert_busy(result: Result<(), ProbeError>) {
    match result {
        Err(ProbeError::Transaction {
            step: Step::Begin,
            error: turso::Error::Busy(message),
        }) => assert_eq!(message, "database is locked"),
        other => panic!("expected Busy on begin, got {other:?}"),
    }
}

#[test]
fn duplicate_key_reports_constraint() {
    let dir = TestDir::new("duplicate");
    block_on(async {
        let database = open(&dir.database()).await.expect("open");
        let connection = connect(&database).expect("connect");
        create_schema(&connection).await.expect("schema");
        put_content(&connection, "k", b"one")
            .await
            .expect("first write");
        match put_content(&connection, "k", b"two").await {
            Err(ProbeError::Write {
                table: CONTENT_TABLE,
                error: turso::Error::Constraint(message),
            }) => assert_eq!(message, "UNIQUE constraint failed: probe_content.probe_key"),
            other => panic!("expected content constraint error, got {other:?}"),
        }
        put_head(&connection, "h", "k").await.expect("first head");
        match put_head(&connection, "h", "k").await {
            Err(ProbeError::Write {
                table: HEAD_TABLE,
                error: turso::Error::Constraint(message),
            }) => assert_eq!(message, "UNIQUE constraint failed: probe_head.head_name"),
            other => panic!("expected head constraint error, got {other:?}"),
        }
        let stored = read_content(&connection, "k")
            .await
            .expect("read")
            .expect("row");
        assert_eq!(stored.bytes, b"one");
    });
}

#[test]
fn open_failures_keep_driver_variants() {
    let dir = TestDir::new("open-failure");
    block_on(async {
        let missing = dir.0.join("missing").join("probe.db");
        match open(&missing).await {
            Err(ProbeError::Open {
                path,
                error: turso::Error::IoError(ErrorKind::NotFound, "open"),
            }) => assert_eq!(path, missing),
            other => panic!("expected NotFound, got {:?}", other.map(drop)),
        }

        match open(&dir.0).await {
            Err(ProbeError::Open {
                error: turso::Error::IoError(ErrorKind::IsADirectory, "open"),
                ..
            }) => {}
            other => panic!("expected IsADirectory, got {:?}", other.map(drop)),
        }

        let garbage = dir.0.join("garbage.db");
        fs::write(&garbage, [0x5a_u8; 8192]).expect("write garbage");
        match open(&garbage).await {
            Err(ProbeError::Open {
                error: turso::Error::NotAdb(message),
                ..
            }) => assert_eq!(message, "file is not a database"),
            other => panic!("expected NotAdb, got {:?}", other.map(drop)),
        }

        let non_utf8 = dir.0.join(OsStr::from_bytes(b"\xff.db"));
        match open(&non_utf8).await {
            Err(ProbeError::NonUtf8Path(path)) => assert_eq!(path, non_utf8),
            other => panic!("expected NonUtf8Path, got {:?}", other.map(drop)),
        }
    });
}

#[test]
fn separate_connection_does_not_see_uncommitted_writes() {
    let dir = TestDir::new("isolation");
    block_on(async {
        let database = open(&dir.database()).await.expect("open");
        let mut writer = connect(&database).expect("writer");
        let reader = connect(&database).expect("reader");
        create_schema(&writer).await.expect("schema");
        let transaction = begin_immediate(&mut writer).await.expect("begin");
        put_interrupt_pair(&transaction).await.expect("write pair");
        assert_eq!(
            interrupt_visibility(&reader)
                .await
                .expect("read during write"),
            NEITHER
        );
        let mut second_writer = connect(&database).expect("second writer");
        assert_busy(begin_immediate(&mut second_writer).await.map(drop));
        commit(transaction).await.expect("commit");
        assert_eq!(
            interrupt_visibility(&reader)
                .await
                .expect("read after commit"),
            BOTH
        );
    });
}

#[test]
fn second_database_handle_in_same_process_shares_committed_state() {
    let dir = TestDir::new("second-handle");
    block_on(async {
        let first = open(&dir.database()).await.expect("first open");
        let mut first_connection = connect(&first).expect("connect first");
        create_schema(&first_connection).await.expect("schema");
        let second = open(&dir.database()).await.expect("second open");
        let second_connection = connect(&second).expect("connect second");
        assert_eq!(
            interrupt_visibility(&second_connection)
                .await
                .expect("read"),
            NEITHER
        );
        let transaction = begin_immediate(&mut first_connection).await.expect("begin");
        put_interrupt_pair(&transaction).await.expect("write pair");
        assert_eq!(
            interrupt_visibility(&second_connection)
                .await
                .expect("read uncommitted"),
            NEITHER
        );
        commit(transaction).await.expect("commit");
        assert_eq!(
            interrupt_visibility(&second_connection)
                .await
                .expect("read committed"),
            BOTH
        );
    });
}

#[test]
fn dropped_transaction_stays_open_until_next_use() {
    let dir = TestDir::new("dropped");
    block_on(async {
        let database = open(&dir.database()).await.expect("open");
        let mut owner = connect(&database).expect("owner");
        let mut other = connect(&database).expect("other");
        create_schema(&owner).await.expect("schema");
        {
            let transaction = begin_immediate(&mut owner).await.expect("begin");
            put_interrupt_pair(&transaction).await.expect("write pair");
        }
        assert!(!owner.is_autocommit().expect("autocommit after drop"));
        assert_busy(begin_immediate(&mut other).await.map(drop));
        assert_eq!(
            interrupt_visibility(&owner).await.expect("owner read"),
            NEITHER
        );
        assert!(owner.is_autocommit().expect("autocommit after use"));
        let transaction = begin_immediate(&mut other)
            .await
            .expect("begin after rollback");
        rollback(transaction).await.expect("rollback");
    });
}
