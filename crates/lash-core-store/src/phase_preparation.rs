//! Exact immutable phase content retained by the acknowledged session head.
//!
//! These two pins memoize encoding, never configuration selection. A phase
//! compares its newly produced text with the key before reusing a root. The
//! terminal head keeps the roots reachable after the phase retires.

use crate::checkpoint_graph::{
    DecodeLimits, EncodeBudget, EncodedNode, ExplicitLimit, GraphDecoder, GraphRefusal,
    GraphStaging, NodeId, NodeKind, NodeSource,
};
use crate::store::BlobRef;
use lash_kernel_value::tokens::{TokenCodec, TokenError, TokenReader, TokenWriter};
use std::collections::BTreeMap;

/// The runtime-owned component whose body is a graph node, not an opaque blob.
pub const COMPONENT: &str = "phase_content_preparation";

/// Immutable content roles. Each role retains at most one exact value.
#[derive(Clone, Copy, Debug)]
pub enum PreparationRole {
    Environment,
    ModelRequest,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PreparedContent {
    key: BlobRef,
    root: NodeId,
}

impl TokenCodec for PreparedContent {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        out.text(self.key.as_str());
        self.root.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            key: BlobRef(input.text()?.to_owned()),
            root: NodeId::decode(input)?,
        })
    }
}

/// A bounded encoding memo, authenticated and retained through graph edges.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PhaseContentPreparations {
    environment: Option<PreparedContent>,
    request: Option<PreparedContent>,
}

impl TokenCodec for PhaseContentPreparations {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.environment.encode(out);
        self.request.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            environment: Option::decode(input)?,
            request: Option::decode(input)?,
        })
    }
}

impl PhaseContentPreparations {
    /// Only an exact match with freshly produced data permits encoding reuse.
    pub fn get(&self, role: PreparationRole, key: &BlobRef) -> Option<NodeId> {
        self.entry(role)
            .as_ref()
            .filter(|entry| entry.key == *key)
            .map(|entry| entry.root)
    }

    /// Remember a root the current phase staged or already owns.
    pub fn record(&mut self, role: PreparationRole, key: BlobRef, root: NodeId) {
        *self.entry_mut(role) = Some(PreparedContent { key, root });
    }

    /// Fill missing restored roles with the acknowledged head's exact pins.
    pub fn with_head(mut self, head: &Self) -> Self {
        if self.environment.is_none() {
            self.environment.clone_from(&head.environment);
        }
        if self.request.is_none() {
            self.request.clone_from(&head.request);
        }
        self
    }

    pub fn is_empty(&self) -> bool {
        self.environment.is_none() && self.request.is_none()
    }

    fn entry(&self, role: PreparationRole) -> &Option<PreparedContent> {
        match role {
            PreparationRole::Environment => &self.environment,
            PreparationRole::ModelRequest => &self.request,
        }
    }

    fn entry_mut(&mut self, role: PreparationRole) -> &mut Option<PreparedContent> {
        match role {
            PreparationRole::Environment => &mut self.environment,
            PreparationRole::ModelRequest => &mut self.request,
        }
    }

    /// Encode the small descriptor. Its content roots are required separately.
    pub fn to_graph(&self) -> Result<(NodeId, BTreeMap<NodeId, EncodedNode>), GraphRefusal> {
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let root = staging.encode_value(NodeKind::PHASE_PREPARATION, self)?;
        Ok((root, staging.into_nodes()))
    }

    /// Read and authenticate both pins before a resumed owner can reuse them.
    pub fn load(
        source: &dyn NodeSource,
        root: NodeId,
        limits: DecodeLimits,
    ) -> Result<Self, GraphRefusal> {
        let mut decoder = GraphDecoder::new(source, limits);
        let value: Self = decoder.decode(root, NodeKind::PHASE_PREPARATION)?;
        for entry in [&value.environment, &value.request].into_iter().flatten() {
            decoder.child_kind(root, entry.root, &[NodeKind::TURN_CONTENT])?;
            let text: String = decoder.decode(entry.root, NodeKind::TURN_CONTENT)?;
            if BlobRef::for_content(text.as_bytes()) != entry.key {
                return Err(GraphRefusal::InvalidBody {
                    kind: NodeKind::PHASE_PREPARATION.0,
                    reason: "prepared content does not match its exact text key".into(),
                });
            }
        }
        Ok(value)
    }
}

/// Typed graph components supply nodes rather than opaque component blobs.
pub fn is_graph_component(key: &str) -> bool {
    key == COMPONENT || crate::plugin_state::namespace_component_plugin(key).is_some()
}
