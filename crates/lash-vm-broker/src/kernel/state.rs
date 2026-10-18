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
use lash_kernel_doc::{Datum, FrozenDatum, FunctionId, FunctionName, Name, TaskLabel};
use lash_kernel_state::AdmittedWait;
use lash_kernel_state::paged::{CheckpointHeader, inspect_checkpoint};
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

fn freeze(staging: &mut GraphStaging, datum: &FrozenDatum) -> Result<ChildRef, GraphRefusal> {
    staging
        .encode_value(NodeKind::FROZEN_VALUE, datum)
        .map(|id| id.child_ref())
        .map_err(GraphRefusal::from)
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
            check_envelope(&node.bytes, *id, &DecodeLimits::within_total(u64::MAX))
                .map_err(GraphRefusal::from)?;
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

    /// Pins `release`, the release a checkpoint root's header names, in
    /// `tx`: the store installs a release's nodes with its pin once, under
    /// its dialect's key ([`HeldManifests::pin_key`]), on the first root of
    /// the first execution that pins it; every later root requires them.
    /// Only a `first` root reads whether the store pins it already, within
    /// the release's own size. Nothing of the heap is decoded.
    ///
    /// # Errors
    ///
    /// [`UnheldRelease`] when this build does not hold `release`.
    pub async fn pin(
        &self,
        reads: &dyn lash_durable::DurableReads,
        tx: &mut lash_durable::ActorTx,
        release: NodeId,
        first: bool,
    ) -> Result<(), UnheldRelease> {
        if self.manifests.pin_key(&release.child_ref()).is_none() {
            return Err(UnheldRelease);
        }
        if !first {
            tx.require(release);
            return Ok(());
        }
        let (key, nodes) = self.pinned(release).ok_or(UnheldRelease)?;
        // A release this store already pins is stored whole: the commit
        // rechecks the requirement under its protection. A pin this read
        // cannot confirm is installed again, and the commit compares it.
        let size = nodes.values().map(EncodedNode::len).sum();
        let pinned = reads
            .hydrate(
                lash_durable::domain::RootOwner::ReleasePin {
                    release: key.clone(),
                },
                Some(release),
                DecodeLimits::within_total(size),
            )
            .await
            .is_ok();
        if pinned {
            tx.require(release);
            return Ok(());
        }
        tx.stage(nodes.into_values());
        tx.write(lash_durable::DomainWrite::ReleasePin(
            lash_durable::domain::ReleasePinWrite::Install {
                release: key,
                root: release,
            },
        ));
        Ok(())
    }
}

/// A checkpoint root's header pins a release this build does not hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the state pins a release this build does not hold")]
pub struct UnheldRelease;

/// The helper releases this build holds for the dialects it was built with,
/// staged. Staging is deterministic:
/// a parent and a worker of one build name every manifest alike. The build's
/// immutable nodes are staged once per process and shared by every cell.
///
/// # Errors
///
/// A refusal of the graph, or the build's own releases not decoding.
pub fn release_manifests() -> Result<ReleaseGraph, GraphStateFailure> {
    held_releases().cloned()
}

/// [`release_manifests`], borrowed from the process's one staging.
///
/// # Errors
///
/// As [`release_manifests`].
pub fn held_releases() -> Result<&'static ReleaseGraph, GraphStateFailure> {
    static RELEASES: std::sync::OnceLock<Result<ReleaseGraph, GraphStateFailure>> =
        std::sync::OnceLock::new();
    RELEASES
        .get_or_init(read_release_manifests)
        .as_ref()
        .map_err(Clone::clone)
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
    EncodedPayload(wait.encode())
}

