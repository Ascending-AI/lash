//! A kernel run as checkpoint graph state (FIG-5887 §3, §4, §9): the one
//! adapter between a machine and the graph's canonical nodes.
//!
//! The worker exports a park through [`export_parked`] and resumes one
//! through [`import_parked`]; it alone decodes guest frames. A run's end is
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
    GraphStaging, NodeId, NodeKind, NodeSource, StoredNode, TypedBody, check_envelope,
};
use lash_kernel_doc::graph_tokens::{ChildRef, decode_body};
use lash_kernel_doc::{Datum, EffectIdentity, FrozenDatum, FunctionRegistry};
use lash_kernel_state::{AdmittedRequest, AdmittedWait, KernelWhole, TerminalWhole, WholeError};
use lash_kernel_vm::{
    Bounds, End, ImportError, KernelMachine, Program, TerminalError, WholeExportError,
    WholeImportError, terminal_end, terminal_whole,
};
use lash_vm_protocol::{
    EncodedPayload, GraphNodeBytes, GraphRoot, KernelRestoreRefusal, ParkedGraph, SealedGraphState,
    VmOwner,
};
use lash_vm_releases::{HeldManifests, ReleaseRefusal};

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

fn stage(staging: &mut GraphStaging, kind: u32, logical: &[u8]) -> Result<ChildRef, GraphRefusal> {
    staging
        .encode(NodeKind(kind), TypedBody::logical(logical))
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

/// The helper releases this build holds, staged. Staging is deterministic:
/// a parent and a worker of one build name every manifest alike.
///
/// # Errors
///
/// A refusal of the graph, or the build's own releases not decoding.
pub fn release_manifests() -> Result<ReleaseGraph, GraphStateFailure> {
    let held = lash_vm_releases::held_releases().map_err(GraphStateFailure::Unwritable)?;
    let mut staging = staging(u64::MAX);
    let manifests =
        HeldManifests::stage(&held, |kind, logical| stage(&mut staging, kind, logical))?;
    Ok(ReleaseGraph {
        manifests,
        nodes: staging.into_nodes(),
    })
}

fn seal(
    staging: GraphStaging,
    owner: VmOwner,
    kernel: u32,
    document: &str,
    root: NodeId,
    kind: NodeKind,
) -> SealedGraphState {
    let nodes = staging
        .into_nodes()
        .into_values()
        .map(|node| GraphNodeBytes {
            id: node.id.0,
            bytes: node.bytes.to_vec(),
        })
        .collect();
    SealedGraphState::seal(
        owner,
        kernel,
        document,
        GraphRoot {
            kind: kind.0,
            id: root.0,
        },
        nodes,
    )
}

/// The run's state at this park as graph state: a KernelWhole that pins
/// `release` and belongs to `binding`, with every node it needs but the
/// release's, staged within `bound` envelope bytes.
///
/// # Errors
///
/// [`GraphStateFailure::Unwritable`] off a park; a graph refusal past
/// `bound` or a node bound.
pub fn export_parked(
    machine: &mut KernelMachine,
    owner: VmOwner,
    binding: &OwnerBinding,
    release: ChildRef,
    document: &str,
    bound: u64,
) -> Result<SealedGraphState, GraphStateFailure> {
    let mut staging = staging(bound);
    let owner_ref = staging.encode_value(NodeKind::OWNER_BINDING, binding)?;
    let whole = machine
        .export_whole(release, owner_ref.child_ref(), |datum| {
            freeze(&mut staging, datum)
        })
        .map_err(|error| match error {
            WholeExportError::Whole(WholeError::Frozen(refusal)) => {
                GraphStateFailure::Graph(refusal)
            }
            other => GraphStateFailure::Unwritable(other.to_string()),
        })?;
    let root = staging.encode_value(NodeKind::KERNEL_WHOLE, &whole)?;
    Ok(seal(
        staging,
        owner,
        whole.kernel,
        document,
        root,
        NodeKind::KERNEL_WHOLE,
    ))
}

/// A sealed state's nodes, each checked against its digest.
struct Nodes(BTreeMap<NodeId, EncodedNode>);

/// A state's nodes over the release nodes this build stages: a state omits
/// the manifest it pins, which its reader holds.
struct Layered<'a> {
    state: &'a BTreeMap<NodeId, EncodedNode>,
    release: &'a BTreeMap<NodeId, EncodedNode>,
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

/// The machine a parked graph state resumes in: the state's KernelWhole and
/// its frozen outcomes read within `limits`, its pinned release admitted
/// against the releases this build holds (whose nodes it reads beside the
/// state's) and `registry`'s functions and natives, and every wait's request
/// taken from its admission.
///
/// Returns the machine and the release manifest the run pins, which its
/// next export pins again.
///
/// # Errors
///
/// A graph refusal of a node, or the kernel's typed refusal: the release
/// manifest, a helper or a native the run needs, a wait no admission
/// matches, or the machine's own import refusal.
pub fn import_parked(
    program: Program,
    bounds: Bounds,
    parked: &ParkedGraph,
    releases: &ReleaseGraph,
    registry: &FunctionRegistry,
    limits: DecodeLimits,
) -> Result<(KernelMachine, ChildRef), GraphStateFailure> {
    let nodes = Nodes::of(&parked.state, &limits)?;
    let source = Layered {
        state: &nodes.0,
        release: &releases.nodes,
    };
    let mut decoder = GraphDecoder::new(&source, limits);
    let root = NodeId(parked.state.root().id);
    let whole: KernelWhole = decoder.decode(root, NodeKind::KERNEL_WHOLE)?;
    decoder.child_kind(root, NodeId(whole.owner.0), &[NodeKind::OWNER_BINDING])?;
    let required: BTreeSet<_> = program
        .document
        .manifest
        .functions
        .keys()
        .copied()
        .collect();
    releases
        .manifests
        .admit(&whole.release, &required, |function| {
            registry
                .get(function)
                .map(|registered| registered.native.is_some())
        })
        .map_err(|refusal| match refusal {
            ReleaseRefusal::ReleaseMismatch { pinned } => KernelRestoreRefusal::ReleaseMismatch {
                pinned: NodeId(pinned.0).to_hex(),
            },
            ReleaseRefusal::MissingHelper { function } => KernelRestoreRefusal::MissingHelper {
                function: function.to_string(),
            },
            ReleaseRefusal::MissingNative { function } => KernelRestoreRefusal::MissingNative {
                function: function.to_string(),
            },
        })?;
    let mut admissions: BTreeMap<EffectIdentity, AdmittedRequest> = BTreeMap::new();
    for admission in &parked.admissions {
        let admitted: AdmittedWait = decode_body(&admission.0).map_err(malformed)?;
        if admissions
            .insert(admitted.identity.clone(), admitted.request)
            .is_some()
        {
            return Err(
                malformed(format!("effect {:?} is admitted twice", admitted.identity)).into(),
            );
        }
    }
    let release = whole.release;
    KernelMachine::import_whole(
        program,
        bounds,
        whole,
        |child| thaw(&mut decoder, child),
        |identity| admissions.get(identity).cloned(),
    )
    .map_err(|error| match error {
        WholeImportError::Whole(WholeError::Frozen(refusal)) => GraphStateFailure::Graph(refusal),
        WholeImportError::Whole(WholeError::AdmissionMissing(identity)) => {
            KernelRestoreRefusal::AdmissionUnresolved {
                effect: format!("{identity:?}"),
            }
            .into()
        }
        WholeImportError::Whole(WholeError::AdmissionMismatch(identity)) => {
            KernelRestoreRefusal::AdmissionMismatch {
                effect: format!("{identity:?}"),
            }
            .into()
        }
        WholeImportError::Whole(WholeError::Unpublished(wait)) => {
            malformed(format!("wait {wait:?} was never handed out")).into()
        }
        WholeImportError::Import(error) => restore_refusal(error).into(),
    })
    .map(|machine| (machine, release))
}

fn restore_refusal(error: ImportError) -> KernelRestoreRefusal {
    match error {
        ImportError::NotAdmitted(reason) => KernelRestoreRefusal::NotAdmitted {
            reason: reason.to_string(),
        },
        ImportError::DocumentMismatch { parked, given } => KernelRestoreRefusal::DocumentMismatch {
            parked: parked.to_string(),
            given: given.to_string(),
        },
        ImportError::KernelVersion { parked, supported } => {
            KernelRestoreRefusal::KernelVersion { parked, supported }
        }
        ImportError::MissingFunction { function } => KernelRestoreRefusal::MissingFunction {
            function: function.to_string(),
        },
        other => malformed(other),
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
    let mut staging = staging(bound);
    let Some(whole) =
        terminal_whole(end, |datum| freeze(&mut staging, datum)).map_err(terminal_failure)?
    else {
        return Ok(None);
    };
    let root = staging.encode_value(NodeKind::TERMINAL_WHOLE, &whole)?;
    Ok(Some(seal(
        staging,
        owner,
        kernel,
        document,
        root,
        NodeKind::TERMINAL_WHOLE,
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
    let mut decoder = GraphDecoder::new(&nodes.0, limits);
    let whole: TerminalWhole = decoder.decode(NodeId(state.root().id), NodeKind::TERMINAL_WHOLE)?;
    terminal_end(whole, |child| thaw(&mut decoder, child)).map_err(|error| match error {
        TerminalError::Frozen(refusal) => GraphStateFailure::Graph(refusal),
        other => malformed(other).into(),
    })
}
