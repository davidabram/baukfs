use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::ownership::WorkspaceState;

/// Durable marker of the last run's lifecycle, written by the owning controller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunRecord {
    Absent,
    /// A run was started and quiescence was never established.
    Running {
        cgroup: Option<PathBuf>,
    },
    Quiescent,
}

const RUNNING: &[u8] = b"running";
const QUIESCENT: &[u8] = b"quiescent";
const CGROUP_PREFIX: &[u8] = b"cgroup ";

/// # Errors
///
/// Returns [`RecordError`] if the record exists but cannot be read or parsed.
pub fn read(state: &WorkspaceState) -> Result<RunRecord, RecordError> {
    let path = state.run_record_path();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(RunRecord::Absent),
        Err(error) => return Err(RecordError::Read { path, error }),
    };
    let mut lines = bytes.split(|byte| *byte == b'\n');
    match lines.next() {
        Some(QUIESCENT) => Ok(RunRecord::Quiescent),
        Some(RUNNING) => {
            let cgroup = lines
                .find_map(|line| line.strip_prefix(CGROUP_PREFIX))
                .map(|raw| PathBuf::from(OsStr::from_bytes(raw)));
            Ok(RunRecord::Running { cgroup })
        }
        _ => Err(RecordError::Malformed { path }),
    }
}

/// Must be written before the workload is spawned so a controller crash can
/// never leave a live workload without a `Running` record.
///
/// # Errors
///
/// Returns [`RecordError::Write`] if the record cannot be durably replaced.
pub fn write_running(state: &WorkspaceState, cgroup: Option<&Path>) -> Result<(), RecordError> {
    let mut contents = RUNNING.to_vec();
    contents.push(b'\n');
    if let Some(cgroup) = cgroup {
        contents.extend_from_slice(CGROUP_PREFIX);
        contents.extend_from_slice(cgroup.as_os_str().as_bytes());
        contents.push(b'\n');
    }
    replace(state, &contents)
}

/// # Errors
///
/// Returns [`RecordError::Write`] if the record cannot be durably replaced.
pub fn write_quiescent(state: &WorkspaceState) -> Result<(), RecordError> {
    let mut contents = QUIESCENT.to_vec();
    contents.push(b'\n');
    replace(state, &contents)
}

fn replace(state: &WorkspaceState, contents: &[u8]) -> Result<(), RecordError> {
    let path = state.run_record_path();
    let staging = path.with_extension("staging");
    let write = || -> io::Result<()> {
        let mut file = File::create(&staging)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&staging, &path)?;
        File::open(state.dir())?.sync_all()
    };
    write().map_err(|error| RecordError::Write {
        path: path.clone(),
        error,
    })
}

#[derive(Debug)]
pub enum RecordError {
    Read { path: PathBuf, error: io::Error },
    Write { path: PathBuf, error: io::Error },
    Malformed { path: PathBuf },
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, error } => {
                write!(f, "cannot read run record {}: {error}", path.display())
            }
            Self::Write { path, error } => {
                write!(f, "cannot write run record {}: {error}", path.display())
            }
            Self::Malformed { path } => write!(f, "malformed run record {}", path.display()),
        }
    }
}

impl Error for RecordError {}
