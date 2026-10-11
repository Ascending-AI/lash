//! Whose state a worker holds, and the seal over a graph state's
//! structural facts.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Whose state it is: the session, or the durable process, the bytes belong
/// to. A worker is handed state only for the owner its lease runs under.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct VmOwner(String);

impl VmOwner {
    pub fn new(owner: impl Into<String>) -> Self {
        Self(owner.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for VmOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A blake3 digest, domain-separated: the seal over a graph state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StateDigest([u8; 32]);

impl StateDigest {
    /// The seal over a graph state's structural facts, in its own domain.
    pub(crate) fn of_graph_facts(facts: &[u8]) -> Self {
        Self(
            *blake3::Hasher::new_derive_key("lash-vm-protocol graph state")
                .update(facts)
                .finalize()
                .as_bytes(),
        )
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn from_hex(text: &str) -> Option<Self> {
        if text.len() != 64 {
            return None;
        }
        let mut digest = [0_u8; 32];
        for (index, slot) in digest.iter_mut().enumerate() {
            *slot = u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok()?;
        }
        Some(Self(digest))
    }
}

impl Serialize for StateDigest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for StateDigest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::from_hex(&text)
            .ok_or_else(|| serde::de::Error::custom("state digest is not 64 hex digits"))
    }
}
