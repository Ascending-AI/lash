//! Invocation and awakeable identifiers.
//!
//! An invocation id is 24 bytes on the wire (`StartMessage.id`): an 8-byte
//! partition key and a 16-byte uuid, as `restate-server` lays it out. Its
//! printed form is `inv_1` and the lowercase hex of those bytes. Both are
//! drawn from the server's seeded generator, so a seed reproduces its ids.
//!
//! An awakeable id is minted by the SDK, not the server: `sign_1` followed by
//! the URL-safe base64 of the owning invocation's id bytes and the awakeable's
//! 32-bit big-endian signal index. The server only decodes it to route a
//! `CompleteAwakeableCommand` to its signal.

use base64::Engine as _;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use bytes::Bytes;

pub const INVOCATION_ID_LEN: usize = 24;
const INVOCATION_PREFIX: &str = "inv_1";
const AWAKEABLE_PREFIX: &str = "sign_1";
const DEPLOYMENT_PREFIX: &str = "dp_";

const URL_SAFE_INDIFFERENT: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new()
        .with_encode_padding(true)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// An invocation's identity: its wire bytes and its printed id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InvocationId {
    bytes: Bytes,
    printed: String,
}

impl InvocationId {
    pub fn from_bytes(bytes: [u8; INVOCATION_ID_LEN]) -> Self {
        let mut printed = String::with_capacity(INVOCATION_PREFIX.len() + 2 * INVOCATION_ID_LEN);
        printed.push_str(INVOCATION_PREFIX);
        for byte in bytes {
            printed.push_str(&format!("{byte:02x}"));
        }
        Self {
            bytes: Bytes::copy_from_slice(&bytes),
            printed,
        }
    }

    pub fn parse(printed: &str) -> Option<Self> {
        let hex = printed.strip_prefix(INVOCATION_PREFIX)?;
        if hex.len() != 2 * INVOCATION_ID_LEN {
            return None;
        }
        let mut bytes = [0_u8; INVOCATION_ID_LEN];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(hex.get(2 * index..2 * index + 2)?, 16).ok()?;
        }
        Some(Self::from_bytes(bytes))
    }

    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    pub fn as_str(&self) -> &str {
        &self.printed
    }
}

impl std::fmt::Display for InvocationId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.printed)
    }
}

/// A registered deployment's id: `dp_` plus seeded hex, minted once per
/// registration from the server's seed and the deployment's index in
/// registration order.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeploymentId(String);

impl DeploymentId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DeploymentId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A decoded awakeable id: the invocation that owns it and its signal index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AwakeableTarget {
    pub invocation: InvocationId,
    pub signal_id: u32,
}

pub fn decode_awakeable_id(id: &str) -> Option<AwakeableTarget> {
    let encoded = id.strip_prefix(AWAKEABLE_PREFIX)?;
    let raw = URL_SAFE_INDIFFERENT.decode(encoded).ok()?;
    if raw.len() != INVOCATION_ID_LEN + 4 {
        return None;
    }
    let (invocation, signal) = raw.split_at(INVOCATION_ID_LEN);
    let invocation: [u8; INVOCATION_ID_LEN] = invocation.try_into().ok()?;
    let signal: [u8; 4] = signal.try_into().ok()?;
    Some(AwakeableTarget {
        invocation: InvocationId::from_bytes(invocation),
        signal_id: u32::from_be_bytes(signal),
    })
}

/// The seeded source of every identifier and random seed the server mints.
///
/// SplitMix64: tiny, stateless per draw, and stable across platforms, so a
/// seed names the same ids everywhere.
#[derive(Clone, Debug)]
pub struct SeededIds {
    seed: u64,
    state: u64,
}

impl SeededIds {
    pub fn new(seed: u64) -> Self {
        Self { seed, state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn invocation_id(&mut self) -> InvocationId {
        let mut bytes = [0_u8; INVOCATION_ID_LEN];
        for chunk in bytes.chunks_mut(8) {
            let draw = self.next_u64().to_be_bytes();
            chunk.copy_from_slice(&draw[..chunk.len()]);
        }
        InvocationId::from_bytes(bytes)
    }

    /// The invocation id `material` names under this generator's seed.
    ///
    /// Ids derive from what an invocation *is* — the call that made it, its
    /// workflow key, its idempotency key — not from the order concurrent
    /// handlers happened to create invocations in, so one seed names the
    /// same invocation the same way on every run. Restate derives workflow
    /// and idempotent ids from their keys the same way.
    pub fn derive(&self, material: &[&[u8]]) -> (InvocationId, u64) {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for part in material {
            for byte in (part.len() as u64).to_be_bytes().iter().chain(part.iter()) {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0100_0000_01b3);
            }
        }
        let mut expand = SeededIds::new(self.seed ^ hash);
        let mut bytes = [0_u8; INVOCATION_ID_LEN];
        for chunk in bytes.chunks_mut(8) {
            let draw = expand.next_u64().to_be_bytes();
            chunk.copy_from_slice(&draw[..chunk.len()]);
        }
        (InvocationId::from_bytes(bytes), expand.next_u64())
    }

    /// The deployment id of the `ordinal`th registration under this seed.
    /// Derived, not drawn: minting it consumes no other id's draws.
    pub fn deployment_id(&self, ordinal: u64) -> DeploymentId {
        let draw = self.derive(&[b"deployment", &ordinal.to_be_bytes()]).1;
        DeploymentId(format!("{DEPLOYMENT_PREFIX}{draw:016x}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printed_ids_round_trip_and_are_seed_stable() {
        let mut first = SeededIds::new(7);
        let mut second = SeededIds::new(7);
        let id = first.invocation_id();
        assert_eq!(id, second.invocation_id());
        assert_eq!(InvocationId::parse(id.as_str()), Some(id.clone()));
        assert_ne!(id, first.invocation_id());
    }

    #[test]
    fn awakeable_ids_decode_to_their_invocation_and_signal() {
        let id = SeededIds::new(3).invocation_id();
        let mut raw = id.bytes().to_vec();
        raw.extend_from_slice(&17_u32.to_be_bytes());
        let awakeable = format!("sign_1{}", URL_SAFE_INDIFFERENT.encode(&raw));
        assert_eq!(
            decode_awakeable_id(&awakeable),
            Some(AwakeableTarget {
                invocation: id,
                signal_id: 17
            })
        );
        assert_eq!(decode_awakeable_id("prom_1abc"), None);
    }
}
