mod connection;
mod content;
mod error;
mod schema;
#[cfg(test)]
mod tests;

use std::path::Path;

use turso::Connection;
use turso::transaction::TransactionBehavior;

use crate::{ContentId, WorkspaceId};

use connection::{Access, driver, open};
use content::Read;
use schema::{SCHEMA, Schema, apply_steps, read_state};

pub use connection::BusyTimeout;
pub use content::InsertOutcome;
pub use error::{
    Abandoned, DriverError, DriverErrorKind, DriverStep, IntegrityError, IntegrityViolation,
    StorageError,
};
pub use schema::{CURRENT_SCHEMA_VERSION, DatabaseState, Unrecognized};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateOutcome {
    /// This call initialized the database.
    Created,
    /// The read-only check found the database already initialized for this
    /// workspace, so no write lock was taken.
    AlreadyInitialized,
    /// Another initializer for the same workspace committed while this call
    /// waited for the write lock.
    InitializedConcurrently,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpgradeOutcome {
    /// This call upgraded the database from version `from`.
    Upgraded { from: i64 },
    /// The read-only check found the database already current, so no write
    /// lock was taken.
    AlreadyCurrent,
    /// Another upgrader committed while this call waited for the write lock.
    UpgradedConcurrently,
}

/// An open, current database for one workspace copy.
#[derive(Debug)]
pub struct WorkspaceStore {
    connection: Connection,
    workspace_id: WorkspaceId,
    busy_timeout: BusyTimeout,
}

impl WorkspaceStore {
    /// Creates and initializes the database at `path` for `workspace_id`, or
    /// opens it if it is already initialized for that workspace. The parent
    /// directory must exist.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::WorkspaceMismatch`] if the database belongs to
    /// another workspace, [`StorageError::UpgradeRequired`],
    /// [`StorageError::UnsupportedSchemaVersion`], or
    /// [`StorageError::Unrecognized`] if it holds anything other than an
    /// empty or current `BaukFS` database, [`StorageError::Driver`] for driver
    /// failures (including lock contention), and
    /// [`StorageError::RollbackFailed`] if abandoning initialization failed.
    pub async fn create(
        path: &Path,
        workspace_id: &WorkspaceId,
        busy_timeout: BusyTimeout,
    ) -> Result<(Self, CreateOutcome), StorageError> {
        match inspect(path, busy_timeout).await? {
            DatabaseState::Missing | DatabaseState::Uninitialized => {}
            DatabaseState::Current(stored) if stored == *workspace_id => {
                let store = Self::open_current(path, workspace_id, busy_timeout).await?;
                return Ok((store, CreateOutcome::AlreadyInitialized));
            }
            state => return Err(StorageError::rejecting(state, workspace_id)),
        }
        let mut connection = open(path, Access::ReadWrite, busy_timeout).await?;
        let outcome = initialize(&mut connection, workspace_id, SCHEMA).await?;
        let store = Self::verified(connection, workspace_id, busy_timeout).await?;
        Ok((store, outcome))
    }

    /// Opens an existing database that is current and belongs to
    /// `workspace_id`. Never creates files or directories, runs DDL, or
    /// writes metadata.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Missing`], [`StorageError::Uninitialized`],
    /// [`StorageError::UpgradeRequired`],
    /// [`StorageError::UnsupportedSchemaVersion`], or
    /// [`StorageError::Unrecognized`] if the database is not current,
    /// [`StorageError::WorkspaceMismatch`] if it belongs to another
    /// workspace, and [`StorageError::Driver`] for driver failures.
    pub async fn open_existing(
        path: &Path,
        workspace_id: &WorkspaceId,
        busy_timeout: BusyTimeout,
    ) -> Result<Self, StorageError> {
        match inspect(path, busy_timeout).await? {
            DatabaseState::Current(stored) if stored == *workspace_id => {
                Self::open_current(path, workspace_id, busy_timeout).await
            }
            state => Err(StorageError::rejecting(state, workspace_id)),
        }
    }

    /// Upgrades an existing database for `workspace_id` from an older schema
    /// version to [`CURRENT_SCHEMA_VERSION`] and opens it. Every pending step
    /// and the version change commit together or not at all. Never creates
    /// or initializes a database.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Missing`], [`StorageError::Uninitialized`],
    /// [`StorageError::UnsupportedSchemaVersion`], or
    /// [`StorageError::Unrecognized`] if the database is not an older or
    /// current `BaukFS` database, [`StorageError::WorkspaceMismatch`] if it
    /// belongs to another workspace, [`StorageError::Driver`] for driver
    /// failures (a failed step is reported at [`DriverStep::Upgrade`] after
    /// a successful rollback), and [`StorageError::RollbackFailed`] if
    /// abandoning the upgrade failed.
    pub async fn upgrade(
        path: &Path,
        workspace_id: &WorkspaceId,
        busy_timeout: BusyTimeout,
    ) -> Result<(Self, UpgradeOutcome), StorageError> {
        match inspect(path, busy_timeout).await? {
            DatabaseState::UpgradeRequired {
                workspace_id: stored,
                ..
            } if stored == *workspace_id => {}
            DatabaseState::Current(stored) if stored == *workspace_id => {
                let store = Self::open_current(path, workspace_id, busy_timeout).await?;
                return Ok((store, UpgradeOutcome::AlreadyCurrent));
            }
            state => return Err(StorageError::rejecting(state, workspace_id)),
        }
        let mut connection = open(path, Access::ReadWrite, busy_timeout).await?;
        let outcome = upgrade(&mut connection, workspace_id, SCHEMA).await?;
        let store = Self::verified(connection, workspace_id, busy_timeout).await?;
        Ok((store, outcome))
    }

    #[must_use]
    pub fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }

    #[must_use]
    pub fn busy_timeout(&self) -> BusyTimeout {
        self.busy_timeout
    }

    /// Reads the database state again through this store's connection,
    /// without writing.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Driver`] for driver failures.
    pub async fn read_state(&self) -> Result<DatabaseState, StorageError> {
        read_state(&self.connection)
            .await
            .map_err(StorageError::Driver)
    }

    /// Stores `bytes` under their [`ContentId`]. Inserting bytes that are
    /// already stored leaves the existing record untouched; an existing
    /// record is never overwritten.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Integrity`] if a record already stored under
    /// the same identifier is damaged or holds different bytes,
    /// [`StorageError::Driver`] for driver failures (including lock
    /// contention at [`DriverStep::Begin`]), and
    /// [`StorageError::RollbackFailed`] if abandoning the insert failed.
    pub async fn insert_content(
        &mut self,
        bytes: &[u8],
    ) -> Result<(ContentId, InsertOutcome), StorageError> {
        let id = ContentId::of(bytes);
        let settled = immediate(&mut self.connection, async |connection| {
            content::insert_under_lock(connection, &id, bytes).await
        })
        .await?;
        match settled {
            Settled::Committed(outcome) => Ok((id, outcome)),
            Settled::RolledBack(abandoned) => Err(rolled_back(abandoned, &self.workspace_id)),
        }
    }

    /// Reads the bytes stored under `id`, or `None` if nothing is stored
    /// under it. Bytes are returned only after their recorded length and
    /// recomputed digest match.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Integrity`] for a damaged record and
    /// [`StorageError::Driver`] for driver failures.
    pub async fn read_content(&self, id: &ContentId) -> Result<Option<Vec<u8>>, StorageError> {
        content::read(&self.connection, id, DriverStep::ReadContent)
            .await
            .map_err(|failure| match failure {
                Read::Failed(error) => StorageError::Driver(error),
                Read::Integrity(error) => StorageError::Integrity(error),
            })
    }

    /// Opens read-write a database that a read-only check just found
    /// current. The driver cannot open read-write without creating, so a file
    /// removed in between is recreated empty and then rejected here as
    /// uninitialized.
    async fn open_current(
        path: &Path,
        workspace_id: &WorkspaceId,
        busy_timeout: BusyTimeout,
    ) -> Result<Self, StorageError> {
        let connection = open(path, Access::ReadWrite, busy_timeout).await?;
        Self::verified(connection, workspace_id, busy_timeout).await
    }

    async fn verified(
        connection: Connection,
        workspace_id: &WorkspaceId,
        busy_timeout: BusyTimeout,
    ) -> Result<Self, StorageError> {
        match read_state(&connection)
            .await
            .map_err(StorageError::Driver)?
        {
            DatabaseState::Current(stored) if stored == *workspace_id => Ok(Self {
                connection,
                workspace_id: stored,
                busy_timeout,
            }),
            state => Err(StorageError::rejecting(state, workspace_id)),
        }
    }
}

