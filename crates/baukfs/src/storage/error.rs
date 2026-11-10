use std::error::Error;
use std::fmt;
use std::io;
use std::path::PathBuf;

use crate::{ContentId, WorkspaceId};

use super::{CURRENT_SCHEMA_VERSION, DatabaseState, Unrecognized};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriverStep {
    Open,
    Connect,
    SetBusyTimeout,
    ReadState,
    Begin,
    Bootstrap,
    Upgrade,
    InsertContent,
    ReadContent,
    History,
    Commit,
    Rollback,
}

impl fmt::Display for DriverStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Open => "open database",
            Self::Connect => "connect",
            Self::SetBusyTimeout => "set busy timeout",
            Self::ReadState => "read database state",
            Self::Begin => "begin immediate transaction",
            Self::Bootstrap => "apply bootstrap schema",
            Self::Upgrade => "apply schema upgrade",
            Self::InsertContent => "insert content",
            Self::ReadContent => "read content",
            Self::History => "workspace history",
            Self::Commit => "commit",
            Self::Rollback => "roll back",
        })
    }
}

/// Classification of a driver failure by its error variant. Diagnostic
/// messages are never inspected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriverErrorKind {
    Busy,
    BusySnapshot,
    Interrupt,
    Constraint,
    ReadOnly,
    DatabaseFull,
    NotADatabase,
    Corrupt,
    Io(io::ErrorKind),
    Misuse,
    Conversion,
    Batch,
    Other,
}

impl DriverErrorKind {
    fn of(error: &turso::Error) -> Self {
        match error {
            turso::Error::Busy(_) => Self::Busy,
            turso::Error::BusySnapshot(_) => Self::BusySnapshot,
            turso::Error::Interrupt(_) => Self::Interrupt,
            turso::Error::Constraint(_) => Self::Constraint,
            turso::Error::Readonly(_) => Self::ReadOnly,
            turso::Error::DatabaseFull(_) => Self::DatabaseFull,
            turso::Error::NotAdb(_) => Self::NotADatabase,
            turso::Error::Corrupt(_) => Self::Corrupt,
            turso::Error::IoError(kind, _) => Self::Io(*kind),
            turso::Error::Misuse(_) => Self::Misuse,
            turso::Error::ToSqlConversionFailure(_) | turso::Error::ConversionFailure(_) => {
                Self::Conversion
            }
            turso::Error::BatchStatementFailed { .. }
            | turso::Error::BatchRollbackFailed { .. } => Self::Batch,
            turso::Error::QueryReturnedNoRows | turso::Error::Error(_) => Self::Other,
        }
    }

    /// Whether the failure was lock contention: `Busy`, or a read snapshot
    /// that could not be upgraded to a write (`BusySnapshot`).
    #[must_use]
    pub fn is_busy(self) -> bool {
        matches!(self, Self::Busy | Self::BusySnapshot)
    }
}

#[derive(Debug)]
pub struct DriverError {
    step: DriverStep,
    kind: DriverErrorKind,
    error: turso::Error,
}

impl DriverError {
    pub(super) fn new(step: DriverStep, error: turso::Error) -> Self {
        Self {
            step,
            kind: DriverErrorKind::of(&error),
            error,
        }
    }

    #[must_use]
    pub fn step(&self) -> DriverStep {
        self.step
    }

    #[must_use]
    pub fn kind(&self) -> DriverErrorKind {
        self.kind
    }
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.step, self.error)
    }
}

impl Error for DriverError {}

/// How a stored content record disagrees with its identifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntegrityViolation {
    /// The row does not hold an integer length and a BLOB.
    MalformedRecord,
    /// The recorded byte length differs from the length of the stored bytes.
    LengthMismatch { recorded: i64, stored: usize },
    /// The stored bytes hash to a different identifier.
    DigestMismatch { computed: ContentId },
    /// The stored bytes hash to the identifier but differ from the bytes
    /// being inserted. Only a BLAKE3 collision produces this.
    ConflictingContents,
}

impl fmt::Display for IntegrityViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedRecord => f.write_str("malformed content record"),
            Self::LengthMismatch { recorded, stored } => write!(
                f,
                "recorded length {recorded} differs from stored length {stored}"
            ),
            Self::DigestMismatch { computed } => {
                write!(f, "stored bytes hash to {computed}")
            }
            Self::ConflictingContents => f.write_str("stored bytes differ from the inserted bytes"),
        }
    }
}

/// A stored content record that does not match its identifier. Its bytes are
/// never returned as valid content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrityError {
    id: ContentId,
    violation: IntegrityViolation,
}

impl IntegrityError {
    pub(super) fn new(id: ContentId, violation: IntegrityViolation) -> Self {
        Self { id, violation }
    }

    #[must_use]
    pub fn id(&self) -> &ContentId {
        &self.id
    }

    #[must_use]
    pub fn violation(&self) -> &IntegrityViolation {
        &self.violation
    }
}

