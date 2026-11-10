use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use turso::{Connection, Value};

use super::{Abandoned, DriverError, DriverStep, content, schema::rows};
use crate::{
    ContentId, EntryKind, Manifest, ManifestError, ManifestId, RevisionId, SessionId,
    TransitionError, Workspace, WorkspaceId, WorkspaceStatus,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision {
    id: RevisionId,
    manifest: ManifestId,
    parent: Option<RevisionId>,
}

impl Revision {
    #[must_use]
    pub fn id(&self) -> &RevisionId {
        &self.id
    }
    #[must_use]
    pub fn manifest(&self) -> &ManifestId {
        &self.manifest
    }
    #[must_use]
    pub fn parent(&self) -> Option<&RevisionId> {
        self.parent.as_ref()
    }

    fn new(manifest: ManifestId, parent: Option<RevisionId>) -> Result<Self, HistoryError> {
        let mut preimage = b"baukfs-revision\0".to_vec();
        preimage.extend_from_slice(&1_u32.to_le_bytes());
        preimage.extend_from_slice(manifest.digest());
        match &parent {
            None => preimage.push(0),
            Some(parent) => {
                preimage.push(1);
                let bytes = parent.as_str().as_bytes();
                let length =
                    u64::try_from(bytes.len()).map_err(|_| HistoryError::MalformedRevision)?;
                preimage.extend_from_slice(&length.to_le_bytes());
                preimage.extend_from_slice(bytes);
            }
        }
        let id = RevisionId::new(format!("rb3:{}", blake3::hash(&preimage).to_hex()))
            .map_err(|_| HistoryError::MalformedRevision)?;
        Ok(Self {
            id,
            manifest,
            parent,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryOutcome {
    Initialized,
    Advanced,
    AlreadyApplied,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryError {
    NotInitialized,
    GitOriginConflict,
    MalformedGitOrigin,
    InitializationConflict,
    MissingContent(ContentId),
    MissingManifest(ManifestId),
    MissingRevision(RevisionId),
    ManifestEncoding(ManifestError),
    ManifestIdentity(ManifestId),
    MalformedManifest,
    MalformedRevision,
    RevisionIdentity(RevisionId),
    ConflictingManifest(ManifestId),
    ConflictingRevision(RevisionId),
    MalformedWorkspace,
    InvalidLineage,
    HeadCompareAndSwapFailed,
    Transition(TransitionError),
    StaleHead {
        expected: RevisionId,
        actual: RevisionId,
    },
}

impl fmt::Display for HistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "history: {self:?}")
    }
}
impl Error for HistoryError {}
impl From<HistoryError> for Abandoned {
    fn from(error: HistoryError) -> Self {
        Self::History(error)
    }
}

fn failed(error: turso::Error) -> Abandoned {
    Abandoned::Failed(DriverError::new(DriverStep::History, error))
}

async fn contents(connection: &Connection, manifest: &Manifest) -> Result<(), Abandoned> {
    let mut seen = BTreeSet::new();
    for entry in manifest.entries() {
        if let EntryKind::RegularFile { content: id, .. } = entry.kind() {
            if !seen.insert(*id.digest()) {
                continue;
            }
            match content::read(connection, id, DriverStep::History).await {
                Ok(Some(_)) => {}
                Ok(None) => return Err(HistoryError::MissingContent(*id).into()),
                Err(content::Read::Failed(error)) => return Err(Abandoned::Failed(error)),
                Err(content::Read::Integrity(error)) => return Err(Abandoned::Integrity(error)),
            }
        }
    }
    Ok(())
}

pub(super) async fn manifest(
    connection: &Connection,
    id: &ManifestId,
) -> Result<Option<Manifest>, Abandoned> {
    let records = rows(
        connection,
        "SELECT encoding FROM baukfs_manifests WHERE digest = ?1",
        (id.digest().as_slice(),),
        DriverStep::History,
    )
    .await
    .map_err(Abandoned::Failed)?;
    if records.is_empty() {
        return Ok(None);
    }
    let [record] = records.as_slice() else {
        return Err(HistoryError::MalformedManifest.into());
    };
    let [Value::Blob(bytes)] = record.as_slice() else {
        return Err(HistoryError::MalformedManifest.into());
    };
    let manifest = Manifest::decode(bytes)
        .map_err(|e| Abandoned::History(HistoryError::ManifestEncoding(e)))?;
    if manifest
        .id()
        .map_err(|e| Abandoned::History(HistoryError::ManifestEncoding(e)))?
        != *id
    {
        return Err(HistoryError::ManifestIdentity(*id).into());
    }
    contents(connection, &manifest).await?;
    Ok(Some(manifest))
}

async fn revision_row(
    connection: &Connection,
    id: &RevisionId,
) -> Result<Option<Revision>, Abandoned> {
    let records = rows(
        connection,
        "SELECT manifest, parent FROM baukfs_revisions WHERE id = ?1",
        (id.as_str(),),
        DriverStep::History,
    )
    .await
    .map_err(Abandoned::Failed)?;
    if records.is_empty() {
        return Ok(None);
    }
    let [record] = records.as_slice() else {
        return Err(HistoryError::MalformedRevision.into());
    };
    let [Value::Blob(digest), parent] = record.as_slice() else {
        return Err(HistoryError::MalformedRevision.into());
    };
    let digest: [u8; 32] = digest
        .as_slice()
        .try_into()
        .map_err(|_| Abandoned::History(HistoryError::MalformedRevision))?;
    let manifest_id = ManifestId::from_digest(digest);
    let parent = match parent {
        Value::Null => None,
        Value::Text(value) => Some(
            RevisionId::new(value.clone())
                .map_err(|_| Abandoned::History(HistoryError::MalformedRevision))?,
        ),
        _ => return Err(HistoryError::MalformedRevision.into()),
    };
    let revision = Revision::new(manifest_id, parent)?;
    if revision.id != *id {
        return Err(HistoryError::RevisionIdentity(id.clone()).into());
    }
    if manifest(connection, &manifest_id).await?.is_none() {
        return Err(HistoryError::MissingManifest(manifest_id).into());
    }
    Ok(Some(revision))
}

pub(super) async fn revision(
    connection: &Connection,
    id: &RevisionId,
) -> Result<Option<Revision>, Abandoned> {
    let Some(first) = revision_row(connection, id).await? else {
        return Ok(None);
    };
    ancestry(connection, &first).await?;
    Ok(Some(first))
}

async fn ancestry(connection: &Connection, first: &Revision) -> Result<RevisionId, Abandoned> {
    let mut current = first.clone();
    let mut seen = BTreeSet::new();
    loop {
        if !seen.insert(current.id.as_str().to_owned()) {
            return Err(HistoryError::InvalidLineage.into());
        }
        let Some(parent) = &current.parent else {
            return Ok(current.id);
        };
        current = revision_row(connection, parent)
            .await?
            .ok_or_else(|| Abandoned::History(HistoryError::MissingRevision(parent.clone())))?;
    }
}

pub(super) async fn workspace(
    connection: &Connection,
    id: &WorkspaceId,
) -> Result<Option<Workspace>, Abandoned> {
    let records = rows(
        connection,
        "SELECT session, base, head, status FROM baukfs_workspace",
        (),
        DriverStep::History,
    )
    .await
    .map_err(Abandoned::Failed)?;
    if records.is_empty() {
        return Ok(None);
    }
    let [record] = records.as_slice() else {
        return Err(HistoryError::MalformedWorkspace.into());
    };
    let [
        Value::Text(session),
        Value::Text(base),
        Value::Text(head),
        Value::Text(status),
    ] = record.as_slice()
    else {
        return Err(HistoryError::MalformedWorkspace.into());
    };
    let malformed = |_| Abandoned::History(HistoryError::MalformedWorkspace);
    let session = SessionId::new(session.clone()).map_err(malformed)?;
    let base = RevisionId::new(base.clone()).map_err(malformed)?;
    let head = RevisionId::new(head.clone()).map_err(malformed)?;
    let status = match status.as_str() {
        "idle" => WorkspaceStatus::Idle,
        "running" => WorkspaceStatus::Running,
        "sealed" => WorkspaceStatus::Sealed,
        _ => return Err(HistoryError::MalformedWorkspace.into()),
    };
    let head_record = revision_row(connection, &head)
        .await?
        .ok_or_else(|| Abandoned::History(HistoryError::MissingRevision(head.clone())))?;
    if ancestry(connection, &head_record).await? != base {
        return Err(HistoryError::InvalidLineage.into());
    }
    Workspace::from_stored(id.clone(), session, base, head, status)
        .map(Some)
        .map_err(|e| Abandoned::History(HistoryError::Transition(e)))
}

pub(super) async fn persist(
    connection: &Connection,
    manifest_value: &Manifest,
    parent: Option<RevisionId>,
) -> Result<Revision, Abandoned> {
    contents(connection, manifest_value).await?;
    if let Some(parent) = &parent
        && revision(connection, parent).await?.is_none()
    {
        return Err(HistoryError::MissingRevision(parent.clone()).into());
    }
    let encoding = manifest_value
        .encode()
        .map_err(|e| Abandoned::History(HistoryError::ManifestEncoding(e)))?;
    let id = manifest_value
        .id()
        .map_err(|e| Abandoned::History(HistoryError::ManifestEncoding(e)))?;
    match manifest(connection, &id).await? {
        Some(stored) if stored == *manifest_value => {}
        Some(_) => return Err(HistoryError::ConflictingManifest(id).into()),
        None => {
            connection
                .execute(
                    "INSERT INTO baukfs_manifests (digest, encoding) VALUES (?1, ?2)",
                    (id.digest().as_slice(), encoding),
                )
                .await
                .map_err(failed)?;
        }
    }
    let target = Revision::new(id, parent)?;
    match revision(connection, &target.id).await? {
        Some(stored) if stored == target => {}
        Some(_) => return Err(HistoryError::ConflictingRevision(target.id).into()),
        None => {
            connection
                .execute(
                    "INSERT INTO baukfs_revisions (id, manifest, parent) VALUES (?1, ?2, ?3)",
                    (
                        target.id.as_str(),
                        id.digest().as_slice(),
                        target.parent.as_ref().map(RevisionId::as_str),
                    ),
                )
                .await
                .map_err(failed)?;
        }
    }
    Ok(target)
}

pub(super) async fn initialize(
    connection: &Connection,
    id: &WorkspaceId,
    session: &SessionId,
    manifest: &Manifest,
) -> Result<(Workspace, HistoryOutcome), Abandoned> {
    if let Some(stored) = workspace(connection, id).await? {
        let manifest_id = manifest
            .id()
            .map_err(|e| Abandoned::History(HistoryError::ManifestEncoding(e)))?;
        let root = Revision::new(manifest_id, None)?;
        if stored.session() == session
            && stored.base() == root.id()
            && stored.head() == root.id()
            && stored.status() == WorkspaceStatus::Idle
        {
            let existing = self::manifest(connection, &manifest_id).await?;
            if existing.as_ref() == Some(manifest) {
                return Ok((stored, HistoryOutcome::AlreadyApplied));
            }
        }
        return Err(HistoryError::InitializationConflict.into());
    }
    let revision = persist(connection, manifest, None).await?;
    connection.execute("INSERT INTO baukfs_workspace (singleton, session, base, head, status) VALUES (1, ?1, ?2, ?2, 'idle')", (session.as_str(), revision.id.as_str())).await.map_err(failed)?;
    #[cfg(test)]
    super::tests::history::before_commit();
    Ok((
        Workspace::new(id.clone(), session.clone(), revision.id),
        HistoryOutcome::Initialized,
    ))
}

pub(super) async fn advance(
    connection: &Connection,
    id: &WorkspaceId,
    expected: &RevisionId,
    manifest: &Manifest,
) -> Result<(Workspace, HistoryOutcome), Abandoned> {
    let mut workspace = workspace(connection, id)
        .await?
        .ok_or(HistoryError::NotInitialized)?;
    let manifest_id = manifest
        .id()
        .map_err(|e| Abandoned::History(HistoryError::ManifestEncoding(e)))?;
    let target = Revision::new(manifest_id, Some(expected.clone()))?;
    let mut transitioned = workspace.clone();
    transitioned
        .record_checkpoint(target.id.clone())
        .map_err(|e| Abandoned::History(HistoryError::Transition(e)))?;
    if workspace.head() != expected {
        if workspace.head() == target.id() {
            let stored = revision(connection, target.id()).await?;
            if stored.as_ref() == Some(&target)
                && self::manifest(connection, &manifest_id).await?.as_ref() == Some(manifest)
            {
                return Ok((workspace, HistoryOutcome::AlreadyApplied));
            }
            return Err(HistoryError::ConflictingRevision(target.id).into());
        }
        return Err(HistoryError::StaleHead {
            expected: expected.clone(),
            actual: workspace.head().clone(),
        }
        .into());
    }
    persist(connection, manifest, Some(expected.clone())).await?;
    let changed = connection.execute("UPDATE baukfs_workspace SET head = ?1 WHERE singleton = 1 AND head = ?2 AND status = 'idle'", (target.id.as_str(), expected.as_str())).await.map_err(failed)?;
    if changed != 1 {
        return Err(HistoryError::HeadCompareAndSwapFailed.into());
    }
    #[cfg(test)]
    super::tests::history::before_commit();
    workspace = transitioned;
    Ok((workspace, HistoryOutcome::Advanced))
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    #[test]
    fn revision_identity_fixes_versioned_framing_and_parent_identity() {
        let manifest = Manifest::new(Vec::from([])).unwrap().id().unwrap();
        let root = Revision::new(manifest, None).unwrap();
        let mut golden = b"baukfs-revision\0\x01\x00\x00\x00".to_vec();
        golden.extend_from_slice(manifest.digest());
        golden.push(0);
        assert_eq!(golden.len(), 53);
        assert_eq!(
            root.id.as_str(),
            format!("rb3:{}", blake3::hash(&golden).to_hex())
        );
        golden.pop();
        golden.push(1);
        golden.extend_from_slice(&68_u64.to_le_bytes());
        golden.extend_from_slice(root.id.as_str().as_bytes());
        let child = Revision::new(manifest, Some(root.id.clone())).unwrap();
        assert_eq!(
            child.id.as_str(),
            format!("rb3:{}", blake3::hash(&golden).to_hex())
        );
        assert_ne!(child.id, root.id);
    }
}
