//! A kernel run as checkpoint graph state (FIG-5887 §3, §4, §9): the one
//! adapter between a machine and the graph's canonical nodes.
//!
//! The worker owns the machine export and import adapters; it alone
//! decodes guest frames into a running machine. A run's end is
//! written by [`export_end`] and read back by [`read_end`]. Every node is
//! staged through the store's graph module, which places text and bytes over
//! the inline bound in shared leaves and streams a structure too large for
//! one node through structure blocks, so a state is the whole structure and
//! no large payload is written twice.
//!
//! A run pins the exact helper release manifest it was written under. Its
//! nodes are installed beside the state by whoever pins the release
//! ([`release_manifests`] stages them deterministically on either side), so
//! a state carries only the manifest's reference.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_core_store::checkpoint_graph::bodies::OwnerBinding;
use lash_core_store::checkpoint_graph::{
    DecodeLimits, EncodeBudget, EncodedNode, ExplicitLimit, GraphDecoder, GraphRefusal,
    GraphStaging, NodeId, NodeKind, NodeSource, StoredNode, check_envelope,
};
use lash_kernel_doc::graph_tokens::ChildRef;
use lash_kernel_doc::{Datum, FrozenDatum, FunctionId};
use lash_kernel_state::{AdmittedWait, KernelWhole, ParkedRun, TerminalWhole, WholeError};
use lash_kernel_vm::{End, TerminalError, terminal_end, terminal_whole};
use lash_vm_protocol::{
    EncodedPayload, GraphNodeBytes, GraphRoot, KernelRestoreRefusal, SealedGraphState, VmOwner,
};
use lash_vm_releases::HeldManifests;

/// Why a run's graph state was not written or not read.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum GraphStateFailure {
    /// The graph refused a node: its encoding, a bound, a missing or
    /// malformed node.
    #[error(transparent)]
    Graph(#[from] GraphRefusal),
    /// The machine cannot be written here: it has ended, or a wait has not
    /// been handed out at a park.
    #[error("the machine's state cannot be written: {0}")]
    Unwritable(String),
    /// The kernel will not resume the state: its precise, typed refusal.
    #[error(transparent)]
    Restore(#[from] KernelRestoreRefusal),
}

fn staging(bound: u64) -> GraphStaging {
    GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(bound)))
}

#[cfg(test)]
fn stage(staging: &mut GraphStaging, kind: u32, logical: &[u8]) -> Result<ChildRef, GraphRefusal> {
    staging
        .encode(
            NodeKind(kind),
            lash_core_store::checkpoint_graph::TypedBody::logical(logical),
        )
        .map(|id| id.child_ref())
}

fn freeze(staging: &mut GraphStaging, datum: &FrozenDatum) -> Result<ChildRef, GraphRefusal> {
    staging
        .encode_value(NodeKind::FROZEN_VALUE, datum)
        .map(|id| id.child_ref())
}

/// The helper releases this build holds as graph nodes.
#[derive(Clone, Debug)]
pub struct ReleaseGraph {
    /// Each retained release's manifest, by reference.
    pub manifests: HeldManifests,
    /// Every manifest and helper-definition node: what a parent installs to
    /// pin the releases.
    pub nodes: BTreeMap<NodeId, EncodedNode>,
}

impl ReleaseGraph {
    /// Checks every node of every release this build holds, each once per
    /// process: a host that installs the releases does it before its pool
    /// is ready, so no turn checks them. A worker checks only what it reads.
    ///
    /// # Errors
    ///
    /// The refusal of a node that does not check: a defect of the build.
    pub fn check(&self) -> Result<(), GraphStateFailure> {
        for (id, node) in &self.nodes {
            check_envelope(&node.bytes, *id, &DecodeLimits::within_total(u64::MAX))?;
            if self.manifests.get(&id.child_ref()).is_some() {
                let _ = lash_core_store::checkpoint_graph::node::immutable_closure(*id);
            }
        }
        Ok(())
    }

    /// The key the release manifest `release` names is pinned under
    /// ([`HeldManifests::pin_key`]) and every node it reaches, when this
    /// build holds it: what a park that pins it installs.
    #[must_use]
    pub fn pinned(&self, release: NodeId) -> Option<(String, BTreeMap<NodeId, EncodedNode>)> {
        let key = self.manifests.pin_key(&release.child_ref())?;
        let mut nodes = BTreeMap::new();
        let mut pending = vec![release];
        while let Some(id) = pending.pop() {
            if nodes.contains_key(&id) {
                continue;
            }
            let node = self.nodes.get(&id)?;
            pending.extend(
                check_envelope(&node.bytes, id, &DecodeLimits::within_total(u64::MAX))
                    .ok()?
                    .children
                    .iter(),
            );
            nodes.insert(id, node.clone());
        }
        Some((key, nodes))
    }
}

