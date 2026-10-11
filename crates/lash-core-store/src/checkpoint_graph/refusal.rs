//! Every refusal of the checkpoint graph, and the decode limits (FIG-5887
//! §9).
//!
//! No refusal permits source replay, a text-format fallback or repair by
//! name: a wrong release digest is not repaired by finding a release of the
//! same name, and a missing node is never accepted as a dangling reference.

use lash_kernel_doc::EffectIdentity;
use lash_kernel_doc::graph_tokens::{CanonicalFault, DecodeDimension, TokenError};
use lash_sansio::ToolCallId;

pub use lash_vm_protocol::KernelRestoreRefusal;

use super::node::NodeId;
use super::roots::RootSource;
use crate::store_failure::StoreFailure;

/// Why a graph encode, check, decode, commit, read or collection refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GraphRefusal {
    #[error("the envelope does not start with LASH")]
    BadMagic,
    #[error("node kind {0} is not supported here")]
    UnsupportedKind(u32),
    #[error("node kind {kind} has no variant {variant}")]
    UnsupportedVariant { kind: u32, variant: u32 },
    #[error("non-canonical node {node:?}: {reason}")]
    NonCanonical {
        node: Option<NodeId>,
        reason: CanonicalFault,
    },
    #[error("the node ends inside a token")]
    Truncated,
    #[error("the node has bytes after its body")]
    TrailingBytes,
    #[error("a count or number overflows its width")]
    LengthOverflow,
    #[error("text is not UTF-8")]
    InvalidUtf8,
    #[error("node {0} does not hash to its identity")]
    HashMismatch(NodeId),
    #[error("node {0} is stored with different bytes")]
    ContentConflict(NodeId),
    #[error("blob {0} is stored under a different encoding")]
    EncodingConflict(NodeId),
    #[error("node {0} is missing")]
    MissingNode(NodeId),
    #[error("node {parent} references {child} of the wrong kind")]
    WrongChildKind { parent: NodeId, child: NodeId },
    /// A node read as one kind is another. (Added by the seam lane: the
    /// root-position twin of [`Self::WrongChildKind`].)
    #[error("node {node} is kind {found}, not {expected}")]
    KindMismatch {
        node: NodeId,
        expected: u32,
        found: u32,
    },
    #[error("node {0}'s stored edges are not its child table")]
    EdgeProjectionMismatch(NodeId),
    #[error("node {0} is on a storage cycle")]
    StorageCycle(NodeId),
    #[error("the root's owner binding is not the reader's")]
    OwnerMismatch,
    #[error("the root's executable identity is not the reader's")]
    ExecutableMismatch,
    #[error("the root's kernel is not the reader's")]
    KernelMismatch,
    #[error("the root's helper release is not the installed one")]
    ReleaseMismatch,
    #[error("a pinned helper is not installed")]
    MissingHelper,
    #[error("a native implementation's fingerprint is not the pinned one")]
    NativeFingerprintMismatch,
    #[error("no admission holds effect {0:?}")]
    AdmissionMissing(Box<EffectIdentity>),
    #[error("the admission of effect {0:?} is not the one the state names")]
    AdmissionMismatch(Box<EffectIdentity>),
    #[error("effect {0:?} is admitted twice")]
    DuplicateEffect(Box<EffectIdentity>),
    #[error("call {0} already has an outcome")]
    DuplicateOutcome(ToolCallId),
    #[error("an owner revision is not the expected one")]
    RevisionConflict,
    #[error("the writer's fence was lost")]
    FenceLost,
    #[error("root source {0:?} did not enumerate completely")]
    RootSourceIncomplete(RootSource),
    #[error("the root was retired")]
    RootRetired,
    /// A publication does not match the domain record it names.
    #[error("the commit's root changes are not the projection of its writes")]
    PublicationMismatch,
    #[error("the read pin expired")]
    ReadPinExpired,
    #[error("the read pin is not the one named")]
    ReadPinMismatch,
    #[error("the presentation's renderer is not available")]
    RendererUnavailable,
    #[error("the reconstructed presentation is not the recorded one")]
    PresentationMismatch,
    #[error("an encoded node of {bytes} bytes exceeds {limit}")]
    EncodedNodeTooLarge { bytes: u64, limit: u64 },
    #[error("a root of {bytes} bytes exceeds {limit}")]
    RootTooLarge { bytes: u64, limit: u64 },
    #[error("the commit of {bytes} encoded bytes exceeds {limit}")]
    CommitTooLarge { bytes: u64, limit: u64 },
    #[error("the commit's {rows} mutation rows exceed {limit}")]
    TooManyMutations { rows: u64, limit: u64 },
    #[error("{dimension:?} {actual} exceeds the decode limit {limit}")]
    DecodeLimit {
        dimension: DecodeDimension,
        actual: u64,
        limit: u64,
    },
    #[error("encoding exceeds its memory lease")]
    EncodeMemoryLimit,
    #[error("collection did not complete")]
    CollectionIncomplete,
    /// A typed body decoder refused well-formed tokens. (Added by the seam
    /// lane: the typed refusal of a body owned outside the kernel.)
    #[error("invalid body of kind {kind}: {reason}")]
    InvalidBody { kind: u32, reason: String },
    /// The kernel's precise refusal of a state it will not resume.
    #[error("kernel state refused: {0}")]
    KernelState(KernelRestoreRefusal),
    /// The store failed; its kind keeps transient apart from corrupt.
    #[error(transparent)]
    Backend(StoreFailure),
}

impl GraphRefusal {
    /// The refusal a token error becomes, for `node` of `kind` when known.
    #[must_use]
    pub fn from_token(error: TokenError, node: Option<NodeId>, kind: Option<u32>) -> Self {
        match error {
            TokenError::Truncated => Self::Truncated,
            TokenError::TrailingBytes => Self::TrailingBytes,
            TokenError::LengthOverflow => Self::LengthOverflow,
            TokenError::InvalidUtf8 => Self::InvalidUtf8,
            TokenError::NonCanonical(reason) => Self::NonCanonical { node, reason },
            TokenError::UnsupportedVariant { variant } => Self::UnsupportedVariant {
                kind: kind.unwrap_or(0),
                variant,
            },
            TokenError::Limit {
                dimension,
                actual,
                limit,
            } => Self::DecodeLimit {
                dimension,
                actual,
                limit,
            },
            TokenError::Invalid(reason) => Self::InvalidBody {
                kind: kind.unwrap_or(0),
                reason,
            },
        }
    }
}

/// What one decode may read. Every field is chosen by the caller: there is
/// no default, and none may exceed the format's own bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeLimits {
    /// The largest envelope read, at most [`MAX_NODE_BYTES`](super::MAX_NODE_BYTES).
    pub node_bytes: u64,
    /// Envelope bytes read in total.
    pub total_bytes: u64,
    /// Nodes read in total.
    pub nodes: u64,
    /// The largest logical body assembled, placed values included.
    pub logical_bytes: u64,
    /// The deepest index or stream tree followed.
    pub depth: u64,
}

impl DecodeLimits {
    /// Limits bounded only by the format and by `total` bytes read: for
    /// tests, tools and readers whose caller bounds the input another way.
    #[must_use]
    pub const fn within_total(total: u64) -> Self {
        Self {
            node_bytes: super::MAX_NODE_BYTES,
            total_bytes: total,
            nodes: total,
            logical_bytes: total,
            depth: 64,
        }
    }
}