/// The header of checkpoint root `id`, from its page's bytes: checked to
/// hash to its identity and frame a CheckpointRoot within the root bound.
/// The parent reads nothing else of the heap.
///
/// # Errors
///
/// [`GraphStateFailure::Restore`] for bytes that are not such a root.
pub fn checkpoint_header(id: NodeId, bytes: &[u8]) -> Result<CheckpointHeader, GraphStateFailure> {
    let page = PageRef {
        id: PageId(id.0),
        grant: PageGrant::NONE,
    };
    let answer =
        super::pages::answer_page(page, bytes, |_| Ok(PageGrant::NONE)).map_err(malformed)?;
    inspect_checkpoint(&answer).map_err(|error| malformed(error).into())
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
        unfinished: Vec<TaskLabel>,
        unobserved: Vec<TaskLabel>,
        sources: Vec<(TaskLabel, lash_kernel_doc::PositionLabel)>,
    },
    Deadlock {
        waiting: Vec<TaskLabel>,
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
        Bound::Effects => out.sum(7, 0),
        Bound::CheckpointBytes => out.sum(8, 0),
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
        7 => Bound::Effects,
        8 => Bound::CheckpointBytes,
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
                sources,
            } => {
                out.sum(3, 3);
                unfinished.encode(out);
                unobserved.encode(out);
                sources.encode(out);
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
                expect(3)?;
                Self::TasksOutstanding {
                    unfinished: Vec::decode(input)?,
                    unobserved: Vec::decode(input)?,
                    sources: Vec::decode(input)?,
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
                sources,
            }) => Self::TasksOutstanding {
                unfinished: unfinished.clone(),
                unobserved: unobserved.clone(),
                sources: sources.clone(),
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
                sources,
            } => End::Error(RunError::TasksOutstanding {
                unfinished: unfinished.clone(),
                unobserved: unobserved.clone(),
                sources: sources.clone(),
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

    /// FIG-6702, `K-TASK-020`: recording a task failure keeps the body
    /// provenance beside its actual library spawn identity and occurrence.
    #[test]
    fn a_recorded_task_failure_keeps_its_source_body_separate_from_its_spawn() {
        let task = lash_kernel_doc::testkit::spawned_task(
            lash_kernel_doc::testkit::root_task(),
            lash_kernel_doc::Site::new(
                lash_kernel_doc::Unit::Library(FunctionId::from_bytes([7; 32])),
                vec![1, 1],
            ),
            3,
        );
        let end = End::Error(RunError::TasksOutstanding {
            unfinished: vec![task.clone()],
            unobserved: Vec::new(),
            sources: vec![(
                task,
                lash_kernel_doc::Site::new(lash_kernel_doc::Unit::Main, vec![0, 0, 0]).position(),
            )],
        });
        let mut staging = staging(u64::MAX);
        let record = EndRecord::stage(&end, &mut staging)
            .expect("a task failure stages")
            .expect("a task failure is recorded");
        let bytes = lash_kernel_doc::graph_tokens::encode_body(&record);
        let record: EndRecord = lash_kernel_doc::graph_tokens::decode_body(&bytes)
            .expect("the stored task failure decodes");
        let nodes = staging.into_nodes();
        let mut decoder = GraphDecoder::new(&nodes, DecodeLimits::within_total(u64::MAX));
        assert_eq!(record.end(&mut decoder).expect("the end reads back"), end);
    }

    /// The stored catalog carries the 1.0 release line under each dialect's
    /// own pin key, independent of the release builder and its encoder.
    #[test]
    fn the_stored_release_catalog_preserves_its_1_0_pin_keys() {
        let built = release_manifests().expect("the stored catalog");
        assert!(built.manifests.current("typescript").is_some());
        for dialect in ["python", "typescript"] {
            if !lash_vm_releases::DIALECTS
                .iter()
                .any(|d| d.dialect == dialect)
            {
                continue;
            }
            let reference = built.manifests.current(dialect).expect("current release");
            let manifest = built.manifests.get(&reference).expect("manifest");
            assert_eq!((manifest.release.as_str(), manifest.ordinal), ("1.0", 1));
            assert_eq!(
                built.manifests.pin_key(&reference),
                Some(format!("{dialect}/1.0"))
            );
            assert_eq!(built.manifests.of_dialect(dialect).count(), 1);
        }
    }
}
