use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use turso::transaction::{Transaction, TransactionBehavior};
use turso::{Builder, Connection, Database, Value};

pub const CONTENT_TABLE: &str = "probe_content";
pub const HEAD_TABLE: &str = "probe_head";
pub const SCHEMA_VERSION: i64 = 1;

pub const INTERRUPT_CONTENT_KEY: &str = "interrupt-content";
pub const INTERRUPT_HEAD_NAME: &str = "interrupt-head";
pub const INTERRUPT_CONTENT: &[u8] = b"\xffinterrupted\x00\xfe";

const SCHEMA: &str = "\
CREATE TABLE probe_content (
    probe_key TEXT PRIMARY KEY NOT NULL,
    bytes BLOB NOT NULL
) STRICT;
CREATE TABLE probe_head (
    head_name TEXT PRIMARY KEY NOT NULL,
    content_key TEXT NOT NULL
) STRICT;
CREATE TABLE probe_schema_version (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    version INTEGER NOT NULL
) STRICT;
INSERT INTO probe_schema_version (singleton, version) VALUES (1, 1);";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    Default,
    MultiprocessWal,
}

impl OpenMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::MultiprocessWal => "multiprocess",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "default" => Some(Self::Default),
            "multiprocess" => Some(Self::MultiprocessWal),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeConfig {
    pub mode: OpenMode,
    pub busy_timeout: Duration,
}

impl fmt::Display for ProbeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "mode={} busy_timeout_ms={}",
            self.mode.as_str(),
            self.busy_timeout.as_millis()
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Begin,
    Commit,
    Rollback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
    Null,
    Integer,
    Real,
    Text,
    Blob,
}

impl ValueKind {
    fn of(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Integer(_) => Self::Integer,
            Value::Real(_) => Self::Real,
            Value::Text(_) => Self::Text,
            Value::Blob(_) => Self::Blob,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriverErrorKind {
    ToSqlConversionFailure,
    QueryReturnedNoRows,
    ConversionFailure,
    Busy,
    BusySnapshot,
    Interrupt,
    Error,
    Misuse,
    Constraint,
    Readonly,
    DatabaseFull,
    NotAdb,
    Corrupt,
    IoError,
    BatchStatementFailed,
    BatchRollbackFailed,
}

impl DriverErrorKind {
    #[must_use]
    pub fn of(error: &turso::Error) -> Self {
        match error {
            turso::Error::ToSqlConversionFailure(_) => Self::ToSqlConversionFailure,
            turso::Error::QueryReturnedNoRows => Self::QueryReturnedNoRows,
            turso::Error::ConversionFailure(_) => Self::ConversionFailure,
            turso::Error::Busy(_) => Self::Busy,
            turso::Error::BusySnapshot(_) => Self::BusySnapshot,
            turso::Error::Interrupt(_) => Self::Interrupt,
            turso::Error::Error(_) => Self::Error,
            turso::Error::Misuse(_) => Self::Misuse,
            turso::Error::Constraint(_) => Self::Constraint,
            turso::Error::Readonly(_) => Self::Readonly,
            turso::Error::DatabaseFull(_) => Self::DatabaseFull,
            turso::Error::NotAdb(_) => Self::NotAdb,
            turso::Error::Corrupt(_) => Self::Corrupt,
            turso::Error::IoError(..) => Self::IoError,
            turso::Error::BatchStatementFailed { .. } => Self::BatchStatementFailed,
            turso::Error::BatchRollbackFailed { .. } => Self::BatchRollbackFailed,
        }
    }
}

#[derive(Debug)]
pub enum ProbeError {
    NonUtf8Path(PathBuf),
    Open {
        path: PathBuf,
        error: turso::Error,
    },
    Connect(turso::Error),
    BusyTimeout(turso::Error),
    Schema(turso::Error),
    UnsupportedSchemaVersion(i64),
    Transaction {
        step: Step,
        error: turso::Error,
    },
    Write {
        table: &'static str,
        error: turso::Error,
    },
    Read {
        query: &'static str,
        error: turso::Error,
    },
    UnexpectedValue {
        query: &'static str,
        column: usize,
        found: ValueKind,
    },
    UnexpectedRecord {
        table: &'static str,
    },
}

impl ProbeError {
    #[must_use]
    pub fn driver_error(&self) -> Option<&turso::Error> {
        match self {
            Self::Open { error, .. }
            | Self::Connect(error)
            | Self::BusyTimeout(error)
            | Self::Schema(error)
            | Self::Transaction { error, .. }
            | Self::Write { error, .. }
            | Self::Read { error, .. } => Some(error),
            Self::NonUtf8Path(_)
            | Self::UnsupportedSchemaVersion(_)
            | Self::UnexpectedValue { .. }
            | Self::UnexpectedRecord { .. } => None,
        }
    }

    #[must_use]
    pub fn driver_kind(&self) -> Option<DriverErrorKind> {
        self.driver_error().map(DriverErrorKind::of)
    }
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonUtf8Path(path) => write!(f, "database path is not UTF-8: {}", path.display()),
            Self::Open { path, error } => write!(f, "open {}: {error}", path.display()),
            Self::Connect(error) => write!(f, "connect: {error}"),
            Self::BusyTimeout(error) => write!(f, "set busy timeout: {error}"),
            Self::Schema(error) => write!(f, "create probe schema: {error}"),
            Self::UnsupportedSchemaVersion(version) => {
                write!(f, "unsupported probe schema version {version}")
            }
            Self::Transaction { step, error } => write!(f, "{step:?} transaction: {error}"),
            Self::Write { table, error } => write!(f, "write {table}: {error}"),
            Self::Read { query, error } => write!(f, "read {query}: {error}"),
            Self::UnexpectedValue {
                query,
                column,
                found,
            } => write!(f, "{query}: column {column} holds unexpected {found:?}"),
            Self::UnexpectedRecord { table } => {
                write!(f, "{table}: probe record has unexpected contents")
            }
        }
    }
}

