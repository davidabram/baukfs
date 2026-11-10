use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(2);
const DIAGNOSTIC_GRACE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Line,
    StdoutEof,
    StderrEof,
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
    Reader {
        label: String,
        error: io::Error,
    },
    Deadline {
        label: String,
        stage: Stage,
        budget: Duration,
        stdout: Vec<String>,
        stderr: String,
    },
    ExitedBeforeLine {
        label: String,
        status: ExitStatus,
        stderr: String,
    },
    UnexpectedLine {
        label: String,
        expected: String,
        got: String,
        stderr: String,
    },
    OutputAfterReadiness {
        label: String,
        lines: Vec<String>,
        stderr: String,
    },
    NotKilled {
        label: String,
        status: ExitStatus,
        stderr: String,
    },
    Failed {
        label: String,
        status: ExitStatus,
        stdout: Vec<String>,
        stderr: String,
    },
}

impl fmt::Display for HarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { label, error } => write!(f, "{label}: spawn failed: {error}"),
            Self::Stdin { label, error } => write!(f, "{label}: writing stdin failed: {error}"),
            Self::Reader { label, error } => write!(f, "{label}: reading output failed: {error}"),
            Self::Deadline {
                label,
                stage,
                budget,
                stdout,
                stderr,
            } => write!(
                f,
                "{label}: deadline of {budget:?} passed waiting for {stage:?}; \
                 stdout so far: {stdout:?}; stderr:\n{stderr}"
            ),
            Self::ExitedBeforeLine {
                label,
                status,
                stderr,
            } => write!(
                f,
                "{label}: stdout closed before the expected line; {status}; stderr:\n{stderr}"
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
            Self::OutputAfterReadiness {
                label,
                lines,
                stderr,
            } => write!(
                f,
                "{label}: output followed readiness: {lines:?}; stderr:\n{stderr}"
            ),
            Self::NotKilled {
                label,
                status,
                stderr,
            } => write!(
                f,
                "{label}: expected termination by SIGKILL, got {status}; stderr:\n{stderr}"
            ),
            Self::Failed {
                label,
                status,
                stdout,
                stderr,
            } => write!(
                f,
                "{label}: {status}; stdout: {stdout:?}; stderr:\n{stderr}"
            ),
        }
    }
}

enum StdoutEvent {
    Line(String),
    Eof,
    Failed(io::Error),
}

#[derive(Debug)]
pub struct Finished {
    pub status: ExitStatus,
    pub lines: Vec<String>,
    pub stderr: String,
}

pub struct Probe {
    label: String,
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Receiver<StdoutEvent>,
    stderr: Receiver<io::Result<Vec<u8>>>,
    stderr_text: Option<String>,
    stdout_closed: bool,
    status: Option<ExitStatus>,
    budget: Duration,
    deadline: Instant,
}

