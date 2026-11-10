use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Identity of a workspace state directory (device and inode), stable across renames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkspaceKey {
    dev: u64,
    ino: u64,
}

/// Controller-private state for one workspace.
///
/// The directory must live outside the agent-writable working directory.
#[derive(Clone, Debug)]
pub struct WorkspaceState {
    dir: PathBuf,
    key: WorkspaceKey,
}

impl WorkspaceState {
    /// # Errors
    ///
    /// Returns [`OwnershipError::StateDir`] if the directory cannot be created or inspected.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, OwnershipError> {
        let dir = dir.into();
        let state_dir_error = |error| OwnershipError::StateDir {
            path: dir.clone(),
            error,
        };
        fs::create_dir_all(&dir).map_err(state_dir_error)?;
        let metadata = fs::metadata(&dir).map_err(state_dir_error)?;
        Ok(Self {
            key: WorkspaceKey {
                dev: metadata.dev(),
                ino: metadata.ino(),
            },
            dir,
        })
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    #[must_use]
    pub fn key(&self) -> WorkspaceKey {
        self.key
    }

    pub(crate) fn controller_lock_path(&self) -> PathBuf {
        self.dir.join("controller.lock")
    }

    pub(crate) fn token_path(&self) -> PathBuf {
        self.dir.join("workload.token")
    }

    pub(crate) fn run_record_path(&self) -> PathBuf {
        self.dir.join("run.record")
    }
}

/// Exclusive controller ownership of one workspace, held as an `flock` on a
/// close-on-exec descriptor so the workload never inherits it and controller
/// death releases it.
#[derive(Debug)]
pub struct ControllerLock {
    state: WorkspaceState,
    file: File,
}

impl ControllerLock {
    /// # Errors
    ///
    /// Returns [`OwnershipError::Held`] if another controller owns the workspace,
    /// or another variant if the lock file cannot be opened or locked.
    pub fn try_acquire(state: WorkspaceState) -> Result<Self, OwnershipError> {
        let path = state.controller_lock_path();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| OwnershipError::Open {
                path: path.clone(),
                error,
            })?;
        match file.try_lock() {
            Ok(()) => Ok(Self { state, file }),
            Err(TryLockError::WouldBlock) => Err(OwnershipError::Held { path }),
            Err(TryLockError::Error(error)) => Err(OwnershipError::Lock { path, error }),
        }
    }

    #[must_use]
    pub fn state(&self) -> &WorkspaceState {
        &self.state
    }

    /// Confirms the locked inode is still the one at the lock path.
    ///
    /// `flock` binds to the inode, so unlinking or replacing the path would let
    /// a second controller lock a different inode for the same workspace.
    ///
    /// # Errors
    ///
    /// Returns [`OwnershipError::Lost`] if the path no longer names the locked
    /// inode, or [`OwnershipError::Inspect`] if either cannot be inspected.
    pub fn verify(&self) -> Result<(), OwnershipError> {
        let path = self.state.controller_lock_path();
        let held = self
            .file
            .metadata()
            .map_err(|error| OwnershipError::Inspect {
                path: path.clone(),
                error,
            })?;
        let current = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(OwnershipError::Lost { path });
            }
            Err(error) => return Err(OwnershipError::Inspect { path, error }),
        };
        if held.dev() == current.dev() && held.ino() == current.ino() {
            Ok(())
        } else {
            Err(OwnershipError::Lost { path })
        }
    }
}

#[derive(Debug)]
pub enum OwnershipError {
    StateDir { path: PathBuf, error: io::Error },
    Open { path: PathBuf, error: io::Error },
    Lock { path: PathBuf, error: io::Error },
    Held { path: PathBuf },
    Inspect { path: PathBuf, error: io::Error },
    Lost { path: PathBuf },
}

impl fmt::Display for OwnershipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StateDir { path, error } => {
                write!(
                    f,
                    "cannot prepare state directory {}: {error}",
                    path.display()
                )
            }
            Self::Open { path, error } => {
                write!(f, "cannot open controller lock {}: {error}", path.display())
            }
            Self::Lock { path, error } => {
                write!(f, "cannot lock controller lock {}: {error}", path.display())
            }
            Self::Held { path } => {
                write!(f, "another controller holds {}", path.display())
            }
            Self::Inspect { path, error } => {
                write!(
                    f,
                    "cannot inspect controller lock {}: {error}",
                    path.display()
                )
            }
            Self::Lost { path } => {
                write!(
                    f,
                    "controller lock {} was replaced or removed",
                    path.display()
                )
            }
        }
    }
}

impl Error for OwnershipError {}
