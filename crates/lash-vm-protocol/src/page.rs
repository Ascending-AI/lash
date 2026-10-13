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

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Names one page exchange within a lease, so its reply answers exactly
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PageExchangeId(pub u64);

/// What authorizes a capability: the activation's lease on its committed
/// root, or its private staging.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageCapability {
    #[serde(with = "serde_bytes")]
    pub page: [u8; 32],
    pub authority: PageAuthority,
    pub epoch: u64,
    #[serde(with = "serde_bytes")]
    pub mac: [u8; 32],
}

/// One page as it crosses: its digest and complete envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageEnvelope {
    #[serde(with = "serde_bytes")]
    pub id: [u8; 32],
    #[serde(with = "serde_bytes")]
    pub envelope: Vec<u8>,
}

/// A worker's page request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

/// One page read, with a capability for each child its envelope declares,
/// ascending by digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadPage {
    pub page: PageEnvelope,
    pub children: Vec<PageCapability>,
}

/// The parent's reply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
