//! The page exchange between a worker and its parent (FIG-6195, seam S3 of
//! `docs/design/persistent-heap-seams.md`).
//!
//! Declarations only. Layer hp-paging (FIG-6190) adds
//! `WorkerMessage::Pages { id, request }` and `ParentMessage::Pages { id,
//! reply }` carrying these bodies, beside the synchronous host read: the
//! worker sends one request mid-slice under the run's fence and blocks with
//! its machine's stack intact until the reply, exactly as a host read does.
//!
//! Pages cross as complete generic envelopes with their digests. The worker
//! re-checks every envelope it receives; the parent re-checks every
//! envelope a worker seals. Neither side decodes the other's semantics: the
//! parent checks hashes, the envelope child table and capabilities only.
//!
//! Like the rest of this crate, nothing here reaches the kernel: a page is
//! opaque bytes.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::Detail;

/// Names one page exchange within a lease, so its reply answers exactly
/// it.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct PageExchangeId(pub u64);

/// What authorizes a capability: the activation's lease on its committed
/// root, or its private staging.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PageAuthority {
    /// A store root lease (S6), by its id.
    Root(u64),
    /// The activation's stage lease (S5), by its id.
    Stage(u64),
}

/// A parent-issued capability to read one page: bound to its digest, its
/// authority and the activation epoch, under a MAC whose key never leaves
/// the parent. It is transport authority, never saved state: a fresh
/// activation derives new ones from its leased root. On the worker it is
/// the kernel's opaque 48-byte `PageGrant` (authority 8, epoch 8, MAC 32).
///
/// It crosses as its [`PAGE_CAPABILITY_BYTES`] fixed bytes, a page's
/// worth of them per page every exchange names (FIG-6321): the digest, the
/// authority's kind and id, the epoch and the MAC; in a human-readable
/// payload, as those bytes in hex.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, JsonSchema)]
#[schemars(transform = capability_schema)]
pub struct PageCapability {
    pub page: [u8; 32],
    pub authority: PageAuthority,
    pub epoch: u64,
    pub mac: [u8; 32],
}

// Keep the type-derived fields as encoding metadata: the JSON value is hex,
// but a change inside the packed capability must change the transitive schema.
fn capability_schema(schema: &mut schemars::Schema) {
    let fields = schema.clone();
    *schema = schemars::json_schema!({
        "type": "string",
        "pattern": format!("^[0-9a-fA-F]{{{}}}$", PAGE_CAPABILITY_BYTES * 2),
        "x-lash-packed-fields": fields,
        "x-lash-binary-encoding": "page[32], authority-kind[1] (root=0, stage=1), authority-id[u64-le], epoch[u64-le], mac[32]",
        "x-lash-binary-bytes": PAGE_CAPABILITY_BYTES,
    });
}

/// The bytes a [`PageCapability`] crosses as.
pub const PAGE_CAPABILITY_BYTES: usize = 32 + 1 + 8 + 8 + 32;

impl PageCapability {
    fn to_bytes(self) -> [u8; PAGE_CAPABILITY_BYTES] {
        let mut bytes = [0_u8; PAGE_CAPABILITY_BYTES];
        let (kind, id) = match self.authority {
            PageAuthority::Root(id) => (0, id),
            PageAuthority::Stage(id) => (1, id),
        };
        bytes[..32].copy_from_slice(&self.page);
        bytes[32] = kind;
        bytes[33..41].copy_from_slice(&id.to_le_bytes());
        bytes[41..49].copy_from_slice(&self.epoch.to_le_bytes());
        bytes[49..].copy_from_slice(&self.mac);
        bytes
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != PAGE_CAPABILITY_BYTES {
            return None;
        }
        let word = |at: usize| {
            let mut word = [0_u8; 8];
            word.copy_from_slice(&bytes[at..at + 8]);
            u64::from_le_bytes(word)
        };
        let authority = match bytes[32] {
            0 => PageAuthority::Root(word(33)),
            1 => PageAuthority::Stage(word(33)),
            _ => return None,
        };
        let (mut page, mut mac) = ([0_u8; 32], [0_u8; 32]);
        page.copy_from_slice(&bytes[..32]);
        mac.copy_from_slice(&bytes[49..]);
        Some(Self {
            page,
            authority,
            epoch: word(41),
            mac,
        })
    }
}

