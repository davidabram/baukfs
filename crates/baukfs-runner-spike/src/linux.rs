use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};

use crate::process::Pid;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Makes the calling process a child subreaper: orphaned descendants are
/// reparented to it instead of to init, including ones that changed session
/// or process group.
///
/// # Errors
///
/// Returns the OS error from `prctl`.
pub fn become_subreaper() -> io::Result<()> {
    // SAFETY: PR_SET_CHILD_SUBREAPER takes an integer argument and no pointers.
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[derive(Clone, Copy, Debug)]
struct ProcEntry {
    pid: Pid,
    parent: libc::pid_t,
    zombie: bool,
}

fn read_entry(pid: Pid) -> io::Result<Option<ProcEntry>> {
    let stat = match fs::read(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
            ) || error.raw_os_error() == Some(libc::ESRCH) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let malformed = || io::Error::new(io::ErrorKind::InvalidData, format!("/proc/{pid}/stat"));
    // The command name may contain spaces and parentheses; fields resume after the last ')'.
    let close = stat
        .iter()
        .rposition(|byte| *byte == b')')
        .ok_or_else(malformed)?;
    let rest = std::str::from_utf8(&stat[close + 1..]).map_err(|_| malformed())?;
    let mut fields = rest.split_ascii_whitespace();
    let state = fields.next().ok_or_else(malformed)?;
    let parent = fields
        .next()
        .and_then(|field| field.parse().ok())
        .ok_or_else(malformed)?;
    Ok(Some(ProcEntry {
        pid,
        parent,
        zombie: state == "Z" || state == "X",
    }))
}

/// Live (non-zombie) descendants of `ancestor`, from one scan of `/proc`.
///
/// The scan is not atomic: processes may appear or exit while it runs.
///
/// # Errors
///
/// Returns the OS error if `/proc` cannot be listed or an entry cannot be read.
pub fn live_descendants(ancestor: Pid) -> io::Result<Vec<Pid>> {
    let mut children: BTreeMap<libc::pid_t, Vec<ProcEntry>> = BTreeMap::new();
    for dir_entry in fs::read_dir("/proc")? {
        let name = dir_entry?.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.parse().ok())
            .and_then(Pid::new)
        else {
            continue;
        };
        if let Some(entry) = read_entry(pid)? {
            children.entry(entry.parent).or_default().push(entry);
        }
    }
    let mut live = Vec::new();
    let mut pending = Vec::from([ancestor.get()]);
    while let Some(parent) = pending.pop() {
        for entry in children.remove(&parent).unwrap_or_default() {
            if !entry.zombie {
                live.push(entry.pid);
            }
            pending.push(entry.pid.get());
        }
    }
    live.sort_unstable();
    Ok(live)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CgroupPopulation {
    Populated,
    Empty,
    /// The directory no longer exists; the kernel only removes empty cgroups.
    Removed,
}

/// Reads `populated` from `cgroup.events`, which counts the whole subtree.
///
/// # Errors
///
/// Returns the OS error if the events file exists but cannot be read, or
/// `InvalidData` if it lacks a `populated` line.
pub fn population_at(dir: &Path) -> io::Result<CgroupPopulation> {
    let events = match fs::read_to_string(dir.join("cgroup.events")) {
        Ok(events) => events,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(CgroupPopulation::Removed);
        }
        Err(error) => return Err(error),
    };
    match events
        .lines()
        .find_map(|line| line.strip_prefix("populated "))
    {
        Some("0") => Ok(CgroupPopulation::Empty),
        Some(_) => Ok(CgroupPopulation::Populated),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} has no populated line", dir.display()),
        )),
    }
}

/// A cgroup v2 leaf created under the controller's own, delegated cgroup.
#[derive(Debug)]
pub struct WorkloadCgroup {
    dir: PathBuf,
    procs: File,
}

