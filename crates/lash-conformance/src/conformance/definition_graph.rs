//! A definition's document and helper declarations as checkpoint graph
//! nodes (FIG-6469), on every store that registers definitions.
//!
//! A host publishes a kernel document and the declaration of every helper
//! it lists under its pin, then a definition naming them. Each is stored
//! once, as its graph node (a Document, kind 10; a HelperDefinition, kind
//! 14), which its artifact row roots: the referrer's edge holds the row, and
//! the row holds the node. These laws execute the real store and the real
//! artifact-cleanup relay, and read the backend's rows through its
//! [`DefinitionGraphProbe`]:
//!
//! - a definition and its declarations store exactly one graph body each,
//!   rooted directly by its artifact row, however many definitions name it;
//! - the last owner's end reclaims the nodes; while a second owner holds
//!   them, they stay;
//! - a crash at publication, at severance or at retirement neither loses a
//!   node a holder still needs nor leaks one nothing holds.
//!
//! A crash is the store's transaction failing at the point the cut names,
//! which is what a process killed there leaves behind; recovery is the
//! publication or the end running again.

use std::collections::BTreeSet;

use pretty_assertions::assert_eq;

use lash_core::store::module_nodes::stage_module_node;
use lash_core::{ModuleArtifactBody, ModuleArtifactHold};

use super::{DEFINITION_ENGINE, World, pin};

/// What a kernel-document artifact's row stores.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModuleRow {
    /// No row.
    Absent,
    /// Opaque bytes.
    Bytes,
    /// A graph node, by its digest: the row is the node's root.
    Node(String),
}

/// Where a cut fails a store's module transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefinitionGraphCut {
    /// The publication's referrer edge to a kernel-document artifact: the
    /// nodes and the row are already written in the transaction.
    Publication,
    /// The severance of a referrer's edge to a kernel-document artifact.
    Severance,
    /// The delete of a graph node a severance peels.
    Retirement,
}

/// Raw access to one backend's module rows and graph nodes, and the cuts
/// the definition graph laws make.
#[async_trait::async_trait]
pub trait DefinitionGraphProbe: Send + Sync {
    /// What the kernel-document artifact `artifact_ref`'s row stores.
    async fn module_row(&self, artifact_ref: &str) -> ModuleRow;
    /// The digest of every stored graph node.
    async fn graph_nodes(&self) -> BTreeSet<String>;
    /// Fail every later module transaction at `cut` until [`Self::clear_cut`].
    async fn cut(&self, cut: DefinitionGraphCut);
    async fn clear_cut(&self);
}

/// A kernel document over one helper, its body long enough to stream into
/// child nodes, and that helper's declaration.
struct Published {
    document_ref: String,
    document: lash_kernel_doc::IdentifiedDocument,
    function_ref: String,
    function: std::sync::Arc<[u8]>,
}

impl Published {
    #[expect(clippy::expect_used, reason = "fixture documents are well formed")]
    fn new(tag: &str) -> Self {
        let mut registry = lash_kernel_doc::FunctionRegistry::new();
        let helper = registry
            .register(
                lash_kernel_doc::parse_definition(
                    "function helper() -> Any\nkernel 1\ncharge 1\nbody { return 7 }",
                )
                .expect("a helper definition"),
                None,
            )
            .expect("the helper registers");
        // 96 KiB of text: past a Document node's bound, so the body streams.
        let text = format!("{tag}-{}", "d".repeat(96 * 1024));
        let document = lash_kernel_doc::IdentifiedDocument::new(
            lash_kernel_doc::parse_document(&format!(
                "kernel 1\nnumbers float\nuse helper = @{helper}\nentry run() -> Any\n\
                 fn run() {{ let n = invoke helper() let pair = (n, \"{text}\") return pair }}\n\
                 main {{ }}"
            ))
            .expect("a document"),
        );
        let declarations = lash_kernel_doc::CheckedDeclarations::required(&document, &registry)
            .expect("the document's helper is declared");
        let (function_id, function) = declarations
            .images()
            .into_iter()
            .next()
            .expect("one declaration");
        Self {
            document_ref: document.id().to_string(),
            document,
            function_ref: format!("function:{function_id}"),
            function,
        }
    }

    /// The definition of entry `n` of the document: another definition,
    /// the same manifest.
    #[expect(clippy::expect_used, reason = "fixture drafts are well formed")]
    fn draft(&self, n: u32) -> crate::ProcessDefinitionDraft {
        crate::ProcessDefinitionDraft::new(
            DEFINITION_ENGINE,
            serde_json::json!({
                "module": self.document_ref,
                "functions": [self.function_ref],
                "entry": n,
            }),
            [&self.document_ref, &self.function_ref].map(|artifact_ref| crate::ArtifactName {
                store: crate::ArtifactStoreId::KernelDocument,
                artifact_ref: artifact_ref.clone(),
            }),
        )
        .expect("a well-formed draft")
    }

