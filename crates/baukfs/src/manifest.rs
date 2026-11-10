use std::error::Error;
use std::fmt;
use std::str::FromStr;

use crate::{ContentId, ContentIdError};

const DOMAIN: &[u8] = b"baukfs-manifest\0";
const VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspacePath(Vec<u8>);

impl WorkspacePath {
    /// # Errors
    /// Rejects empty, absolute, NUL-containing, or noncanonical component paths.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, ByteStringError> {
        let bytes = bytes.into();
        validate_bytes(&bytes)?;
        if bytes[0] == b'/' {
            return Err(ByteStringError::AbsolutePath);
        }
        for component in bytes.split(|byte| *byte == b'/') {
            match component {
                b"" => return Err(ByteStringError::EmptyComponent),
                b"." | b".." => return Err(ByteStringError::DotComponent),
                _ => {}
            }
        }
        Ok(Self(bytes))
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SymlinkTarget(Vec<u8>);

impl SymlinkTarget {
    /// # Errors
    /// Rejects empty targets and targets containing NUL; performs no resolution.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, ByteStringError> {
        let bytes = bytes.into();
        validate_bytes(&bytes)?;
        Ok(Self(bytes))
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

fn validate_bytes(bytes: &[u8]) -> Result<(), ByteStringError> {
    if bytes.is_empty() {
        return Err(ByteStringError::Empty);
    }
    if let Some(offset) = bytes.iter().position(|byte| *byte == 0) {
        return Err(ByteStringError::Nul { offset });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteStringError {
    Empty,
    Nul { offset: usize },
    AbsolutePath,
    EmptyComponent,
    DotComponent,
}

impl fmt::Display for ByteStringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("byte string is empty"),
            Self::Nul { offset } => write!(f, "byte string contains NUL at byte {offset}"),
            Self::AbsolutePath => f.write_str("workspace path is absolute"),
            Self::EmptyComponent => f.write_str("workspace path contains an empty component"),
            Self::DotComponent => f.write_str("workspace path contains a dot or dot-dot component"),
        }
    }
}

impl Error for ByteStringError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryKind {
    RegularFile {
        content: ContentId,
        executable: bool,
    },
    Directory,
    Symlink {
        target: SymlinkTarget,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestEntry {
    path: WorkspacePath,
    kind: EntryKind,
}

impl ManifestEntry {
    #[must_use]
    pub fn new(path: WorkspacePath, kind: EntryKind) -> Self {
        Self { path, kind }
    }

    #[must_use]
    pub fn path(&self) -> &WorkspacePath {
        &self.path
    }

    #[must_use]
    pub fn kind(&self) -> &EntryKind {
        &self.kind
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    entries: Vec<ManifestEntry>,
}

impl Manifest {
    /// # Errors
    /// Rejects duplicates, missing parent directories, and non-directory parents.
    pub fn new(mut entries: Vec<ManifestEntry>) -> Result<Self, ManifestError> {
        entries.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        let manifest = Self { entries };
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<(), ManifestError> {
        for pair in self.entries.windows(2) {
            match pair[0].path.cmp(&pair[1].path) {
                std::cmp::Ordering::Equal => {
                    return Err(ManifestError::DuplicatePath(pair[1].path.clone()));
                }
                std::cmp::Ordering::Greater => return Err(ManifestError::NoncanonicalOrder),
                std::cmp::Ordering::Less => {}
            }
        }
        for entry in &self.entries {
            if let Some(separator) = entry.path.0.iter().rposition(|byte| *byte == b'/') {
                let parent = &entry.path.0[..separator];
                match self.lookup_bytes(parent) {
                    None => return Err(ManifestError::MissingParent(entry.path.clone())),
                    Some(entry) if entry.kind != EntryKind::Directory => {
                        return Err(ManifestError::NonDirectoryParent(entry.path.clone()));
                    }
                    Some(_) => {}
                }
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn entries(&self) -> &[ManifestEntry] {
        &self.entries
    }

    #[must_use]
    pub fn lookup(&self, path: &WorkspacePath) -> Option<&ManifestEntry> {
        self.lookup_bytes(path.as_bytes())
    }

    fn lookup_bytes(&self, path: &[u8]) -> Option<&ManifestEntry> {
        self.entries
            .binary_search_by(|entry| entry.path.as_bytes().cmp(path))
            .ok()
            .map(|index| &self.entries[index])
    }

    /// # Errors
    /// Returns `LengthOverflow` if a count or byte length cannot fit in a u64.
    pub fn encode(&self) -> Result<Vec<u8>, ManifestError> {
        let mut output = DOMAIN.to_vec();
        output.extend(VERSION.to_le_bytes());
        write_length(&mut output, self.entries.len())?;
        for entry in &self.entries {
            write_bytes(&mut output, entry.path.as_bytes())?;
            match &entry.kind {
                EntryKind::RegularFile {
                    content,
                    executable,
                } => {
                    output.push(1);
                    output.extend(content.digest());
                    output.push(u8::from(*executable));
                }
                EntryKind::Directory => output.push(2),
                EntryKind::Symlink { target } => {
                    output.push(3);
                    write_bytes(&mut output, target.as_bytes())?;
                }
            }
        }
        Ok(output)
    }

    /// # Errors
    /// Rejects malformed formats, noncanonical ordering, and invalid domain values.
    pub fn decode(bytes: &[u8]) -> Result<Self, ManifestError> {
        let mut reader = Reader { remaining: bytes };
        if reader.take(DOMAIN.len())? != DOMAIN {
            return Err(ManifestError::InvalidDomain);
        }
        let version = u32::from_le_bytes(reader.array()?);
        if version != VERSION {
            return Err(ManifestError::UnknownVersion(version));
        }
        let count = reader.length()?;
        // Each entry needs a path length and kind tag, even before path validation.
        if count > reader.remaining.len() / 9 {
            return Err(ManifestError::Truncated);
        }
        let mut entries = Vec::new();
        for _ in 0..count {
            let path = WorkspacePath::new(reader.bytes()?).map_err(ManifestError::InvalidPath)?;
            let kind = match reader.array::<1>()?[0] {
                1 => {
                    let content = ContentId::from_digest(reader.array()?);
                    let executable = match reader.array::<1>()?[0] {
                        0 => false,
                        1 => true,
                        flag => return Err(ManifestError::InvalidExecutableFlag(flag)),
                    };
                    EntryKind::RegularFile {
                        content,
                        executable,
                    }
                }
                2 => EntryKind::Directory,
                3 => EntryKind::Symlink {
                    target: SymlinkTarget::new(reader.bytes()?)
                        .map_err(ManifestError::InvalidTarget)?,
                },
                tag => return Err(ManifestError::UnknownTag(tag)),
            };
            entries.push(ManifestEntry::new(path, kind));
        }
        if !reader.remaining.is_empty() {
            return Err(ManifestError::TrailingBytes);
        }
        let manifest = Self { entries };
        manifest.validate()?;
        Ok(manifest)
    }

    /// # Errors
    /// Returns the same length-conversion errors as [`Self::encode`].
    pub fn id(&self) -> Result<ManifestId, ManifestError> {
        Ok(ManifestId(ContentId::of(&self.encode()?)))
    }
}

fn write_length(output: &mut Vec<u8>, length: usize) -> Result<(), ManifestError> {
    let length = u64::try_from(length).map_err(|_| ManifestError::LengthOverflow)?;
    output.extend(length.to_le_bytes());
    Ok(())
}

fn write_bytes(output: &mut Vec<u8>, bytes: &[u8]) -> Result<(), ManifestError> {
    write_length(output, bytes.len())?;
    output.extend(bytes);
    Ok(())
}

struct Reader<'a> {
    remaining: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], ManifestError> {
        let (field, rest) = self
            .remaining
            .split_at_checked(length)
            .ok_or(ManifestError::Truncated)?;
        self.remaining = rest;
        Ok(field)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], ManifestError> {
        self.take(N)?
            .try_into()
            .map_err(|_| ManifestError::Truncated)
    }

    fn length(&mut self) -> Result<usize, ManifestError> {
        usize::try_from(u64::from_le_bytes(self.array()?))
            .map_err(|_| ManifestError::LengthOverflow)
    }

    fn bytes(&mut self) -> Result<&'a [u8], ManifestError> {
        let length = self.length()?;
        self.take(length)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestError {
    DuplicatePath(WorkspacePath),
    MissingParent(WorkspacePath),
    NonDirectoryParent(WorkspacePath),
    NoncanonicalOrder,
    InvalidPath(ByteStringError),
    InvalidTarget(ByteStringError),
    InvalidDomain,
    UnknownVersion(u32),
    UnknownTag(u8),
    InvalidExecutableFlag(u8),
    LengthOverflow,
    Truncated,
    TrailingBytes,
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicatePath(path) => write!(f, "duplicate manifest path: {path:?}"),
            Self::MissingParent(path) => write!(f, "missing parent directory for {path:?}"),
            Self::NonDirectoryParent(path) => {
                write!(f, "manifest parent is not a directory: {path:?}")
            }
            Self::NoncanonicalOrder => f.write_str("manifest paths are not in canonical order"),
            Self::InvalidPath(error) => write!(f, "invalid manifest path: {error}"),
            Self::InvalidTarget(error) => write!(f, "invalid symlink target: {error}"),
            Self::InvalidDomain => f.write_str("invalid manifest domain header"),
            Self::UnknownVersion(version) => write!(f, "unknown manifest version: {version}"),
            Self::UnknownTag(tag) => write!(f, "unknown manifest entry tag: {tag}"),
            Self::InvalidExecutableFlag(flag) => write!(f, "invalid executable flag: {flag}"),
            Self::LengthOverflow => f.write_str("manifest length exceeds integer representation"),
            Self::Truncated => f.write_str("truncated manifest encoding"),
            Self::TrailingBytes => f.write_str("trailing bytes after manifest entries"),
        }
    }
}

impl Error for ManifestError {}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ManifestId(ContentId);

impl ManifestId {
    /// # Errors
    /// Rejects text outside the canonical `b3:` plus 64 lowercase hex digit form.
    pub fn parse(text: &str) -> Result<Self, ManifestIdError> {
        ContentId::parse(text).map(Self).map_err(ManifestIdError)
    }

    #[must_use]
    pub fn digest(&self) -> &[u8; 32] {
        self.0.digest()
    }
}

impl fmt::Display for ManifestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for ManifestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ManifestId({self})")
    }
}

impl FromStr for ManifestId {
    type Err = ManifestIdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManifestIdError(ContentIdError);

impl ManifestIdError {
    #[must_use]
    pub fn kind(&self) -> ContentIdError {
        self.0
    }
}

impl fmt::Display for ManifestIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid manifest id: {}", self.0)
    }
}

impl Error for ManifestIdError {}
