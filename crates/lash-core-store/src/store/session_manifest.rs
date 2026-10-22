//! A session checkpoint as a SessionManifest node (FIG-5887 §3, kind 34).
//!
//! The manifest is the root a session revision and an admission base name.
//! Its sorted component descriptors each reference an OpaqueLeaf whose
//! declared raw-blob arm names the component's opaque blob, so the collector
//! retains every component through ordinary graph edges and no manifest is
//! ever opened to find them. Component bodies stay opaque bytes their
//! owning codecs read. The session's execution state, its SessionRoot
//! descriptor page, is a typed child of the manifest, so its pages are
//! retained the same way.

use std::collections::BTreeMap;

use crate::checkpoint_graph::bodies::{OpaqueLeaf, OpaquePayload};
use crate::checkpoint_graph::{
    BlobEncoding, DecodeLimits, EncodeBudget, EncodedNode, ExplicitLimit, GraphDecoder,
    GraphRefusal, GraphStaging, NodeId, NodeKind, NodeSource, StoredNode, TokenCodec, TokenError,
    TokenReader, TokenWriter,
};

use super::{BlobRef, CheckpointComponentDescriptor, SessionCheckpoint};

/// The codec identity of a session checkpoint component's opaque bytes.
pub const SESSION_COMPONENT_CODEC: &str = "lash.session.component";

/// The manifest body: the schema stamp, the turn state, each component's
/// name, encoding and OpaqueLeaf, by name, and the execution state's SessionRoot descriptor page.
struct ManifestBody {
    schema_version: u32,
    turn_state: serde_json::Value,
    components: Vec<(String, u32, NodeId)>,
    execution: Option<NodeId>,
}

impl TokenCodec for ManifestBody {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(4);
        out.unsigned(u64::from(self.schema_version));
        self.turn_state.encode(out);
        out.seq(self.components.len() as u64);
        for (name, encoding_version, leaf) in &self.components {
            out.seq(3);
            out.text(name);
            out.unsigned(u64::from(*encoding_version));
            leaf.encode(out);
        }
        self.execution.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(4)?;
        let schema_version = input.unsigned_u32()?;
        let turn_state = serde_json::Value::decode(input)?;
        let count = input.seq()?;
        let mut components = Vec::new();
        for _ in 0..count {
            input.expect_struct(3)?;
            let name = input.text()?.to_owned();
            let encoding_version = input.unsigned_u32()?;
            components.push((name, encoding_version, NodeId::decode(input)?));
        }
        Ok(Self {
            schema_version,
            turn_state,
            components,
            execution: Option::<NodeId>::decode(input)?,
        })
    }
}

/// What a SessionManifest's body names, as [`SessionCheckpoint::named`]
/// reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedManifest {
    pub schema_version: u32,
    /// Each component's name and encoding version, by name.
    pub components: Vec<(String, u32)>,
    /// The execution state's SessionRoot descriptor page.
    pub execution: Option<NodeId>,
}

/// A manifest node alone, every other node answered as an opaque blob: a
/// decoder reads each child a body names to tell a placed value from a
/// reference, and here every child is a reference it does not open. A body
/// whose turn state placed a long text in a leaf does not read this way.
struct BodyOnly<'a>(&'a EncodedNode);

impl NodeSource for BodyOnly<'_> {
    fn fetch(&self, id: &NodeId) -> Option<StoredNode<'_>> {
        Some(if *id == self.0.id {
            StoredNode {
                encoding: BlobEncoding::Graph,
                bytes: &self.0.bytes,
            }
        } else {
            StoredNode {
                encoding: BlobEncoding::Opaque,
                bytes: &[],
            }
        })
    }
}

fn invalid(reason: impl Into<String>) -> GraphRefusal {
    GraphRefusal::InvalidBody {
        kind: NodeKind::SESSION_MANIFEST.0,
        reason: reason.into(),
    }
}