impl fmt::Display for IntegrityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "content {}: {}", self.id, self.violation)
    }
}

impl Error for IntegrityError {}

/// Why a write transaction was being rolled back.
#[derive(Debug)]
pub enum Abandoned {
    /// The state found under the write lock did not allow the operation.
    /// `Current` with the requested identity means another initializer or
    /// upgrader won.
    Found(DatabaseState),
    Failed(DriverError),
    Integrity(IntegrityError),
    History(super::history::HistoryError),
}

impl fmt::Display for Abandoned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Found(state) => write!(f, "found {state} under the write lock"),
            Self::Failed(error) => write!(f, "{error}"),
            Self::Integrity(error) => write!(f, "{error}"),
            Self::History(error) => write!(f, "{error}"),
        }
    }
}

#[derive(Debug)]
pub enum StorageError {
    ConnectionDiscarded,
    NonUtf8Path {
        path: PathBuf,
    },
    Driver(DriverError),
    Missing,
    Uninitialized,
    /// The database is at an older schema version. Run
    /// [`super::WorkspaceStore::upgrade`].
    UpgradeRequired {
        found: i64,
    },
    UnsupportedSchemaVersion {
        found: i64,
    },
    Unrecognized(Unrecognized),
    WorkspaceMismatch {
        requested: WorkspaceId,
        stored: WorkspaceId,
    },
    Integrity(IntegrityError),
    History(super::history::HistoryError),
    /// A transaction was abandoned and the rollback also failed.
    /// The connection was discarded.
    RollbackFailed {
        abandoned: Abandoned,
        rollback: DriverError,
    },
}

impl StorageError {
    /// The primary driver failure, if this error has one. For
    /// `RollbackFailed` this is the failure that caused the rollback, not the
    /// rollback failure itself.
    #[must_use]
    pub fn driver_error(&self) -> Option<&DriverError> {
        match self {
            Self::Driver(error)
            | Self::RollbackFailed {
                abandoned: Abandoned::Failed(error),
                ..
            } => Some(error),
            Self::ConnectionDiscarded
            | Self::History(_)
            | Self::NonUtf8Path { .. }
            | Self::Missing
            | Self::Uninitialized
            | Self::UpgradeRequired { .. }
            | Self::UnsupportedSchemaVersion { .. }
            | Self::Unrecognized(_)
            | Self::WorkspaceMismatch { .. }
            | Self::Integrity(_)
            | Self::RollbackFailed {
                abandoned: Abandoned::Found(_) | Abandoned::Integrity(_) | Abandoned::History(_),
                ..
            } => None,
        }
    }

    pub(super) fn rejecting(state: DatabaseState, requested: &WorkspaceId) -> Self {
        match state {
            DatabaseState::Missing => Self::Missing,
            DatabaseState::Uninitialized => Self::Uninitialized,
            DatabaseState::Current(stored)
            | DatabaseState::UpgradeRequired {
                workspace_id: stored,
                ..
            } if stored != *requested => Self::WorkspaceMismatch {
                requested: requested.clone(),
                stored,
            },
            DatabaseState::UpgradeRequired { found, .. } => Self::UpgradeRequired { found },
            DatabaseState::Current(stored) => Self::WorkspaceMismatch {
                requested: requested.clone(),
                stored,
            },
            DatabaseState::UnsupportedSchemaVersion { found } => {
                Self::UnsupportedSchemaVersion { found }
            }
            DatabaseState::Unrecognized(reason) => Self::Unrecognized(reason),
        }
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConnectionDiscarded => {
                f.write_str("connection discarded; reopen and reconcile history")
            }
            Self::NonUtf8Path { path } => {
                write!(f, "database path is not UTF-8: {}", path.display())
            }
            Self::Driver(error) => write!(f, "{error}"),
            Self::Missing => f.write_str("database does not exist"),
            Self::Uninitialized => f.write_str("database is not initialized"),
            Self::UpgradeRequired { found } => write!(
                f,
                "database schema version {found} must be upgraded to {CURRENT_SCHEMA_VERSION}"
            ),
            Self::UnsupportedSchemaVersion { found } => write!(
                f,
                "database schema version {found} is not supported (supported: {CURRENT_SCHEMA_VERSION})"
            ),
            Self::Unrecognized(reason) => write!(f, "unrecognized database: {reason}"),
            Self::Integrity(error) => write!(f, "{error}"),
            Self::History(error) => write!(f, "{error}"),
            Self::WorkspaceMismatch { requested, stored } => write!(
                f,
                "database belongs to workspace {:?}, not {:?}",
                stored.as_str(),
                requested.as_str()
            ),
            Self::RollbackFailed {
                abandoned,
                rollback,
            } => write!(
                f,
                "transaction abandoned ({abandoned}), then rollback failed: {rollback}"
            ),
        }
    }
}

impl Error for StorageError {}
