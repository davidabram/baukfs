use std::error::Error;
use std::fmt;
use std::str::FromStr;

const PREFIX: &str = "b3:";
const DIGEST_LEN: usize = 32;
const HEX_DIGITS: usize = DIGEST_LEN * 2;

/// Identity of an exact byte sequence: the BLAKE3 digest of those bytes.
///
/// The canonical text form is `b3:` followed by 64 lowercase hexadecimal
/// digits. Bytes are hashed as given; nothing is normalized or decoded, and
/// no path or name takes part in the hash.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContentId([u8; DIGEST_LEN]);

impl ContentId {
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    /// Parses the canonical text form. Uppercase digits are rejected, so
    /// every identifier has exactly one accepted spelling.
    ///
    /// # Errors
    ///
    /// Returns a [`ContentIdError`] naming the first defect found.
    pub fn parse(text: &str) -> Result<Self, ContentIdError> {
        let Some(hex) = text.strip_prefix(PREFIX) else {
            return Err(ContentIdError::MissingPrefix);
        };
        if let Some((index, character)) = hex
            .char_indices()
            .find(|(_, c)| !matches!(c, '0'..='9' | 'a'..='f'))
        {
            return Err(ContentIdError::InvalidCharacter {
                offset: PREFIX.len() + index,
                character,
            });
        }
        if hex.len() != HEX_DIGITS {
            return Err(ContentIdError::InvalidLength { digits: hex.len() });
        }
        let mut digest = [0; DIGEST_LEN];
        for (byte, [high, low]) in digest.iter_mut().zip(hex.as_bytes().as_chunks::<2>().0) {
            *byte = nibble(*high) << 4 | nibble(*low);
        }
        Ok(Self(digest))
    }

    #[must_use]
    pub fn digest(&self) -> &[u8; DIGEST_LEN] {
        &self.0
    }
}

/// Value of one already-validated lowercase hexadecimal digit.
fn nibble(digit: u8) -> u8 {
    match digit {
        b'0'..=b'9' => digit - b'0',
        _ => digit - b'a' + 10,
    }
}

impl fmt::Display for ContentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(PREFIX)?;
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Debug for ContentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContentId({self})")
    }
}

impl FromStr for ContentId {
    type Err = ContentIdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentIdError {
    /// The text does not start with `b3:`.
    MissingPrefix,
    /// A character after the prefix is not a lowercase hexadecimal digit.
    InvalidCharacter {
        /// Byte offset of the character in the rejected text.
        offset: usize,
        character: char,
    },
    /// The prefix is followed by a number of digits other than 64.
    InvalidLength { digits: usize },
}

impl fmt::Display for ContentIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPrefix => write!(f, "content id does not start with {PREFIX:?}"),
            Self::InvalidCharacter { offset, character } => write!(
                f,
                "content id has {character:?} at byte {offset}, expected a lowercase hexadecimal digit"
            ),
            Self::InvalidLength { digits } => write!(
                f,
                "content id has {digits} hexadecimal digits, expected {HEX_DIGITS}"
            ),
        }
    }
}

impl Error for ContentIdError {}
