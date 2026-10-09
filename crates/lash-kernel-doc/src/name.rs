//! Names and content-addressed identities.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Implements `JsonSchema` for a type stored as a string of one pattern.
macro_rules! string_schema {
    ($type:ty, $name:literal, $pattern:literal) => {
        impl schemars::JsonSchema for $type {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                $name.into()
            }

            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({ "type": "string", "pattern": $pattern })
            }
        }
    };
}
pub(crate) use string_schema;

/// A variable, parameter or declared-function name.
///
/// Any text is a name; the kernel gives none of them meaning. Structural
/// validation refuses the empty name.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct Name(String);

impl Name {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for Name {
    fn from(name: &str) -> Self {
        Self::new(name)
    }
}

/// A text that is not a qualified name.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{text}` is not a qualified name: write identifiers joined by `.`")]
pub struct InvalidQualifiedName {
    pub text: String,
}

/// The name of a library function or an effect: one or more identifiers
/// (`[A-Za-z_][A-Za-z0-9_]*`) joined by `.`, as in `text.len` or
/// `regex.ecma.exec`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct QualifiedName(String);

string_schema!(
    QualifiedName,
    "QualifiedName",
    r"^[A-Za-z_][A-Za-z0-9_]*(\.[A-Za-z_][A-Za-z0-9_]*)*$"
);

impl QualifiedName {
    pub fn new(name: impl Into<String>) -> Result<Self, InvalidQualifiedName> {
        let text = name.into();
        if text.split('.').all(is_identifier) {
            Ok(Self(text))
        } else {
            Err(InvalidQualifiedName { text })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The identifiers the name is made of, in order.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('.')
    }
}

impl fmt::Display for QualifiedName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for QualifiedName {
    type Error = InvalidQualifiedName;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::new(text)
    }
}

impl From<QualifiedName> for String {
    fn from(name: QualifiedName) -> Self {
        name.0
    }
}

/// The name of a library function.
pub type FunctionName = QualifiedName;
/// The name of an effect a host supplies.
pub type EffectName = QualifiedName;

/// Whether `text` is one identifier: `[A-Za-z_][A-Za-z0-9_]*`.
pub(crate) fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A text that is not a SHA-256 digest in lower-case hexadecimal.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{text}` is not an identity: write 64 lower-case hexadecimal digits")]
pub struct InvalidIdentity {
    pub text: String,
}

fn parse_digest(text: &str) -> Result<[u8; 32], InvalidIdentity> {
    let invalid = || InvalidIdentity {
        text: text.to_string(),
    };
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return Err(invalid());
    }
    let mut digest = [0u8; 32];
    for (byte, pair) in digest.iter_mut().zip(bytes.as_chunks::<2>().0) {
        let high = hex_value(pair[0]).ok_or_else(invalid)?;
        let low = hex_value(pair[1]).ok_or_else(invalid)?;
        *byte = (high << 4) | low;
    }
    Ok(digest)
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

pub(crate) fn write_hex(bytes: &[u8], out: &mut String) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
}

pub(crate) fn parse_hex(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let (pairs, rest) = bytes.as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    pairs
        .iter()
        .map(|pair| Some((hex_value(pair[0])? << 4) | hex_value(pair[1])?))
        .collect()
}

macro_rules! identity {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name([u8; 32]);

        impl JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({ "type": "string", "pattern": "^[0-9a-f]{64}$" })
            }
        }

        impl $name {
            pub fn from_bytes(digest: [u8; 32]) -> Self {
                Self(digest)
            }

            pub fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }

            /// Reads 64 lower-case hexadecimal digits.
            pub fn parse(text: &str) -> Result<Self, InvalidIdentity> {
                parse_digest(text).map(Self)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let mut text = String::with_capacity(64);
                write_hex(&self.0, &mut text);
                f.write_str(&text)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({self})", stringify!($name))
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidIdentity;

            fn try_from(text: String) -> Result<Self, Self::Error> {
                Self::parse(&text)
            }
        }

        impl From<$name> for String {
            fn from(identity: $name) -> Self {
                identity.to_string()
            }
        }
    };
}

identity!(
    /// The content-addressed identity of a library function: the SHA-256 of
    /// its definition's canonical form (`K-ID-002`).
    FunctionId
);
identity!(
    /// A document's behavioural identity: the SHA-256 of its canonical form,
    /// which holds no annotation (`K-ID-001`).
    DocumentId
);
