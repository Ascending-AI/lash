//! What the parent reads and writes of a kernel run beside its heap pages
//! (FIG-6193): the checkpoint root's header, a recorded end and the
//! admissions a resumed run is handed.
//!
//! A run's state is its persistent heap: pages the worker seals into the
//! activation's spool and a park publishes (S5, S6). The parent never
//! decodes the heap. It reads one page of it, the root-bounded
//! [`CheckpointRoot`] header, for the closure and owner the root pins
//! ([`checkpoint_header`]). A run's end is a small host record
//! ([`EndRecord`]) whose datums are frozen values.
//!
//! A run pins the library functions it calls by identity, through its
//! execution closure ([`super::closure`]): the root carries only the
//! closure's reference.

use lash_core_store::checkpoint_graph::{
    GraphDecoder, GraphRefusal, GraphStaging, NodeId, NodeKind,
};
use lash_kernel_doc::graph_tokens::{ChildRef, TokenCodec, TokenError, TokenReader, TokenWriter};
use lash_kernel_doc::heap::{PageGrant, PageId, PageRef};
use lash_kernel_doc::{Datum, FrozenDatum, FunctionId, FunctionName, Name, TaskLabel};
use lash_kernel_state::AdmittedWait;
use lash_kernel_state::paged::{CheckpointHeader, inspect_checkpoint};
use lash_kernel_vm::{Bound, BoundExceeded, End, Finished, RunError};
use lash_vm_protocol::{EncodedPayload, KernelRestoreRefusal};

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
/// the machine reported, each datum a FrozenValue child, but an uncaught
/// error's site, which a cell's feedback does not report and a recorded
/// end reads back as `None`. A cancelled run records no end; a finished
/// cell's carry is the session descriptor its CellEnd names beside it.
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
            End::Error(RunError::Uncaught { error, .. }) => Self::Uncaught(freeze(error)?),
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
            Self::Uncaught(error) => End::Error(RunError::Uncaught {
                error: thaw(decoder, *error)?,
                site: None,
            }),
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
    use lash_core_store::checkpoint_graph::DecodeLimits;

    use super::*;

    /// `K-ERR-003`, ADR 0132 §8: a recorded end reads back as the end it
    /// records, the thrown value unchanged even when it is not an error.
    #[test]
    fn a_recorded_uncaught_end_keeps_the_thrown_value() {
        let end = End::Error(RunError::Uncaught {
            error: Datum::Int(7.into()),
            site: None,
        });
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
}
