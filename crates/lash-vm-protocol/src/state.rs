//! A parked run as the parent holds it: opaque bytes under structural checks.

use base64::Engine as _;
use lash_sansio::VersionRange;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

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

/// The blake3 digest of the state bytes, domain-separated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StateDigest([u8; 32]);

impl StateDigest {
    pub fn of(bytes: &[u8]) -> Self {
        Self(
            *blake3::Hasher::new_derive_key("lash-vm-protocol parked run v1")
                .update(bytes)
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

/// A parked run the parent stores, forwards and receives without
/// understanding.
///
/// The parent may check the structure: the byte count, the owner, the kernel
/// version the bytes were written under, the document they were parked
/// under, and the hash. It never decodes the bytes. Decoding them rebuilds
/// guest values and compiles the document against guest-controlled input,
/// and that work belongs in the worker's crash domain, not the parent's.
/// This type offers no decoder, and this crate depends on nothing that has
/// one.
///
/// A live transfer between a parent and its worker also checks the wire
/// protocol version; parked state crosses builds within the kernel versions
/// a reader admits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpaqueVmState {
    owner: VmOwner,
    /// The kernel version the run was parked under.
    kernel: u32,
    /// The identity of the document the run was parked under, as the worker
    /// stated it.
    document: String,
    hash: StateDigest,
    #[serde(
        serialize_with = "serialize_bytes",
        deserialize_with = "deserialize_bytes"
    )]
    bytes: Vec<u8>,
}

/// What a holder of opaque state expects it to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateExpectation<'a> {
    pub owner: &'a VmOwner,
    /// The kernel versions the holder's workers resume.
    pub kernel: VersionRange,
    /// The document the run is resumed under, when the holder knows it.
    pub document: Option<&'a str>,
    pub max_bytes: u64,
}

/// Why opaque state failed a structural check.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OpaqueStateRefusal {
    #[error("the parked run belongs to `{found}`, expected `{expected}`")]
    WrongOwner { expected: VmOwner, found: VmOwner },
    #[error("the parked run was written under kernel version {found}, outside read range {reads}")]
    KernelOutsideReadRange { found: u32, reads: VersionRange },
    #[error("the parked run was parked under document {found}, expected {expected}")]
    WrongDocument { expected: String, found: String },
    #[error("the parked run is {len} bytes, over the {limit}-byte bound")]
    TooLarge { limit: u64, len: u64 },
    #[error("the parked run's hash does not match its bytes")]
    HashMismatch,
}

impl OpaqueVmState {
    /// Seals bytes the worker produced (or the parent read from its store)
    /// under their structural facts.
    pub fn seal(owner: VmOwner, kernel: u32, document: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            owner,
            kernel,
            document: document.into(),
            hash: StateDigest::of(&bytes),
            bytes,
        }
    }

    /// The structural check, and the only check the parent makes.
    pub fn check(&self, expected: &StateExpectation<'_>) -> Result<(), OpaqueStateRefusal> {
        if self.len() > expected.max_bytes {
            return Err(OpaqueStateRefusal::TooLarge {
                limit: expected.max_bytes,
                len: self.len(),
            });
        }
        if &self.owner != expected.owner {
            return Err(OpaqueStateRefusal::WrongOwner {
                expected: expected.owner.clone(),
                found: self.owner.clone(),
            });
        }
        if !expected.kernel.contains(self.kernel) {
            return Err(OpaqueStateRefusal::KernelOutsideReadRange {
                found: self.kernel,
                reads: expected.kernel,
            });
        }
        if let Some(document) = expected.document
            && document != self.document
        {
            return Err(OpaqueStateRefusal::WrongDocument {
                expected: document.to_owned(),
                found: self.document.clone(),
            });
        }
        if StateDigest::of(&self.bytes) != self.hash {
            return Err(OpaqueStateRefusal::HashMismatch);
        }
        Ok(())
    }

    pub fn owner(&self) -> &VmOwner {
        &self.owner
    }

    /// The kernel version the run was parked under.
    pub fn kernel(&self) -> u32 {
        self.kernel
    }

    /// The identity of the document the run was parked under.
    pub fn document(&self) -> &str {
        &self.document
    }

    pub fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn hash(&self) -> StateDigest {
        self.hash
    }

    /// The sealed bytes, for the worker that opens them and for a store that
    /// persists them. Holding them confers no way to decode them.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// Raw bytes in a binary format; base64 text in a human-readable one, so a
/// JSON envelope that embeds the state stays compact and readable.
fn serialize_bytes<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    if serializer.is_human_readable() {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    } else {
        serde_bytes::serialize(bytes, serializer)
    }
}

fn deserialize_bytes<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    if deserializer.is_human_readable() {
        let text = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_err(serde::de::Error::custom)
    } else {
        serde_bytes::deserialize(deserializer)
    }
}

#[cfg(test)]
mod tests;
