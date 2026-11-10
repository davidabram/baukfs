use std::error::Error;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdKind {
    Workspace,
    Revision,
    Session,
}

impl fmt::Display for IdKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Workspace => "workspace",
            Self::Revision => "revision",
            Self::Session => "session",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdError {
    Empty {
        kind: IdKind,
    },
    ControlCharacter {
        kind: IdKind,
        /// Byte offset of the first control character in the rejected value.
        offset: usize,
        character: char,
    },
}

impl IdError {
    #[must_use]
    pub fn kind(&self) -> IdKind {
        match self {
            Self::Empty { kind } | Self::ControlCharacter { kind, .. } => *kind,
        }
    }
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty { kind } => write!(f, "{kind} id is empty"),
            Self::ControlCharacter {
                kind,
                offset,
                character,
            } => write!(
                f,
                "{kind} id contains control character {character:?} at byte {offset}"
            ),
        }
    }
}

impl Error for IdError {}

fn validate(kind: IdKind, value: &str) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError::Empty { kind });
    }
    match value.char_indices().find(|(_, c)| c.is_control()) {
        Some((offset, character)) => Err(IdError::ControlCharacter {
            kind,
            offset,
            character,
        }),
        None => Ok(()),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceId(String);

impl WorkspaceId {
    /// # Errors
    ///
    /// Returns [`IdError`] if `value` is empty or contains a control character.
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        validate(IdKind::Workspace, &value)?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevisionId(String);

impl RevisionId {
    /// # Errors
    ///
    /// Returns [`IdError`] if `value` is empty or contains a control character.
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        validate(IdKind::Revision, &value)?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionId(String);

impl SessionId {
    /// # Errors
    ///
    /// Returns [`IdError`] if `value` is empty or contains a control character.
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        validate(IdKind::Session, &value)?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
