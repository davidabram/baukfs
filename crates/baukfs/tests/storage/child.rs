use std::env;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use baukfs::WorkspaceId;
use baukfs::storage::{BusyTimeout, DatabaseState, StorageError, WorkspaceStore, inspect};

use crate::harness::{ChildProcess, PROTOCOL_PREFIX, ok};
use crate::support::{block_on, payload, raw_connection};

const ROLE: &str = "BAUKFS_STORAGE_CHILD_ROLE";
const DATABASE: &str = "BAUKFS_STORAGE_CHILD_DATABASE";
const WORKSPACE: &str = "BAUKFS_STORAGE_CHILD_WORKSPACE";
const BUSY_TIMEOUT_MS: &str = "BAUKFS_STORAGE_CHILD_BUSY_TIMEOUT_MS";
const HOLD_SQL: &str = "BAUKFS_STORAGE_CHILD_HOLD_SQL";

/// Every child, including its startup, must finish within this budget.
pub const CHILD_DEADLINE: Duration = Duration::from_mins(1);

/// How many payloads each `insert` child stores, in the same order.
pub const SHARED_PAYLOADS: u64 = 16;

/// The payloads every `insert` child stores. Seed 0 is empty.
pub fn shared_payload(seed: u64) -> Vec<u8> {
    let len = usize::try_from(seed * 4099 % 70_000).expect("small length");
    payload(seed, len)
}

/// Runs the requested child role when this test binary was re-executed as a
/// child; otherwise returns immediately.
pub fn run_if_requested() {
    let Ok(role) = env::var(ROLE) else {
        return;
    };
    let database = PathBuf::from(env::var_os(DATABASE).expect("child database path"));
    match role.as_str() {
        "create" => create(&database),
        "upgrade" => upgrade(&database),
        "insert" => insert(&database),
        "observe" => observe(&database),
        "hold" => hold(&database),
        other => panic!("unknown child role {other:?}"),
    }
}

fn report(line: &str) {
    println!("{PROTOCOL_PREFIX}{line}");
}

fn read_command() -> String {
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .expect("read child command");
    line.trim_end().to_owned()
}

fn workspace_id() -> WorkspaceId {
    WorkspaceId::new(env::var(WORKSPACE).expect("child workspace")).expect("valid workspace")
}

fn busy_timeout() -> BusyTimeout {
    env::var(BUSY_TIMEOUT_MS)
        .expect("child busy timeout")
        .parse()
        .map(Duration::from_millis)
        .map(BusyTimeout::new)
        .expect("busy timeout in milliseconds")
}

fn await_start() {
    report("ready");
    assert_eq!(read_command(), "go");
    report("started");
}

fn failure(error: StorageError) -> String {
    match error {
        StorageError::WorkspaceMismatch { requested, stored } => {
            format!("mismatch {} {}", requested.as_str(), stored.as_str())
        }
        StorageError::Driver(error) => format!("driver {:?} {:?}", error.step(), error.kind()),
        error => format!("failed {error}"),
    }
}

fn create(database: &Path) {
    let (workspace_id, busy_timeout) = (workspace_id(), busy_timeout());
    await_start();
    let line = match block_on(WorkspaceStore::create(
        database,
        &workspace_id,
        busy_timeout,
    )) {
        Ok((store, outcome)) => format!("created {outcome:?} {}", store.workspace_id().as_str()),
        Err(error) => failure(error),
    };
    report(&line);
}

fn upgrade(database: &Path) {
    let (workspace_id, busy_timeout) = (workspace_id(), busy_timeout());
    await_start();
    let line = match block_on(WorkspaceStore::upgrade(
        database,
        &workspace_id,
        busy_timeout,
    )) {
        Ok((store, outcome)) => format!("upgraded {outcome:?} {}", store.workspace_id().as_str()),
        Err(error) => failure(error),
    };
    report(&line);
}

/// Stores every shared payload, then reads each back.
fn insert(database: &Path) {
    let (workspace_id, busy_timeout) = (workspace_id(), busy_timeout());
    let mut store = block_on(WorkspaceStore::open_existing(
        database,
        &workspace_id,
        busy_timeout,
    ))
    .expect("open existing");
    await_start();
    let mut outcomes = Vec::new();
    for seed in 0..SHARED_PAYLOADS {
        let bytes = shared_payload(seed);
        match block_on(store.insert_content(&bytes)) {
            Ok((id, outcome)) => outcomes.push(format!("{outcome:?}:{id}")),
            Err(error) => return report(&failure(error)),
        }
    }
    for seed in 0..SHARED_PAYLOADS {
        let bytes = shared_payload(seed);
        let id = baukfs::ContentId::of(&bytes);
        match block_on(store.read_content(&id)) {
            Ok(Some(stored)) if stored == bytes => {}
            other => return report(&format!("failed read {id}: {other:?}")),
        }
    }
    report(&format!("inserted {}", outcomes.join(",")));
}

