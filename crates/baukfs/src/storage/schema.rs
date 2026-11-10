use std::fmt;

use turso::{Connection, Value};

use crate::WorkspaceId;

use super::error::{DriverError, DriverErrorKind, DriverStep};

pub const CURRENT_SCHEMA_VERSION: i64 = 3;

/// The version the bootstrap creates. Every later version is reached by the
/// ordered steps in [`UPGRADES`].
const BOOTSTRAP_VERSION: i64 = 1;

const METADATA_TABLE: &str = "baukfs_metadata";

type SchemaObject = (&'static str, &'static str);

/// Schema objects each version introduces, as `(type, name)`. Entry `i`
/// belongs to version `i + 1`; a database at version `v` must contain every
/// object of entries `0..v`.
const INTRODUCED_OBJECTS: [&[SchemaObject]; 3] = [
    &[
        ("table", METADATA_TABLE),
        ("trigger", "baukfs_metadata_single_row"),
        ("trigger", "baukfs_metadata_identity_immutable"),
        ("trigger", "baukfs_metadata_undeletable"),
    ],
    &[
        ("table", "baukfs_content"),
        ("trigger", "baukfs_content_insert_once"),
        ("trigger", "baukfs_content_immutable"),
    ],
    &[
        ("table", "baukfs_manifests"),
        ("table", "baukfs_revisions"),
        ("table", "baukfs_workspace"),
        ("trigger", "baukfs_manifests_insert_once"),
        ("trigger", "baukfs_manifests_undeletable"),
        ("trigger", "baukfs_manifests_immutable"),
        ("trigger", "baukfs_revisions_insert_once"),
        ("trigger", "baukfs_revisions_undeletable"),
        ("trigger", "baukfs_revisions_immutable"),
        ("trigger", "baukfs_workspace_insert_once"),
        ("trigger", "baukfs_workspace_undeletable"),
        ("trigger", "baukfs_workspace_immutable"),
    ],
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SchemaStep {
    Execute(&'static str),
    InsertMetadata,
}

/// The bootstrap and the ordered upgrade steps that together define the
/// current schema. Creation applies all of them; an upgrade from version `v`
/// applies `upgrades[v - 1..]`.
#[derive(Clone, Copy, Debug)]
pub(super) struct Schema<'a> {
    pub(super) bootstrap: &'a [SchemaStep],
    pub(super) upgrades: &'a [&'a [SchemaStep]],
}

pub(super) const SCHEMA: Schema<'static> = Schema {
    bootstrap: &BOOTSTRAP,
    upgrades: &UPGRADES,
};

const _: () = assert!(INTRODUCED_OBJECTS.len() == UPGRADES.len() + 1);

/// Version-1 bootstrap.
///
/// The insert trigger runs before conflict resolution, so it also stops
/// `INSERT OR REPLACE` from swapping the identity row.
pub(super) const BOOTSTRAP: [SchemaStep; 5] = [
    SchemaStep::Execute(
        "CREATE TABLE baukfs_metadata (
            singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
            schema_version INTEGER NOT NULL CHECK (schema_version >= 1),
            workspace_id TEXT NOT NULL CHECK (length(workspace_id) > 0)
        ) STRICT",
    ),
    SchemaStep::Execute(
        "CREATE TRIGGER baukfs_metadata_single_row
        BEFORE INSERT ON baukfs_metadata
        WHEN EXISTS (SELECT 1 FROM baukfs_metadata)
        BEGIN
            SELECT RAISE(ABORT, 'baukfs_metadata holds exactly one row');
        END",
    ),
    SchemaStep::Execute(
        "CREATE TRIGGER baukfs_metadata_identity_immutable
        BEFORE UPDATE OF workspace_id ON baukfs_metadata
        WHEN NEW.workspace_id IS NOT OLD.workspace_id
        BEGIN
            SELECT RAISE(ABORT, 'baukfs workspace identity is immutable');
        END",
    ),
    SchemaStep::Execute(
        "CREATE TRIGGER baukfs_metadata_undeletable
        BEFORE DELETE ON baukfs_metadata
        BEGIN
            SELECT RAISE(ABORT, 'baukfs_metadata row cannot be deleted');
        END",
    ),
    SchemaStep::InsertMetadata,
];

/// Version 1 to 2: immutable whole-file contents keyed by BLAKE3 digest.
///
/// As for the metadata, the insert trigger runs before conflict resolution,
/// so `INSERT OR REPLACE` cannot swap an existing record. `byte_length` is
/// not tied to `length(contents)` by a CHECK; reads compare the two and
/// report a disagreement as an integrity error.
const VERSION_2: [SchemaStep; 3] = [
    SchemaStep::Execute(
        "CREATE TABLE baukfs_content (
            digest BLOB PRIMARY KEY NOT NULL CHECK (length(digest) = 32),
            byte_length INTEGER NOT NULL CHECK (byte_length >= 0),
            contents BLOB NOT NULL
        ) STRICT",
    ),
    SchemaStep::Execute(
        "CREATE TRIGGER baukfs_content_insert_once
        BEFORE INSERT ON baukfs_content
        WHEN EXISTS (SELECT 1 FROM baukfs_content WHERE digest = NEW.digest)
        BEGIN
            SELECT RAISE(ABORT, 'baukfs content is stored once per digest');
        END",
    ),
    SchemaStep::Execute(
        "CREATE TRIGGER baukfs_content_immutable
        BEFORE UPDATE ON baukfs_content
        BEGIN
            SELECT RAISE(ABORT, 'baukfs content is immutable');
        END",
    ),
];

