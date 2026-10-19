//! The digests that name a package artifact and a package set (FIG-6917).
//!
//! An artifact is one build of a package: its path, the exact artifacts it
//! was built against, its exports and its code. A set is one artifact per
//! package line. Each is named by the blake3 digest of its canonical facts,
//! computed by its owner and never stored inside those facts. Both are
//! plain 32-byte values here so that a store or a wire can carry them
//! without linking the package model.

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
    };
}

digest! {
    /// The digest of one package artifact's canonical facts.
    ArtifactDigest
}

digest! {
    /// The digest of one package set: its members, one artifact per line.
    PackageSetDigest
}

/// A digest's text is not 64 lowercase hex digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a digest is 64 lowercase hex digits")]
pub struct InvalidDigest;