    /// The root each artifact's row must point at, and every node the two
    /// bodies encode to.
    #[expect(clippy::expect_used, reason = "fixture bodies encode")]
    fn nodes(&self) -> (String, String, BTreeSet<String>) {
        let document = stage_module_node(
            ModuleArtifactBody::Document(self.document.body())
                .node_kind()
                .expect("a document is a node"),
            self.document.body(),
        )
        .expect("the document encodes");
        let function = stage_module_node(
            ModuleArtifactBody::HelperDefinition(&self.function)
                .node_kind()
                .expect("a declaration is a node"),
            &self.function,
        )
        .expect("the declaration encodes");
        let nodes = document
            .nodes
            .keys()
            .chain(function.nodes.keys())
            .map(ToString::to_string)
            .collect();
        (document.root.to_string(), function.root.to_string(), nodes)
    }

    /// Hold the document and its declaration under `claim`, as
    /// `KernelDocuments::publish` does, then publish definition `n`.
    async fn publish(
        &self,
        world: &World,
        claim: &crate::ReferrerClaim,
        n: u32,
    ) -> Result<crate::ProcessDefinition, crate::PluginError> {
        world
            .ports
            .modules()
            .hold_module_artifacts(
                claim,
                &[
                    ModuleArtifactHold::Acquire {
                        module_ref: &self.function_ref,
                        body: ModuleArtifactBody::HelperDefinition(&self.function),
                    },
                    ModuleArtifactHold::Publish {
                        module_ref: &self.document_ref,
                        body: ModuleArtifactBody::Document(self.document.body()),
                    },
                ],
            )
            .await?;
        world
            .ports
            .publish_definition(&world.engines, claim, &self.draft(n))
            .await
    }

    /// Whether every node is stored and each artifact's row roots its own,
    /// or none is stored and neither row is: the only two states a holder
    /// may observe.
    async fn stored(&self, probe: &dyn DefinitionGraphProbe, what: &str) -> bool {
        let (document_root, function_root, nodes) = self.nodes();
        let stored = probe.graph_nodes().await;
        let rows = (
            probe.module_row(&self.document_ref).await,
            probe.module_row(&self.function_ref).await,
        );
        if nodes.is_subset(&stored) {
            assert_eq!(
                rows,
                (
                    ModuleRow::Node(document_root),
                    ModuleRow::Node(function_root)
                ),
                "{what}: each row roots its stored node"
            );
            true
        } else {
            assert_eq!(
                nodes.intersection(&stored).count(),
                0,
                "{what}: a node of an unheld body leaked, or a held one was lost"
            );
            assert_eq!(
                rows,
                (ModuleRow::Absent, ModuleRow::Absent),
                "{what}: a row outlived its node"
            );
            false
        }
    }
}

/// FIG-6469: a definition and its declarations are stored as exactly one
/// graph body each, which its artifact row roots directly.
///
/// A host publishes a document over one helper and a definition naming
/// both. Each row points at the graph node its body encodes to, the store
/// gained exactly those nodes and no opaque copy, and the document reads
/// back as its body. A second definition naming the same document adds no
/// node.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn a_definition_and_its_declarations_store_one_graph_body_under_a_direct_edge(
    registry: std::sync::Arc<dyn crate::ProcessRegistry>,
    ports: crate::ArtifactReferrerPorts,
    probe: std::sync::Arc<dyn DefinitionGraphProbe>,
) {
    let world = World::new(registry, ports);
    let published = Published::new(&world.tag);
    let (document_root, function_root, nodes) = published.nodes();
    assert!(nodes.len() > 2, "the document streams into child nodes");
    let before = probe.graph_nodes().await;
    let (_, claim) = pin();
    let definition = published
        .publish(&world, &claim, 1)
        .await
        .expect("publish under the pin");
    assert_eq!(
        probe.module_row(&published.document_ref).await,
        ModuleRow::Node(document_root),
        "the document's row roots its Document node, not a copy of its body"
    );
    assert_eq!(
        probe.module_row(&published.function_ref).await,
        ModuleRow::Node(function_root),
        "the declaration's row roots its HelperDefinition node"
    );
    let gained: BTreeSet<String> = probe
        .graph_nodes()
        .await
        .difference(&before)
        .cloned()
        .collect();
    assert_eq!(gained, nodes, "the store gained exactly the bodies' nodes");
    assert_eq!(
        world
            .ports
            .modules()
            .get_module_artifact(&published.document_ref)
            .await
            .expect("read the document"),
        Some(published.document.body().to_vec()),
        "the document reads back as its body"
    );
    assert!(
        world
            .ports
            .definitions()
            .get_process_definition(&definition.id)
            .await
            .expect("read the descriptor")
            .is_some()
    );
    let (_, other) = pin();
    published
        .publish(&world, &other, 2)
        .await
        .expect("a second definition of the document");
    assert_eq!(
        probe
            .graph_nodes()
            .await
            .difference(&before)
            .cloned()
            .collect::<BTreeSet<_>>(),
        nodes,
        "a second definition naming the document stores no second body"
    );
}

