use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use turso::transaction::{Transaction, TransactionBehavior};
use turso::{Builder, Connection, Database, Value};

pub const CONTENT_TABLE: &str = "probe_content";
pub const HEAD_TABLE: &str = "probe_head";

pub const INTERRUPT_CONTENT_KEY: &str = "interrupt-content";
pub const INTERRUPT_HEAD_NAME: &str = "interrupt-head";
pub const INTERRUPT_CONTENT: &[u8] = b"\xffinterrupted\x00\xfe";

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS probe_content (
    probe_key TEXT PRIMARY KEY NOT NULL,
    bytes BLOB NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS probe_head (
    head_name TEXT PRIMARY KEY NOT NULL,
    content_key TEXT NOT NULL
) STRICT;";

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

#[derive(Debug)]
pub enum ProbeError {
    NonUtf8Path(PathBuf),
    Open {
        path: PathBuf,
        error: turso::Error,
    },
    Connect(turso::Error),
    Schema(turso::Error),
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

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonUtf8Path(path) => write!(f, "database path is not UTF-8: {}", path.display()),
            Self::Open { path, error } => write!(f, "open {}: {error}", path.display()),
            Self::Connect(error) => write!(f, "connect: {error}"),
            Self::Schema(error) => write!(f, "create probe schema: {error}"),
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

pub async fn open(path: &Path) -> Result<Database, ProbeError> {
    let Some(path_str) = path.to_str() else {
        return Err(ProbeError::NonUtf8Path(path.to_path_buf()));
    };
    Builder::new_local(path_str)
        .build()
        .await
        .map_err(|error| ProbeError::Open {
            path: path.to_path_buf(),
            error,
        })
}

pub fn connect(database: &Database) -> Result<Connection, ProbeError> {
    database.connect().map_err(ProbeError::Connect)
}

pub async fn create_schema(connection: &Connection) -> Result<(), ProbeError> {
    connection
        .execute_batch(SCHEMA)
        .await
        .map_err(ProbeError::Schema)
}

pub async fn begin_immediate(connection: &mut Connection) -> Result<Transaction<'_>, ProbeError> {
    connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await
        .map_err(|error| ProbeError::Transaction {
            step: Step::Begin,
            error,
        })
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
    let unexpected = |column, value: &Value| ProbeError::UnexpectedValue {
        query: QUERY,
        column,
        found: ValueKind::of(value),
    };
    let bytes = match column(0)? {
        Value::Blob(bytes) => bytes,
        other => return Err(unexpected(0, &other)),
    };
    let sql_type = match column(1)? {
        Value::Text(text) => text,
        other => return Err(unexpected(1, &other)),
    };
    let sql_length = match column(2)? {
        Value::Integer(length) => length,
        other => return Err(unexpected(2, &other)),
    };
    Ok(Some(StoredContent {
        bytes,
        sql_type,
        sql_length,
    }))
}

pub async fn read_head(connection: &Connection, name: &str) -> Result<Option<String>, ProbeError> {
    const QUERY: &str = "SELECT content_key FROM probe_head WHERE head_name = ?1";
    let read_error = |error| ProbeError::Read {
        query: QUERY,
        error,
    };
    let mut rows = connection.query(QUERY, (name,)).await.map_err(read_error)?;
    let Some(row) = rows.next().await.map_err(read_error)? else {
        return Ok(None);
    };
    match row.get_value(0).map_err(read_error)? {
        Value::Text(content_key) => Ok(Some(content_key)),
        other => Err(ProbeError::UnexpectedValue {
            query: QUERY,
            column: 0,
            found: ValueKind::of(&other),
        }),
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
    let journal_mode = match pragma_value(connection, "PRAGMA journal_mode").await? {
        Value::Text(mode) => mode,
        other => {
            return Err(ProbeError::UnexpectedValue {
                query: "PRAGMA journal_mode",
                column: 0,
                found: ValueKind::of(&other),
            });
        }
    };
    let synchronous = match pragma_value(connection, "PRAGMA synchronous").await? {
        Value::Integer(level) => level,
        other => {
            return Err(ProbeError::UnexpectedValue {
                query: "PRAGMA synchronous",
                column: 0,
                found: ValueKind::of(&other),
            });
        }
    };
    Ok(DurabilitySettings {
        journal_mode,
        synchronous,
    })
}

async fn pragma_value(connection: &Connection, query: &'static str) -> Result<Value, ProbeError> {
    let read_error = |error| ProbeError::Read { query, error };
    let mut rows = connection.query(query, ()).await.map_err(read_error)?;
    match rows.next().await.map_err(read_error)? {
        Some(row) => row.get_value(0).map_err(read_error),
        None => Ok(Value::Null),
    }
}
