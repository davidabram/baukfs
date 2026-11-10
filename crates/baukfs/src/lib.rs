mod content;
pub mod git_import;
mod id;
mod manifest;
pub mod restoration;
pub mod storage;
mod workspace;

pub use content::{ContentId, ContentIdError};
pub use id::{IdError, IdKind, RevisionId, SessionId, WorkspaceId};
pub use manifest::{
    ByteStringError, EntryKind, Manifest, ManifestEntry, ManifestError, ManifestId,
    ManifestIdError, SymlinkTarget, WorkspacePath,
};
pub use workspace::{Operation, TransitionError, Workspace, WorkspaceStatus};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
