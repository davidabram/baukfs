use std::error::Error;
use std::fmt;
use std::io;
use std::path::PathBuf;

use crate::admission::{QuiescenceBasis, QuiescenceProof};
use crate::ownership::ControllerLock;
use crate::record::{self, RecordError, RunRecord};
use crate::token::{self, TokenError, TokenState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbandonedEvidence {
    TokenHeld,
    CgroupPopulated,
}

impl fmt::Display for AbandonedEvidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TokenHeld => "token-held",
            Self::CgroupPopulated => "cgroup-populated",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnresolvedReason {
    /// A run was interrupted and the token is free, but a descendant that
    /// closed its inherited descriptors cannot be ruled out.
    InterruptedRunTokenFree,
    /// The record names a cgroup but this platform cannot inspect cgroups.
    CgroupUnsupported,
}

impl fmt::Display for UnresolvedReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InterruptedRunTokenFree => "interrupted-run-token-free",
            Self::CgroupUnsupported => "cgroup-unsupported",
        })
    }
}

/// What a newly started controller can conclude about earlier runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryState {
    NoWorkload(QuiescenceProof),
    LiveAbandoned(AbandonedEvidence),
    Unresolved(UnresolvedReason),
}

impl fmt::Display for RecoveryState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoWorkload(proof) => write!(f, "no-workload({})", proof.basis()),
            Self::LiveAbandoned(evidence) => write!(f, "live-abandoned({evidence})"),
            Self::Unresolved(reason) => write!(f, "unresolved({reason})"),
        }
    }
}

/// Classifies prior runs of the workspace owned by `lock`.
///
/// # Errors
///
/// Returns [`RecoveryError`] if the run record, token, or cgroup cannot be inspected.
pub fn classify(lock: &ControllerLock) -> Result<RecoveryState, RecoveryError> {
    let state = lock.state();
    let token = token::probe(state).map_err(RecoveryError::Token)?;
    if token == TokenState::Held {
        return Ok(RecoveryState::LiveAbandoned(AbandonedEvidence::TokenHeld));
    }
    let no_workload = |basis| RecoveryState::NoWorkload(QuiescenceProof::new(state.key(), basis));
    match record::read(state).map_err(RecoveryError::Record)? {
        RunRecord::Absent | RunRecord::Quiescent => {
            Ok(no_workload(QuiescenceBasis::NoInterruptedRun))
        }
        RunRecord::Running { cgroup: None } => Ok(RecoveryState::Unresolved(
            UnresolvedReason::InterruptedRunTokenFree,
        )),
        RunRecord::Running { cgroup: Some(path) } => classify_cgroup(path, no_workload),
    }
}

#[cfg(target_os = "linux")]
fn classify_cgroup(
    path: PathBuf,
    no_workload: impl FnOnce(QuiescenceBasis) -> RecoveryState,
) -> Result<RecoveryState, RecoveryError> {
    use crate::linux::{CgroupPopulation, population_at};
    match population_at(&path) {
        Ok(CgroupPopulation::Populated) => Ok(RecoveryState::LiveAbandoned(
            AbandonedEvidence::CgroupPopulated,
        )),
        Ok(CgroupPopulation::Empty | CgroupPopulation::Removed) => {
            Ok(no_workload(QuiescenceBasis::CgroupEmpty))
        }
        Err(error) => Err(RecoveryError::Cgroup { path, error }),
    }
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::unnecessary_wraps)]
fn classify_cgroup(
    _path: PathBuf,
    _no_workload: impl FnOnce(QuiescenceBasis) -> RecoveryState,
) -> Result<RecoveryState, RecoveryError> {
    Ok(RecoveryState::Unresolved(
        UnresolvedReason::CgroupUnsupported,
    ))
}

#[derive(Debug)]
pub enum RecoveryError {
    Record(RecordError),
    Token(TokenError),
    Cgroup { path: PathBuf, error: io::Error },
}

impl fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Record(error) => error.fmt(f),
            Self::Token(error) => error.fmt(f),
            Self::Cgroup { path, error } => {
                write!(f, "cannot inspect cgroup {}: {error}", path.display())
            }
        }
    }
}

impl Error for RecoveryError {}