impl SessionCheckpoint {
    /// Stages this manifest and an OpaqueLeaf per component, and returns the
    /// manifest's node. The component blobs themselves are not staged: a
    /// commit installs them as opaque blobs before it installs the graph.
    ///
    /// # Errors
    ///
    /// A component reference that is not a blob digest, a turn state that
    /// does not encode, or any refusal of the staging area.
    pub fn stage(&self, staging: &mut GraphStaging) -> Result<NodeId, GraphRefusal> {
        let turn_state = serde_json::to_value(&self.turn_state)
            .map_err(|error| invalid(format!("the turn state does not encode: {error}")))?;
        let mut components = Vec::with_capacity(self.components.len());
        for (name, descriptor) in &self.components {
            let blob = NodeId::from_hex(descriptor.blob_ref.as_str()).ok_or_else(|| {
                invalid(format!(
                    "component `{name}` names `{}`, not a blob digest",
                    descriptor.blob_ref
                ))
            })?;
            let leaf = staging.encode_value(
                NodeKind::OPAQUE_LEAF,
                &OpaqueLeaf {
                    codec: SESSION_COMPONENT_CODEC.to_owned(),
                    payload: OpaquePayload::RawBlob(blob),
                },
            )?;
            components.push((name.clone(), descriptor.encoding_version, leaf));
        }
        let execution = self
            .execution_ref
            .as_ref()
            .map(|root| {
                NodeId::from_hex(root.as_str()).ok_or_else(|| {
                    invalid(format!("the execution state names `{root}`, not a node"))
                })
            })
            .transpose()?;
        Ok(staging.encode_value(
            NodeKind::SESSION_MANIFEST,
            &ManifestBody {
                schema_version: self.schema_version,
                turn_state,
                components,
                execution,
            },
        )?)
    }

    /// This manifest's node and every node it stages, for a store to install.
    ///
    /// # Errors
    ///
    /// Any refusal of [`Self::stage`].
    pub fn to_graph(&self) -> Result<(NodeId, BTreeMap<NodeId, EncodedNode>), GraphRefusal> {
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let root = self.stage(&mut staging)?;
        Ok((root, staging.into_nodes()))
    }

    /// What manifest `manifest`'s body names, read from the body alone
    /// without checking a child: what a walk that must not fail on another
    /// build's manifest reads. `None` when the node is not a SessionManifest
    /// or its body does not decode.
    #[must_use]
    pub fn named(manifest: &EncodedNode) -> Option<NamedManifest> {
        // One node is read, but its logical body spells each child it
        // names as a whole digest wherever it is named, so the body may
        // outgrow the node: a reference grows at most to a digest's size.
        let read = manifest.bytes.len() as u64;
        let limits = DecodeLimits {
            logical_bytes: read.saturating_mul(32),
            ..DecodeLimits::within_total(read)
        };
        let body: ManifestBody = GraphDecoder::new(&BodyOnly(manifest), limits)
            .decode(manifest.id, NodeKind::SESSION_MANIFEST)
            .ok()?;
        Some(NamedManifest {
            schema_version: body.schema_version,
            components: body
                .components
                .into_iter()
                .map(|(name, encoding_version, _)| (name, encoding_version))
                .collect(),
            execution: body.execution,
        })
    }

    /// Reads manifest `root` from `source`, which need not hold the
    /// component blobs: they are only named.
    ///
    /// # Errors
    ///
    /// Any refusal of the node reads, components out of name order, a
    /// component leaf of another codec or with inline bytes, or a turn
    /// state that does not decode.
    pub fn load(
        source: &dyn NodeSource,
        root: NodeId,
        limits: DecodeLimits,
    ) -> Result<Self, GraphRefusal> {
        let mut decoder = GraphDecoder::new(source, limits);
        let body: ManifestBody = decoder.decode(root, NodeKind::SESSION_MANIFEST)?;
        let turn_state = serde_json::from_value(body.turn_state)
            .map_err(|error| invalid(format!("the turn state does not decode: {error}")))?;
        let mut components = BTreeMap::new();
        for (name, encoding_version, leaf) in body.components {
            if components
                .keys()
                .next_back()
                .is_some_and(|last: &String| *last >= name)
            {
                return Err(invalid(format!("component `{name}` is out of name order")));
            }
            let opaque: OpaqueLeaf = decoder.decode(leaf, NodeKind::OPAQUE_LEAF)?;
            let OpaquePayload::RawBlob(blob) = opaque.payload else {
                return Err(invalid(format!("component `{name}` holds inline bytes")));
            };
            if opaque.codec != SESSION_COMPONENT_CODEC {
                return Err(invalid(format!(
                    "component `{name}` is owned by codec `{}`",
                    opaque.codec
                )));
            }
            components.insert(
                name,
                CheckpointComponentDescriptor {
                    blob_ref: BlobRef(blob.to_hex()).into(),
                    encoding_version,
                },
            );
        }
        if let Some(execution) = body.execution {
            decoder.child_kind(root, execution, &[NodeKind::SESSION_ROOT])?;
        }
        Ok(Self {
            schema_version: body.schema_version,
            turn_state,
            components,
            execution_ref: body.execution.map(|execution| BlobRef(execution.to_hex())),
        })
    }
}
