use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::harness::{Finished, HarnessError, Probe};

pub const PROBE: &str = env!("CARGO_BIN_EXE_baukfs-turso-spike");
pub const PROBE_DEADLINE: Duration = Duration::from_mins(1);
pub const NO_BUSY_TIMEOUT: u64 = 0;
pub const BUSY_TIMEOUT_MS: u64 = 1000;

pub const INTERRUPT_KEY: &str = "interrupt-content";
pub const INTERRUPT_HEAD: &str = "interrupt-head";
pub const INTERRUPT_HEX: &str = "ff696e74657272757074656400fe";
pub const PRIOR_KEY: &str = "prior-content";
pub const PRIOR_HEAD: &str = "prior-head";
pub const PRIOR_HEX: &str = "00ff01fe";
pub const OTHER_KEY: &str = "other-content";
pub const OTHER_HEAD: &str = "other-head";
pub const OTHER_HEX: &str = "c328ff00";

pub fn ok<T>(result: Result<T, HarnessError>) -> T {
    result.unwrap_or_else(|error| panic!("{error}"))
}

pub struct TestDir(PathBuf);

impl TestDir {
    pub fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("turso-spike-{}-{unique}-{name}", process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn database(&self, mode: Mode) -> TestDb {
        self.database_named("probe.db", mode)
    }

    pub fn database_named(&self, name: &str, mode: Mode) -> TestDb {
        TestDb {
            path: self.0.join(name),
            mode,
        }
    }

    pub fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.0)
            .expect("list test directory")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Default,
    Multiprocess,
}

impl Mode {
    pub fn as_arg(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Multiprocess => "multiprocess",
        }
    }
}

#[derive(Clone, Debug)]
pub struct TestDb {
    path: PathBuf,
    mode: Mode,
}

impl TestDb {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn deliberately_in_other_mode(&self, mode: Mode) -> Self {
        assert_ne!(mode, self.mode, "use the database's own mode instead");
        Self {
            path: self.path.clone(),
            mode,
        }
    }

    pub fn file_bytes(&self, suffix: &str) -> Option<Vec<u8>> {
        let mut name = self.path.clone().into_os_string();
        name.push(suffix);
        fs::read(PathBuf::from(name)).ok()
    }

    fn label(&self, command: &str, busy_timeout_ms: u64) -> String {
        format!(
            "{command} --mode {} --busy-timeout-ms {busy_timeout_ms} {}",
            self.mode.as_arg(),
            self.path.display()
        )
    }

    pub fn spawn(&self, command: &str, busy_timeout_ms: u64) -> Result<Probe, HarnessError> {
        let mut process = Command::new(PROBE);
        process
            .arg(command)
            .arg("--mode")
            .arg(self.mode.as_arg())
            .arg("--busy-timeout-ms")
            .arg(busy_timeout_ms.to_string())
            .arg(&self.path);
        let mut probe = Probe::spawn(
            &self.label(command, busy_timeout_ms),
            process,
            PROBE_DEADLINE,
        )?;
        probe.expect_line(&format!(
            "config mode={} busy_timeout_ms={busy_timeout_ms}",
            self.mode.as_arg()
        ))?;
        Ok(probe)
    }

    pub fn run(&self, command: &str, busy_timeout_ms: u64) -> Finished {
        ok(ok(self.spawn(command, busy_timeout_ms)).finish())
    }

    pub fn run_ok(&self, command: &str, busy_timeout_ms: u64) -> Vec<String> {
        ok(ok(self.spawn(command, busy_timeout_ms)).finish_ok())
    }

    pub fn initialize(&self) {
        let lines = self.run_ok("initialize", BUSY_TIMEOUT_MS);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0], "precheck uninitialized", "{lines:?}");
        assert!(
            lines[1].starts_with("ok initialize outcome=applied "),
            "{lines:?}"
        );
    }

    pub fn session(&self, busy_timeout_ms: u64) -> Session {
        let mut probe = ok(self.spawn("session", busy_timeout_ms));
        ok(probe.expect_line("ready"));
        Session { probe }
    }
}

