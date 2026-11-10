use std::error::Error;
use std::fmt;
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crate::admission::{
    ActiveWorkload, ActivityIndicator, QuiescenceBasis, QuiescenceProof, WorkloadState,
};
use crate::ownership::{ControllerLock, WorkspaceKey, WorkspaceState};
use crate::process::{self, Delivery, Pid, Reaped, Signal};
use crate::record::{self, RecordError};
use crate::token::{self, TokenError, TokenState, WorkloadToken};

#[cfg(target_os = "linux")]
use crate::linux::{self, CgroupError, CgroupPopulation, WorkloadCgroup};

const POLL_INTERVAL: Duration = Duration::from_millis(2);
const DRAIN_LIMIT: usize = 64 * 1024;

/// Prepares the controller process before any workload is spawned.
///
/// On Linux this makes the controller a child subreaper. It must run before
/// the first spawn so no descendant can be orphaned past the controller.
///
/// # Errors
///
/// Returns the OS error from the platform call.
pub fn prepare_controller() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    linux::become_subreaper()?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SupervisorOptions {
    /// Linux only: contain the workload in a child cgroup of the controller's
    /// delegated cgroup.
    pub cgroup: bool,
}

/// Bounded, continuously drained copy of one diagnostic stream.
#[derive(Clone, Debug)]
pub struct Drain {
    buffer: Arc<Mutex<Vec<u8>>>,
    open: Arc<AtomicBool>,
}

impl Drain {
    fn start(mut reader: impl Read + Send + 'static) -> Self {
        let drain = Self {
            buffer: Arc::new(Mutex::new(Vec::new())),
            open: Arc::new(AtomicBool::new(true)),
        };
        let shared = drain.clone();
        thread::spawn(move || {
            let mut chunk = [0_u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        let mut buffer =
                            shared.buffer.lock().unwrap_or_else(PoisonError::into_inner);
                        let room = DRAIN_LIMIT.saturating_sub(buffer.len());
                        buffer.extend_from_slice(&chunk[..read.min(room)]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            shared.open.store(false, Ordering::Release);
        });
        drain
    }

    /// Whether any process still holds the write end of the pipe.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn captured(&self) -> Vec<u8> {
        self.buffer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootState {
    Running,
    Exited(ExitStatus),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tracking {
    /// Linux child subreaper. `children_remain` is authoritative (from
    /// `waitpid`); `live` is a best-effort `/proc` listing.
    Subreaper {
        children_remain: bool,
        live: Vec<Pid>,
    },
    Untracked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CgroupView {
    NotConfigured,
    Populated,
    Empty,
}

/// One snapshot of every mechanism's view of the workload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub root: RootState,
    pub group_alive: bool,
    pub token: TokenState,
    pub tracking: Tracking,
    pub cgroup: CgroupView,
}

impl fmt::Display for Observation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.root {
            RootState::Running => f.write_str("root=running")?,
            RootState::Exited(status) => write!(f, "root={}", process::describe_status(status))?,
        }
        write!(
            f,
            " group={} token={}",
            if self.group_alive { "alive" } else { "empty" },
            match self.token {
                TokenState::Held => "held",
                TokenState::Free => "free",
            }
        )?;
        match &self.tracking {
            Tracking::Subreaper {
                children_remain,
                live,
            } => write!(
                f,
                " children={} descendants={}",
                if *children_remain { "yes" } else { "no" },
                PidList(live)
            )?,
            Tracking::Untracked => f.write_str(" children=untracked descendants=untracked")?,
        }
        f.write_str(match self.cgroup {
            CgroupView::NotConfigured => " cgroup=none",
            CgroupView::Populated => " cgroup=populated",
            CgroupView::Empty => " cgroup=empty",
        })
    }
}

pub struct PidList<'a>(pub &'a [Pid]);

impl fmt::Display for PidList<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[")?;
        for (index, pid) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(",")?;
            }
            pid.fmt(f)?;
        }
        f.write_str("]")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminationReport {
    pub group_terminate: Delivery,
    /// Processes individually sent `SIGTERM` (Linux descendants and cgroup members).
    pub terminated: Vec<Pid>,
    /// Linux: workload processes still alive when the grace period expired.
    pub killed: Vec<Pid>,
    pub escalated: bool,
    pub quiescent: bool,
    pub last: Observation,
}