impl WorkloadCgroup {
    /// Creates (or reuses an empty) child cgroup `name` of the calling
    /// process's cgroup.
    ///
    /// # Errors
    ///
    /// Returns [`CgroupError::NotDelegated`] if the current cgroup is not
    /// writable by this user, or another variant on inspection failures.
    pub fn create(name: &str) -> Result<Self, CgroupError> {
        let membership =
            fs::read_to_string("/proc/self/cgroup").map_err(CgroupError::Membership)?;
        let relative = membership
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .ok_or(CgroupError::NotUnified)?;
        let dir = Path::new(CGROUP_ROOT)
            .join(relative.trim_start_matches('/'))
            .join(name);
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if population_at(&dir).map_err(|error| CgroupError::Io {
                    path: dir.clone(),
                    error,
                })? == CgroupPopulation::Populated
                {
                    return Err(CgroupError::Populated { path: dir });
                }
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                return Err(CgroupError::NotDelegated { path: dir });
            }
            Err(error) => return Err(CgroupError::Io { path: dir, error }),
        }
        let procs_path = dir.join("cgroup.procs");
        let procs =
            OpenOptions::new()
                .write(true)
                .open(&procs_path)
                .map_err(|error| match error.kind() {
                    io::ErrorKind::PermissionDenied => CgroupError::NotDelegated {
                        path: procs_path.clone(),
                    },
                    _ => CgroupError::Io {
                        path: procs_path.clone(),
                        error,
                    },
                })?;
        Ok(Self { dir, procs })
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    #[must_use]
    pub fn procs_fd(&self) -> RawFd {
        self.procs.as_raw_fd()
    }

    /// # Errors
    ///
    /// See [`population_at`].
    pub fn population(&self) -> io::Result<CgroupPopulation> {
        population_at(&self.dir)
    }

    /// # Errors
    ///
    /// Returns the OS error if `cgroup.procs` cannot be read.
    pub fn members(&self) -> io::Result<Vec<Pid>> {
        let procs = match fs::read_to_string(self.dir.join("cgroup.procs")) {
            Ok(procs) => procs,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        Ok(procs
            .lines()
            .filter_map(|line| line.parse().ok().and_then(Pid::new))
            .collect())
    }

    /// Sends `SIGKILL` to every process in the subtree (kernel 5.14+).
    ///
    /// # Errors
    ///
    /// Returns the OS error if `cgroup.kill` cannot be written.
    pub fn kill(&self) -> io::Result<()> {
        fs::write(self.dir.join("cgroup.kill"), b"1")
    }
}

/// Moves the calling process into the cgroup whose `cgroup.procs` is open as `fd`.
///
/// Async-signal-safe; intended for use between `fork` and `exec` so the
/// workload is contained before it can run any code of its own.
///
/// # Errors
///
/// Returns the OS error from `write`.
pub fn join_cgroup(fd: RawFd) -> io::Result<()> {
    // Writing "0" to cgroup.procs moves the writing process.
    let byte = b"0";
    // SAFETY: the buffer is valid for one byte for the duration of the call.
    if unsafe { libc::write(fd, byte.as_ptr().cast(), byte.len()) } == 1 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[derive(Debug)]
pub enum CgroupError {
    Membership(io::Error),
    NotUnified,
    NotDelegated { path: PathBuf },
    Populated { path: PathBuf },
    Io { path: PathBuf, error: io::Error },
}

impl fmt::Display for CgroupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Membership(error) => write!(f, "cannot read /proc/self/cgroup: {error}"),
            Self::NotUnified => f.write_str("process is not in a cgroup v2 hierarchy"),
            Self::NotDelegated { path } => {
                write!(f, "cgroup {} is not delegated to this user", path.display())
            }
            Self::Populated { path } => {
                write!(f, "cgroup {} still contains processes", path.display())
            }
            Self::Io { path, error } => write!(f, "cgroup {}: {error}", path.display()),
        }
    }
}

impl Error for CgroupError {}