/// The helper releases this build holds for the dialects it was built with,
/// staged. Staging is deterministic:
/// a parent and a worker of one build name every manifest alike. The build's
/// immutable nodes are staged once per process and shared by every cell.
///
/// # Errors
///
/// A refusal of the graph, or the build's own releases not decoding.
pub fn release_manifests() -> Result<ReleaseGraph, GraphStateFailure> {
    static RELEASES: std::sync::OnceLock<Result<ReleaseGraph, GraphStateFailure>> =
        std::sync::OnceLock::new();
    RELEASES.get_or_init(read_release_manifests).clone()
}

fn read_release_manifests() -> Result<ReleaseGraph, GraphStateFailure> {
    use lash_kernel_doc::graph_tokens::{TokenCodec as _, TokenReader};
    let build_error = |error: lash_kernel_doc::graph_tokens::TokenError| {
        GraphStateFailure::Unwritable(error.to_string())
    };
    let mut manifests = HeldManifests::default();
    let mut nodes = BTreeMap::new();
    // Two dialects share the kernel library's definition nodes, by digest.
    for built in lash_vm_releases::DIALECTS {
        manifests
            .add_staged(built.dialect, built.checkpoint_manifests)
            .map_err(build_error)?;
        let mut input = TokenReader::new(built.checkpoint_nodes);
        let count = input.seq().map_err(build_error)?;
        for _ in 0..count {
            input.expect_struct(2).map_err(build_error)?;
            let id = NodeId(ChildRef::decode(&mut input).map_err(build_error)?.0);
            let bytes = input.bytes().map_err(build_error)?;
            nodes.entry(id).or_insert_with(|| EncodedNode {
                id,
                bytes: Arc::from(bytes),
            });
        }
        input.finish().map_err(build_error)?;
    }
    // Each node is checked the first time a reader meets it.
    lash_core_store::checkpoint_graph::node::register_immutable_catalog(&nodes);
    Ok(ReleaseGraph { manifests, nodes })
}

/// A run's state as graph nodes: a root (a KernelWhole at a park, a
/// TerminalWhole at an end), the kernel version it was written under, and
/// every node it staged but the release manifest's it pins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WholeState {
    pub root: NodeId,
    pub kind: NodeKind,
    pub kernel: u32,
    pub nodes: BTreeMap<NodeId, EncodedNode>,
}

impl WholeState {
    fn staged(staging: GraphStaging, root: NodeId, kind: NodeKind, kernel: u32) -> Self {
        Self {
            root,
            kind,
            kernel,
            nodes: staging.into_nodes(),
        }
    }

    /// This state sealed under `owner` and `document`, as a worker hands it
    /// over.
    #[must_use]
    pub fn sealed(&self, owner: VmOwner, document: &str) -> SealedGraphState {
        let nodes = self
            .nodes
            .values()
            .map(|node| GraphNodeBytes {
                id: node.id.0,
                bytes: node.bytes.to_vec(),
            })
            .collect();
        SealedGraphState::seal(
            owner,
            self.kernel,
            document,
            GraphRoot {
                kind: self.kind.0,
                id: self.root.0,
            },
            nodes,
        )
    }

    /// The state `sealed` carries, each node checked against its digest
    /// within `limits`.
    ///
    /// # Errors
    ///
    /// A node whose envelope or digest is refused.
    pub fn of_sealed(
        sealed: &SealedGraphState,
        limits: &DecodeLimits,
    ) -> Result<Self, GraphRefusal> {
        Ok(Self {
            root: NodeId(sealed.root().id),
            kind: NodeKind(sealed.root().kind),
            kernel: sealed.kernel(),
            nodes: Nodes::of(sealed, limits)?.0,
        })
    }

    /// The KernelWhole `root` in `source` and every node it reaches but the
    /// release manifest it pins: what a parked root's state resumes from.
    ///
    /// # Errors
    ///
    /// A missing node, or one whose envelope is refused.
    pub fn parked_in(
        source: &dyn NodeSource,
        root: NodeId,
        kernel: u32,
        limits: &DecodeLimits,
    ) -> Result<Self, GraphRefusal> {
        let mut nodes = BTreeMap::new();
        let mut pending = vec![root];
        while let Some(id) = pending.pop() {
            if nodes.contains_key(&id) {
                continue;
            }
            let stored = source.fetch(&id).ok_or(GraphRefusal::MissingNode(id))?;
            if stored.encoding == lash_core_store::checkpoint_graph::BlobEncoding::Graph {
                let envelope = check_envelope(stored.bytes, id, limits)?;
                if envelope.kind == NodeKind::RELEASE_MANIFEST {
                    continue;
                }
                pending.extend(envelope.children.iter());
            }
            nodes.insert(
                id,
                EncodedNode {
                    id,
                    bytes: Arc::from(stored.bytes),
                },
            );
        }
        Ok(Self {
            root,
            kind: NodeKind::KERNEL_WHOLE,
            kernel,
            nodes,
        })
    }
}