/// Supervises one workload spawned as its own process group.
pub struct Supervisor {
    workspace: WorkspaceKey,
    state: WorkspaceState,
    root: Pid,
    root_status: Option<ExitStatus>,
    stdout: Drain,
    stderr: Drain,
    #[cfg(target_os = "linux")]
    cgroup: Option<WorkloadCgroup>,
}

impl Supervisor {
    /// Spawns `command` as the workload root.
    ///
    /// The run record is marked `Running` before spawn. The root is placed in
    /// a new process group, inherits the workload token, and on Linux with
    /// `options.cgroup` joins the workload cgroup before `exec`.
    ///
    /// # Errors
    ///
    /// Returns [`SpawnError`] if any of those steps fail.
    pub fn spawn(
        lock: &ControllerLock,
        mut command: Command,
        options: SupervisorOptions,
    ) -> Result<Self, SpawnError> {
        let state = lock.state().clone();

        #[cfg(target_os = "linux")]
        let cgroup = if options.cgroup {
            Some(WorkloadCgroup::create("baukfs-workload").map_err(SpawnError::Cgroup)?)
        } else {
            None
        };
        #[cfg(target_os = "linux")]
        let cgroup_dir = cgroup.as_ref().map(WorkloadCgroup::dir);
        #[cfg(not(target_os = "linux"))]
        let cgroup_dir = if options.cgroup {
            return Err(SpawnError::CgroupUnsupported);
        } else {
            None
        };

        record::write_running(&state, cgroup_dir).map_err(SpawnError::Record)?;
        let token = WorkloadToken::acquire(&state).map_err(SpawnError::Token)?;
        let token_fd = token.as_raw_fd();
        #[cfg(target_os = "linux")]
        let cgroup_fd = cgroup.as_ref().map(WorkloadCgroup::procs_fd);

        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        // SAFETY: the hook only calls async-signal-safe fcntl and write.
        unsafe {
            command.pre_exec(move || {
                #[cfg(target_os = "linux")]
                if let Some(fd) = cgroup_fd {
                    linux::join_cgroup(fd)?;
                }
                process::clear_close_on_exec(token_fd)
            });
        }
        let mut child = command.spawn().map_err(SpawnError::Spawn)?;
        drop(token);

        let root = Pid::from_child_id(child.id()).ok_or(SpawnError::InvalidPid(child.id()))?;
        let stdout = Drain::start(child.stdout.take().ok_or(SpawnError::MissingPipe)?);
        let stderr = Drain::start(child.stderr.take().ok_or(SpawnError::MissingPipe)?);
        Ok(Self {
            workspace: state.key(),
            state,
            root,
            root_status: None,
            stdout,
            stderr,
            #[cfg(target_os = "linux")]
            cgroup,
        })
    }

    #[must_use]
    pub fn root(&self) -> Pid {
        self.root
    }

    #[must_use]
    pub fn stdout(&self) -> &Drain {
        &self.stdout
    }

    #[must_use]
    pub fn stderr(&self) -> &Drain {
        &self.stderr
    }

    #[must_use]
    pub fn root_status(&self) -> Option<ExitStatus> {
        self.root_status
    }

