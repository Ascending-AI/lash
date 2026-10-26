//! Typed refusals and bounds of the canonical checkpoint codec.

use crate::NodeId;
use lash_kernel_value::tokens::CanonicalFault;
use lash_kernel_value::tokens::DecodeDimension;
use lash_kernel_value::tokens::TokenError;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GraphRefusal {
    #[error("the envelope does not start with LASH")]
    BadMagic,
    #[error("node kind {0} is not supported here")]
    UnsupportedKind(u32),
    /// A node of a retired kind ([`crate::NodeKind::RETIRED`]): the annotation
    /// layer (FIG-6376) or a pre-heap image's `format` (FIG-6193), refused
    /// before its body is read, never misread.
    #[error("node kind {kind} is a retired {format} image, which this build does not read")]
    RetiredFormat {
        node: Option<NodeId>,
        kind: u32,
        format: &'static str,
    },
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
    #[error("node {0} is missing")]
    MissingNode(NodeId),
    #[error("node {0} is a persistent-heap page, which a host graph read does not descend into")]
    SemanticPage(NodeId),
    #[error("node {parent} references {child} of the wrong kind")]
    WrongChildKind { parent: NodeId, child: NodeId },
    #[error("an encoded node of {bytes} bytes exceeds {limit}")]
    EncodedNodeTooLarge { bytes: u64, limit: u64 },
    #[error("a root of {bytes} bytes exceeds {limit}")]
    RootTooLarge { bytes: u64, limit: u64 },
    #[error("{dimension:?} {actual} exceeds the decode limit {limit}")]
    DecodeLimit {
        dimension: DecodeDimension,
        actual: u64,
        limit: u64,
    },
    #[error("encoding exceeds its memory lease")]
    EncodeMemoryLimit,
    /// A typed body decoder refused well-formed tokens. (Added by the seam
    /// lane: the typed refusal of a body owned outside the kernel.)
    #[error("invalid body of kind {kind}: {reason}")]
    InvalidBody { kind: u32, reason: String },
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
    pub kernel_data: lash_kernel_value::data_limits::KernelDataLimits,
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
            kernel_data: lash_kernel_value::data_limits::KernelDataLimits::standard(),
            node_bytes: super::MAX_NODE_BYTES,
            total_bytes: total,
            nodes: total,
            logical_bytes: total,
            depth: 64,
        }
    }
}