impl Probe {
    pub fn spawn(
        label: &str,
        mut command: Command,
        budget: Duration,
    ) -> Result<Self, HarnessError> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| HarnessError::Spawn {
                label: label.to_owned(),
                error,
            })?;
        let deadline = Instant::now() + budget;
        let stdin = child.stdin.take();
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let (stdout_sender, stdout) = mpsc::channel();
        let (stderr_sender, stderr) = mpsc::channel();
        if let Some(pipe) = stdout_pipe {
            thread::spawn(move || {
                let mut reader = BufReader::new(pipe);
                let mut buffer = Vec::new();
                loop {
                    buffer.clear();
                    let event = match reader.read_until(b'\n', &mut buffer) {
                        Ok(0) => StdoutEvent::Eof,
                        Ok(_) => {
                            if buffer.last() == Some(&b'\n') {
                                buffer.pop();
                            }
                            StdoutEvent::Line(String::from_utf8_lossy(&buffer).into_owned())
                        }
                        Err(error) => StdoutEvent::Failed(error),
                    };
                    let last = !matches!(event, StdoutEvent::Line(_));
                    if stdout_sender.send(event).is_err() || last {
                        break;
                    }
                }
            });
        }
        if let Some(mut pipe) = stderr_pipe {
            thread::spawn(move || {
                let mut collected = Vec::new();
                let read = pipe.read_to_end(&mut collected).map(|_| collected);
                let _ = stderr_sender.send(read);
            });
        }
        Ok(Self {
            label: label.to_owned(),
            child,
            stdin,
            stdout,
            stderr,
            stderr_text: None,
            stdout_closed: false,
            status: None,
            budget,
            deadline,
        })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    pub fn is_running(&mut self) -> bool {
        if self.status.is_some() {
            return false;
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.status = Some(status);
                false
            }
            Ok(None) => true,
            Err(_) => false,
        }
    }

    fn kill_and_reap(&mut self) -> Option<ExitStatus> {
        if self.status.is_none() {
            let _ = self.child.kill();
            let grace_deadline = Instant::now() + DIAGNOSTIC_GRACE;
            while Instant::now() < grace_deadline {
                match self.child.try_wait() {
                    Ok(Some(status)) => {
                        self.status = Some(status);
                        break;
                    }
                    Ok(None) => thread::sleep(EXIT_POLL_INTERVAL),
                    Err(_) => break,
                }
            }
        }
        self.status
    }

    fn stderr_for_diagnostics(&mut self) -> String {
        if let Some(text) = &self.stderr_text {
            return text.clone();
        }
        match self.stderr.recv_timeout(DIAGNOSTIC_GRACE) {
            Ok(Ok(bytes)) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                self.stderr_text = Some(text.clone());
                text
            }
            Ok(Err(error)) => format!("<stderr read failed: {error}>"),
            Err(_) => "<stderr unavailable>".to_owned(),
        }
    }

    fn drain_stdout_for_diagnostics(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.stdout_closed {
            return lines;
        }
        while let Ok(event) = self.stdout.recv_timeout(DIAGNOSTIC_GRACE) {
            match event {
                StdoutEvent::Line(line) => lines.push(line),
                StdoutEvent::Eof | StdoutEvent::Failed(_) => {
                    self.stdout_closed = true;
                    break;
                }
            }
        }
        lines
    }

    fn abort(&mut self, stage: Stage, mut stdout: Vec<String>) -> HarnessError {
        self.kill_and_reap();
        stdout.extend(self.drain_stdout_for_diagnostics());
        HarnessError::Deadline {
            label: self.label.clone(),
            stage,
            budget: self.budget,
            stdout,
            stderr: self.stderr_for_diagnostics(),
        }
    }

    fn reader_failure(&mut self, error: io::Error) -> HarnessError {
        self.kill_and_reap();
        HarnessError::Reader {
            label: self.label.clone(),
            error,
        }
    }

    pub fn next_line(&mut self) -> Result<String, HarnessError> {
        if self.stdout_closed {
            return Err(self.exited_before_line());
        }
        match self.stdout.recv_timeout(self.remaining()) {
            Ok(StdoutEvent::Line(line)) => Ok(line),
            Ok(StdoutEvent::Eof) => {
                self.stdout_closed = true;
                Err(self.exited_before_line())
            }
            Ok(StdoutEvent::Failed(error)) => {
                self.stdout_closed = true;
                Err(self.reader_failure(error))
            }
            Err(RecvTimeoutError::Timeout) => Err(self.abort(Stage::Line, Vec::new())),
            Err(RecvTimeoutError::Disconnected) => {
                self.stdout_closed = true;
                Err(self.reader_failure(io::Error::other("stdout reader stopped")))
            }
        }
    }

    fn exited_before_line(&mut self) -> HarnessError {
        match self.wait_exit() {
            Ok(status) => HarnessError::ExitedBeforeLine {
                label: self.label.clone(),
                status,
                stderr: self.stderr_for_diagnostics(),
            },
            Err(error) => error,
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
            stderr: self.stderr_for_diagnostics(),
        })
    }

    pub fn send(&mut self, line: &str) -> Result<(), HarnessError> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(HarnessError::Stdin {
                label: self.label.clone(),
                error: io::Error::new(io::ErrorKind::BrokenPipe, "stdin already closed"),
            });
        };
        let written = writeln!(stdin, "{line}").and_then(|()| stdin.flush());
        written.map_err(|error| HarnessError::Stdin {
            label: self.label.clone(),
            error,
        })
    }

    pub fn request(&mut self, line: &str) -> Result<String, HarnessError> {
        self.send(line)?;
        self.next_line()
    }

    pub fn try_next_line(&mut self) -> Option<String> {
        match self.stdout.try_recv() {
            Ok(StdoutEvent::Line(line)) => Some(line),
            Ok(StdoutEvent::Eof | StdoutEvent::Failed(_)) => {
                self.stdout_closed = true;
                None
            }
            Err(_) => None,
        }
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
                        return Err(self.abort(Stage::Exit, Vec::new()));
                    }
                    thread::sleep(remaining.min(EXIT_POLL_INTERVAL));
                }
                Err(error) => return Err(self.reader_failure(error)),
            }
        }
    }

    fn stdout_until_eof(&mut self) -> Result<Vec<String>, HarnessError> {
        let mut lines = Vec::new();
        while !self.stdout_closed {
            match self.stdout.recv_timeout(self.remaining()) {
                Ok(StdoutEvent::Line(line)) => lines.push(line),
                Ok(StdoutEvent::Eof) | Err(RecvTimeoutError::Disconnected) => {
                    self.stdout_closed = true;
                }
                Ok(StdoutEvent::Failed(error)) => {
                    self.stdout_closed = true;
                    return Err(self.reader_failure(error));
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(self.abort(Stage::StdoutEof, lines));
                }
            }
        }
        Ok(lines)
    }

    fn stderr_until_eof(&mut self) -> Result<String, HarnessError> {
        if let Some(text) = &self.stderr_text {
            return Ok(text.clone());
        }
        match self.stderr.recv_timeout(self.remaining()) {
            Ok(Ok(bytes)) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                self.stderr_text = Some(text.clone());
                Ok(text)
            }
            Ok(Err(error)) => Err(self.reader_failure(error)),
            Err(RecvTimeoutError::Timeout) => Err(self.abort(Stage::StderrEof, Vec::new())),
            Err(RecvTimeoutError::Disconnected) => Ok(String::new()),
        }
    }

    pub fn finish(mut self) -> Result<Finished, HarnessError> {
        let lines = self.stdout_until_eof()?;
        let status = self.wait_exit()?;
        let stderr = self.stderr_until_eof()?;
        Ok(Finished {
            status,
            lines,
            stderr,
        })
    }

    pub fn finish_ok(self) -> Result<Vec<String>, HarnessError> {
        let label = self.label.clone();
        let finished = self.finish()?;
        if finished.status.success() {
            Ok(finished.lines)
        } else {
            Err(HarnessError::Failed {
                label,
                status: finished.status,
                stdout: finished.lines,
                stderr: finished.stderr,
            })
        }
    }

    pub fn terminate(mut self) -> Result<Finished, HarnessError> {
        let _ = self.child.kill();
        let status = self.wait_exit()?;
        let lines = self.stdout_until_eof()?;
        let stderr = self.stderr_until_eof()?;
        Ok(Finished {
            status,
            lines,
            stderr,
        })
    }

    pub fn terminate_after_readiness(self) -> Result<(), HarnessError> {
        use std::os::unix::process::ExitStatusExt;
        const SIGKILL: i32 = 9;
        let label = self.label.clone();
        let finished = self.terminate()?;
        if !finished.lines.is_empty() {
            return Err(HarnessError::OutputAfterReadiness {
                label,
                lines: finished.lines,
                stderr: finished.stderr,
            });
        }
        if finished.status.signal() != Some(SIGKILL) {
            return Err(HarnessError::NotKilled {
                label,
                status: finished.status,
                stderr: finished.stderr,
            });
        }
        Ok(())
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}
