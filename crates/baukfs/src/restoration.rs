//! Restore an explicit immutable revision into an absent native directory.

use std::error::Error;
use std::ffi::{CString, OsStr};
use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};

use crate::storage::{HistoryError, StorageError, WorkspaceStore};
use crate::{ContentId, EntryKind, Manifest, ManifestId, RevisionId};

#[derive(Debug)]
pub enum RestoreFailure {
    Storage(StorageError),
    MissingRevision(RevisionId),
    MissingContent(ContentId),
    InvalidDestination(PathBuf),
    DestinationExists(PathBuf),
    PublicationUnavailable(io::Error),
    Io {
        operation: &'static str,
        path: PathBuf,
        error: io::Error,
    },
}

#[derive(Debug)]
pub struct RestoreError {
    pub failure: RestoreFailure,
    /// A failed explicit cleanup leaves this operation's staging at this path.
    pub cleanup: Option<(PathBuf, io::Error)>,
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "restoration: {:?}; cleanup: {:?}",
            self.failure, self.cleanup
        )
    }
}
impl Error for RestoreError {}
impl From<RestoreFailure> for RestoreError {
    fn from(failure: RestoreFailure) -> Self {
        Self {
            failure,
            cleanup: None,
        }
    }
}
impl From<StorageError> for RestoreFailure {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct RestoreReceipt {
    pub revision: RevisionId,
    pub manifest: ManifestId,
    pub destination: PathBuf,
    pub directories: usize,
    pub regular_files: usize,
    pub symlinks: usize,
}

fn io_failure(operation: &'static str, path: &Path, error: io::Error) -> RestoreFailure {
    RestoreFailure::Io {
        operation,
        path: path.to_owned(),
        error,
    }
}

struct Staging(Option<PathBuf>);
impl Staging {
    fn create(parent: &Path) -> Result<Self, RestoreFailure> {
        let mut random =
            File::open("/dev/urandom").map_err(|e| io_failure("open randomness", parent, e))?;
        loop {
            let mut bytes = [0_u8; 16];
            random
                .read_exact(&mut bytes)
                .map_err(|e| io_failure("read randomness", parent, e))?;
            let name = bytes
                .iter()
                .fold(String::from(".baukfs-restore-"), |mut name, byte| {
                    use fmt::Write;
                    write!(name, "{byte:02x}").expect("writing to String");
                    name
                });
            let path = parent.join(name);
            match DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self(Some(path))),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(io_failure("create staging", &path, e)),
            }
        }
    }
    fn cleanup(&mut self) -> Option<(PathBuf, io::Error)> {
        self.0
            .take()
            .and_then(|path| fs::remove_dir_all(&path).err().map(|e| (path, e)))
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

/// Restores one explicit revision. Filesystem work blocks the calling thread.
/// See `docs/restoration.md` for trusted-parent, permission and durability policy.
///
/// # Errors
/// Returns validated storage errors, missing revisions, exclusive creation/name
/// collision errors, or atomic publication errors. Cleanup failures retain their
/// staging path. No successful publication is followed by fallible operations.
pub async fn restore_revision(
    store: &mut WorkspaceStore,
    revision: &RevisionId,
    destination: &Path,
) -> Result<RestoreReceipt, RestoreError> {
    restore_with(store, revision, destination, |_, _| Ok(())).await
}

#[derive(Clone, Copy)]
enum Phase {
    Entry,
    Publish,
}

async fn restore_with<F: FnMut(Phase, &Path) -> io::Result<()>>(
    store: &mut WorkspaceStore,
    revision: &RevisionId,
    destination: &Path,
    mut hook: F,
) -> Result<RestoreReceipt, RestoreError> {
    let result = store
        .read_revision(revision)
        .await
        .map_err(RestoreFailure::from)?
        .ok_or_else(|| RestoreFailure::MissingRevision(revision.clone()))?;
    let manifest = store
        .read_manifest(result.manifest())
        .await
        .map_err(RestoreFailure::from)?
        .ok_or_else(|| {
            RestoreFailure::Storage(StorageError::History(HistoryError::MissingManifest(
                *result.manifest(),
            )))
        })?;
    let (parent, destination) = destination_path(destination)?;
    match fs::symlink_metadata(&destination) {
        Ok(_) => return Err(RestoreFailure::DestinationExists(destination).into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_failure("inspect destination", &destination, e).into()),
    }
    let mut staging = Staging::create(&parent)?;
    let root = staging.0.as_ref().expect("owned staging");
    let mut receipt = RestoreReceipt {
        revision: result.id().clone(),
        manifest: *result.manifest(),
        destination,
        directories: 0,
        regular_files: 0,
        symlinks: 0,
    };
    let built = async {
        fs::set_permissions(root, Permissions::from_mode(0o700))
            .map_err(|e| io_failure("set staging permissions", root, e))?;
        materialize(store, &manifest, root, &mut receipt, &mut hook).await?;
        hook(Phase::Publish, root).map_err(|e| io_failure("before publication", root, e))?;
        fs::set_permissions(root, Permissions::from_mode(0o755))
            .map_err(|e| io_failure("set root permissions", root, e))?;
        publish(root, &receipt.destination)
    }
    .await;
    if let Err(failure) = built {
        return Err(RestoreError {
            failure,
            cleanup: staging.cleanup(),
        });
    }
    staging.0 = None;
    Ok(receipt)
}

fn destination_path(destination: &Path) -> Result<(PathBuf, PathBuf), RestoreFailure> {
    let invalid = || RestoreFailure::InvalidDestination(destination.to_owned());
    if !matches!(
        destination.components().next_back(),
        Some(Component::Normal(_))
    ) || destination.as_os_str().as_bytes().ends_with(b"/.")
        || destination.as_os_str().as_bytes().ends_with(b"/")
    {
        return Err(invalid());
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent).map_err(|e| io_failure("resolve parent", parent, e))?;
    if !parent.is_dir() {
        return Err(invalid());
    }
    let target = parent.join(destination.file_name().ok_or_else(invalid)?);
    Ok((parent, target))
}

async fn materialize<F: FnMut(Phase, &Path) -> io::Result<()>>(
    store: &WorkspaceStore,
    manifest: &Manifest,
    root: &Path,
    receipt: &mut RestoreReceipt,
    hook: &mut F,
) -> Result<(), RestoreFailure> {
    for entry in manifest.entries() {
        let path = root.join(OsStr::from_bytes(entry.path().as_bytes()));
        hook(Phase::Entry, root).map_err(|e| io_failure("before entry", &path, e))?;
        match entry.kind() {
            EntryKind::Directory => {
                DirBuilder::new()
                    .mode(0o700)
                    .create(&path)
                    .map_err(|e| io_failure("create directory", &path, e))?;
                fs::set_permissions(&path, Permissions::from_mode(0o700))
                    .map_err(|e| io_failure("set private directory permissions", &path, e))?;
                receipt.directories += 1;
            }
            EntryKind::RegularFile {
                content,
                executable,
            } => {
                let bytes = store
                    .read_content(content)
                    .await?
                    .ok_or(RestoreFailure::MissingContent(*content))?;
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)
                    .map_err(|e| io_failure("create file", &path, e))?;
                file.write_all(&bytes)
                    .map_err(|e| io_failure("write file", &path, e))?;
                file.set_permissions(Permissions::from_mode(if *executable {
                    0o755
                } else {
                    0o644
                }))
                .map_err(|e| io_failure("set file permissions", &path, e))?;
                receipt.regular_files += 1;
            }
            EntryKind::Symlink { .. } => {}
        }
    }
    for entry in manifest.entries() {
        if matches!(entry.kind(), EntryKind::Directory) {
            let path = root.join(OsStr::from_bytes(entry.path().as_bytes()));
            fs::set_permissions(&path, Permissions::from_mode(0o755))
                .map_err(|e| io_failure("set directory permissions", &path, e))?;
        }
    }
    for entry in manifest.entries() {
        if let EntryKind::Symlink { target } = entry.kind() {
            let path = root.join(OsStr::from_bytes(entry.path().as_bytes()));
            symlink(OsStr::from_bytes(target.as_bytes()), &path)
                .map_err(|e| io_failure("create symlink", &path, e))?;
            receipt.symlinks += 1;
        }
    }
    Ok(())
}

fn publish(staging: &Path, destination: &Path) -> Result<(), RestoreFailure> {
    let from = CString::new(staging.as_os_str().as_bytes())
        .map_err(|_| RestoreFailure::InvalidDestination(staging.to_owned()))?;
    let to = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| RestoreFailure::InvalidDestination(destination.to_owned()))?;
    // Both pointers are live NUL-terminated paths; the operation never replaces a name.
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(libc::EEXIST | libc::ENOTEMPTY)) {
        Err(RestoreFailure::DestinationExists(destination.to_owned()))
    } else if matches!(
        error.raw_os_error(),
        Some(libc::ENOSYS | libc::EINVAL | libc::ENOTSUP)
    ) {
        Err(RestoreFailure::PublicationUnavailable(error))
    } else {
        Err(io_failure("publish", destination, error))
    }
}

#[cfg(test)]
mod tests;
