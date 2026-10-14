//! What the parent reads and writes of a kernel run beside its heap pages
//! (FIG-6193): the checkpoint root's header, a recorded end, the
//! admissions a resumed run is handed and the helper releases a run pins.
//!
//! A run's state is its persistent heap: pages the worker seals into the
//! activation's spool and a park publishes (S5, S6). The parent never
//! decodes the heap. It reads one page of it, the root-bounded
//! [`CheckpointRoot`] header, for the release and owner the root pins
//! ([`checkpoint_header`]). A run's end is a small host record
//! ([`EndRecord`]) whose datums are frozen values.
//!
//! A run pins the exact helper release manifest it was written under. Its
//! nodes are installed beside the root by whoever pins the release
//! ([`release_manifests`] stages them deterministically on either side), so
//! a root carries only the manifest's reference.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core_store::checkpoint_graph::{
    DecodeLimits, EncodedNode, GraphDecoder, GraphRefusal, GraphStaging, NodeId, NodeKind,
    check_envelope,
};
use lash_kernel_doc::graph_tokens::{ChildRef, TokenCodec, TokenError, TokenReader, TokenWriter};
use lash_kernel_doc::heap::{PageGrant, PageId, PageRef};
use lash_kernel_doc::{Datum, FrozenDatum, FunctionId, FunctionName, Name, TaskIdentity};
use lash_kernel_state::AdmittedWait;
use lash_kernel_state::paged::{CheckpointRoot, PageKind};
use lash_kernel_vm::{Bound, BoundExceeded, End, Finished, RunError};
use lash_vm_protocol::{EncodedPayload, KernelRestoreRefusal};
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

