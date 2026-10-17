//! A node-bodied module artifact's graph nodes (FIG-6469): what a durable
//! store installs and roots for a [`crate::ModuleArtifactBody`] that is a
//! node, and what a read of one answers.

/// A node-bodied module artifact as the graph nodes a store installs: the
/// root its artifact row points at and every node the body encodes to (a
/// large body streams into children).
#[derive(Clone, Debug)]
pub struct StagedModuleNode {
    pub root: lash_core_store::checkpoint_graph::NodeId,
    pub nodes: std::collections::BTreeMap<
        lash_core_store::checkpoint_graph::NodeId,
        lash_core_store::checkpoint_graph::EncodedNode,
    >,
}

/// The graph nodes the logical body `logical` of a module node of `kind`
/// ([`crate::ModuleArtifactBody::node_kind`]) is stored as.
///
/// # Errors
///
/// `Encode` for a body that is not one canonical token.
pub fn stage_module_node(
    kind: lash_core_store::checkpoint_graph::NodeKind,
    logical: &[u8],
) -> Result<StagedModuleNode, crate::ArtifactStoreError> {
    use lash_core_store::checkpoint_graph::{EncodeBudget, ExplicitLimit, GraphStaging, TypedBody};
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let root = staging
        .encode(kind, TypedBody::logical(logical))
        .map_err(|error| crate::ArtifactStoreError::Encode(error.to_string()))?;
    Ok(StagedModuleNode {
        root,
        nodes: staging.into_nodes(),
    })
}

/// The logical body of the module node `root`, read from `source`: what a
/// read of a node-bodied artifact answers. Only a Document or a
/// HelperDefinition node is a module artifact's body.
///
/// # Errors
///
/// The node's refusal, as stored data that is corrupt.
pub fn module_node_body(
    root: lash_core_store::checkpoint_graph::NodeId,
    source: &dyn lash_core_store::checkpoint_graph::NodeSource,
) -> Result<Vec<u8>, crate::ArtifactStoreError> {
    use lash_core_store::checkpoint_graph::{GraphDecoder, NodeKind};
    let corrupt = |message: String| crate::ArtifactStoreError::StoredDataCorrupt {
        source: crate::ModuleArtifactCorruption::Storage {
            record_kind: "module artifact node".into(),
            message,
        },
    };
    let mut decoder = GraphDecoder::new(source, lash_durable::domain::graph::OWNER_READ_LIMITS);
    let kind = decoder
        .envelope(root)
        .map_err(|error| corrupt(error.to_string()))?
        .kind;
    if kind != NodeKind::DOCUMENT && kind != NodeKind::HELPER_DEFINITION {
        return Err(corrupt(format!(
            "node {root} is of kind {}, not a module body",
            kind.0
        )));
    }
    decoder
        .logical_body(root, kind)
        .map_err(|error| corrupt(error.to_string()))
}