impl Error for ProbeError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Readiness {
    Uninitialized,
    Current,
    Version(i64),
}

impl fmt::Display for Readiness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Uninitialized => f.write_str("uninitialized"),
            Self::Current => f.write_str("current"),
            Self::Version(version) => write!(f, "version-{version}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitOutcome {
    AlreadyCurrent,
    CurrentAfterLock,
    Applied,
}

impl fmt::Display for InitOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AlreadyCurrent => "already-current",
            Self::CurrentAfterLock => "current-after-lock",
            Self::Applied => "applied",
        })
    }
}

pub enum ExistingOpen {
    Missing,
    SchemaNotCurrent(Readiness),
    Open {
        database: Database,
        connection: Connection,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Visibility {
    pub content: bool,
    pub head: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurabilitySettings {
    pub journal_mode: String,
    pub synchronous: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredContent {
    pub bytes: Vec<u8>,
    pub sql_type: String,
    pub sql_length: i64,
}

#[must_use]
pub fn binary_cases() -> [(&'static str, Vec<u8>); 6] {
    let every_byte: Vec<u8> = (0..=u8::MAX).collect();
    let large: Vec<u8> = (0..(1_u32 << 20))
        .map(|i| u8::try_from(i.wrapping_mul(2_654_435_761) >> 24).unwrap_or(0))
        .collect();
    [
        ("empty", Vec::new()),
        ("single-nul", Vec::from([0])),
        ("embedded-nul", b"a\0b\0\0c".to_vec()),
        ("invalid-utf8", Vec::from([0xff, 0xfe, 0xc3, 0x28, 0x80])),
        ("every-byte", every_byte),
        ("large-1mib", large),
    ]
}

fn local_builder(path: &Path, mode: OpenMode) -> Result<Builder, ProbeError> {
    let Some(path_str) = path.to_str() else {
        return Err(ProbeError::NonUtf8Path(path.to_path_buf()));
    };
    Ok(Builder::new_local(path_str)
        .experimental_multiprocess_wal(mode == OpenMode::MultiprocessWal))
}

pub async fn open(path: &Path, mode: OpenMode) -> Result<Database, ProbeError> {
    local_builder(path, mode)?
        .build()
        .await
        .map_err(|error| ProbeError::Open {
            path: path.to_path_buf(),
            error,
        })
}

pub fn connect(database: &Database, busy_timeout: Duration) -> Result<Connection, ProbeError> {
    let connection = database.connect().map_err(ProbeError::Connect)?;
    connection
        .busy_timeout(busy_timeout)
        .map_err(ProbeError::BusyTimeout)?;
    Ok(connection)
}

pub async fn open_existing(path: &Path, config: ProbeConfig) -> Result<ExistingOpen, ProbeError> {
    let opened = local_builder(path, config.mode)?
        .read_only(true)
        .build()
        .await;
    let database = match opened {
        Ok(database) => database,
        Err(turso::Error::IoError(io::ErrorKind::NotFound, "open")) => {
            return Ok(ExistingOpen::Missing);
        }
        Err(error) => {
            return Err(ProbeError::Open {
                path: path.to_path_buf(),
                error,
            });
        }
    };
    let connection = connect(&database, config.busy_timeout)?;
    match readiness(&connection).await? {
        Readiness::Current => Ok(ExistingOpen::Open {
            database,
            connection,
        }),
        other => Ok(ExistingOpen::SchemaNotCurrent(other)),
    }
}

pub async fn readiness(connection: &Connection) -> Result<Readiness, ProbeError> {
    const TABLE_QUERY: &str =
        "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = 'probe_schema_version'";
    const VERSION_QUERY: &str = "SELECT version FROM probe_schema_version WHERE singleton = 1";
    match single_value(connection, TABLE_QUERY, ()).await? {
        Some(Value::Integer(0)) => return Ok(Readiness::Uninitialized),
        Some(Value::Integer(_)) => {}
        other => return Err(unexpected(TABLE_QUERY, 0, other.as_ref())),
    }
    match single_value(connection, VERSION_QUERY, ()).await? {
        None => Ok(Readiness::Uninitialized),
        Some(Value::Integer(SCHEMA_VERSION)) => Ok(Readiness::Current),
        Some(Value::Integer(version)) => Ok(Readiness::Version(version)),
        Some(other) => Err(unexpected(VERSION_QUERY, 0, Some(&other))),
    }
}

pub async fn apply_schema(connection: &Connection) -> Result<(), ProbeError> {
    connection
        .execute_batch(SCHEMA)
        .await
        .map_err(ProbeError::Schema)
}

pub async fn initialize_under_lock(connection: &Connection) -> Result<InitOutcome, ProbeError> {
    let transaction = begin_immediate_unchecked(connection).await?;
    let outcome = match readiness(&transaction).await {
        Ok(Readiness::Current) => Ok(InitOutcome::CurrentAfterLock),
        Ok(Readiness::Uninitialized) => apply_schema(&transaction)
            .await
            .map(|()| InitOutcome::Applied),
        Ok(Readiness::Version(version)) => Err(ProbeError::UnsupportedSchemaVersion(version)),
        Err(error) => Err(error),
    };
    match outcome {
        Ok(InitOutcome::Applied) => {
            commit(transaction).await?;
            Ok(InitOutcome::Applied)
        }
        Ok(other) => {
            rollback(transaction).await?;
            Ok(other)
        }
        Err(error) => {
            let _ = rollback(transaction).await;
            Err(error)
        }
    }
}

pub async fn ensure_initialized(connection: &Connection) -> Result<InitOutcome, ProbeError> {
    match readiness(connection).await? {
        Readiness::Current => Ok(InitOutcome::AlreadyCurrent),
        Readiness::Version(version) => Err(ProbeError::UnsupportedSchemaVersion(version)),
        Readiness::Uninitialized => initialize_under_lock(connection).await,
    }
}

pub async fn schema_cookie(connection: &Connection) -> Result<i64, ProbeError> {
    const QUERY: &str = "PRAGMA schema_version";
    match single_value(connection, QUERY, ()).await? {
        Some(Value::Integer(cookie)) => Ok(cookie),
        other => Err(unexpected(QUERY, 0, other.as_ref())),
    }
}

fn begin_error(error: turso::Error) -> ProbeError {
    ProbeError::Transaction {
        step: Step::Begin,
        error,
    }
}

pub async fn begin_immediate(connection: &mut Connection) -> Result<Transaction<'_>, ProbeError> {
    connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await
        .map_err(begin_error)
}

pub async fn begin_immediate_unchecked(
    connection: &Connection,
) -> Result<Transaction<'_>, ProbeError> {
    Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
        .await
        .map_err(begin_error)
}

pub async fn commit(transaction: Transaction<'_>) -> Result<(), ProbeError> {
    transaction
        .commit()
        .await
        .map_err(|error| ProbeError::Transaction {
            step: Step::Commit,
            error,
        })
}

pub async fn rollback(transaction: Transaction<'_>) -> Result<(), ProbeError> {
    transaction
        .rollback()
        .await
        .map_err(|error| ProbeError::Transaction {
            step: Step::Rollback,
            error,
        })
}

pub async fn put_content(
    connection: &Connection,
    key: &str,
    bytes: &[u8],
) -> Result<(), ProbeError> {
    connection
        .execute(
            "INSERT INTO probe_content (probe_key, bytes) VALUES (?1, ?2)",
            (key, bytes),
        )
        .await
        .map(drop)
        .map_err(|error| ProbeError::Write {
            table: CONTENT_TABLE,
            error,
        })
}

pub async fn put_head(
    connection: &Connection,
    name: &str,
    content_key: &str,
) -> Result<(), ProbeError> {
    connection
        .execute(
            "INSERT INTO probe_head (head_name, content_key) VALUES (?1, ?2)",
            (name, content_key),
        )
        .await
        .map(drop)
        .map_err(|error| ProbeError::Write {
            table: HEAD_TABLE,
            error,
        })
}

pub async fn put_interrupt_pair(connection: &Connection) -> Result<(), ProbeError> {
    put_content(connection, INTERRUPT_CONTENT_KEY, INTERRUPT_CONTENT).await?;
    put_head(connection, INTERRUPT_HEAD_NAME, INTERRUPT_CONTENT_KEY).await
}

pub async fn read_content(
    connection: &Connection,
    key: &str,
) -> Result<Option<StoredContent>, ProbeError> {
    const QUERY: &str =
        "SELECT bytes, typeof(bytes), length(bytes) FROM probe_content WHERE probe_key = ?1";
    let read_error = |error| ProbeError::Read {
        query: QUERY,
        error,
    };
    let mut rows = connection.query(QUERY, (key,)).await.map_err(read_error)?;
    let Some(row) = rows.next().await.map_err(read_error)? else {
        return Ok(None);
    };
    let column = |index| row.get_value(index).map_err(read_error);
    let bytes = match column(0)? {
        Value::Blob(bytes) => bytes,
        other => return Err(unexpected(QUERY, 0, Some(&other))),
    };
    let sql_type = match column(1)? {
        Value::Text(text) => text,
        other => return Err(unexpected(QUERY, 1, Some(&other))),
    };
    let sql_length = match column(2)? {
        Value::Integer(length) => length,
        other => return Err(unexpected(QUERY, 2, Some(&other))),
    };
    Ok(Some(StoredContent {
        bytes,
        sql_type,
        sql_length,
    }))
}

pub async fn read_head(connection: &Connection, name: &str) -> Result<Option<String>, ProbeError> {
    const QUERY: &str = "SELECT content_key FROM probe_head WHERE head_name = ?1";
    match single_value(connection, QUERY, (name,)).await? {
        None => Ok(None),
        Some(Value::Text(content_key)) => Ok(Some(content_key)),
        Some(other) => Err(unexpected(QUERY, 0, Some(&other))),
    }
}

pub async fn interrupt_visibility(connection: &Connection) -> Result<Visibility, ProbeError> {
    let content = read_content(connection, INTERRUPT_CONTENT_KEY).await?;
    let head = read_head(connection, INTERRUPT_HEAD_NAME).await?;
    if content
        .as_ref()
        .is_some_and(|stored| stored.bytes != INTERRUPT_CONTENT)
    {
        return Err(ProbeError::UnexpectedRecord {
            table: CONTENT_TABLE,
        });
    }
    if head
        .as_deref()
        .is_some_and(|key| key != INTERRUPT_CONTENT_KEY)
    {
        return Err(ProbeError::UnexpectedRecord { table: HEAD_TABLE });
    }
    Ok(Visibility {
        content: content.is_some(),
        head: head.is_some(),
    })
}

pub async fn durability_settings(
    connection: &Connection,
) -> Result<DurabilitySettings, ProbeError> {
    const JOURNAL_MODE: &str = "PRAGMA journal_mode";
    const SYNCHRONOUS: &str = "PRAGMA synchronous";
    let journal_mode = match single_value(connection, JOURNAL_MODE, ()).await? {
        Some(Value::Text(mode)) => mode,
        other => return Err(unexpected(JOURNAL_MODE, 0, other.as_ref())),
    };
    let synchronous = match single_value(connection, SYNCHRONOUS, ()).await? {
        Some(Value::Integer(level)) => level,
        other => return Err(unexpected(SYNCHRONOUS, 0, other.as_ref())),
    };
    Ok(DurabilitySettings {
        journal_mode,
        synchronous,
    })
}

fn unexpected(query: &'static str, column: usize, value: Option<&Value>) -> ProbeError {
    ProbeError::UnexpectedValue {
        query,
        column,
        found: value.map_or(ValueKind::Null, ValueKind::of),
    }
}

async fn single_value(
    connection: &Connection,
    query: &'static str,
    params: impl turso::IntoParams,
) -> Result<Option<Value>, ProbeError> {
    let read_error = |error| ProbeError::Read { query, error };
    let mut rows = connection.query(query, params).await.map_err(read_error)?;
    match rows.next().await.map_err(read_error)? {
        Some(row) => row.get_value(0).map(Some).map_err(read_error),
        None => Ok(None),
    }
}