/// Reports the state of the database at `path` through a read-only open.
/// Never creates files or directories, takes a write lock, or writes.
///
/// # Errors
///
/// Returns [`StorageError::NonUtf8Path`] for a non-UTF-8 path and
/// [`StorageError::Driver`] for driver failures other than a missing file or
/// a file that is not a database.
pub async fn inspect(
    path: &Path,
    busy_timeout: BusyTimeout,
) -> Result<DatabaseState, StorageError> {
    let connection = match open(path, Access::ReadOnly, busy_timeout).await {
        Ok(connection) => connection,
        Err(StorageError::Driver(error)) => {
            return match error.kind() {
                DriverErrorKind::Io(std::io::ErrorKind::NotFound)
                    if error.step() == DriverStep::Open =>
                {
                    Ok(DatabaseState::Missing)
                }
                DriverErrorKind::NotADatabase => {
                    Ok(DatabaseState::Unrecognized(Unrecognized::NotADatabase))
                }
                _ => Err(StorageError::Driver(error)),
            };
        }
        Err(error) => return Err(error),
    };
    read_state(&connection).await.map_err(StorageError::Driver)
}

enum Settled<T> {
    Committed(T),
    /// The work was abandoned and the rollback succeeded.
    RolledBack(Abandoned),
}

/// Runs `work` under a checked `BEGIN IMMEDIATE` transaction and ends it
/// with an explicit `COMMIT` when `work` succeeds or `ROLLBACK` when it is
/// abandoned.
async fn immediate<T>(
    connection: &mut Connection,
    work: impl AsyncFnOnce(&Connection) -> Result<T, Abandoned>,
) -> Result<Settled<T>, StorageError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await
        .map_err(|error| driver(DriverStep::Begin, error))?;
    match work(&transaction).await {
        Ok(value) => {
            transaction
                .commit()
                .await
                .map_err(|error| driver(DriverStep::Commit, error))?;
            Ok(Settled::Committed(value))
        }
        Err(abandoned) => match transaction.rollback().await {
            Ok(()) => Ok(Settled::RolledBack(abandoned)),
            Err(error) => Err(StorageError::RollbackFailed {
                abandoned,
                rollback: DriverError::new(DriverStep::Rollback, error),
            }),
        },
    }
}