/// Inspects in a loop until the database reads as current, then reports
/// every distinct state seen, in order of first appearance.
fn observe(database: &Path) {
    let busy_timeout = busy_timeout();
    await_start();
    let mut seen: Vec<String> = Vec::new();
    loop {
        let state = match block_on(inspect(database, busy_timeout)) {
            Ok(state) => state,
            Err(error) => return report(&failure(error)),
        };
        let text = format!("{state:?}");
        if !seen.contains(&text) {
            seen.push(text);
        }
        if matches!(state, DatabaseState::Current(_)) {
            break;
        }
    }
    report(&format!("observed {}", seen.join(" | ")));
}

/// Holds `BEGIN IMMEDIATE`, plus an optional uncommitted write, until told
/// to release, then rolls back.
fn hold(database: &Path) {
    let connection = raw_connection(database);
    block_on(connection.execute("BEGIN IMMEDIATE", ())).expect("begin immediate");
    if let Ok(sql) = env::var(HOLD_SQL) {
        block_on(connection.execute(sql.as_str(), ())).expect("uncommitted write");
    }
    report("holding");
    assert_eq!(read_command(), "release");
    block_on(connection.execute("ROLLBACK", ())).expect("rollback");
    report("released");
}

fn command(role: &str, database: &Path) -> Command {
    let mut command = Command::new(env::current_exe().expect("path of the running test binary"));
    command
        .args([
            "child_process_entry",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROLE, role)
        .env(DATABASE, database);
    command
}

/// A child waiting at its start barrier.
pub struct Starter {
    process: ChildProcess,
}

impl Starter {
    pub fn create(database: &Path, workspace_id: &str, busy_timeout: Duration) -> Self {
        Self::spawn("create", database, workspace_id, busy_timeout)
    }

    pub fn upgrade(database: &Path, workspace_id: &str, busy_timeout: Duration) -> Self {
        Self::spawn("upgrade", database, workspace_id, busy_timeout)
    }

    pub fn insert(database: &Path, workspace_id: &str, busy_timeout: Duration) -> Self {
        Self::spawn("insert", database, workspace_id, busy_timeout)
    }

    pub fn observe(database: &Path, busy_timeout: Duration) -> Self {
        Self::spawn("observe", database, "-", busy_timeout)
    }

    fn spawn(role: &str, database: &Path, workspace_id: &str, busy_timeout: Duration) -> Self {
        let mut command = command(role, database);
        command
            .env(WORKSPACE, workspace_id)
            .env(BUSY_TIMEOUT_MS, busy_timeout.as_millis().to_string());
        let label = format!(
            "{role} {workspace_id} busy={}ms {}",
            busy_timeout.as_millis(),
            database.display()
        );
        let mut process = ok(ChildProcess::spawn(label, command, CHILD_DEADLINE));
        ok(process.expect_line("ready"));
        Self { process }
    }

    pub fn go(&mut self) {
        ok(self.process.send("go"));
        ok(self.process.expect_line("started"));
    }

    pub fn has_replied(&mut self) -> Option<String> {
        self.process.try_next_line()
    }

    pub fn result(mut self) -> String {
        let line = ok(self.process.next_line());
        ok(self.process.finish());
        line
    }
}

/// A child holding a write transaction on a database.
pub struct Holder {
    process: ChildProcess,
}

impl Holder {
    pub fn spawn(database: &Path, uncommitted_sql: Option<&str>) -> Self {
        let mut command = command("hold", database);
        if let Some(sql) = uncommitted_sql {
            command.env(HOLD_SQL, sql);
        }
        let label = format!("hold {uncommitted_sql:?} {}", database.display());
        let mut process = ok(ChildProcess::spawn(label, command, CHILD_DEADLINE));
        ok(process.expect_line("holding"));
        Self { process }
    }

    pub fn release(mut self) {
        ok(self.process.send("release"));
        ok(self.process.expect_line("released"));
        ok(self.process.finish());
    }
}
