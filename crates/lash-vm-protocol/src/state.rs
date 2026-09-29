//! VM state as the parent holds it: opaque bytes under structural checks.

use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// Which VM state the bytes are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmStateKind {
    /// A suspended execution: a parked process segment or a suspended run.
    Continuation,
    /// A session's guest heap and roots between runs (the RLM worker
    /// envelope).
    Snapshot,
}

/// Whose state it is: the session, or the durable process, the bytes belong
/// to. A worker is handed state only for the owner its lease runs under.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
            *blake3::Hasher::new_derive_key("lash-vm-protocol opaque vm state v1")
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

/// VM state the parent stores, forwards and receives without understanding.
///
/// The parent may check the structure: the byte count, the owner, the VM
/// contract the bytes were written under, the format version, the kind and
/// the hash. It never decodes the bytes. Decoding them means restoring guest
/// values, which validates and compiles regular expressions and checks
/// artifacts against guest-controlled input, and that work belongs in the
/// worker's crash domain, not the parent's. This type offers no decoder, and
/// this crate depends on nothing that has one.
///
/// `vm_contract` is the identity of the VM contracts that decide how the
/// bytes decode (bytecode, continuation and snapshot formats). It is the part
/// of the build identity durable state is fenced by: a live transfer between
/// a parent and its worker also shares the exact [`crate::BuildIdentity`], but
/// state parked by one build and resumed by the next is fenced by the contract
/// it was written under.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpaqueVmState {
    kind: VmStateKind,
    owner: VmOwner,
    vm_contract: String,
    format_version: u32,
    len: u64,
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
    pub kind: VmStateKind,
    pub owner: &'a VmOwner,
    pub vm_contract: &'a str,
    pub format_version: u32,
    pub max_bytes: u64,
}

/// Why opaque state failed a structural check.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum OpaqueStateRefusal {
    #[error("opaque VM state is {found:?}, expected {expected:?}")]
    WrongKind {
        expected: VmStateKind,
        found: VmStateKind,
    },
    #[error("opaque VM state belongs to `{found}`, expected `{expected}`")]
    WrongOwner { expected: VmOwner, found: VmOwner },
    #[error("opaque VM state was written under VM contract `{found}`, expected `{expected}`")]
    WrongVmContract { expected: String, found: String },
    #[error("opaque VM state has format version {found}, expected {expected}")]
    WrongFormatVersion { expected: u32, found: u32 },
    #[error("opaque VM state is {len} bytes, over the {limit}-byte bound")]
    TooLarge { limit: u64, len: u64 },
    #[error("opaque VM state declares {declared} bytes but carries {actual}")]
    LengthMismatch { declared: u64, actual: u64 },
    #[error("opaque VM state hash does not match its bytes")]
    HashMismatch,
}

impl OpaqueVmState {
    /// Seals bytes the worker produced (or the parent read from its store)
    /// under their structural facts.
    pub fn seal(
        kind: VmStateKind,
        owner: VmOwner,
        vm_contract: impl Into<String>,
        format_version: u32,
        bytes: Vec<u8>,
    ) -> Self {
        Self {
            kind,
            owner,
            vm_contract: vm_contract.into(),
            format_version,
            len: bytes.len() as u64,
            hash: StateDigest::of(&bytes),
            bytes,
        }
    }

    /// The structural check, and the only check the parent makes.
    pub fn check(&self, expected: &StateExpectation<'_>) -> Result<(), OpaqueStateRefusal> {
        let actual = self.bytes.len() as u64;
        if self.len != actual {
            return Err(OpaqueStateRefusal::LengthMismatch {
                declared: self.len,
                actual,
            });
        }
        if actual > expected.max_bytes {
            return Err(OpaqueStateRefusal::TooLarge {
                limit: expected.max_bytes,
                len: actual,
            });
        }
        if self.kind != expected.kind {
            return Err(OpaqueStateRefusal::WrongKind {
                expected: expected.kind,
                found: self.kind,
            });
        }
        if &self.owner != expected.owner {
            return Err(OpaqueStateRefusal::WrongOwner {
                expected: expected.owner.clone(),
                found: self.owner.clone(),
            });
        }
        if self.vm_contract != expected.vm_contract {
            return Err(OpaqueStateRefusal::WrongVmContract {
                expected: expected.vm_contract.to_string(),
                found: self.vm_contract.clone(),
            });
        }
        if self.format_version != expected.format_version {
            return Err(OpaqueStateRefusal::WrongFormatVersion {
                expected: expected.format_version,
                found: self.format_version,
            });
        }
        if StateDigest::of(&self.bytes) != self.hash {
            return Err(OpaqueStateRefusal::HashMismatch);
        }
        Ok(())
    }

    pub fn kind(&self) -> VmStateKind {
        self.kind
    }

    pub fn owner(&self) -> &VmOwner {
        &self.owner
    }

    pub fn vm_contract(&self) -> &str {
        &self.vm_contract
    }

    pub fn format_version(&self) -> u32 {
        self.format_version
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
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
