//! Every refusal of the checkpoint graph, and the decode limits (FIG-5887
//! §9).
//!
//! No refusal permits source replay, a text-format fallback or repair by
//! name: a wrong release digest is not repaired by finding a release of the
//! same name, and a missing node is never accepted as a dangling reference.

use lash_protocol_types::ToolCallId;
use lash_vocabulary::sites::EffectId;
use lash_vocabulary::tokens::CanonicalFault;
use lash_vocabulary::tokens::DecodeDimension;
use lash_vocabulary::tokens::TokenError;

pub use lash_protocol_types::engine_fault::KernelRestoreRefusal;

use super::node::NodeId;
use crate::store_failure::StoreFailure;

/// Why a graph encode, check, decode, commit or read refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GraphRefusal {
    #[error("the envelope does not start with LASH")]
    BadMagic,
    #[error("node kind {0} is not supported here")]
    UnsupportedKind(u32),
    /// A node of a retired kind ([`super::NodeKind::RETIRED`]): the annotation
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
    #[error("node {0} is stored with different bytes")]
    ContentConflict(NodeId),
    #[error("blob {0} is stored under a different encoding")]
    EncodingConflict(NodeId),
    #[error("node {0} is missing")]
    MissingNode(NodeId),
    /// A host graph read was asked to start at a persistent-heap page
    /// (FIG-6193). A read that reaches one from a host node takes the page
    /// itself, its header, and never descends into it: semantic pages are
    /// read under a root lease through the page exchange, a page at a time.
    #[error("node {0} is a persistent-heap page, which a host graph read does not descend into")]
    SemanticPage(NodeId),
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
    #[error("node {0} is on a storage cycle")]
    StorageCycle(NodeId),
    #[error("the root's owner binding is not the reader's")]
    OwnerMismatch,
    #[error("the root's executable identity is not the reader's")]
    ExecutableMismatch,
    #[error("the root's kernel is not the reader's")]
    KernelMismatch,
    #[error("a native implementation's fingerprint is not the pinned one")]
    NativeFingerprintMismatch,
    #[error("no admission holds effect {0:?}")]
    AdmissionMissing(Box<EffectId>),
    #[error("the admission of effect {0:?} is not the one the state names")]
    AdmissionMismatch(Box<EffectId>),
    #[error("effect {0:?} is admitted twice")]
    DuplicateEffect(Box<EffectId>),
    #[error("call {0} already has an outcome")]
    DuplicateOutcome(ToolCallId),
    #[error("an owner revision is not the expected one")]
    RevisionConflict,
    #[error("the writer's fence was lost")]
    FenceLost,
    #[error("the root was retired")]
    RootRetired,
    /// A root lease past its expiry, released, or reaped: no read or
    /// renewal runs under it, and the activation it served is fenced.
    #[error("the root lease is lost")]
    LeaseLost,
    /// A publication does not match the domain record it names.
    #[error("the commit's root changes are not the projection of its writes")]
    PublicationMismatch,
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

/// Deployment graph-read admission, independent of write-side commit admission.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(try_from = "GraphReadPolicyWire")]
pub struct GraphReadPolicy {
    owner_bytes: u64,
    engine_bytes: u64,
    execution_bytes: u64,
    depth: u64,
    kernel_data: lash_vocabulary::data_limits::KernelDataLimits,
}
impl GraphReadPolicy {
    /// Existing read presets: 4 GiB owners, 16 GiB engine and execution state,
    /// and graph-tree depth 64. Depth's hard ceiling protects recursive graph walks.
    pub const fn standard() -> Self {
        Self {
            owner_bytes: 1 << 32,
            engine_bytes: 1 << 34,
            execution_bytes: 1 << 34,
            depth: 64,
            kernel_data: lash_vocabulary::data_limits::KernelDataLimits::standard(),
        }
    }
    pub fn new(
        owner_bytes: u64,
        engine_bytes: u64,
        execution_bytes: u64,
        depth: u64,
        kernel_data: lash_vocabulary::data_limits::KernelDataLimits,
    ) -> Option<Self> {
        (depth <= 64).then_some(Self {
            owner_bytes,
            engine_bytes,
            execution_bytes,
            depth,
            kernel_data,
        })
    }
    const fn limits(self, bytes: u64) -> DecodeLimits {
        DecodeLimits {
            depth: self.depth,
            kernel_data: self.kernel_data,
            ..DecodeLimits::within_total(bytes)
        }
    }
    pub const fn owner(self) -> DecodeLimits {
        self.limits(self.owner_bytes)
    }
    pub const fn engine(self) -> DecodeLimits {
        self.limits(self.engine_bytes)
    }
    pub const fn execution(self) -> DecodeLimits {
        self.limits(self.execution_bytes)
    }
}
impl Default for GraphReadPolicy {
    fn default() -> Self {
        Self::standard()
    }
}
#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct GraphReadPolicyWire {
    owner_bytes: u64,
    engine_bytes: u64,
    execution_bytes: u64,
    #[schemars(range(max = 64))]
    depth: u64,
    kernel_data: lash_vocabulary::data_limits::KernelDataLimits,
}
impl TryFrom<GraphReadPolicyWire> for GraphReadPolicy {
    type Error = &'static str;
    fn try_from(w: GraphReadPolicyWire) -> Result<Self, Self::Error> {
        Self::new(
            w.owner_bytes,
            w.engine_bytes,
            w.execution_bytes,
            w.depth,
            w.kernel_data,
        )
        .ok_or("graph read depth exceeds the 64-level safety ceiling")
    }
}

pub use lash_checkpoint_encoder::DecodeLimits;

impl From<lash_checkpoint_encoder::GraphRefusal> for GraphRefusal {
    fn from(error: lash_checkpoint_encoder::GraphRefusal) -> Self {
        use lash_checkpoint_encoder::GraphRefusal as Codec;
        match error {
            Codec::BadMagic => Self::BadMagic,
            Codec::UnsupportedKind(value) => Self::UnsupportedKind(value),
            Codec::RetiredFormat { node, kind, format } => {
                Self::RetiredFormat { node, kind, format }
            }
            Codec::UnsupportedVariant { kind, variant } => {
                Self::UnsupportedVariant { kind, variant }
            }
            Codec::NonCanonical { node, reason } => Self::NonCanonical { node, reason },
            Codec::Truncated => Self::Truncated,
            Codec::TrailingBytes => Self::TrailingBytes,
            Codec::LengthOverflow => Self::LengthOverflow,
            Codec::InvalidUtf8 => Self::InvalidUtf8,
            Codec::HashMismatch(value) => Self::HashMismatch(value),
            Codec::SemanticPage(value) => Self::SemanticPage(value),
            Codec::MissingNode(value) => Self::MissingNode(value),
            Codec::WrongChildKind { parent, child } => Self::WrongChildKind { parent, child },
            Codec::EncodedNodeTooLarge { bytes, limit } => {
                Self::EncodedNodeTooLarge { bytes, limit }
            }
            Codec::RootTooLarge { bytes, limit } => Self::RootTooLarge { bytes, limit },
            Codec::DecodeLimit {
                dimension,
                actual,
                limit,
            } => Self::DecodeLimit {
                dimension,
                actual,
                limit,
            },
            Codec::EncodeMemoryLimit => Self::EncodeMemoryLimit,
            Codec::InvalidBody { kind, reason } => Self::InvalidBody { kind, reason },
        }
    }
}
