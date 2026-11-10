mod id;
pub mod storage;
mod workspace;

pub use id::{IdError, IdKind, RevisionId, SessionId, WorkspaceId};
pub use workspace::{Operation, TransitionError, Workspace, WorkspaceStatus};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
