//! Imports immutable local Git objects without consulting checkout contents.

mod process;

use std::error::Error;
use std::fmt;
use std::path::Path;

use crate::storage::{HistoryOutcome, StorageError, WorkspaceStore};
use crate::{
    ByteStringError, EntryKind, Manifest, ManifestEntry, ManifestError, ManifestId, RevisionId,
    SessionId, SymlinkTarget, WorkspacePath,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}
impl ObjectFormat {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
        }
    }
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "sha1" => Some(Self::Sha1),
            "sha256" => Some(Self::Sha256),
            _ => None,
        }
    }
    pub(crate) fn valid_id(self, value: &str) -> bool {
        value.len() == if self == Self::Sha1 { 40 } else { 64 }
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitOrigin {
    pub(crate) commit: String,
    pub(crate) tree: String,
    pub(crate) format: ObjectFormat,
    pub(crate) root: RevisionId,
}
impl GitOrigin {
    #[must_use]
    pub fn commit(&self) -> &str {
        &self.commit
    }
    #[must_use]
    pub fn tree(&self) -> &str {
        &self.tree
    }
    #[must_use]
    pub fn object_format(&self) -> ObjectFormat {
        self.format
    }
    #[must_use]
    pub fn root_revision(&self) -> &RevisionId {
        &self.root
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportOutcome {
    Imported,
    AlreadyImported,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportReceipt {
    pub origin: GitOrigin,
    pub manifest: ManifestId,
    pub outcome: ImportOutcome,
}

#[derive(Debug)]
pub enum ImportError {
    Io(std::io::Error),
    Command {
        operation: String,
        code: Option<i32>,
        stderr: Vec<u8>,
    },
    Deadline,
    OutputLimit,
    ReaderPanicked,
    InvalidSelector {
        diagnostic: Vec<u8>,
    },
    MissingObject {
        id: String,
        diagnostic: Vec<u8>,
    },
    NonCommit {
        kind: Vec<u8>,
    },
    InvalidObjectData,
    UnsupportedObjectFormat(Vec<u8>),
    Gitlink {
        path: Vec<u8>,
    },
    UnsupportedMode {
        mode: Vec<u8>,
        path: Vec<u8>,
    },
    InvalidPath(ByteStringError),
    InvalidTarget(ByteStringError),
    Manifest(ManifestError),
    Storage(StorageError),
}
impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Git import: {self:?}")
    }
}
impl Error for ImportError {}
impl From<StorageError> for ImportError {
    fn from(value: StorageError) -> Self {
        Self::Storage(value)
    }
}

fn text(bytes: &[u8]) -> Result<&str, ImportError> {
    std::str::from_utf8(bytes).map_err(|_| ImportError::InvalidObjectData)
}
fn object(repo: &Path, kind: &str, id: &str) -> Result<Vec<u8>, ImportError> {
    process::run(repo, &["cat-file", kind, id]).map_err(|error| match error {
        ImportError::Command { stderr, .. } => ImportError::MissingObject {
            id: id.to_owned(),
            diagnostic: stderr,
        },
        other => other,
    })
}

fn validate_tree(
    repository: &Path,
    tree: &str,
    parent: &[u8],
    format: ObjectFormat,
) -> Result<(), ImportError> {
    let bytes = object(repository, "tree", tree)?;
    let mut rest = bytes.as_slice();
    let id_bytes = if format == ObjectFormat::Sha1 { 20 } else { 32 };
    while !rest.is_empty() {
        let space = rest
            .iter()
            .position(|b| *b == b' ')
            .ok_or(ImportError::InvalidObjectData)?;
        let mode = &rest[..space];
        let name_start = space + 1;
        let nul = rest[name_start..]
            .iter()
            .position(|b| *b == 0)
            .ok_or(ImportError::InvalidObjectData)?
            + name_start;
        let name = &rest[name_start..nul];
        if name.is_empty() || name.contains(&b'/') {
            return Err(ImportError::InvalidObjectData);
        }
        let mut path = parent.to_vec();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(name);
        WorkspacePath::new(path.clone()).map_err(ImportError::InvalidPath)?;
        match mode {
            b"40000" | b"100644" | b"100755" | b"120000" => {}
            b"160000" => return Err(ImportError::Gitlink { path }),
            _ => {
                return Err(ImportError::UnsupportedMode {
                    mode: mode.to_vec(),
                    path,
                });
            }
        }
        rest = rest
            .get(nul + 1 + id_bytes..)
            .ok_or(ImportError::InvalidObjectData)?;
    }
    Ok(())
}

fn resolve(
    repository: &Path,
    selector: &str,
) -> Result<(String, String, ObjectFormat), ImportError> {
    let format_bytes = process::run(repository, &["rev-parse", "--show-object-format=storage"])?;
    let format = ObjectFormat::parse(text(&format_bytes)?.trim_end())
        .ok_or(ImportError::UnsupportedObjectFormat(format_bytes))?;
    if selector.is_empty() || selector.contains('\0') {
        return Err(ImportError::InvalidSelector {
            diagnostic: Vec::new(),
        });
    }
    let resolved = process::run(
        repository,
        &["rev-parse", "--verify", "--end-of-options", selector],
    )
    .map_err(|error| match error {
        ImportError::Command { stderr, .. } => ImportError::InvalidSelector { diagnostic: stderr },
        other => other,
    })?;
    let commit = text(&resolved)?.trim_end().to_owned();
    if !format.valid_id(&commit) {
        return Err(ImportError::InvalidObjectData);
    }
    let kind = object(repository, "-t", &commit)?;
    if kind != b"commit\n" {
        return Err(ImportError::NonCommit { kind });
    }
    let commit_bytes = object(repository, "commit", &commit)?;
    let tree = commit_bytes
        .split(|b| *b == b'\n')
        .next()
        .and_then(|line| line.strip_prefix(b"tree "))
        .ok_or(ImportError::InvalidObjectData)?;
    let tree = text(tree)?.to_owned();
    if !format.valid_id(&tree) {
        return Err(ImportError::InvalidObjectData);
    }
    Ok((commit, tree, format))
}

/// Imports one explicitly selected commit from a local repository into root history.
/// Git subprocesses block the calling thread; the caller supplies the async executor.
///
/// # Errors
/// Returns typed Git, manifest, storage, and history conflict errors. Content already
/// inserted may remain unreferenced after failure. History and provenance commit together.
pub async fn import_commit(
    repository: &Path,
    selector: &str,
    session: &SessionId,
    store: &mut WorkspaceStore,
) -> Result<ImportReceipt, ImportError> {
    let (commit, tree, format) = resolve(repository, selector)?;
    validate_tree(repository, &tree, &[], format)?;
    let listing = process::run(
        repository,
        &["ls-tree", "-r", "-t", "-z", "--full-tree", &tree],
    )
    .map_err(|error| match error {
        ImportError::Command { stderr, .. } => ImportError::MissingObject {
            id: tree.clone(),
            diagnostic: stderr,
        },
        other => other,
    })?;
    let mut entries = Vec::new();
    if !listing.is_empty() && listing.last() != Some(&0) {
        return Err(ImportError::InvalidObjectData);
    }
    for record in listing
        .split(|b| *b == 0)
        .filter(|record| !record.is_empty())
    {
        let tab = record
            .iter()
            .position(|b| *b == b'\t')
            .ok_or(ImportError::InvalidObjectData)?;
        let mut header = record[..tab].split(|b| *b == b' ');
        let mode = header.next().ok_or(ImportError::InvalidObjectData)?;
        let kind = header.next().ok_or(ImportError::InvalidObjectData)?;
        let id = text(header.next().ok_or(ImportError::InvalidObjectData)?)?;
        if header.next().is_some() || !format.valid_id(id) {
            return Err(ImportError::InvalidObjectData);
        }
        let path = record[tab + 1..].to_vec();
        let entry_kind = match (mode, kind) {
            (b"040000", b"tree") => {
                validate_tree(repository, id, &path, format)?;
                EntryKind::Directory
            }
            (b"100644" | b"100755", b"blob") => {
                let bytes = object(repository, "blob", id)?;
                let (content, _) = store.insert_content(&bytes).await?;
                EntryKind::RegularFile {
                    content,
                    executable: mode == b"100755",
                }
            }
            (b"120000", b"blob") => EntryKind::Symlink {
                target: SymlinkTarget::new(object(repository, "blob", id)?)
                    .map_err(ImportError::InvalidTarget)?,
            },
            (b"160000", _) => return Err(ImportError::Gitlink { path }),
            _ => {
                return Err(ImportError::UnsupportedMode {
                    mode: mode.to_vec(),
                    path,
                });
            }
        };
        entries.push(ManifestEntry::new(
            WorkspacePath::new(path).map_err(ImportError::InvalidPath)?,
            entry_kind,
        ));
    }
    let manifest = Manifest::new(entries).map_err(ImportError::Manifest)?;
    let manifest_id = manifest.id().map_err(ImportError::Manifest)?;
    let (origin, outcome) = store
        .initialize_git_history(session, &manifest, commit, tree, format)
        .await?;
    Ok(ImportReceipt {
        origin,
        manifest: manifest_id,
        outcome: if outcome == HistoryOutcome::Initialized {
            ImportOutcome::Imported
        } else {
            ImportOutcome::AlreadyImported
        },
    })
}
