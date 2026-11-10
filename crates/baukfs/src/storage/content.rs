use turso::{Connection, Value};

use crate::ContentId;

use super::error::{Abandoned, DriverError, DriverStep, IntegrityError, IntegrityViolation};
use super::schema::rows;

const SELECT_CONTENT: &str = "SELECT byte_length, contents FROM baukfs_content WHERE digest = ?1";

/// No `ON CONFLICT` clause: the caller holds the write lock and has just
/// found no record for this digest, so a duplicate is an error, never an
/// overwrite.
const INSERT_CONTENT: &str =
    "INSERT INTO baukfs_content (digest, byte_length, contents) VALUES (?1, length(?2), ?2)";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertOutcome {
    /// This call stored the bytes.
    Inserted,
    /// Identical bytes were already stored under the same identifier.
    AlreadyPresent,
}

/// Inserts `bytes` under `id` unless a record already exists, in which case
/// the record is verified and compared with `bytes`. Must run under a
/// `BEGIN IMMEDIATE` transaction so no other writer can insert in between.
pub(super) async fn insert_under_lock(
    connection: &Connection,
    id: &ContentId,
    bytes: &[u8],
) -> Result<InsertOutcome, Abandoned> {
    match read(connection, id, DriverStep::InsertContent).await {
        Ok(None) => {}
        Ok(Some(stored)) if stored == bytes => return Ok(InsertOutcome::AlreadyPresent),
        Ok(Some(_)) => {
            return Err(Abandoned::Integrity(IntegrityError::new(
                *id,
                IntegrityViolation::ConflictingContents,
            )));
        }
        Err(Read::Failed(error)) => return Err(Abandoned::Failed(error)),
        Err(Read::Integrity(error)) => return Err(Abandoned::Integrity(error)),
    }
    connection
        .execute(INSERT_CONTENT, (id.digest().as_slice(), bytes))
        .await
        .map_err(|error| Abandoned::Failed(DriverError::new(DriverStep::InsertContent, error)))?;
    Ok(InsertOutcome::Inserted)
}

pub(super) enum Read {
    Failed(DriverError),
    Integrity(IntegrityError),
}

/// Reads the record for `id` and returns its bytes only after checking the
/// recorded length and recomputing the digest.
pub(super) async fn read(
    connection: &Connection,
    id: &ContentId,
    step: DriverStep,
) -> Result<Option<Vec<u8>>, Read> {
    let records = rows(connection, SELECT_CONTENT, (id.digest().as_slice(),), step)
        .await
        .map_err(Read::Failed)?;
    let Some(record) = records.into_iter().next() else {
        return Ok(None);
    };
    verified(id, record).map(Some).map_err(Read::Integrity)
}

fn verified(id: &ContentId, record: Vec<Value>) -> Result<Vec<u8>, IntegrityError> {
    let violation = |violation| IntegrityError::new(*id, violation);
    let Ok([Value::Integer(recorded), Value::Blob(bytes)]) = <[Value; 2]>::try_from(record) else {
        return Err(violation(IntegrityViolation::MalformedRecord));
    };
    if usize::try_from(recorded).ok() != Some(bytes.len()) {
        return Err(violation(IntegrityViolation::LengthMismatch {
            recorded,
            stored: bytes.len(),
        }));
    }
    let computed = ContentId::of(&bytes);
    if computed != *id {
        return Err(violation(IntegrityViolation::DigestMismatch { computed }));
    }
    Ok(bytes)
}