    /// Reaps exited children and reports whether any children remain.
    #[cfg(target_os = "linux")]
    fn reap(&mut self) -> Result<bool, ProbeError> {
        loop {
            match process::reap_any().map_err(ProbeError::Reap)? {
                Reaped::Exited(pid, status) => {
                    if pid == self.root {
                        self.root_status = Some(status);
                    }
                }
                Reaped::Running => return Ok(true),
                Reaped::NoChildren => return Ok(false),
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn reap(&mut self) -> Result<bool, ProbeError> {
        if self.root_status.is_none() {
            match process::reap(self.root).map_err(ProbeError::Reap)? {
                Reaped::Exited(_, status) => self.root_status = Some(status),
                Reaped::Running => return Ok(true),
                Reaped::NoChildren => return Err(ProbeError::RootLost(self.root)),
            }
        }
        Ok(false)
    }

    /// # Errors
    ///
    /// Returns [`ProbeError`] if any mechanism cannot be inspected.
    pub fn observe(&mut self) -> Result<Observation, ProbeError> {
        let children_remain = self.reap()?;
        let group_alive = process::group_exists(self.root).map_err(ProbeError::Group)?;
        let token = token::probe(&self.state).map_err(ProbeError::Token)?;

        #[cfg(target_os = "linux")]
        let tracking = Tracking::Subreaper {
            children_remain,
            live: linux::live_descendants(Pid::current()).map_err(ProbeError::Scan)?,
        };
        #[cfg(not(target_os = "linux"))]
        let tracking = {
            let _ = children_remain;
            Tracking::Untracked
        };

        #[cfg(target_os = "linux")]
        let cgroup = match &self.cgroup {
            None => CgroupView::NotConfigured,
            Some(cgroup) => match cgroup.population().map_err(ProbeError::Cgroup)? {
                CgroupPopulation::Populated => CgroupView::Populated,
                CgroupPopulation::Empty | CgroupPopulation::Removed => CgroupView::Empty,
            },
        };
        #[cfg(not(target_os = "linux"))]
        let cgroup = CgroupView::NotConfigured;

        Ok(Observation {
            root: self
                .root_status
                .map_or(RootState::Running, RootState::Exited),
            group_alive,
            token,
            tracking,
            cgroup,
        })
    }

    /// Applies this platform's supported quiescence condition to `observation`.
    #[must_use]
    pub fn assess(&self, observation: &Observation) -> WorkloadState {
        let mut active = Vec::new();
        if observation.root == RootState::Running {
            active.push(ActivityIndicator::RootRunning);
        }
        if let Tracking::Subreaper {
            children_remain: true,
            live,
        } = &observation.tracking
        {
            active.push(ActivityIndicator::ChildrenRemain(live.clone()));
        }
        if observation.group_alive {
            active.push(ActivityIndicator::ProcessGroupAlive);
        }
        if observation.token == TokenState::Held {
            active.push(ActivityIndicator::TokenHeld);
        }
        if observation.cgroup == CgroupView::Populated {
            active.push(ActivityIndicator::CgroupPopulated);
        }
        if !active.is_empty() {
            return WorkloadState::Active(ActiveWorkload::Supervised(active));
        }
        let basis = match (&observation.tracking, observation.cgroup) {
            (_, CgroupView::Empty) => QuiescenceBasis::CgroupEmpty,
            (Tracking::Subreaper { .. }, _) => QuiescenceBasis::SubreaperEmpty,
            (Tracking::Untracked, _) => QuiescenceBasis::GroupAndTokenReleased,
        };
        WorkloadState::Quiescent(QuiescenceProof::new(self.workspace, basis))
    }

    /// Waits up to `timeout` for the root process to be reaped.
    ///
    /// # Errors
    ///
    /// Returns [`ProbeError`] if reaping fails.
    pub fn wait_root(&mut self, timeout: Duration) -> Result<Option<ExitStatus>, ProbeError> {
        let deadline = Instant::now() + timeout;
        loop {
            self.reap()?;
            if self.root_status.is_some() || Instant::now() >= deadline {
                return Ok(self.root_status);
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Re-observes until the quiescence condition holds or `timeout` elapses.
    ///
    /// Polling only bounds the wait; each decision comes from OS state.
    ///
    /// # Errors
    ///
    /// Returns [`ProbeError`] if observation fails.
    pub fn settle(&mut self, timeout: Duration) -> Result<(bool, Observation), ProbeError> {
        let deadline = Instant::now() + timeout;
        loop {
            let observation = self.observe()?;
            let quiescent = matches!(self.assess(&observation), WorkloadState::Quiescent(_));
            if quiescent || Instant::now() >= deadline {
                return Ok((quiescent, observation));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    #[cfg(target_os = "linux")]
    fn individual_targets(&self) -> Result<Vec<Pid>, ProbeError> {
        let mut targets = linux::live_descendants(Pid::current()).map_err(ProbeError::Scan)?;
        if let Some(cgroup) = &self.cgroup {
            targets.extend(cgroup.members().map_err(ProbeError::Cgroup)?);
        }
        targets.sort_unstable();
        targets.dedup();
        Ok(targets)
    }

    #[cfg(not(target_os = "linux"))]
    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    fn individual_targets(&self) -> Result<Vec<Pid>, ProbeError> {
        Ok(Vec::new())
    }

    fn signal_all(&self, signal: Signal) -> Result<(Delivery, Vec<Pid>), ProbeError> {
        #[cfg(target_os = "linux")]
        if signal == Signal::Kill
            && let Some(cgroup) = &self.cgroup
        {
            cgroup.kill().map_err(ProbeError::Cgroup)?;
        }
        let group = process::signal_group(self.root, signal).map_err(ProbeError::Group)?;
        let mut delivered = Vec::new();
        for pid in self.individual_targets()? {
            // PID reuse between the scan and the signal is possible for
            // processes that are not our direct children; pidfds would close it.
            match process::signal_process(pid, signal)
                .map_err(|error| ProbeError::Signal { pid, error })?
            {
                Delivery::Delivered => delivered.push(pid),
                Delivery::NoSuchProcess => {}
            }
        }
        Ok((group, delivered))
    }

    /// Sends `SIGTERM` to every reachable workload process, waits up to
    /// `grace`, then escalates to `SIGKILL` and waits up to `kill_timeout`.
    ///
    /// # Errors
    ///
    /// Returns [`ProbeError`] if signalling or observation fails.
    pub fn terminate(
        &mut self,
        grace: Duration,
        kill_timeout: Duration,
    ) -> Result<TerminationReport, ProbeError> {
        let (group_terminate, terminated) = self.signal_all(Signal::Terminate)?;
        let (quiescent, last) = self.settle(grace)?;
        if quiescent {
            return Ok(TerminationReport {
                group_terminate,
                terminated,
                killed: Vec::new(),
                escalated: false,
                quiescent,
                last,
            });
        }
        let deadline = Instant::now() + kill_timeout;
        let killed = self.individual_targets()?;
        loop {
            self.signal_all(Signal::Kill)?;
            let (quiescent, last) = self.settle(POLL_INTERVAL * 5)?;
            if quiescent || Instant::now() >= deadline {
                return Ok(TerminationReport {
                    group_terminate,
                    terminated,
                    killed,
                    escalated: true,
                    quiescent,
                    last,
                });
            }
        }
    }
}

#[derive(Debug)]
pub enum SpawnError {
    Record(RecordError),
    Token(TokenError),
    #[cfg(target_os = "linux")]
    Cgroup(CgroupError),
    #[cfg(not(target_os = "linux"))]
    CgroupUnsupported,
    Spawn(io::Error),
    InvalidPid(u32),
    MissingPipe,
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Record(error) => error.fmt(f),
            Self::Token(error) => error.fmt(f),
            #[cfg(target_os = "linux")]
            Self::Cgroup(error) => error.fmt(f),
            #[cfg(not(target_os = "linux"))]
            Self::CgroupUnsupported => f.write_str("cgroup containment requires Linux"),
            Self::Spawn(error) => write!(f, "cannot spawn workload: {error}"),
            Self::InvalidPid(id) => write!(f, "spawned child has invalid pid {id}"),
            Self::MissingPipe => f.write_str("spawned child is missing a diagnostic pipe"),
        }
    }
}

impl Error for SpawnError {}

#[derive(Debug)]
pub enum ProbeError {
    Reap(io::Error),
    #[cfg(not(target_os = "linux"))]
    RootLost(Pid),
    Group(io::Error),
    Token(TokenError),
    #[cfg(target_os = "linux")]
    Scan(io::Error),
    #[cfg(target_os = "linux")]
    Cgroup(io::Error),
    Signal {
        pid: Pid,
        error: io::Error,
    },
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reap(error) => write!(f, "waitpid failed: {error}"),
            #[cfg(not(target_os = "linux"))]
            Self::RootLost(pid) => write!(f, "root {pid} is no longer a child"),
            Self::Group(error) => write!(f, "process group probe failed: {error}"),
            Self::Token(error) => error.fmt(f),
            #[cfg(target_os = "linux")]
            Self::Scan(error) => write!(f, "/proc scan failed: {error}"),
            #[cfg(target_os = "linux")]
            Self::Cgroup(error) => write!(f, "cgroup probe failed: {error}"),
            Self::Signal { pid, error } => write!(f, "cannot signal {pid}: {error}"),
        }
    }
}

impl Error for ProbeError {}
