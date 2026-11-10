use std::fmt;

use crate::ownership::{ControllerLock, OwnershipError, WorkspaceKey};
use crate::process::Pid;
use crate::recovery::{AbandonedEvidence, UnresolvedReason};
use crate::supervisor::ProbeError;

/// Which supported condition established quiescence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuiescenceBasis {
    /// No interrupted run is recorded and the workload token is free.
    NoInterruptedRun,
    /// Linux: the controller is a child subreaper, has reaped the root, has no
    /// children left, and the process group and workload token are released.
    SubreaperEmpty,
    /// Linux: the workload cgroup subtree is unpopulated, in addition to the
    /// subreaper condition when the controller is alive.
    CgroupEmpty,
    /// macOS narrower contract: root reaped, process group empty, workload
    /// token released. Does not cover a descendant that both left the process
    /// group and closed its inherited descriptors.
    GroupAndTokenReleased,
}

impl fmt::Display for QuiescenceBasis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoInterruptedRun => "no-interrupted-run",
            Self::SubreaperEmpty => "subreaper-empty",
            Self::CgroupEmpty => "cgroup-empty",
            Self::GroupAndTokenReleased => "group-and-token-released",
        })
    }
}

/// Evidence that the managed workload of one workspace has stopped.
///
/// Only the supervisor and recovery classification construct it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuiescenceProof {
    workspace: WorkspaceKey,
    basis: QuiescenceBasis,
}

impl QuiescenceProof {
    pub(crate) fn new(workspace: WorkspaceKey, basis: QuiescenceBasis) -> Self {
        Self { workspace, basis }
    }

    #[must_use]
    pub fn basis(&self) -> QuiescenceBasis {
        self.basis
    }
}

/// One mechanism's report that the supervised workload is still active.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivityIndicator {
    RootRunning,
    /// Linux subreaper: `waitpid` reports unreaped children; lists the live
    /// descendants found in `/proc`, which may be empty for a process that is
    /// exiting but not yet reaped.
    ChildrenRemain(Vec<Pid>),
    ProcessGroupAlive,
    TokenHeld,
    CgroupPopulated,
}

