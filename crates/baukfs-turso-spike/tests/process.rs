use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{self, Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

const PROBE: &str = env!("CARGO_BIN_EXE_baukfs-turso-spike");
const CHILD_DEADLINE: Duration = Duration::from_mins(1);
const SIGKILL: i32 = 9;

struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("turso-spike-{}-{unique}-{name}", process::id()));
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

struct ChildGuard {
    child: Child,
    label: String,
}

impl ChildGuard {
    fn spawn(command: &str, database: &Path, stdin: Stdio) -> Self {
        let child = Command::new(PROBE)
            .arg(command)
            .arg(database)
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn probe");
        Self {
            child,
            label: format!("{command} {}", database.display()),
        }
    }

    fn take_stdout(&mut self) -> ChildStdout {
        self.child.stdout.take().expect("stdout piped")
    }

    fn stderr(&mut self) -> String {
        let mut text = String::new();
        if let Some(mut stderr) = self.child.stderr.take() {
            let _ = stderr.read_to_string(&mut text);
        }
        text
    }

    fn kill_and_reap(&mut self) -> ExitStatus {
        let _ = self.child.kill();
        self.child.wait().expect("reap probe")
    }

    fn fail(&mut self, reason: &str) -> ! {
        let status = self.kill_and_reap();
        let stderr = self.stderr();
        panic!(
            "{}: {reason}; status {status}; stderr:\n{stderr}",
            self.label
        );
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

struct Finished {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

fn run_probe(command: &str, database: &Path) -> Finished {
    let mut guard = ChildGuard::spawn(command, database, Stdio::null());
    let mut stdout = guard.take_stdout();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut text = String::new();
        let read = stdout.read_to_string(&mut text);
        let _ = sender.send(read.map(|_| text));
    });
    let stdout = match receiver.recv_timeout(CHILD_DEADLINE) {
        Ok(Ok(text)) => text,
        Ok(Err(error)) => guard.fail(&format!("reading stdout failed: {error}")),
        Err(RecvTimeoutError::Timeout) => guard.fail("did not finish before the deadline"),
        Err(RecvTimeoutError::Disconnected) => guard.fail("stdout reader vanished"),
    };
    let status = guard.child.wait().expect("reap probe");
    let stderr = guard.stderr();
    Finished {
        status,
        stdout,
        stderr,
    }
}

fn run_probe_ok(command: &str, database: &Path) -> String {
    let finished = run_probe(command, database);
    assert!(
        finished.status.success(),
        "{command}: status {}; stdout:\n{}\nstderr:\n{}",
        finished.status,
        finished.stdout,
        finished.stderr
    );
    finished.stdout
}

struct HeldProbe {
    guard: ChildGuard,
    lines: Receiver<String>,
}

impl HeldProbe {
    fn start(command: &str, database: &Path, ready: &str) -> Self {
        let mut guard = ChildGuard::spawn(command, database, Stdio::piped());
        let stdout = guard.take_stdout();
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let mut held = Self { guard, lines };
        match held.lines.recv_timeout(CHILD_DEADLINE) {
            Ok(line) if line == ready => held,
            Ok(line) => held
                .guard
                .fail(&format!("expected '{ready}', got '{line}'")),
            Err(RecvTimeoutError::Timeout) => {
                held.guard.fail("no readiness report before the deadline")
            }
            Err(RecvTimeoutError::Disconnected) => {
                held.guard.fail("exited before reporting readiness")
            }
        }
    }

    fn assert_still_running(&mut self) {
        if let Ok(Some(status)) = self.guard.child.try_wait() {
            let stderr = self.guard.stderr();
            panic!(
                "{} exited early with {status}; stderr:\n{stderr}",
                self.guard.label
            );
        }
    }

    fn terminate(mut self) {
        self.assert_still_running();
        let status = self.guard.kill_and_reap();
        assert_eq!(
            status.signal(),
            Some(SIGKILL),
            "{}: {status}",
            self.guard.label
        );
        assert!(self.lines.try_recv().is_err(), "no output after readiness");
    }
}

fn assert_second_process_cannot_open(database: &Path) {
    let concurrent = run_probe("inspect", database);
    assert_eq!(concurrent.status.code(), Some(1), "{}", concurrent.stderr);
    assert_eq!(concurrent.stdout, "");
    assert!(
        concurrent
            .stderr
            .contains("error: Error(\"Locking error: Failed locking file")
            && concurrent
                .stderr
                .contains("File is locked by another process"),
        "unexpected concurrent open failure:\n{}",
        concurrent.stderr
    );
}

#[test]
fn binary_contents_survive_a_fresh_process() {
    let dir = TestDir::new("roundtrip");
    assert_eq!(
        run_probe_ok("roundtrip-write", &dir.database()),
        "wrote 6 cases\n"
    );
    assert_eq!(
        run_probe_ok("roundtrip-verify", &dir.database()),
        "verified 6 cases\n"
    );
}

#[test]
fn committed_transaction_is_visible_to_a_fresh_process() {
    let dir = TestDir::new("commit");
    assert_eq!(
        run_probe_ok("transaction-commit", &dir.database()),
        "content=present head=present\n"
    );
    assert_eq!(
        run_probe_ok("inspect", &dir.database()),
        "content=present head=present\n"
    );
}

#[test]
fn rolled_back_transaction_is_invisible_to_a_fresh_process() {
    let dir = TestDir::new("rollback");
    assert_eq!(
        run_probe_ok("transaction-rollback", &dir.database()),
        "content=absent head=absent\n"
    );
    assert_eq!(
        run_probe_ok("inspect", &dir.database()),
        "content=absent head=absent\n"
    );
}

#[test]
fn durability_settings_in_a_fresh_process() {
    let dir = TestDir::new("settings");
    assert_eq!(
        run_probe_ok("settings", &dir.database()),
        "journal_mode=wal synchronous=2\n"
    );
}

#[test]
fn termination_before_commit_exposes_neither_record() {
    let dir = TestDir::new("kill-before-commit");
    let held = HeldProbe::start("hold-uncommitted", &dir.database(), "ready: uncommitted");
    assert_second_process_cannot_open(&dir.database());
    held.terminate();
    assert_eq!(
        run_probe_ok("inspect", &dir.database()),
        "content=absent head=absent\n"
    );
}

#[test]
fn termination_after_commit_exposes_both_records() {
    let dir = TestDir::new("kill-after-commit");
    let held = HeldProbe::start("hold-committed", &dir.database(), "ready: committed");
    assert_second_process_cannot_open(&dir.database());
    held.terminate();
    assert_eq!(
        run_probe_ok("inspect", &dir.database()),
        "content=present head=present\n"
    );
}
