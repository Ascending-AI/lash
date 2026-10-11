//! The helper release a function bundle names (FIG-5887 §4, §7).
//!
//! A bundle's members pin library functions by identity in their
//! documents' manifests. The bundle names the ReleaseManifest of a helper
//! release that holds them, so a stored function names the exact
//! definitions it was written against. A capture holds the manifest and its
//! definitions like any node its root reaches, so it sends them until a
//! commit installs them: a session whose cells never parked, and so never
//! pinned a release (`graph_release_pins`), installs the release its bundles
//! name with its own state. A read finds a manifest among the nodes supplied
//! or the releases this build holds. A manifest neither holds is missing,
//! never repaired by finding a release of the same name.
//!
//! A pin no release holds (a declared native, a helper this build changed)
//! is still pinned by identity, and a cell that installs the function is
//! refused for a helper its environment lacks, as ever
//! ([`lash_kernel_dialect::Unusable::LibraryMissing`]).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use lash_core_store::checkpoint_graph::{
    EncodedNode, GraphDecoder, GraphRefusal, NodeId, NodeKind, NodeSource, StoredNode,
};
use lash_kernel_dialect::Bundle;
use lash_kernel_doc::FunctionId;
use lash_kernel_doc::graph_tokens::{ChildRef, decode_body};
use lash_vm_runtime::ReleaseManifest;

use super::CodeModeSnapshotError;

/// The releases this build holds, each staged once in a process: a build's
/// releases never change while it runs.
struct Held {
    /// Each dialect's releases, oldest first: the manifest and every
    /// function it holds.
    lines: BTreeMap<String, Vec<(ChildRef, BTreeSet<FunctionId>)>>,
    /// Every node of every manifest's closure.
    nodes: BTreeMap<NodeId, EncodedNode>,
}

fn held() -> Result<&'static Held, GraphRefusal> {
    static HELD: OnceLock<Result<Held, GraphRefusal>> = OnceLock::new();
    HELD.get_or_init(|| {
        let graph = lash_vm_broker::kernel::state::release_manifests().map_err(|error| {
            GraphRefusal::InvalidBody {
                kind: NodeKind::RELEASE_MANIFEST.0,
                reason: error.to_string(),
            }
        })?;
        let lines = graph
            .manifests
            .dialects()
            .map(|dialect| {
                let mut releases: Vec<&ReleaseManifest> = Vec::new();
                let mut line: Vec<(u32, ChildRef, BTreeSet<FunctionId>)> = graph
                    .manifests
                    .of_dialect(dialect)
                    .map(|(reference, manifest)| {
                        releases.push(manifest);
                        (
                            manifest.ordinal,
                            reference,
                            manifest.function_ids().collect(),
                        )
                    })
                    .collect();
                line.sort_by_key(|(ordinal, _, _)| *ordinal);
                (
                    dialect.to_owned(),
                    line.into_iter()
                        .map(|(_, reference, functions)| (reference, functions))
                        .collect(),
                )
            })
            .collect();
        Ok(Held {
            lines,
            nodes: graph.nodes,
        })
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// The manifest `bundle`, written in `dialect`, names: the newest release
/// this build holds for the dialect that holds every helper of `bundle`'s
/// pins that any of the dialect's releases holds.
pub(super) fn manifest(bundle: &Bundle, dialect: &str) -> Result<ChildRef, GraphRefusal> {
    let held = held()?;
    let line = held
        .lines
        .get(dialect)
        .ok_or(GraphRefusal::ReleaseMismatch)?;
    let pinned: BTreeSet<FunctionId> = bundle
        .members()
        .values()
        .flat_map(|member| member.document.manifest.functions.keys().copied())
        .collect();
    let released: BTreeSet<&FunctionId> =
        line.iter().flat_map(|(_, functions)| functions).collect();
    line.iter()
        .rev()
        .find(|(_, functions)| {
            pinned
                .iter()
                .filter(|function| released.contains(function))
                .all(|function| functions.contains(function))
        })
        .or(line.last())
        .map(|(reference, _)| *reference)
        .ok_or(GraphRefusal::ReleaseMismatch)
}

/// Completes `nodes` with every node of a held release they reach and do
/// not hold: the manifests their bundles name, with their definitions. A
/// node no held release has is left out, for a read to refuse as missing.
///
/// # Errors
///
/// A node whose child table does not read, or a build whose releases do
/// not stage.
pub(super) fn complete(nodes: &mut BTreeMap<NodeId, EncodedNode>) -> Result<(), GraphRefusal> {
    let held = held()?;
    let mut wanted = Vec::new();
    for node in nodes.values() {
        wanted.extend(node.children()?);
    }
    while let Some(id) = wanted.pop() {
        if nodes.contains_key(&id) {
            continue;
        }
        let Some(node) = held.nodes.get(&id) else {
            continue;
        };
        wanted.extend(node.children()?);
        nodes.insert(id, node.clone());
    }
    Ok(())
}

/// The nodes supplied for a read, and beside them the release manifests
/// this build holds.
pub(super) struct WithHeld<'a> {
    supplied: &'a BTreeMap<NodeId, EncodedNode>,
    held: Option<&'static Held>,
}

impl<'a> WithHeld<'a> {
    pub(super) fn new(supplied: &'a BTreeMap<NodeId, EncodedNode>) -> Self {
        Self {
            supplied,
            held: held().ok(),
        }
    }

    /// The bytes a read may find here.
    pub(super) fn bytes(&self) -> u64 {
        self.supplied
            .values()
            .chain(self.held.into_iter().flat_map(|held| held.nodes.values()))
            .map(EncodedNode::len)
            .sum()
    }
}

impl NodeSource for WithHeld<'_> {
    fn fetch(&self, id: &NodeId) -> Option<StoredNode<'_>> {
        self.supplied
            .fetch(id)
            .or_else(|| self.held.and_then(|held| held.nodes.fetch(id)))
    }
}

/// Refuses `manifest` unless it is a release manifest.
pub(super) fn check(
    decoder: &mut GraphDecoder<'_>,
    manifest: NodeId,
) -> Result<(), CodeModeSnapshotError> {
    let logical = decoder.logical_body(manifest, NodeKind::RELEASE_MANIFEST)?;
    decode_body::<ReleaseManifest>(&logical).map_err(|error| {
        GraphRefusal::from_token(error, Some(manifest), Some(NodeKind::RELEASE_MANIFEST.0))
    })?;
    Ok(())
}