/// A parked run as the kernel's own structure, outside any machine: what a
/// migration or an adoption carries.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenedRun {
    pub run: ParkedRun,
    /// The helper release manifest it pins.
    pub release: ChildRef,
    /// The execution it belongs to.
    pub binding: OwnerBinding,
}

/// The run `state`'s KernelWhole holds, read over the nodes of `releases`
/// (the state omits the manifest it pins), each wait's request taken from
/// `admitted` and the run's library functions `functions`.
///
/// # Errors
///
/// A graph refusal of a node, a whole whose kernel version is not the
/// state's, or a wait no admission matches.
pub fn open_whole(
    state: &WholeState,
    releases: &ReleaseGraph,
    admitted: &[AdmittedWait],
    functions: BTreeSet<FunctionId>,
    limits: DecodeLimits,
) -> Result<OpenedRun, GraphStateFailure> {
    let source = Layered {
        state: &state.nodes,
        release: &releases.nodes,
    };
    let mut decoder = GraphDecoder::new(&source, limits);
    let whole: KernelWhole = decoder.decode(state.root, NodeKind::KERNEL_WHOLE)?;
    if whole.kernel != state.kernel {
        return Err(malformed(format!(
            "the state is sealed under kernel version {}, and its whole states {}",
            state.kernel, whole.kernel
        ))
        .into());
    }
    let binding: OwnerBinding = decoder.decode(NodeId(whole.owner.0), NodeKind::OWNER_BINDING)?;
    let release = whole.release;
    let mut thawing = GraphDecoder::new(&state.nodes, limits);
    let run = whole
        .restore(
            functions,
            |child| thaw(&mut thawing, child),
            |identity| {
                admitted
                    .iter()
                    .find(|wait| &wait.identity == identity)
                    .map(|wait| wait.request.clone())
            },
        )
        .map_err(|error| match error {
            WholeError::Frozen(refusal) => GraphStateFailure::Graph(refusal),
            WholeError::AdmissionMissing(identity) => KernelRestoreRefusal::AdmissionUnresolved {
                effect: format!("{identity:?}"),
            }
            .into(),
            WholeError::AdmissionMismatch(identity) => KernelRestoreRefusal::AdmissionMismatch {
                effect: format!("{identity:?}"),
            }
            .into(),
            other => malformed(other).into(),
        })?;
    Ok(OpenedRun {
        run,
        release,
        binding,
    })
}

/// `opened` as a whole state again, staged within `bound` envelope bytes:
/// a carried run's state.
///
/// # Errors
///
/// A graph refusal past `bound`, or a wait not handed out at a park.
pub fn capture_whole(opened: &OpenedRun, bound: u64) -> Result<WholeState, GraphStateFailure> {
    let mut staging = staging(bound);
    let owner = staging.encode_value(NodeKind::OWNER_BINDING, &opened.binding)?;
    let whole = KernelWhole::capture(&opened.run, opened.release, owner.child_ref(), |datum| {
        freeze(&mut staging, datum)
    })
    .map_err(|error| match error {
        WholeError::Frozen(refusal) => GraphStateFailure::Graph(refusal),
        other => GraphStateFailure::Unwritable(other.to_string()),
    })?;
    let root = staging.encode_value(NodeKind::KERNEL_WHOLE, &whole)?;
    Ok(WholeState::staged(
        staging,
        root,
        NodeKind::KERNEL_WHOLE,
        whole.kernel,
    ))
}

/// A sealed state's nodes, each checked against its digest.
struct Nodes(BTreeMap<NodeId, EncodedNode>);

/// A state's nodes over the release nodes this build stages: a state omits
/// the manifest it pins, which its reader holds.
pub(crate) struct Layered<'a> {
    pub(crate) state: &'a BTreeMap<NodeId, EncodedNode>,
    pub(crate) release: &'a BTreeMap<NodeId, EncodedNode>,
}

impl NodeSource for Layered<'_> {
    fn fetch(&self, id: &NodeId) -> Option<StoredNode<'_>> {
        self.state.fetch(id).or_else(|| self.release.fetch(id))
    }
}

impl Nodes {
    fn of(state: &SealedGraphState, limits: &DecodeLimits) -> Result<Self, GraphRefusal> {
        let mut nodes = BTreeMap::new();
        for node in state.nodes() {
            let id = NodeId(node.id);
            check_envelope(&node.bytes, id, limits)?;
            nodes.insert(
                id,
                EncodedNode {
                    id,
                    bytes: Arc::from(node.bytes.as_slice()),
                },
            );
        }
        Ok(Self(nodes))
    }
}

