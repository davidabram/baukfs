use std::fmt;

use turso::{Connection, Value};

use crate::WorkspaceId;

use super::error::{DriverError, DriverErrorKind, DriverStep};

pub const CURRENT_SCHEMA_VERSION: i64 = 1;

const METADATA_TABLE: &str = "baukfs_metadata";

/// Schema objects a version-1 database must contain, as `(type, name)`.
const CURRENT_OBJECTS: [(&str, &str); 4] = [
    ("table", METADATA_TABLE),
    ("trigger", "baukfs_metadata_single_row"),
    ("trigger", "baukfs_metadata_identity_immutable"),
    ("trigger", "baukfs_metadata_undeletable"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BootstrapStep {
    Execute(&'static str),
    InsertMetadata,
}

/// Version-1 bootstrap, applied in order inside one `BEGIN IMMEDIATE`
/// transaction.
///
/// The insert trigger runs before conflict resolution, so it also stops
/// `INSERT OR REPLACE` from swapping the identity row.
pub(super) const BOOTSTRAP: [BootstrapStep; 5] = [
    BootstrapStep::Execute(
        "CREATE TABLE baukfs_metadata (
            singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
            schema_version INTEGER NOT NULL CHECK (schema_version >= 1),
            workspace_id TEXT NOT NULL CHECK (length(workspace_id) > 0)
        ) STRICT",
    ),
    BootstrapStep::Execute(
        "CREATE TRIGGER baukfs_metadata_single_row
        BEFORE INSERT ON baukfs_metadata
        WHEN EXISTS (SELECT 1 FROM baukfs_metadata)
        BEGIN
            SELECT RAISE(ABORT, 'baukfs_metadata holds exactly one row');
        END",
    ),
    BootstrapStep::Execute(
        "CREATE TRIGGER baukfs_metadata_identity_immutable
        BEFORE UPDATE OF workspace_id ON baukfs_metadata
        WHEN NEW.workspace_id IS NOT OLD.workspace_id
        BEGIN
            SELECT RAISE(ABORT, 'baukfs workspace identity is immutable');
        END",
    ),
    BootstrapStep::Execute(
        "CREATE TRIGGER baukfs_metadata_undeletable
        BEFORE DELETE ON baukfs_metadata
        BEGIN
            SELECT RAISE(ABORT, 'baukfs_metadata row cannot be deleted');
        END",
    ),
    BootstrapStep::InsertMetadata,
];

const INSERT_METADATA: &str =
    "INSERT INTO baukfs_metadata (singleton, schema_version, workspace_id) VALUES (1, ?1, ?2)";

/// What an existing database file holds, as far as `BaukFS` is concerned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DatabaseState {
    Missing,
    /// A database with no schema objects at all.
    Uninitialized,
    /// Initialized at [`CURRENT_SCHEMA_VERSION`] for this workspace.
    Current(WorkspaceId),
    /// `BaukFS` metadata names a schema version other than the current one,
    /// older or newer. Nothing upgrades it implicitly.
    UnsupportedSchemaVersion {
        found: i64,
    },
    Unrecognized(Unrecognized),
}

impl fmt::Display for DatabaseState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str("missing"),
            Self::Uninitialized => f.write_str("uninitialized"),
            Self::Current(id) => write!(f, "current for workspace {:?}", id.as_str()),
            Self::UnsupportedSchemaVersion { found } => {
                write!(f, "unsupported schema version {found}")
            }
            Self::Unrecognized(reason) => write!(f, "unrecognized ({reason})"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unrecognized {
    /// The file is not a database.
    NotADatabase,
    /// The database has schema objects but no `BaukFS` metadata table.
    ForeignSchema,
    /// The metadata names the current version, but required schema objects
    /// are missing.
    IncompleteSchema,
    /// The metadata table does not hold exactly one well-formed row.
    MalformedMetadata,
}

impl fmt::Display for Unrecognized {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotADatabase => "not a database",
            Self::ForeignSchema => "schema without BaukFS metadata",
            Self::IncompleteSchema => "required BaukFS schema objects are missing",
            Self::MalformedMetadata => "malformed BaukFS metadata",
        })
    }
}

/// Classifies an open database without writing to it.
pub(super) async fn read_state(connection: &Connection) -> Result<DatabaseState, DriverError> {
    match classify(connection).await {
        Err(error) if error.kind() == DriverErrorKind::NotADatabase => {
            Ok(DatabaseState::Unrecognized(Unrecognized::NotADatabase))
        }
        result => result,
    }
}

async fn classify(connection: &Connection) -> Result<DatabaseState, DriverError> {
    let objects = rows(connection, "SELECT type, name FROM sqlite_schema", ()).await?;
    if objects.is_empty() {
        return Ok(DatabaseState::Uninitialized);
    }
    let has_object = |kind: &str, name: &str| {
        objects.iter().any(|row| {
            matches!(row.as_slice(), [Value::Text(k), Value::Text(n)] if k == kind && n == name)
        })
    };
    if !has_object("table", METADATA_TABLE) {
        return Ok(DatabaseState::Unrecognized(Unrecognized::ForeignSchema));
    }
    let metadata = rows(
        connection,
        "SELECT schema_version, workspace_id FROM baukfs_metadata",
        (),
    )
    .await?;
    let malformed = DatabaseState::Unrecognized(Unrecognized::MalformedMetadata);
    let [row] = metadata.as_slice() else {
        return Ok(malformed);
    };
    let [Value::Integer(version), workspace_id] = row.as_slice() else {
        return Ok(malformed);
    };
    if *version != CURRENT_SCHEMA_VERSION {
        return Ok(DatabaseState::UnsupportedSchemaVersion { found: *version });
    }
    if !CURRENT_OBJECTS
        .iter()
        .all(|(kind, name)| has_object(kind, name))
    {
        return Ok(DatabaseState::Unrecognized(Unrecognized::IncompleteSchema));
    }
    let Value::Text(workspace_id) = workspace_id else {
        return Ok(malformed);
    };
    Ok(WorkspaceId::new(workspace_id.as_str()).map_or(malformed, DatabaseState::Current))
}

pub(super) async fn apply_step(
    connection: &Connection,
    step: BootstrapStep,
    workspace_id: &WorkspaceId,
) -> Result<(), DriverError> {
    let executed = match step {
        BootstrapStep::Execute(sql) => connection.execute(sql, ()).await,
        BootstrapStep::InsertMetadata => {
            connection
                .execute(
                    INSERT_METADATA,
                    (CURRENT_SCHEMA_VERSION, workspace_id.as_str()),
                )
                .await
        }
    };
    executed
        .map(drop)
        .map_err(|error| DriverError::new(DriverStep::Bootstrap, error))
}

async fn rows(
    connection: &Connection,
    sql: &'static str,
    params: impl turso::IntoParams,
) -> Result<Vec<Vec<Value>>, DriverError> {
    let read_error = |error| DriverError::new(DriverStep::ReadState, error);
    let mut rows = connection.query(sql, params).await.map_err(read_error)?;
    let mut collected = Vec::new();
    while let Some(row) = rows.next().await.map_err(read_error)? {
        let values = (0..row.column_count())
            .map(|index| row.get_value(index))
            .collect::<Result<Vec<_>, _>>()
            .map_err(read_error)?;
        collected.push(values);
    }
    Ok(collected)
}
