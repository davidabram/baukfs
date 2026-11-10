use std::error::Error;
use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;

use crate::ownership::WorkspaceState;

/// Workload liveness token: an exclusive `flock` on one open file description
/// that the workload inherits.
///
/// The lock stays held while any process keeps a descriptor for that
/// description, including after the controller dies. A process that closes
/// the inherited descriptor escapes the token.
#[derive(Debug)]
pub struct WorkloadToken {
    file: File,
}

impl WorkloadToken {
    /// # Errors
    ///
    /// Returns [`TokenError::HeldByPriorWorkload`] if a previous workload still
    /// holds the token, or [`TokenError::Io`] on I/O failure.
    pub fn acquire(state: &WorkspaceState) -> Result<Self, TokenError> {
        let path = state.token_path();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| TokenError::Io {
                path: path.clone(),
                error,
            })?;
        match file.try_lock() {
            Ok(()) => Ok(Self { file }),
            Err(TryLockError::WouldBlock) => Err(TokenError::HeldByPriorWorkload { path }),
            Err(TryLockError::Error(error)) => Err(TokenError::Io { path, error }),
        }
    }

    #[must_use]
    pub fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenState {
    Held,
    Free,
}

/// Probes the token through a fresh open file description.
///
/// The caller must not hold its own token descriptor, or the probe reports
/// `Held` because of the caller itself.
///
/// # Errors
///
/// Returns [`TokenError::Io`] if the token file cannot be opened or locked.
pub fn probe(state: &WorkspaceState) -> Result<TokenState, TokenError> {
    let path = state.token_path();
    let file = match OpenOptions::new().read(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(TokenState::Free),
        Err(error) => return Err(TokenError::Io { path, error }),
    };
    match file.try_lock() {
        Ok(()) => Ok(TokenState::Free),
        Err(TryLockError::WouldBlock) => Ok(TokenState::Held),
        Err(TryLockError::Error(error)) => Err(TokenError::Io { path, error }),
    }
}

#[derive(Debug)]
pub enum TokenError {
    HeldByPriorWorkload { path: PathBuf },
    Io { path: PathBuf, error: io::Error },
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeldByPriorWorkload { path } => write!(
                f,
                "workload token {} is still held by a previous workload",
                path.display()
            ),
            Self::Io { path, error } => {
                write!(f, "workload token {}: {error}", path.display())
            }
        }
    }
}

impl Error for TokenError {}