fn thaw(decoder: &mut GraphDecoder<'_>, child: ChildRef) -> Result<Datum, GraphRefusal> {
    decoder
        .decode::<FrozenDatum>(NodeId(child.0), NodeKind::FROZEN_VALUE)
        .map(|frozen| frozen.0)
}

fn malformed(problem: impl ToString) -> KernelRestoreRefusal {
    KernelRestoreRefusal::Malformed {
        problem: problem.to_string(),
    }
}

/// An admitted wait in the encoding [`ParkedGraph::admissions`] carries.
#[must_use]
pub fn admission(wait: &AdmittedWait) -> EncodedPayload {
    EncodedPayload(lash_kernel_doc::graph_tokens::encode_body(wait))
}

/// A run's end as graph state: a TerminalWhole root with its one object
/// table and frozen result, or `None` for a cancelled run, which records no
/// end.
///
/// # Errors
///
/// A graph refusal past `bound`, or [`GraphStateFailure::Unwritable`] for
/// an end whose binding sets disagree on an object.
pub fn export_end(
    end: &End,
    owner: VmOwner,
    kernel: u32,
    document: &str,
    bound: u64,
) -> Result<Option<SealedGraphState>, GraphStateFailure> {
    Ok(export_end_state(end, kernel, bound)?.map(|state| state.sealed(owner, document)))
}

/// [`export_end`], unsealed: the TerminalWhole a run's end commits.
///
/// # Errors
///
/// As [`export_end`].
pub fn export_end_state(
    end: &End,
    kernel: u32,
    bound: u64,
) -> Result<Option<WholeState>, GraphStateFailure> {
    let mut staging = staging(bound);
    let Some(whole) =
        terminal_whole(end, |datum| freeze(&mut staging, datum)).map_err(terminal_failure)?
    else {
        return Ok(None);
    };
    let root = staging.encode_value(NodeKind::TERMINAL_WHOLE, &whole)?;
    Ok(Some(WholeState::staged(
        staging,
        root,
        NodeKind::TERMINAL_WHOLE,
        kernel,
    )))
}

fn terminal_failure(error: TerminalError<GraphRefusal>) -> GraphStateFailure {
    match error {
        TerminalError::Frozen(refusal) => GraphStateFailure::Graph(refusal),
        other => GraphStateFailure::Unwritable(other.to_string()),
    }
}

/// The end a TerminalWhole graph state records.
///
/// # Errors
///
/// A graph refusal of a node, or a terminal whose table and bindings do
/// not match.
pub fn read_end(state: &SealedGraphState, limits: DecodeLimits) -> Result<End, GraphStateFailure> {
    let nodes = Nodes::of(state, &limits)?;
    read_end_in(&nodes.0, NodeId(state.root().id), limits)
}

/// The end TerminalWhole `root` in `source` records.
///
/// # Errors
///
/// As [`read_end`].
pub fn read_end_in(
    source: &dyn NodeSource,
    root: NodeId,
    limits: DecodeLimits,
) -> Result<End, GraphStateFailure> {
    let mut decoder = GraphDecoder::new(source, limits);
    let whole: TerminalWhole = decoder.decode(root, NodeKind::TERMINAL_WHOLE)?;
    terminal_end(whole, |child| thaw(&mut decoder, child)).map_err(|error| match error {
        TerminalError::Frozen(refusal) => GraphStateFailure::Graph(refusal),
        other => malformed(other).into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// FIG-5894: immutable build release nodes are shared by independent
    /// cells, rather than encoding the entire helper catalog for each cell.
    #[test]
    fn independent_cells_share_the_builds_staged_release_nodes() {
        let first = release_manifests().expect("the build's release graph");
        let second = release_manifests().expect("the same build's release graph");
        assert!(!first.nodes.is_empty());
        assert_eq!(first.manifests, second.manifests);
        for (id, node) in &first.nodes {
            assert!(
                Arc::ptr_eq(&node.bytes, &second.nodes[id].bytes),
                "independent cells restaged release node {id:?}"
            );
        }
    }

    /// Build-time preparation retains exactly the checkpoint nodes and
    /// release identity the runtime encoder produces.
    #[test]
    fn the_builds_checkpoint_catalog_is_the_canonical_release_graph() {
        let mut staging = staging(u64::MAX);
        let mut manifests = HeldManifests::default();
        for dialect in lash_vm_releases::DIALECTS {
            let held = dialect.held_releases().expect("the build's releases");
            manifests
                .stage(dialect.dialect, &held, |kind, logical| {
                    stage(&mut staging, kind, logical)
                })
                .expect("canonical release graph");
        }
        let built = release_manifests().expect("the build's staged catalog");
        assert_eq!(built.manifests, manifests);
        assert_eq!(built.nodes, staging.into_nodes());
    }
}
