//! T04 spike: when is a managed workspace safe to capture or seal?
//!
//! Not part of the production `baukfs` API. See `docs/runner-quiescence.md`.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("baukfs-runner-spike supports Linux and macOS only");

pub mod admission;
#[cfg(target_os = "linux")]
pub mod linux;
pub mod ownership;
pub mod process;
pub mod record;
pub mod recovery;
pub mod supervisor;
pub mod token;