/// Initializes an empty database with every step of `schema`, rechecking the
/// state once the write lock is held. Commits only after the database reads
/// back as current for `workspace_id`.
async fn initialize(
    connection: &mut Connection,
    workspace_id: &WorkspaceId,
    schema: Schema<'_>,
) -> Result<CreateOutcome, StorageError> {
    let settled = immediate(connection, async |connection| {
        match read_state(connection).await.map_err(Abandoned::Failed)? {
            DatabaseState::Uninitialized => {}
            state => return Err(Abandoned::Found(state)),
        }
        apply_steps(
            connection,
            schema.creation_steps(),
            CURRENT_SCHEMA_VERSION,
            workspace_id,
            DriverStep::Bootstrap,
        )
        .await
        .map_err(Abandoned::Failed)?;
        read_back_current(connection, workspace_id).await
    })
    .await?;
    match settled {
        Settled::Committed(()) => Ok(CreateOutcome::Created),
        Settled::RolledBack(Abandoned::Found(DatabaseState::Current(stored)))
            if stored == *workspace_id =>
        {
            Ok(CreateOutcome::InitializedConcurrently)
        }
        Settled::RolledBack(abandoned) => Err(rolled_back(abandoned, workspace_id)),
    }
}

/// Applies the pending upgrade steps of `schema`, rechecking the version once
/// the write lock is held. Commits only after the database reads back as
/// current for `workspace_id`.
async fn upgrade(
    connection: &mut Connection,
    workspace_id: &WorkspaceId,
    schema: Schema<'_>,
) -> Result<UpgradeOutcome, StorageError> {
    let settled = immediate(connection, async |connection| {
        let state = read_state(connection).await.map_err(Abandoned::Failed)?;
        let from = match &state {
            DatabaseState::UpgradeRequired {
                found,
                workspace_id: stored,
            } if stored == workspace_id => *found,
            _ => return Err(Abandoned::Found(state)),
        };
        let Some(steps) = schema.upgrade_steps(from) else {
            return Err(Abandoned::Found(state));
        };
        apply_steps(
            connection,
            steps,
            CURRENT_SCHEMA_VERSION,
            workspace_id,
            DriverStep::Upgrade,
        )
        .await
        .map_err(Abandoned::Failed)?;
        read_back_current(connection, workspace_id).await?;
        Ok(from)
    })
    .await?;
    match settled {
        Settled::Committed(from) => Ok(UpgradeOutcome::Upgraded { from }),
        Settled::RolledBack(Abandoned::Found(DatabaseState::Current(stored)))
            if stored == *workspace_id =>
        {
            Ok(UpgradeOutcome::UpgradedConcurrently)
        }
        Settled::RolledBack(abandoned) => Err(rolled_back(abandoned, workspace_id)),
    }
}

async fn read_back_current(
    connection: &Connection,
    workspace_id: &WorkspaceId,
) -> Result<(), Abandoned> {
    match read_state(connection).await.map_err(Abandoned::Failed)? {
        DatabaseState::Current(stored) if stored == *workspace_id => Ok(()),
        state => Err(Abandoned::Found(state)),
    }
}

fn rolled_back(abandoned: Abandoned, workspace_id: &WorkspaceId) -> StorageError {
    match abandoned {
        Abandoned::Found(state) => StorageError::rejecting(state, workspace_id),
        Abandoned::Failed(error) => StorageError::Driver(error),
        Abandoned::Integrity(error) => StorageError::Integrity(error),
    }
}
