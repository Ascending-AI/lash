//! Machine construction and checkpoint adapters, owned by the worker.
use lash_core_store::checkpoint_graph::bodies::OwnerBinding;
use lash_core_store::checkpoint_graph::{
    DecodeLimits, EncodeBudget, EncodedNode, ExplicitLimit, GraphDecoder, GraphRefusal,
    GraphStaging, NodeId, NodeKind, NodeSource, StoredNode, check_envelope,
};
use lash_kernel_doc::graph_tokens::{ChildRef, decode_body};
use lash_kernel_doc::{Datum, EffectIdentity, FrozenDatum, FunctionRegistry};
use lash_kernel_state::{AdmittedRequest, AdmittedWait, KernelWhole, WholeError};
use lash_kernel_vm::{
    Bounds, ImportError, KernelMachine, Program, WholeExportError, WholeImportError,
};
pub use lash_vm_client::graph::*;
use lash_vm_protocol::{KernelRestoreRefusal, ParkedGraph, SealedGraphState, VmOwner};
use lash_vm_releases::ReleaseRefusal;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

fn staging(bound: u64) -> GraphStaging {
    GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(bound)))
}
fn freeze(staging: &mut GraphStaging, datum: &FrozenDatum) -> Result<ChildRef, GraphRefusal> {
    staging
        .encode_value(NodeKind::FROZEN_VALUE, datum)
        .map(|id| id.child_ref())
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
struct Layered<'a> {
    state: &'a BTreeMap<NodeId, EncodedNode>,
    release: &'a BTreeMap<NodeId, EncodedNode>,
}
impl NodeSource for Layered<'_> {
    fn fetch(&self, id: &NodeId) -> Option<StoredNode<'_>> {
        self.state.fetch(id).or_else(|| self.release.fetch(id))
    }
}
struct Nodes(BTreeMap<NodeId, EncodedNode>);
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
    export_whole_state(machine, binding, release, bound).map(|state| state.sealed(owner, document))
}

/// [`export_parked`], unsealed: the state a machine in this process writes.
///
/// # Errors
///
/// As [`export_parked`].
pub fn export_whole_state(
    machine: &mut KernelMachine,
    binding: &OwnerBinding,
    release: ChildRef,
    bound: u64,
) -> Result<WholeState, GraphStateFailure> {
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
    Ok(WholeState {
        root,
        kind: NodeKind::KERNEL_WHOLE,
        kernel: whole.kernel,
        nodes: staging.into_nodes(),
    })
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
    let mut admitted = Vec::with_capacity(parked.admissions.len());
    for admission in &parked.admissions {
        admitted.push(decode_body::<AdmittedWait>(&admission.0).map_err(malformed)?);
    }
    import_whole_state(
        program,
        bounds,
        NodeId(parked.state.root().id),
        &nodes.0,
        admitted,
        releases,
        registry,
        limits,
    )
}

/// [`import_parked`] from a state's unsealed nodes and its admitted waits:
/// the machine a parked root resumes in this process.
///
/// # Errors
///
/// As [`import_parked`].
#[expect(clippy::too_many_arguments, reason = "one resume's whole input")]
pub fn import_whole_state(
    program: Program,
    bounds: Bounds,
    root: NodeId,
    nodes: &BTreeMap<NodeId, EncodedNode>,
    admitted: Vec<AdmittedWait>,
    releases: &ReleaseGraph,
    registry: &FunctionRegistry,
    limits: DecodeLimits,
) -> Result<(KernelMachine, ChildRef), GraphStateFailure> {
    let source = Layered {
        state: nodes,
        release: &releases.nodes,
    };
    let mut decoder = GraphDecoder::new(&source, limits);
    let whole: KernelWhole = decoder.decode(root, NodeKind::KERNEL_WHOLE)?;
    decoder.child_kind(root, NodeId(whole.owner.0), &[NodeKind::OWNER_BINDING])?;
    // The pinned release answers for the helpers a release holds; a
    // function of a package the embedding installed is its registry's,
    // which the import checks.
    let required: BTreeSet<_> = program
        .document
        .manifest
        .functions
        .keys()
        .filter(|function| releases.manifests.releases(function))
        .copied()
        .collect();
    releases
        .manifests
        .admit(&whole.release, &required, |function| {
            registry
                .get(function)
                .map(|registered| registered.native().is_some())
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
    for admitted in admitted {
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
    KernelMachine::import_whole_with_data_limits(
        program,
        bounds,
        whole,
        limits.kernel_data,
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
