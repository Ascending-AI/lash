//! The build identity a parent and its worker must share exactly.

use serde::{Deserialize, Serialize};

/// The exact build a worker runs and its parent expects.
///
/// There is no negotiation and no historical reader: during the version freeze
/// shapes change in place, so a worker of any other build is refused at the
/// handshake and every frame it sends is refused at decode. The identity is a
/// caller-supplied string (the host composes it from its binary's build
/// facts); frames carry its 32-byte digest.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BuildIdentity(String);

impl BuildIdentity {
    pub fn new(identity: impl Into<String>) -> Self {
        Self(identity.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The digest every frame header carries.
    pub fn digest(&self) -> [u8; 32] {
        *blake3::Hasher::new_derive_key("lash-vm-protocol build identity v1")
            .update(self.0.as_bytes())
            .finalize()
            .as_bytes()
    }
}

impl std::fmt::Display for BuildIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
