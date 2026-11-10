mod connection;
mod error;
mod schema;
#[cfg(test)]
mod tests;

use std::path::Path;

use turso::Connection;
use turso::transaction::TransactionBehavior;

use crate::WorkspaceId;

use connection::{Access, driver, open};
use schema::{BOOTSTRAP, BootstrapStep, apply_step, read_state};

pub use connection::BusyTimeout;
pub use error::{Abandoned, DriverError, DriverErrorKind, DriverStep, StorageError};
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
    /// another workspace, [`StorageError::UnsupportedSchemaVersion`] or
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
        let outcome = initialize(&mut connection, workspace_id, &BOOTSTRAP).await?;
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

/// Initializes under a checked `BEGIN IMMEDIATE` transaction, rechecking the
/// state once the write lock is held. Commits only after the bootstrap reads
/// back as current for `workspace_id`; every other path rolls back.
async fn initialize(
    connection: &mut Connection,
    workspace_id: &WorkspaceId,
    bootstrap: &[BootstrapStep],
) -> Result<CreateOutcome, StorageError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await
        .map_err(|error| driver(DriverStep::Begin, error))?;
    match bootstrap_under_lock(&transaction, workspace_id, bootstrap).await {
        Ok(()) => {
            transaction
                .commit()
                .await
                .map_err(|error| driver(DriverStep::Commit, error))?;
            Ok(CreateOutcome::Created)
        }
        Err(abandoned) => match transaction.rollback().await {
            Ok(()) => match abandoned {
                Abandoned::Found(DatabaseState::Current(stored)) if stored == *workspace_id => {
                    Ok(CreateOutcome::InitializedConcurrently)
                }
                Abandoned::Found(state) => Err(StorageError::rejecting(state, workspace_id)),
                Abandoned::Failed(error) => Err(StorageError::Driver(error)),
            },
            Err(error) => Err(StorageError::RollbackFailed {
                abandoned,
                rollback: DriverError::new(DriverStep::Rollback, error),
            }),
        },
    }
}

async fn bootstrap_under_lock(
    connection: &Connection,
    workspace_id: &WorkspaceId,
    bootstrap: &[BootstrapStep],
) -> Result<(), Abandoned> {
    match read_state(connection).await.map_err(Abandoned::Failed)? {
        DatabaseState::Uninitialized => {}
        state => return Err(Abandoned::Found(state)),
    }
    for step in bootstrap {
        apply_step(connection, *step, workspace_id)
            .await
            .map_err(Abandoned::Failed)?;
    }
    match read_state(connection).await.map_err(Abandoned::Failed)? {
        DatabaseState::Current(stored) if stored == *workspace_id => Ok(()),
        state => Err(Abandoned::Found(state)),
    }
}
