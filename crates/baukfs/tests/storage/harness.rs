use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

/// Child protocol messages follow this marker so the test harness's own
/// output in the re-executed test binary is ignored. libtest prints the test
/// name without a newline, so the marker can appear mid-line.
pub const PROTOCOL_PREFIX: &str = "storage-child> ";

const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(2);
const DIAGNOSTIC_GRACE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Line,
    Exit,
}

#[derive(Debug)]
pub enum HarnessError {
    Spawn {
        label: String,
        error: io::Error,
    },
    Stdin {
        label: String,
        error: io::Error,
    },
    Deadline {
        label: String,
        stage: Stage,
        budget: Duration,
        stderr: String,
    },
    ExitedBeforeLine {
        label: String,
        status: Option<ExitStatus>,
        stderr: String,
    },
    UnexpectedLine {
        label: String,
        expected: String,
        got: String,
        stderr: String,
    },
    Failed {
        label: String,
        status: ExitStatus,
        stderr: String,
    },
}

impl fmt::Display for HarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { label, error } => write!(f, "{label}: spawn failed: {error}"),
            Self::Stdin { label, error } => write!(f, "{label}: writing stdin failed: {error}"),
            Self::Deadline {
                label,
                stage,
                budget,
                stderr,
            } => write!(
                f,
                "{label}: deadline of {budget:?} passed waiting for {stage:?}; stderr:\n{stderr}"
            ),
            Self::ExitedBeforeLine {
                label,
                status,
                stderr,
            } => write!(
                f,
                "{label}: stdout closed before the expected line; status {status:?}; stderr:\n{stderr}"
            ),
            Self::UnexpectedLine {
                label,
                expected,
                got,
                stderr,
            } => write!(
                f,
                "{label}: expected line {expected:?}, got {got:?}; stderr:\n{stderr}"
            ),
            Self::Failed {
                label,
                status,
                stderr,
            } => write!(f, "{label}: {status}; stderr:\n{stderr}"),
        }
    }
}

pub fn ok<T>(result: Result<T, HarnessError>) -> T {
    result.unwrap_or_else(|error| panic!("{error}"))
}

enum StdoutEvent {
    Line(String),
    Closed,
}

/// A child process bounded by one deadline covering every line, stdout EOF,
/// and exit. Dropping it kills and reaps the child.
pub struct ChildProcess {
    label: String,
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Receiver<StdoutEvent>,
    stderr: Receiver<Vec<u8>>,
    stderr_text: Option<String>,
    status: Option<ExitStatus>,
    budget: Duration,
    deadline: Instant,
}

