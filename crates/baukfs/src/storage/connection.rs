use std::path::Path;
use std::time::{Duration, Instant};

use turso::{Builder, Connection};

use super::error::{DriverError, DriverStep, StorageError};

/// How long one connection waits for a single contended lock before the
/// driver reports `Busy`. Opening also retries for up to this long while
/// another process initializes the shared WAL coordination file.
///
/// This is a per-wait policy, not an operation deadline: one operation can
/// wait once for every lock it acquires, and the driver spins a CPU core
/// while it waits. Callers that need a bound on a whole operation must impose
/// it themselves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BusyTimeout(Duration);

impl BusyTimeout {
    /// Report `Busy` immediately instead of waiting.
    pub const IMMEDIATE: Self = Self(Duration::ZERO);

    #[must_use]
    pub const fn new(wait: Duration) -> Self {
        Self(wait)
    }

    #[must_use]
    pub const fn as_duration(self) -> Duration {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Access {
    /// Never creates the database file or takes a write lock.
    ReadOnly,
    /// Creates an empty database file if none exists.
    ReadWrite,
}

/// The only way storage opens a database. Every opener of a workspace
/// database must use multiprocess WAL: the driver does not reject mixed
/// modes, and mixing them risks corruption.
///
/// The first multiprocess opener of a database creates its `-tshm`
/// coordination file and locks it before sizing and initializing it. Turso
/// 0.8.3 reports `Corrupt` to any other opener that arrives in that window,
/// so a `Corrupt` open is retried until the busy timeout elapses. A file that
/// is really corrupt still fails, once the timeout is spent.
pub(super) async fn open(
    path: &Path,
    access: Access,
    busy_timeout: BusyTimeout,
) -> Result<Connection, StorageError> {
    let Some(utf8_path) = path.to_str() else {
        return Err(StorageError::NonUtf8Path {
            path: path.to_path_buf(),
        });
    };
    let started = Instant::now();
    let database = loop {
        let opened = Builder::new_local(utf8_path)
            .experimental_multiprocess_wal(true)
            .read_only(access == Access::ReadOnly)
            .build()
            .await;
        match opened {
            Ok(database) => break database,
            Err(turso::Error::Corrupt(_)) if started.elapsed() < busy_timeout.as_duration() => {}
            Err(error) => return Err(driver(DriverStep::Open, error)),
        }
    };
    let connection = database
        .connect()
        .map_err(|error| driver(DriverStep::Connect, error))?;
    connection
        .busy_timeout(busy_timeout.as_duration())
        .map_err(|error| driver(DriverStep::SetBusyTimeout, error))?;
    Ok(connection)
}

pub(super) fn driver(step: DriverStep, error: turso::Error) -> StorageError {
    StorageError::Driver(DriverError::new(step, error))
}