impl Serialize for PageCapability {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let bytes = self.to_bytes();
        if serializer.is_human_readable() {
            let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
            serializer.serialize_str(&hex)
        } else {
            serializer.serialize_bytes(&bytes)
        }
    }
}

impl<'de> Deserialize<'de> for PageCapability {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = PageCapability;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "a page capability's {PAGE_CAPABILITY_BYTES} bytes")
            }

            fn visit_bytes<E: serde::de::Error>(self, bytes: &[u8]) -> Result<Self::Value, E> {
                PageCapability::from_bytes(bytes)
                    .ok_or_else(|| E::invalid_length(bytes.len(), &self))
            }

            fn visit_str<E: serde::de::Error>(self, hex: &str) -> Result<Self::Value, E> {
                let digit = |byte: u8| (byte as char).to_digit(16);
                let bytes: Option<Vec<u8>> = hex
                    .len()
                    .is_multiple_of(2)
                    .then(|| {
                        hex.as_bytes()
                            .chunks(2)
                            .map(|pair| Some((digit(pair[0])? * 16 + digit(pair[1])?) as u8))
                            .collect()
                    })
                    .flatten();
                bytes
                    .as_deref()
                    .and_then(PageCapability::from_bytes)
                    .ok_or_else(|| E::invalid_value(serde::de::Unexpected::Str(hex), &self))
            }
        }
        if deserializer.is_human_readable() {
            deserializer.deserialize_str(Visitor)
        } else {
            deserializer.deserialize_bytes(Visitor)
        }
    }
}

/// One page as it crosses: its complete envelope, with its digest.
///
/// Only the envelope crosses (FIG-6321): a page is named by the digest of
/// its envelope, so the receiver derives the digest as it decodes, and a
/// decoded page's `id` is its envelope's digest by construction.
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(with = "Vec<u8>")]
pub struct PageEnvelope {
    pub id: [u8; 32],
    pub envelope: Vec<u8>,
}

/// The domain a page's digest is hashed in: a page is stored as the blob of
/// its envelope, so its digest is that blob's (`lash-core-store`'s
/// `BlobRef::digest_for_content`). It must equal
/// `lash-checkpoint-encoder`'s blob domain: the law
/// `a_transported_page_keeps_its_blob_address_and_token_vector` in `lash-vm-worker`
/// pins it.
/// version_surface = "coexist"
/// version_guard(items(PAGE_DIGEST_DOMAIN), items(digest))
const PAGE_DIGEST_DOMAIN: &str = "lash-blob/v2";

impl PageEnvelope {
    /// `envelope`, named by its digest.
    #[must_use]
    pub fn new(envelope: Vec<u8>) -> Self {
        Self {
            id: Self::digest(&envelope),
            envelope,
        }
    }

    /// The digest that names `envelope`.
    #[must_use]
    pub fn digest(envelope: &[u8]) -> [u8; 32] {
        lash_sansio::core_support::blake3_domain_hash(PAGE_DIGEST_DOMAIN, envelope)
    }
}

impl Serialize for PageEnvelope {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde_bytes::Bytes::new(&self.envelope).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PageEnvelope {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        serde_bytes::ByteBuf::deserialize(deserializer).map(|bytes| Self::new(bytes.into_vec()))
    }
}

/// A worker's page request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PageRequest {
    /// Read these pages: at most the protocol's batch page and byte bounds.
    Read { pages: Vec<PageCapability> },
    /// Seal these pages into the activation's staging. Each child of each
    /// envelope must be authorized by a capability in `children` or be an
    /// earlier page of this batch.
    Seal {
        pages: Vec<PageEnvelope>,
        children: Vec<PageCapability>,
    },
}