#[cfg(test)]
fn staging(bound: u64) -> GraphStaging {
    use lash_core_store::checkpoint_graph::{EncodeBudget, ExplicitLimit};
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

/// An admitted wait in the encoding a checkpoint start's admissions carry.
#[must_use]
pub fn admission(wait: &AdmittedWait) -> EncodedPayload {
    EncodedPayload(lash_kernel_doc::graph_tokens::encode_body(wait))
}

/// The header of checkpoint root `id`, from its page's bytes: checked to
/// hash to its identity and frame a CheckpointRoot within the root bound.
/// The parent reads nothing else of the heap.
///
/// # Errors
///
/// [`GraphStateFailure::Restore`] for bytes that are not such a root.
pub fn checkpoint_header(id: NodeId, bytes: &[u8]) -> Result<CheckpointRoot, GraphStateFailure> {
    let page = PageRef {
        id: PageId(id.0),
        grant: PageGrant::NONE,
    };
    let answer = super::pages::answer_page(page, bytes, |_| Ok(PageGrant::NONE))
        .map_err(|fault| malformed(fault))?;
    if answer.kind != PageKind::CHECKPOINT_ROOT {
        return Err(malformed(format!(
            "page {id} is kind {}, not a checkpoint root",
            answer.kind.0
        ))
        .into());
    }
    lash_kernel_doc::graph_tokens::decode_body(&answer.tokens)
        .map_err(|error| malformed(error).into())
}

/// The validator this build is (D-ADM): the kernel version, the semantic
/// page format and the digest of the page kinds it registers, so a root
/// admitted by another build's grammar is never trusted by this one.
#[must_use]
pub fn validator() -> String {
    static VALIDATOR: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VALIDATOR
        .get_or_init(|| {
            let mut kinds = blake3::Hasher::new();
            for spec in lash_core_store::checkpoint_graph::SEMANTIC_REGISTRY {
                kinds.update(&spec.kind.0.to_le_bytes());
                kinds.update(spec.name.as_bytes());
                kinds.update(&[0]);
            }
            format!(
                "lash-kernel/{}/pages-{}/{}",
                lash_kernel_doc::KERNEL_VERSION,
                lash_kernel_state::paged::source::PAGE_FORMAT,
                kinds.finalize().to_hex()
            )
        })
        .clone()
}

/// The semantic admission of `root`, published under `commit` by this
/// build's validator.
#[must_use]
pub fn admission_of(
    root: NodeId,
    commit: lash_core_store::checkpoint_graph::CommitId,
) -> lash_durable::domain::pages::SemanticAdmission {
    lash_durable::domain::pages::SemanticAdmission {
        root,
        kernel: lash_kernel_doc::KERNEL_VERSION,
        page_format: lash_kernel_state::paged::source::PAGE_FORMAT,
        validator: validator(),
        commit,
    }
}

/// How a run that parked ended, as its CellEnd records it: every detail
/// the machine reported, each datum a FrozenValue child. A cancelled run
/// records no end; a finished cell's carry is the session descriptor its
/// CellEnd names beside it.
#[derive(Clone, Debug, PartialEq)]
pub enum EndRecord {
    Finished {
        result: ChildRef,
        finish: bool,
        not_carried: Vec<(Name, String)>,
    },
    Failed(ChildRef),
    Uncaught(ChildRef),
    TasksOutstanding {
        unfinished: Vec<TaskIdentity>,
        unobserved: Vec<TaskIdentity>,
    },
    Deadlock {
        waiting: Vec<TaskIdentity>,
    },
    Bound {
        bound: Bound,
        limit: u64,
        function: Option<FunctionName>,
    },
}

fn encode_bound(bound: &Bound, out: &mut TokenWriter) {
    match bound {
        Bound::Charge => out.sum(0, 0),
        Bound::Memory => out.sum(1, 0),
        Bound::CallDepth => out.sum(2, 0),
        Bound::LiveTasks => out.sum(3, 0),
        Bound::RequestsPerPark => out.sum(4, 0),
        Bound::JoinMembers => out.sum(5, 0),
        Bound::Guard { function } => {
            out.sum(6, 1);
            function.encode(out);
        }
    }
}

fn decode_bound(input: &mut TokenReader<'_>) -> Result<Bound, TokenError> {
    let (tag, fields) = input.sum()?;
    TokenReader::expect_fields(fields, u64::from(tag == 6))?;
    Ok(match tag {
        0 => Bound::Charge,
        1 => Bound::Memory,
        2 => Bound::CallDepth,
        3 => Bound::LiveTasks,
        4 => Bound::RequestsPerPark,
        5 => Bound::JoinMembers,
        6 => Bound::Guard {
            function: FunctionId::decode(input)?,
        },
        _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
    })
}

impl TokenCodec for EndRecord {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Finished {
                result,
                finish,
                not_carried,
            } => {
                out.sum(0, 3);
                out.child(*result);
                finish.encode(out);
                not_carried.encode(out);
            }
            Self::Failed(reason) => {
                out.sum(1, 1);
                out.child(*reason);
            }
            Self::Uncaught(error) => {
                out.sum(2, 1);
                out.child(*error);
            }
            Self::TasksOutstanding {
                unfinished,
                unobserved,
            } => {
                out.sum(3, 2);
                unfinished.encode(out);
                unobserved.encode(out);
            }
            Self::Deadlock { waiting } => {
                out.sum(4, 1);
                waiting.encode(out);
            }
            Self::Bound {
                bound,
                limit,
                function,
            } => {
                out.sum(5, 3);
                encode_bound(bound, out);
                limit.encode(out);
                function.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        let expect = |count| TokenReader::expect_fields(fields, count);
        Ok(match tag {
            0 => {
                expect(3)?;
                Self::Finished {
                    result: input.child()?,
                    finish: bool::decode(input)?,
                    not_carried: Vec::decode(input)?,
                }
            }
            1 => {
                expect(1)?;
                Self::Failed(input.child()?)
            }
            2 => {
                expect(1)?;
                Self::Uncaught(input.child()?)
            }
            3 => {
                expect(2)?;
                Self::TasksOutstanding {
                    unfinished: Vec::decode(input)?,
                    unobserved: Vec::decode(input)?,
                }
            }
            4 => {
                expect(1)?;
                Self::Deadlock {
                    waiting: Vec::decode(input)?,
                }
            }
            5 => {
                expect(3)?;
                Self::Bound {
                    bound: decode_bound(input)?,
                    limit: u64::decode(input)?,
                    function: Option::decode(input)?,
                }
            }
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        })
    }
}

impl EndRecord {
    /// The record of `end`, its datums staged as FrozenValues in
    /// `staging`; none for a cancelled run.
    ///
    /// # Errors
    ///
    /// The staging area's refusal.
    pub fn stage(end: &End, staging: &mut GraphStaging) -> Result<Option<Self>, GraphRefusal> {
        let mut freeze = |datum: &Datum| freeze(staging, &FrozenDatum(datum.clone()));
        Ok(Some(match end {
            End::Finished(finished) => Self::Finished {
                result: freeze(&finished.result)?,
                finish: finished.finish,
                not_carried: finished.not_carried.clone(),
            },
            End::Failed(reason) => Self::Failed(freeze(reason)?),
            End::Error(RunError::Uncaught(error)) => Self::Uncaught(freeze(error)?),
            End::Error(RunError::TasksOutstanding {
                unfinished,
                unobserved,
            }) => Self::TasksOutstanding {
                unfinished: unfinished.clone(),
                unobserved: unobserved.clone(),
            },
            End::Error(RunError::Deadlock { waiting }) => Self::Deadlock {
                waiting: waiting.clone(),
            },
            End::Error(RunError::Bound(exceeded)) => Self::Bound {
                bound: exceeded.bound.clone(),
                limit: exceeded.limit,
                function: exceeded.function.clone(),
            },
            End::Cancelled => return Ok(None),
        }))
    }

    /// The end this record states, its datums read through `decoder`. A
    /// finished run's carry is its CellEnd's session descriptor, which the
    /// session reads.
    ///
    /// # Errors
    ///
    /// A refusal of a FrozenValue read.
    pub fn end(&self, decoder: &mut GraphDecoder<'_>) -> Result<End, GraphRefusal> {
        Ok(match self {
            Self::Finished {
                result,
                finish,
                not_carried,
            } => End::Finished(Box::new(Finished {
                result: thaw(decoder, *result)?,
                finish: *finish,
                carry: None,
                not_carried: not_carried.clone(),
            })),
            Self::Failed(reason) => End::Failed(thaw(decoder, *reason)?),
            Self::Uncaught(error) => End::Error(RunError::Uncaught(thaw(decoder, *error)?)),
            Self::TasksOutstanding {
                unfinished,
                unobserved,
            } => End::Error(RunError::TasksOutstanding {
                unfinished: unfinished.clone(),
                unobserved: unobserved.clone(),
            }),
            Self::Deadlock { waiting } => End::Error(RunError::Deadlock {
                waiting: waiting.clone(),
            }),
            Self::Bound {
                bound,
                limit,
                function,
            } => End::Error(RunError::Bound(BoundExceeded {
                bound: bound.clone(),
                limit: *limit,
                function: function.clone(),
            })),
        })
    }
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

    /// `K-ERR-003`, ADR 0132 §8: a recorded end reads back as the end it
    /// records, the thrown value unchanged even when it is not an error.
    #[test]
    fn a_recorded_uncaught_end_keeps_the_thrown_value() {
        let end = End::Error(RunError::Uncaught(Datum::Int(7.into())));
        let mut staging = staging(u64::MAX);
        let record = EndRecord::stage(&end, &mut staging)
            .expect("an uncaught end stages")
            .expect("an uncaught end is recorded");
        let nodes = staging.into_nodes();
        let mut decoder = GraphDecoder::new(&nodes, DecodeLimits::within_total(u64::MAX));
        assert_eq!(record.end(&mut decoder).expect("the end reads back"), end);
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