pub struct Session {
    probe: Probe,
}

#[derive(Debug, PartialEq)]
pub struct ErrReply {
    pub step: String,
    pub kind: String,
    pub elapsed_ms: f64,
    pub autocommit: String,
    pub message: String,
}

impl ErrReply {
    pub fn is_busy_family(&self) -> bool {
        self.kind == "Busy" || self.kind == "BusySnapshot"
    }
}

pub struct Timed {
    pub line: String,
    pub parent_ms: f64,
}

pub fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

pub fn elapsed_field(line: &str) -> f64 {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix("elapsed_ms="))
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("no elapsed_ms in {line:?}"))
}

pub fn parse_err(line: &str) -> ErrReply {
    let mut words = line.splitn(6, ' ');
    let tag = words.next();
    assert_eq!(tag, Some("err"), "expected an error reply, got {line:?}");
    let step = words.next().unwrap_or_default().to_owned();
    let kind = words.next().unwrap_or_default().to_owned();
    let elapsed_ms = elapsed_field(line);
    let _ = words.next();
    let autocommit = words
        .next()
        .and_then(|word| word.strip_prefix("autocommit="))
        .unwrap_or_default()
        .to_owned();
    let message = words.next().unwrap_or_default().to_owned();
    ErrReply {
        step,
        kind,
        elapsed_ms,
        autocommit,
        message,
    }
}

impl Session {
    pub fn label(&self) -> &str {
        self.probe.label()
    }

    pub fn request(&mut self, line: &str) -> String {
        ok(self.probe.request(line))
    }

    pub fn timed(&mut self, line: &str) -> Timed {
        let started = Instant::now();
        let line = self.request(line);
        Timed {
            line,
            parent_ms: millis(started.elapsed()),
        }
    }

    pub fn expect_ok(&mut self, line: &str) -> String {
        let reply = self.request(line);
        assert!(
            reply.starts_with("ok "),
            "{}: {line} -> {reply}",
            self.label()
        );
        reply
    }

    pub fn get(&mut self, key: &str, head: &str) -> String {
        self.expect_ok(&format!("get {key} {head}"))
    }

    pub fn start_write_pair(&mut self, key: &str, head: &str, hex: &str) -> Instant {
        ok(self.probe.send(&format!("write-pair {key} {head} {hex}")));
        ok(self.probe.expect_line("started"));
        Instant::now()
    }

    pub fn write_pair(&mut self, key: &str, head: &str, hex: &str) -> String {
        self.start_write_pair(key, head, hex);
        self.next_line()
    }

    pub fn next_line(&mut self) -> String {
        ok(self.probe.next_line())
    }

    pub fn has_replied(&mut self) -> Option<String> {
        self.probe.try_next_line()
    }

    pub fn assert_running(&mut self) {
        assert!(self.probe.is_running(), "{} exited early", self.label());
    }

    pub fn close(mut self) {
        ok(self.probe.send("quit"));
        ok(self.probe.expect_line("bye"));
        let finished = ok(self.probe.finish());
        assert!(
            finished.status.success() && finished.lines.is_empty(),
            "{finished:?}"
        );
    }

    pub fn kill(mut self) {
        self.assert_running();
        ok(self.probe.terminate_after_readiness());
    }
}

pub fn interrupt_pair_uncommitted(db: &TestDb, busy_timeout_ms: u64) -> Session {
    let mut writer = db.session(busy_timeout_ms);
    writer.expect_ok("begin");
    writer.expect_ok(&format!("put {INTERRUPT_KEY} {INTERRUPT_HEX}"));
    writer.expect_ok(&format!("head {INTERRUPT_HEAD} {INTERRUPT_KEY}"));
    writer
}

pub fn presence_line(content_hex: Option<&str>, head_target: Option<&str>) -> String {
    format!(
        "ok content={} head={}",
        content_hex.unwrap_or("absent"),
        head_target.unwrap_or("absent")
    )
}
