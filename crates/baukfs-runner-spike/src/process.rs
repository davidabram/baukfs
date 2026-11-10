use std::fmt;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pid(libc::pid_t);

impl Pid {
    /// Returns `None` for values that cannot name a single process.
    #[must_use]
    pub fn new(raw: libc::pid_t) -> Option<Self> {
        (raw > 0).then_some(Self(raw))
    }

    #[must_use]
    pub fn from_child_id(id: u32) -> Option<Self> {
        libc::pid_t::try_from(id).ok().and_then(Self::new)
    }

    #[must_use]
    pub fn current() -> Self {
        // SAFETY: getpid takes no arguments and cannot fail.
        Self(unsafe { libc::getpid() })
    }

    #[must_use]
    pub fn get(self) -> libc::pid_t {
        self.0
    }
}

impl fmt::Display for Pid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Terminate,
    Kill,
}

impl Signal {
    fn raw(self) -> libc::c_int {
        match self {
            Self::Terminate => libc::SIGTERM,
            Self::Kill => libc::SIGKILL,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Delivered,
    NoSuchProcess,
}

fn kill(target: libc::pid_t, signal: libc::c_int) -> io::Result<Delivery> {
    // SAFETY: kill has no memory-safety preconditions.
    if unsafe { libc::kill(target, signal) } == 0 {
        return Ok(Delivery::Delivered);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(Delivery::NoSuchProcess),
        _ => Err(error),
    }
}

/// # Errors
///
/// Returns the OS error for failures other than a missing process.
pub fn signal_process(pid: Pid, signal: Signal) -> io::Result<Delivery> {
    kill(pid.0, signal.raw())
}

/// # Errors
///
/// Returns the OS error for failures other than a missing process group.
pub fn signal_group(pgid: Pid, signal: Signal) -> io::Result<Delivery> {
    kill(-pgid.0, signal.raw())
}

/// Whether any process, including an unreaped zombie, is still a member of
/// process group `pgid`.
///
/// # Errors
///
/// Returns the OS error for failures other than a missing group or a
/// permission denial (which proves a member exists).
pub fn group_exists(pgid: Pid) -> io::Result<bool> {
    match kill(-pgid.0, 0) {
        Ok(Delivery::Delivered) => Ok(true),
        Ok(Delivery::NoSuchProcess) => Ok(false),
        Err(error) if error.raw_os_error() == Some(libc::EPERM) => Ok(true),
        Err(error) => Err(error),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reaped {
    Exited(Pid, ExitStatus),
    Running,
    NoChildren,
}

fn wait_nohang(target: libc::pid_t) -> io::Result<Reaped> {
    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: status is a valid, writable c_int for the duration of the call.
        let rc = unsafe { libc::waitpid(target, &raw mut status, libc::WNOHANG) };
        match rc {
            0 => return Ok(Reaped::Running),
            -1 => {
                let error = io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(libc::EINTR) => {}
                    Some(libc::ECHILD) => return Ok(Reaped::NoChildren),
                    _ => return Err(error),
                }
            }
            pid => return Ok(Reaped::Exited(Pid(pid), ExitStatus::from_raw(status))),
        }
    }
}

/// Reaps one exited child of the calling process without blocking.
///
/// # Errors
///
/// Returns the OS error if `waitpid` fails for a reason other than `EINTR`
/// or having no children.
pub fn reap_any() -> io::Result<Reaped> {
    wait_nohang(-1)
}

/// Reaps `pid` if it has exited, without blocking.
///
/// # Errors
///
/// Returns the OS error if `waitpid` fails for a reason other than `EINTR`
/// or `pid` not being a child.
pub fn reap(pid: Pid) -> io::Result<Reaped> {
    wait_nohang(pid.0)
}

/// Clears `FD_CLOEXEC` so `fd` survives `exec`.
///
/// Async-signal-safe; intended for use between `fork` and `exec`.
///
/// # Errors
///
/// Returns the OS error from `fcntl`.
pub fn clear_close_on_exec(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl with F_GETFD/F_SETFD takes no pointers.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn describe_status(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exited(code={code})"),
        (None, Some(signal)) => format!("exited(signal={signal})"),
        (None, None) => format!("exited(raw={})", status.into_raw()),
    }
}