/// FIG-6469: a definition's nodes are reclaimed with their last owner, and
/// kept while a second owner holds them.
///
/// Two hosts each publish a definition of the same document under their
/// own pin. The first pin's end leaves the nodes to the second; the
/// second's end reclaims them, and their rows.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn a_definitions_graph_nodes_are_reclaimed_with_their_last_owner(
    registry: std::sync::Arc<dyn crate::ProcessRegistry>,
    ports: crate::ArtifactReferrerPorts,
    probe: std::sync::Arc<dyn DefinitionGraphProbe>,
) {
    let world = World::new(registry, ports);
    let published = Published::new(&world.tag);
    let (first, first_claim) = pin();
    let (second, second_claim) = pin();
    let one = published
        .publish(&world, &first_claim, 1)
        .await
        .expect("the first host publishes");
    let two = published
        .publish(&world, &second_claim, 2)
        .await
        .expect("the second host publishes");
    assert!(published.stored(probe.as_ref(), "both pins hold").await);
    world.end(first).await;
    world.drain().await;
    assert!(
        published
            .stored(probe.as_ref(), "the second pin still holds")
            .await,
        "a second owner keeps the nodes"
    );
    let descriptors = world.ports.definitions();
    assert!(
        descriptors
            .get_process_definition(&one.id)
            .await
            .expect("read")
            .is_none()
    );
    assert!(
        descriptors
            .get_process_definition(&two.id)
            .await
            .expect("read")
            .is_some()
    );
    world.end(second).await;
    world.drain().await;
    assert!(
        !published
            .stored(probe.as_ref(), "the last owner ended")
            .await,
        "the last owner's end reclaims the nodes"
    );
}

/// FIG-6469: a crash at a definition's publication, its severance or its
/// nodes' retirement neither loses a node a holder needs nor leaks one.
///
/// A publication cut before its edge commits leaves nothing behind, and its
/// retry, and a replay of it, store the nodes once. A severance cut before
/// the edge goes, and a retirement cut at the nodes' delete, each leave the
/// pin's nodes and rows whole; the end run again reclaims them all.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn a_crash_at_definition_publication_severance_or_retirement_neither_loses_nor_leaks_a_node(
    registry: std::sync::Arc<dyn crate::ProcessRegistry>,
    ports: crate::ArtifactReferrerPorts,
    probe: std::sync::Arc<dyn DefinitionGraphProbe>,
) {
    let world = World::new(registry, ports);
    let published = Published::new(&world.tag);
    let (holder, claim) = pin();
    let before = probe.graph_nodes().await;

    probe.cut(DefinitionGraphCut::Publication).await;
    published
        .publish(&world, &claim, 1)
        .await
        .expect_err("the publication is cut");
    probe.clear_cut().await;
    assert_eq!(
        probe.graph_nodes().await,
        before,
        "a cut publication leaks no node"
    );
    assert!(!published.stored(probe.as_ref(), "after the cut").await);
    published
        .publish(&world, &claim, 1)
        .await
        .expect("the retried publication");
    let after = probe.graph_nodes().await;
    published
        .publish(&world, &claim, 1)
        .await
        .expect("a replayed publication, its answer lost");
    assert_eq!(
        probe.graph_nodes().await,
        after,
        "a replay stores nothing again"
    );
    assert!(published.stored(probe.as_ref(), "published").await);

    // A pin's end severs its edges and peels what they held in the
    // transaction that arms its cleanup.
    for cut in [
        DefinitionGraphCut::Severance,
        DefinitionGraphCut::Retirement,
    ] {
        probe.cut(cut).await;
        world
            .ports
            .end(holder.clone())
            .await
            .expect_err("the end is cut");
        probe.clear_cut().await;
        assert!(
            published
                .stored(probe.as_ref(), &format!("after the {cut:?} cut"))
                .await,
            "a {cut:?} cut loses no node of a held body"
        );
    }
    world.end(holder).await;
    world.drain().await;
    assert!(
        !published
            .stored(probe.as_ref(), "after the end ran again")
            .await,
        "the end's recovery reclaims every node"
    );
    assert_eq!(
        probe.graph_nodes().await,
        before,
        "nothing the definition stored is left"
    );
}