const VERSION_3: [SchemaStep; 12] = [
    SchemaStep::Execute(
        "CREATE TABLE baukfs_manifests (digest BLOB PRIMARY KEY NOT NULL CHECK (length(digest) = 32), encoding BLOB NOT NULL) STRICT",
    ),
    SchemaStep::Execute(
        "CREATE TABLE baukfs_revisions (id TEXT PRIMARY KEY NOT NULL, manifest BLOB NOT NULL CHECK (length(manifest) = 32), parent TEXT) STRICT",
    ),
    SchemaStep::Execute(
        "CREATE TABLE baukfs_workspace (singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1), session TEXT NOT NULL, base TEXT NOT NULL, head TEXT NOT NULL, status TEXT NOT NULL CHECK (status IN ('idle', 'running', 'sealed'))) STRICT",
    ),
    SchemaStep::Execute(
        "CREATE TRIGGER baukfs_manifests_insert_once BEFORE INSERT ON baukfs_manifests WHEN EXISTS (SELECT 1 FROM baukfs_manifests WHERE digest = NEW.digest) BEGIN SELECT RAISE(ABORT, 'immutable history record'); END",
    ),
    SchemaStep::Execute(
        "CREATE TRIGGER baukfs_manifests_undeletable BEFORE DELETE ON baukfs_manifests BEGIN SELECT RAISE(ABORT, 'immutable history record'); END",
    ),
    SchemaStep::Execute(
        "CREATE TRIGGER baukfs_manifests_immutable BEFORE UPDATE ON baukfs_manifests BEGIN SELECT RAISE(ABORT, 'immutable history record'); END",
    ),
    SchemaStep::Execute(
        "CREATE TRIGGER baukfs_revisions_insert_once BEFORE INSERT ON baukfs_revisions WHEN EXISTS (SELECT 1 FROM baukfs_revisions WHERE id = NEW.id) BEGIN SELECT RAISE(ABORT, 'immutable history record'); END",
    ),
    SchemaStep::Execute(
        "CREATE TRIGGER baukfs_revisions_undeletable BEFORE DELETE ON baukfs_revisions BEGIN SELECT RAISE(ABORT, 'immutable history record'); END",
    ),
    SchemaStep::Execute(
        "CREATE TRIGGER baukfs_revisions_immutable BEFORE UPDATE ON baukfs_revisions BEGIN SELECT RAISE(ABORT, 'immutable history record'); END",
    ),
    SchemaStep::Execute(
        "CREATE TRIGGER baukfs_workspace_insert_once BEFORE INSERT ON baukfs_workspace WHEN EXISTS (SELECT 1 FROM baukfs_workspace WHERE singleton = NEW.singleton) BEGIN SELECT RAISE(ABORT, 'immutable history record'); END",
    ),
    SchemaStep::Execute(
        "CREATE TRIGGER baukfs_workspace_undeletable BEFORE DELETE ON baukfs_workspace BEGIN SELECT RAISE(ABORT, 'immutable history record'); END",
    ),
    SchemaStep::Execute(
        "CREATE TRIGGER baukfs_workspace_immutable BEFORE UPDATE OF session, base, singleton ON baukfs_workspace BEGIN SELECT RAISE(ABORT, 'immutable history record'); END",
    ),
];

/// Entry `i` upgrades version `i + 1` to `i + 2`.
const UPGRADES: [&[SchemaStep]; 2] = [&VERSION_2, &VERSION_3];

const INSERT_METADATA: &str =
    "INSERT INTO baukfs_metadata (singleton, schema_version, workspace_id) VALUES (1, ?1, ?2)";

const SET_SCHEMA_VERSION: &str = "UPDATE baukfs_metadata SET schema_version = ?1";

