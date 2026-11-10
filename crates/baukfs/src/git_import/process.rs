use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::ImportError;

const STDOUT_LIMIT: u64 = 256 * 1024 * 1024;
const STDERR_LIMIT: u64 = 64 * 1024;

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn collect(reader: impl Read, limit: u64) -> Result<Vec<u8>, ImportError> {
    let mut bytes = Vec::new();
    reader
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(ImportError::Io)?;
    if bytes.len() as u64 > limit {
        return Err(ImportError::OutputLimit);
    }
    Ok(bytes)
}

pub(super) fn run(repository: &Path, args: &[&str]) -> Result<Vec<u8>, ImportError> {
    let mut command = Command::new("git");
    command.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("LC_ALL", "C")
        .arg("--no-pager")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "protocol.allow=never",
            "-c",
            "maintenance.auto=false",
            "-c",
            "gc.auto=0",
        ])
        .arg("-C")
        .arg(repository)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ChildGuard(command.spawn().map_err(ImportError::Io)?);
    let stdout = child
        .0
        .stdout
        .take()
        .ok_or(ImportError::InvalidObjectData)?;
    let stderr = child
        .0
        .stderr
        .take()
        .ok_or(ImportError::InvalidObjectData)?;
    let out = thread::Builder::new()
        .spawn(move || collect(stdout, STDOUT_LIMIT))
        .map_err(ImportError::Io)?;
    let err = thread::Builder::new()
        .spawn(move || collect(stderr, STDERR_LIMIT))
        .map_err(ImportError::Io)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.0.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) => break Err(ImportError::Deadline),
            Err(error) => break Err(ImportError::Io(error)),
        }
    };
    if status.is_err() {
        let _ = child.0.kill();
        let _ = child.0.wait();
    }
    let stdout = out.join().map_err(|_| ImportError::ReaderPanicked)?;
    let stderr = err.join().map_err(|_| ImportError::ReaderPanicked)?;
    let status = status?;
    let stdout = stdout?;
    let stderr = stderr?;
    if !status.success() {
        return Err(ImportError::Command {
            operation: args.join(" "),
            code: status.code(),
            stderr,
        });
    }
    Ok(stdout)
}
