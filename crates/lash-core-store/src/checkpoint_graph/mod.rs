//! The checkpoint graph: canonical, content-addressed binary nodes for every
//! durable checkpoint (FIG-5887, the hybrid binary checkpoint format).
//!
//! A checkpoint is a graph of immutable nodes. Each node is one envelope:
//!
//! ```text
//! "LASH" | kind: LEB128 u32 | child_count: LEB128 | child digests (32 B each,
//! strictly ascending) | body_len: LEB128 | body
//! ```
//!
//! and its [`NodeId`] is the envelope's digest under the blob hash domain
//! (`BlobRef::for_content`), so a node's lowercase hex id is its SQL key. The
//! body is a stream of the canonical tokens of
//! [`lash_kernel_doc::graph_tokens`], where a child reference is the slot of
//! the child in the sorted child table. Every declared child is referenced,
//! and nothing else is: a node's child table is exactly its storage edges.
//!
//! This module owns the format's fixed seams:
//!
//! - [`node`]: the envelope, its digest, and [`check_envelope`], the generic
//!   check every reader runs;
//! - [`registry`]: the explicit node-kind numbers and each kind's body shape
//!   and size bound;
//! - [`encode`]: [`GraphStaging`], which places large text and bytes in
//!   shared leaves, streams a body too large for its bound through structure
//!   blocks, and stages every node it makes;
//! - [`decode`]: [`GraphDecoder`], which reads a node back into the logical
//!   body a typed codec decodes, refusing every non-canonical form;
//! - [`refusal`]: [`GraphRefusal`] and the decode limits;
//! - [`commit`]: commit identity, budgets and measurements;
//! - [`roots`]: the closed inventory of root sources;
//! - [`bodies`]: the typed bodies of the generic kinds this crate owns.
//!
//! Typed bodies of kernel, host, cell and turn kinds are encoded by their
//! owning crates with [`lash_kernel_doc::graph_tokens::TokenCodec`] and
//! staged here through [`GraphStaging::encode`].
//!
//! There is no format version field, no fallback reader and no second
//! spelling of any value.

pub mod bodies;
pub mod commit;
pub mod decode;
pub mod encode;
pub mod node;
pub mod refusal;
pub mod registry;
pub mod roots;
/// The persistent heap's staging spool and candidate manifest (FIG-6195):
/// declared ahead of the cutover, implemented by nothing yet.
pub mod stage;
#[cfg(any(test, feature = "testing"))]
pub mod vectors;

#[cfg(test)]
mod tests;

pub use commit::{
    CommitId, EncodeBudget, EncodedCommitMeasurement, ExplicitLimit, GraphCommitBudget,
    InstalledGraphMeasurement,
};
pub use decode::{BlobEncoding, GraphDecoder, NodeSource, StoredNode, closure};
pub use encode::{GraphStaging, TypedBody, encode_node};
pub use lash_kernel_doc::KernelDataLimits;
pub use lash_kernel_doc::graph_tokens::{
    CanonicalFault, ChildRef, CodecVariant, DecodeDimension, TokenCodec, TokenError, TokenReader,
    TokenWriter,
};
pub use node::{
    ChildTable, EncodedNode, INDEX_FANOUT, INLINE_TOKEN_MAX, LEAF_PAYLOAD_MAX, MAGIC,
    MAX_NODE_BYTES, MAX_ROOT_BYTES, NodeEnvelope, NodeId, check_envelope, parse_envelope,
};
pub use refusal::{DecodeLimits, GraphReadPolicy, GraphRefusal, KernelRestoreRefusal};
pub use registry::{BodyShape, KindSpec, NodeKind, OwningLane, REGISTRY, SizeBound};
pub use roots::RootSource;