/// What an existing database file holds, as far as `BaukFS` is concerned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DatabaseState {
    Missing,
    /// A database with no schema objects at all.
    Uninitialized,
    /// Initialized at [`CURRENT_SCHEMA_VERSION`] for this workspace.
    Current(WorkspaceId),
    /// A complete database at an older schema version for this workspace.
    /// Only [`super::WorkspaceStore::upgrade`] brings it to the current
    /// version.
    UpgradeRequired {
        found: i64,
        workspace_id: WorkspaceId,
    },
    /// `BaukFS` metadata names a schema version this binary does not know:
    /// newer than the current one, or below 1.
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
            Self::UpgradeRequired {
                found,
                workspace_id,
            } => write!(
                f,
                "schema version {found} for workspace {:?}, upgrade required",
                workspace_id.as_str()
            ),
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
    /// The metadata names a known version, but schema objects that version
    /// requires are missing.
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
    let objects = rows(
        connection,
        "SELECT type, name FROM sqlite_schema",
        (),
        DriverStep::ReadState,
    )
    .await?;
    if objects.is_empty() {
        return Ok(DatabaseState::Uninitialized);
    }
    if !objects
        .iter()
        .any(|row| is_object(row, "table", METADATA_TABLE))
    {
        return Ok(DatabaseState::Unrecognized(Unrecognized::ForeignSchema));
    }
    // One statement reads one committed snapshot. Reading the object list and
    // the metadata separately could pair a pre-upgrade object list with a
    // post-upgrade version and misreport a concurrent upgrade as incomplete.
    let snapshot = rows(
        connection,
        "SELECT 'object', type, name FROM sqlite_schema
        UNION ALL
        SELECT 'metadata', schema_version, workspace_id FROM baukfs_metadata",
        (),
        DriverStep::ReadState,
    )
    .await?;
    let mut objects = Vec::new();
    let mut metadata = Vec::new();
    for row in &snapshot {
        if let Some((Value::Text(source), rest)) = row.split_first() {
            match source.as_str() {
                "object" => objects.push(rest),
                "metadata" => metadata.push(rest),
                _ => {}
            }
        }
    }
    let malformed = DatabaseState::Unrecognized(Unrecognized::MalformedMetadata);
    let [[Value::Integer(version), workspace_id]] = metadata.as_slice() else {
        return Ok(malformed);
    };
    let Some(required) = version_index(*version).and_then(|count| INTRODUCED_OBJECTS.get(..count))
    else {
        return Ok(DatabaseState::UnsupportedSchemaVersion { found: *version });
    };
    let complete = required
        .iter()
        .copied()
        .flatten()
        .all(|(kind, name)| objects.iter().any(|row| is_object(row, kind, name)));
    if !complete {
        return Ok(DatabaseState::Unrecognized(Unrecognized::IncompleteSchema));
    }
    let Value::Text(workspace_id) = workspace_id else {
        return Ok(malformed);
    };
    let Ok(workspace_id) = WorkspaceId::new(workspace_id.as_str()) else {
        return Ok(malformed);
    };
    Ok(if *version == CURRENT_SCHEMA_VERSION {
        DatabaseState::Current(workspace_id)
    } else {
        DatabaseState::UpgradeRequired {
            found: *version,
            workspace_id,
        }
    })
}

fn is_object(row: &[Value], kind: &str, name: &str) -> bool {
    matches!(row, [Value::Text(k), Value::Text(n)] if k == kind && n == name)
}

/// The number of introduced-object entries a known version requires, or
/// `None` for a version below 1.
fn version_index(version: i64) -> Option<usize> {
    usize::try_from(version).ok().filter(|count| *count >= 1)
}

/// Applies `steps` in order and then records `version`. Runs inside the
/// caller's transaction.
pub(super) async fn apply_steps(
    connection: &Connection,
    steps: impl IntoIterator<Item = SchemaStep>,
    version: i64,
    workspace_id: &WorkspaceId,
    driver_step: DriverStep,
) -> Result<(), DriverError> {
    let failed = |error| DriverError::new(driver_step, error);
    for step in steps {
        match step {
            SchemaStep::Execute(sql) => connection.execute(sql, ()).await,
            SchemaStep::InsertMetadata => {
                connection
                    .execute(INSERT_METADATA, (BOOTSTRAP_VERSION, workspace_id.as_str()))
                    .await
            }
        }
        .map_err(failed)?;
    }
    connection
        .execute(SET_SCHEMA_VERSION, (version,))
        .await
        .map(drop)
        .map_err(failed)
}

impl Schema<'_> {
    /// Every step from an empty database to the newest version.
    pub(super) fn creation_steps(&self) -> impl Iterator<Item = SchemaStep> {
        self.bootstrap
            .iter()
            .chain(self.upgrades.iter().copied().flatten())
            .copied()
    }

    /// The pending steps from `version`, or `None` if `version` is not an
    /// older known version.
    pub(super) fn upgrade_steps(&self, version: i64) -> Option<impl Iterator<Item = SchemaStep>> {
        let pending = self.upgrades.get(version_index(version)? - 1..)?;
        (!pending.is_empty()).then(|| pending.iter().copied().flatten().copied())
    }
}

pub(super) async fn rows(
    connection: &Connection,
    sql: &'static str,
    params: impl turso::IntoParams,
    step: DriverStep,
) -> Result<Vec<Vec<Value>>, DriverError> {
    let read_error = |error| DriverError::new(step, error);
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
