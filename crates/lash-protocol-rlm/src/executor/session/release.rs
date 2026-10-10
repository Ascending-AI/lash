//! The helper release a function bundle names (FIG-5887 §4, §7).
//!
//! A bundle's members pin library functions by identity in their
//! documents' manifests. The bundle names the ReleaseManifest of a helper
//! release that holds them, so a stored function names the exact
//! definitions it was written against. A release manifest and its
//! definitions are the release pin's (`graph_release_pins`), installed once
//! for every session a build serves: a capture names the manifest and never
//! sends it, and a read finds it among the nodes supplied or the releases
//! this build holds. A manifest neither holds is missing, never repaired by
//! finding a release of the same name.
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
use lash_vm_runtime::{HelperReleaseIndex, ReleaseManifest};

use super::CodeModeSnapshotError;

/// The releases this build holds, each staged once in a process: a build's
/// releases never change while it runs.
struct Held {
    releases: Vec<HelperReleaseIndex>,
    /// Each release's manifest, by ordinal.
    manifests: BTreeMap<u32, ChildRef>,
    /// Every node of every manifest's closure.
    nodes: BTreeMap<NodeId, EncodedNode>,
}

fn held() -> Result<&'static Held, GraphRefusal> {
    static HELD: OnceLock<Result<Held, GraphRefusal>> = OnceLock::new();
    HELD.get_or_init(|| {
        let releases = lash_vm_runtime::standard_helper_releases().map_err(|error| {
            GraphRefusal::InvalidBody {
                kind: NodeKind::RELEASE_MANIFEST.0,
                reason: error.to_string(),
            }
        })?;
        let mut staging = super::graph::staging();
        let mut manifests = BTreeMap::new();
        for release in &releases {
            let manifest =
                lash_vm_runtime::stage_held_release(release.ordinal, |kind, logical| {
                    super::graph::stage_node(&mut staging, kind, logical)
                })?
                .ok_or(GraphRefusal::ReleaseMismatch)?;
            manifests.insert(release.ordinal, manifest);
        }
        Ok(Held {
            releases,
            manifests,
            nodes: staging.into_nodes(),
        })
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// The manifest `bundle` names: the newest release this build holds that
/// holds every helper of `bundle`'s pins that any release holds.
pub(super) fn manifest(bundle: &Bundle) -> Result<ChildRef, GraphRefusal> {
    let held = held()?;
    let pinned: BTreeSet<FunctionId> = bundle
        .members()
        .values()
        .flat_map(|member| member.document.manifest.functions.keys().copied())
        .collect();
    let released: BTreeSet<&FunctionId> = held
        .releases
        .iter()
        .flat_map(|release| &release.functions)
        .collect();
    held.releases
        .iter()
        .rev()
        .find(|release| {
            pinned
                .iter()
                .filter(|function| released.contains(function))
                .all(|function| release.functions.contains(function))
        })
        .or(held.releases.last())
        .and_then(|release| held.manifests.get(&release.ordinal).copied())
        .ok_or(GraphRefusal::ReleaseMismatch)
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