impl ChildProcess {
    pub fn spawn(
        label: String,
        mut command: Command,
        budget: Duration,
    ) -> Result<Self, HarnessError> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| HarnessError::Spawn {
                label: label.clone(),
                error,
            })?;
        let deadline = Instant::now() + budget;
        let stdin = child.stdin.take();
        let (stdout_sender, stdout) = mpsc::channel();
        let (stderr_sender, stderr) = mpsc::channel();
        if let Some(pipe) = child.stdout.take() {
            thread::spawn(move || {
                for line in BufReader::new(pipe).lines() {
                    let Ok(line) = line else { break };
                    if let Some((_, message)) = line.split_once(PROTOCOL_PREFIX)
                        && stdout_sender
                            .send(StdoutEvent::Line(message.to_owned()))
                            .is_err()
                    {
                        return;
                    }
                }
                let _ = stdout_sender.send(StdoutEvent::Closed);
            });
        }
        if let Some(mut pipe) = child.stderr.take() {
            thread::spawn(move || {
                let mut collected = Vec::new();
                let _ = pipe.read_to_end(&mut collected);
                let _ = stderr_sender.send(collected);
            });
        }
        Ok(Self {
            label,
            child,
            stdin,
            stdout,
            stderr,
            stderr_text: None,
            status: None,
            budget,
            deadline,
        })
    }

    fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    fn kill_and_reap(&mut self) {
        if self.status.is_some() {
            return;
        }
        let _ = self.child.kill();
        let grace = Instant::now() + DIAGNOSTIC_GRACE;
        while Instant::now() < grace {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.status = Some(status);
                    return;
                }
                Ok(None) => thread::sleep(EXIT_POLL_INTERVAL),
                Err(_) => return,
            }
        }
    }

    fn stderr(&mut self, wait: Duration) -> String {
        if let Some(text) = &self.stderr_text {
            return text.clone();
        }
        match self.stderr.recv_timeout(wait) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                self.stderr_text = Some(text.clone());
                text
            }
            Err(_) => "<stderr unavailable>".to_owned(),
        }
    }

    fn deadline_passed(&mut self, stage: Stage) -> HarnessError {
        self.kill_and_reap();
        HarnessError::Deadline {
            label: self.label.clone(),
            stage,
            budget: self.budget,
            stderr: self.stderr(DIAGNOSTIC_GRACE),
        }
    }

    pub fn next_line(&mut self) -> Result<String, HarnessError> {
        match self.stdout.recv_timeout(self.remaining()) {
            Ok(StdoutEvent::Line(line)) => Ok(line),
            Ok(StdoutEvent::Closed) | Err(RecvTimeoutError::Disconnected) => {
                let status = self.wait_exit().ok();
                Err(HarnessError::ExitedBeforeLine {
                    label: self.label.clone(),
                    status,
                    stderr: self.stderr(DIAGNOSTIC_GRACE),
                })
            }
            Err(RecvTimeoutError::Timeout) => Err(self.deadline_passed(Stage::Line)),
        }
    }

    /// Returns a line only if one has already arrived.
    pub fn try_next_line(&mut self) -> Option<String> {
        match self.stdout.try_recv() {
            Ok(StdoutEvent::Line(line)) => Some(line),
            Ok(StdoutEvent::Closed) | Err(_) => None,
        }
    }

    pub fn expect_line(&mut self, expected: &str) -> Result<(), HarnessError> {
        let got = self.next_line()?;
        if got == expected {
            return Ok(());
        }
        self.kill_and_reap();
        Err(HarnessError::UnexpectedLine {
            label: self.label.clone(),
            expected: expected.to_owned(),
            got,
            stderr: self.stderr(DIAGNOSTIC_GRACE),
        })
    }

    pub fn send(&mut self, line: &str) -> Result<(), HarnessError> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(HarnessError::Stdin {
                label: self.label.clone(),
                error: io::Error::new(io::ErrorKind::BrokenPipe, "stdin already closed"),
            });
        };
        writeln!(stdin, "{line}")
            .and_then(|()| stdin.flush())
            .map_err(|error| HarnessError::Stdin {
                label: self.label.clone(),
                error,
            })
    }

    fn wait_exit(&mut self) -> Result<ExitStatus, HarnessError> {
        loop {
            if let Some(status) = self.status {
                return Ok(status);
            }
            match self.child.try_wait() {
                Ok(Some(status)) => self.status = Some(status),
                Ok(None) => {
                    let remaining = self.remaining();
                    if remaining.is_zero() {
                        return Err(self.deadline_passed(Stage::Exit));
                    }
                    thread::sleep(remaining.min(EXIT_POLL_INTERVAL));
                }
                Err(_) => return Err(self.deadline_passed(Stage::Exit)),
            }
        }
    }

    /// Closes stdin and waits for a successful exit within the deadline.
    pub fn finish(mut self) -> Result<(), HarnessError> {
        drop(self.stdin.take());
        let status = self.wait_exit()?;
        if status.success() {
            return Ok(());
        }
        let wait = self.remaining();
        Err(HarnessError::Failed {
            label: self.label.clone(),
            status,
            stderr: self.stderr(wait),
        })
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}
