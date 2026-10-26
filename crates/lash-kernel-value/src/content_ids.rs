//! Content identities. Canonical hashing is implemented by pkg-doc.
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

macro_rules! digest {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; 32]);

        impl $name {
            pub const fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }

            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }

            /// The first twelve hex digits: enough to tell two digests
            /// apart in a message.
            pub fn short(&self) -> String {
                let mut text = self.to_string();
                text.truncate(12);
                text
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for byte in self.0 {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.short())
            }
        }

        impl std::str::FromStr for $name {
            type Err = InvalidDigest;

            fn from_str(text: &str) -> Result<Self, InvalidDigest> {
                let bytes = text.as_bytes();
                if bytes.len() != 64 {
                    return Err(InvalidDigest);
                }
                let mut digest = [0u8; 32];
                for (at, pair) in bytes.chunks_exact(2).enumerate() {
                    let digit = |byte: u8| match byte {
                        b'0'..=b'9' => Ok(byte - b'0'),
                        b'a'..=b'f' => Ok(byte - b'a' + 10),
                        _ => Err(InvalidDigest),
                    };
                    digest[at] = digit(pair[0])? << 4 | digit(pair[1])?;
                }
                Ok(Self(digest))
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = String::deserialize(deserializer)?;
                text.parse().map_err(serde::de::Error::custom)
            }
        }

        crate::names::string_schema!($name, stringify!($name), "^[0-9a-f]{64}$");
    };
}

/// A content identity must be exactly 64 lowercase hexadecimal characters.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a digest is exactly 64 lowercase hexadecimal characters")]
pub struct InvalidDigest;
digest!(/// Immutable linked code identity.
 LinkId);
digest!(/// Immutable package content identity.
 PackageId);

digest!(/// Identity of one immutable build-owned helper library, including its static closure.
 HelperLibraryId);

impl HelperLibraryId {
    /// Canonical member-set identity. Callers supply the validated, closed member
    /// universe; order and duplicate observations do not affect its identity.
    pub fn of_members(members: impl IntoIterator<Item = crate::names::FunctionId>) -> Self {
        let members: std::collections::BTreeSet<_> = members.into_iter().collect();
        let domain = b"lash.helper-library";
        let mut hash = blake3::Hasher::new();
        hash.update(&(domain.len() as u64).to_be_bytes());
        hash.update(domain);
        hash.update(&(members.len() as u64).to_be_bytes());
        for member in members {
            hash.update(member.as_bytes());
        }
        Self::from_bytes(*hash.finalize().as_bytes())
    }
}
