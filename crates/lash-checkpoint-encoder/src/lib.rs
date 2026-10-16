//! Canonical checkpoint encoding, content identities and envelope validation.
//! Shared by release generation and runtime storage without pulling runtime
//! services into the build host.

mod blob;
mod budget;
pub mod encode;
pub mod node;
#[cfg(feature = "perf-witness")]
pub mod perf_witness;
pub mod refusal;
pub mod registry;

pub use blob::BlobRef;
pub use budget::{EncodeBudget, ExplicitLimit};
pub use encode::{GraphStaging, TypedBody, encode_node};
pub use node::{
    ChildTable, EncodedNode, INDEX_FANOUT, INLINE_TOKEN_MAX, LEAF_PAYLOAD_MAX, MAGIC,
    MAX_NODE_BYTES, MAX_ROOT_BYTES, NodeEnvelope, NodeId, check_envelope, parse_envelope,
};
pub use refusal::{DecodeLimits, GraphRefusal};
pub use registry::{
    BodyShape, KindSpec, NodeKind, OwningLane, REGISTRY, SEMANTIC_REGISTRY, SizeBound,
};