/// Pages a worker sealed that cross with the frame that hands its run to
/// the parent (a `Parked`, `Sealed` or `Ended`) instead of in a page
/// exchange of their own (FIG-6288). The parent admits them exactly as a
/// [`PageRequest::Seal`] of the same pages and children, and returns their
/// stage capabilities on the run's next `Run`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SealBatch {
    pub pages: Vec<PageEnvelope>,
    pub children: Vec<PageCapability>,
}

impl SealBatch {
    /// The seal request the parent admits this batch as.
    #[must_use]
    pub fn into_request(self) -> PageRequest {
        PageRequest::Seal {
            pages: self.pages,
            children: self.children,
        }
    }
}

/// One page read, with a capability for each child its envelope declares,
/// ascending by digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadPage {
    pub page: PageEnvelope,
    pub children: Vec<PageCapability>,
}

/// The parent's reply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PageReply {
    /// The pages, in request order.
    Read {
        pages: Vec<ReadPage>,
    },
    /// A stage capability for each sealed page, in request order.
    Sealed {
        pages: Vec<PageCapability>,
    },
    Refused(PageRefusal),
}

/// Why the parent refused a page exchange. Every refusal ends the
/// activation as infrastructure; none is a guest outcome.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PageRefusal {
    #[error("a capability is forged, foreign or of another epoch")]
    Unauthorized,
    #[error("the activation's root or stage lease is no longer valid")]
    LeaseLost,
    #[error("a page is missing from the store")]
    Missing,
    #[error("a page does not check against its digest or envelope")]
    Corrupt,
    #[error("the request exceeds the batch bound of {pages} pages or {bytes} bytes")]
    BatchTooLarge { pages: u64, bytes: u64 },
    #[error("the activation's staging quota of {bytes} bytes is exhausted")]
    StagingExhausted { bytes: u64 },
    #[error("the activation's page deadline passed")]
    Deadline,
    #[error("the store is unavailable")]
    StoreUnavailable,
}

impl PageRefusal {
    /// Whether redriving the owning invocation can succeed: a page the
    /// store lacks or holds corrupt is so on every attempt; a lease,
    /// deadline, quota or transport refusal is the host's moment.
    pub fn is_retryable(&self) -> bool {
        !matches!(self, Self::Missing | Self::Corrupt)
    }
}

/// A heap fault the worker met itself (FIG-6422), as it crosses to the
/// parent: infrastructure, never a guest outcome. The wire mirrors the
/// kernel's `lash_kernel_doc::heap::HeapFault`, which this crate cannot
/// name: the worker carries each fault's `Display`, which names the page,
/// as its `detail`.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HeapFault {
    #[error("{detail}")]
    Missing { detail: Detail },
    #[error("{detail}")]
    Corrupt { detail: Detail },
    #[error("{detail}")]
    Unauthorized { detail: Detail },
    #[error("the activation's root or stage lease is no longer valid")]
    LeaseLost,
    #[error("host resource {resource:?} is exhausted")]
    Exhausted { resource: HostResource },
    #[error("{detail}")]
    Transport { detail: Detail },
    #[error("{detail}")]
    Malformed { detail: Detail },
}

/// The host resources a heap draws on beside guest memory, as they cross.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostResource {
    /// Quiescent census bitmap, work-list and root scratch budget.
    TraceBudget,
    CleanResidency,
    DirtyResidency,
    PinnedPages,
    StagingBytes,
    ReadBatch,
    HostWork,
}

impl HeapFault {
    /// Whether redriving the owning invocation can succeed: missing and
    /// corrupt content is so on every attempt, and the heap's own stored
    /// state is no different; a lease, grant, host resource or transport
    /// fault is the host's moment.
    pub fn is_retryable(&self) -> bool {
        !matches!(
            self,
            Self::Missing { .. } | Self::Corrupt { .. } | Self::Malformed { .. }
        )
    }
}