impl fmt::Display for ActivityIndicator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootRunning => f.write_str("root-running"),
            Self::ChildrenRemain(live) => {
                f.write_str("children[")?;
                for (index, pid) in live.iter().enumerate() {
                    if index > 0 {
                        f.write_str(",")?;
                    }
                    pid.fmt(f)?;
                }
                f.write_str("]")
            }
            Self::ProcessGroupAlive => f.write_str("group-alive"),
            Self::TokenHeld => f.write_str("token-held"),
            Self::CgroupPopulated => f.write_str("cgroup-populated"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActiveWorkload {
    /// Never empty.
    Supervised(Vec<ActivityIndicator>),
    Abandoned(AbandonedEvidence),
}

#[derive(Debug)]
pub enum UnknownReason {
    Probe(ProbeError),
    PriorRunUnresolved(UnresolvedReason),
}

#[derive(Debug)]
pub enum WorkloadState {
    Quiescent(QuiescenceProof),
    Active(ActiveWorkload),
    Unknown(UnknownReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureAdmission {
    pub basis: QuiescenceBasis,
}

#[derive(Debug)]
pub enum AdmissionRefusal {
    Ownership(OwnershipError),
    ForeignWorkspace,
    Active(ActiveWorkload),
    Unknown(UnknownReason),
}

/// Decides whether capture of the workspace owned by `lock` may proceed.
///
/// # Errors
///
/// Returns [`AdmissionRefusal`] unless ownership is still valid and `state`
/// carries quiescence evidence for the same workspace.
pub fn admit(
    lock: &ControllerLock,
    state: WorkloadState,
) -> Result<CaptureAdmission, AdmissionRefusal> {
    lock.verify().map_err(AdmissionRefusal::Ownership)?;
    match state {
        WorkloadState::Quiescent(proof) if proof.workspace == lock.state().key() => {
            Ok(CaptureAdmission { basis: proof.basis })
        }
        WorkloadState::Quiescent(_) => Err(AdmissionRefusal::ForeignWorkspace),
        WorkloadState::Active(active) => Err(AdmissionRefusal::Active(active)),
        WorkloadState::Unknown(reason) => Err(AdmissionRefusal::Unknown(reason)),
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::ownership::WorkspaceState;
    use crate::recovery::UnresolvedReason;

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = env::temp_dir().join(format!(
                "baukfs-admission-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).expect("create scratch dir");
            Self(dir)
        }

        fn lock(&self, name: &str) -> ControllerLock {
            let state = WorkspaceState::open(self.0.join(name)).expect("state dir");
            ControllerLock::try_acquire(state).expect("acquire")
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn quiescence_for_owned_workspace_is_admitted() {
        let scratch = ScratchDir::new();
        let lock = scratch.lock("a");
        let proof = QuiescenceProof::new(lock.state().key(), QuiescenceBasis::SubreaperEmpty);
        let admission = admit(&lock, WorkloadState::Quiescent(proof)).expect("admitted");
        assert_eq!(admission.basis, QuiescenceBasis::SubreaperEmpty);
    }

    #[test]
    fn quiescence_for_another_workspace_is_refused() {
        let scratch = ScratchDir::new();
        let lock_a = scratch.lock("a");
        let lock_b = scratch.lock("b");
        let proof = QuiescenceProof::new(lock_b.state().key(), QuiescenceBasis::SubreaperEmpty);
        assert!(matches!(
            admit(&lock_a, WorkloadState::Quiescent(proof)),
            Err(AdmissionRefusal::ForeignWorkspace)
        ));
    }

    #[test]
    fn unknown_state_is_refused() {
        let scratch = ScratchDir::new();
        let lock = scratch.lock("a");
        let state = WorkloadState::Unknown(UnknownReason::PriorRunUnresolved(
            UnresolvedReason::InterruptedRunTokenFree,
        ));
        assert!(matches!(
            admit(&lock, state),
            Err(AdmissionRefusal::Unknown(_))
        ));
    }

    #[test]
    fn active_state_is_refused() {
        let scratch = ScratchDir::new();
        let lock = scratch.lock("a");
        let state = WorkloadState::Active(ActiveWorkload::Abandoned(AbandonedEvidence::TokenHeld));
        assert!(matches!(
            admit(&lock, state),
            Err(AdmissionRefusal::Active(_))
        ));
    }

    #[test]
    fn replaced_lock_file_invalidates_ownership() {
        let scratch = ScratchDir::new();
        let lock = scratch.lock("a");
        let lock_path = lock.state().controller_lock_path();
        fs::remove_file(&lock_path).expect("unlink lock");
        let usurper = ControllerLock::try_acquire(lock.state().clone())
            .expect("a second controller can lock the replacement inode");
        let proof = QuiescenceProof::new(lock.state().key(), QuiescenceBasis::SubreaperEmpty);
        assert!(matches!(
            admit(&lock, WorkloadState::Quiescent(proof)),
            Err(AdmissionRefusal::Ownership(OwnershipError::Lost { .. }))
        ));
        let proof = QuiescenceProof::new(usurper.state().key(), QuiescenceBasis::SubreaperEmpty);
        assert!(admit(&usurper, WorkloadState::Quiescent(proof)).is_ok());
    }

    #[test]
    fn second_lock_on_same_workspace_is_held() {
        let scratch = ScratchDir::new();
        let _first = scratch.lock("a");
        let state = WorkspaceState::open(scratch.0.join("a")).expect("state dir");
        assert!(matches!(
            ControllerLock::try_acquire(state),
            Err(OwnershipError::Held { .. })
        ));
        let _other = scratch.lock("b");
    }
}
