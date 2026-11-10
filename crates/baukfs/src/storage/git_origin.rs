use turso::{Connection, Value};

use super::{
    Abandoned, DriverError, DriverStep, HistoryError, HistoryOutcome, history, schema::rows,
};
use crate::git_import::{GitOrigin, ObjectFormat};
use crate::{Manifest, RevisionId, SessionId, WorkspaceId};

pub(super) async fn read(
    connection: &Connection,
    id: &WorkspaceId,
) -> Result<Option<GitOrigin>, Abandoned> {
    let records = rows(
        connection,
        "SELECT commit_id, tree_id, object_format, root_revision FROM baukfs_git_origin",
        (),
        DriverStep::History,
    )
    .await
    .map_err(Abandoned::Failed)?;
    if records.is_empty() {
        return Ok(None);
    }
    let malformed = || Abandoned::History(HistoryError::MalformedGitOrigin);
    let [record] = records.as_slice() else {
        return Err(malformed());
    };
    let [
        Value::Text(commit),
        Value::Text(tree),
        Value::Text(format),
        Value::Text(root),
    ] = record.as_slice()
    else {
        return Err(malformed());
    };
    let format = ObjectFormat::parse(format).ok_or_else(malformed)?;
    if !format.valid_id(commit) || !format.valid_id(tree) {
        return Err(malformed());
    }
    let root = RevisionId::new(root.clone()).map_err(|_| malformed())?;
    let workspace = history::workspace(connection, id)
        .await?
        .ok_or_else(malformed)?;
    if workspace.base() != &root {
        return Err(malformed());
    }
    Ok(Some(GitOrigin {
        commit: commit.clone(),
        tree: tree.clone(),
        format,
        root,
    }))
}

pub(super) async fn initialize(
    connection: &Connection,
    id: &WorkspaceId,
    session: &SessionId,
    manifest: &Manifest,
    commit: String,
    tree: String,
    format: ObjectFormat,
) -> Result<(GitOrigin, HistoryOutcome), Abandoned> {
    let existing = read(connection, id).await?;
    if let Some(origin) = &existing {
        if origin.commit != commit || origin.tree != tree || origin.format != format {
            return Err(HistoryError::GitOriginConflict.into());
        }
    } else if history::workspace(connection, id).await?.is_some() {
        return Err(HistoryError::GitOriginConflict.into());
    }
    let (workspace, outcome) = history::initialize(connection, id, session, manifest).await?;
    let origin = GitOrigin {
        commit,
        tree,
        format,
        root: workspace.base().clone(),
    };
    if let Some(existing) = existing {
        if existing != origin {
            return Err(HistoryError::GitOriginConflict.into());
        }
    } else {
        connection.execute("INSERT INTO baukfs_git_origin (singleton, commit_id, tree_id, object_format, root_revision) VALUES (1, ?1, ?2, ?3, ?4)", (origin.commit.as_str(), origin.tree.as_str(), origin.format.as_str(), origin.root.as_str())).await.map_err(|error| Abandoned::Failed(DriverError::new(DriverStep::History, error)))?;
    }
    Ok((origin, outcome))
}
