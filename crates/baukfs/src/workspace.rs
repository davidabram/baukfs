use std::error::Error;
use std::fmt;

use crate::id::{RevisionId, SessionId, WorkspaceId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceStatus {
    Idle,
    Running,
    Sealed,
}

impl fmt::Display for WorkspaceStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Sealed => "sealed",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    BeginRun,
    RecordQuiescence,
    RecordCheckpoint,
    Seal,
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::BeginRun => "begin run",
            Self::RecordQuiescence => "record quiescence",
            Self::RecordCheckpoint => "record checkpoint",
            Self::Seal => "seal",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransitionError {
    pub operation: Operation,
    pub status: WorkspaceStatus,
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot {} while workspace is {}",
            self.operation, self.status
        )
    }
}

impl Error for TransitionError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    id: WorkspaceId,
    session: SessionId,
    base: RevisionId,
    head: RevisionId,
    status: WorkspaceStatus,
}

impl Workspace {
    #[must_use]
    pub fn new(id: WorkspaceId, session: SessionId, base: RevisionId) -> Self {
        Self {
            id,
            session,
            head: base.clone(),
            base,
            status: WorkspaceStatus::Idle,
        }
    }

    #[must_use]
    pub fn id(&self) -> &WorkspaceId {
        &self.id
    }

    #[must_use]
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    #[must_use]
    pub fn base(&self) -> &RevisionId {
        &self.base
    }

    #[must_use]
    pub fn head(&self) -> &RevisionId {
        &self.head
    }

    #[must_use]
    pub fn status(&self) -> WorkspaceStatus {
        self.status
    }

    /// # Errors
    ///
    /// Returns [`TransitionError`] unless the workspace is idle.
    pub fn begin_run(&mut self) -> Result<(), TransitionError> {
        self.require(Operation::BeginRun, WorkspaceStatus::Idle)?;
        self.status = WorkspaceStatus::Running;
        Ok(())
    }

    /// # Errors
    ///
    /// Returns [`TransitionError`] unless the workspace is running.
    pub fn record_quiescence(&mut self) -> Result<(), TransitionError> {
        self.require(Operation::RecordQuiescence, WorkspaceStatus::Running)?;
        self.status = WorkspaceStatus::Idle;
        Ok(())
    }

    /// # Errors
    ///
    /// Returns [`TransitionError`] unless the workspace is idle.
    pub fn record_checkpoint(&mut self, revision: RevisionId) -> Result<(), TransitionError> {
        self.require(Operation::RecordCheckpoint, WorkspaceStatus::Idle)?;
        self.head = revision;
        Ok(())
    }

    /// # Errors
    ///
    /// Returns [`TransitionError`] if the workspace is running.
    pub fn seal(&mut self) -> Result<(), TransitionError> {
        match self.status {
            WorkspaceStatus::Idle | WorkspaceStatus::Sealed => {
                self.status = WorkspaceStatus::Sealed;
                Ok(())
            }
            WorkspaceStatus::Running => Err(TransitionError {
                operation: Operation::Seal,
                status: self.status,
            }),
        }
    }

    fn require(
        &self,
        operation: Operation,
        required: WorkspaceStatus,
    ) -> Result<(), TransitionError> {
        if self.status == required {
            Ok(())
        } else {
            Err(TransitionError {
                operation,
                status: self.status,
            })
        }
    }
}
