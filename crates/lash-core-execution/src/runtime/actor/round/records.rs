//! The run records' bodies and where they go (the layout is in the module
//! documentation of [`super`]).

use std::sync::Arc;

use lash_core_store::checkpoint_graph::{
    DecodeLimits, EncodeBudget, ExplicitLimit, GraphDecoder, GraphRefusal, GraphStaging, NodeId,
    NodeKind,
};
use lash_durable::domain::{
    Ordinal, OwnerKey, RunFactKind, RunFactRow, RunFactWrite, RunFacts, RunSeq,
};
use lash_durable::{ActorTx, DomainWrite};

use super::graph::{self, AdmitMember, MaterialNode, PinnedMember, RoundFact, SettledFact};
use super::{CompletedCall, ExecutionDraft};
use lash_core_store::tool_run::{Material, NamesMaterial, SettledOutput};
use lash_protocol_types::ToolCallId;

/// The admission's ordinal.
pub(super) const ADMIT_ORDINAL: Ordinal = Ordinal(0);

/// The ordinal of member `member`'s first `x_start`, committed with the
/// admission.
pub(super) fn first_start(member: u64) -> Ordinal {
    Ordinal(1 + member)
}

/// The admission record's body: each member's pin, in declared order.
pub(super) struct AdmitBody {
    pub(super) members: Vec<PinnedMember>,
    pub(super) reads: Vec<super::ModelReadPlan>,
    /// Each member's Admission, when a park admitted them by admission:
    /// the fact names those and no pin of its own.
    pub(super) admissions: Vec<NodeId>,
}

/// An `x_start` record's body: which member, and which attempt of its call.
pub(super) struct StartBody {
    pub(super) member: u64,
    pub(super) attempt: u32,
}

/// An `x_outcome` or `x_wait` record's body: the call's final output, or
/// its park, settling the attempt started at `start`. Its material's
/// payload is a node of its own, read back through the digest check. A
/// completion's plugin-state resolutions commit in it (ADR 0132 §5), with
/// the member's end: the store's clock at the commit (a park's is unused).
pub(super) struct OutcomeBody {
    pub(super) start: u64,
    pub(super) output: SettledOutput,
    pub(super) state: Vec<lash_core_store::tool_run::StateResolution>,
    pub(super) ended_at_ms: Option<u64>,
    /// The CheckpointRoot of the heap pages an engine step's completion
    /// published with it, which the record owns (FIG-6193).
    pub(super) pages: Option<NodeId>,
}

/// A `retry` record's body: the attempt started at `start` failed in a way
/// its pinned `Repeatable` contract repeats, and the next attempt is due at
/// `due_at_ms`.
pub(super) struct RetryBody {
    pub(super) start: u64,
    pub(super) output: SettledOutput,
    pub(super) due_at_ms: i64,
}

/// One record's typed body, as its RoundFact holds it.
pub(super) enum Record {
    Admit(AdmitBody),
    Start(StartBody),
    Wait(OutcomeBody),
    Outcome(OutcomeBody),
    FinalOutcome(OutcomeBody),
    Retry(RetryBody),
    Cancel {
        start: u64,
        cancellation: lash_protocol_types::ToolCancellation,
    },
    Final {
        start: u64,
        payload: serde_json::Value,
    },
    Present(Vec<NodeId>),
    CellReport(NodeId),
}

impl Record {
    pub(super) fn kind(&self) -> RunFactKind {
        match self {
            Self::Admit(_) => RunFactKind::Admit,
            Self::Start(_) => RunFactKind::XStart,
            Self::Wait(_) => RunFactKind::XWait,
            Self::Outcome(_) => RunFactKind::XOutcome,
            Self::Retry(_) => RunFactKind::Retry,
            Self::Cancel { .. } => RunFactKind::XCancel,
            Self::Final { .. } => RunFactKind::XFinal,
            Self::FinalOutcome(_) => RunFactKind::XFinalOutcome,
            Self::Present(_) => RunFactKind::Present,
            Self::CellReport(_) => RunFactKind::CellReport,
        }
    }
}

/// `output` as its fact holds it: its material staged as the node it names,
/// typed as the producer held it ([`graph::stage_material`]), and whether a
/// read of that node gives back the material as it is held: a typed answer
/// as its outcome, a text payload as its text.
fn settled_fact(
    staging: &mut GraphStaging,
    output: &SettledOutput,
) -> Result<(SettledFact, bool, Option<NodeId>), GraphRefusal> {
    fn staged<T: NamesMaterial>(
        staging: &mut GraphStaging,
        material: &Material<T>,
    ) -> Result<(MaterialNode, bool, Option<NodeId>), GraphRefusal> {
        let (node, body) = graph::stage_material_with_body(staging, material)?;
        let held = match node {
            MaterialNode::CallPayload(_) => material.typed::<CompletedCall>().is_some(),
            MaterialNode::CallRecord(_) => material.typed::<graph::ArchivalCall>().is_some(),
            MaterialNode::ProcessReadOutput(_) => {
                material.typed::<graph::ProcessReadOutput>().is_some()
            }
            MaterialNode::Value(_) => material.holds_text(),
            MaterialNode::Outcome(_) => false,
        };
        Ok((node, held, body))
    }
    let (material, held, body) = match output {
        SettledOutput::Completed(material) => staged(staging, material)?,
        SettledOutput::Failed(material) => staged(staging, material)?,
        SettledOutput::Waiting(material) => staged(staging, material)?,
        SettledOutput::Interrupted
        | SettledOutput::TimedOut { .. }
        | SettledOutput::Cancelled { .. } => {
            return Ok((
                SettledFact {
                    outcome: output.outcome(),
                    material: None,
                },
                true,
                None,
            ));
        }
    };
    Ok((
        SettledFact {
            outcome: output.outcome(),
            material: Some(material),
        },
        held,
        body,
    ))
}

/// Stages `record`'s RoundFact and returns its node, with whether a read of
/// that node gives back `record` exactly ([`StagedRecord`]).
fn stage(
    staging: &mut GraphStaging,
    record: &Record,
) -> Result<(NodeId, bool, Option<MaterialNode>, Option<NodeId>), GraphRefusal> {
    let mut call_body = None;
    let (fact, held) = match record {
        Record::Admit(body) if body.admissions.is_empty() => (
            RoundFact::Admit {
                reads: body.reads.clone(),
                members: body
                    .members
                    .iter()
                    .cloned()
                    .map(|member| AdmitMember::Pinned(Box::new(member)))
                    .collect(),
            },
            true,
        ),
        Record::Admit(body) => (
            RoundFact::Admit {
                reads: body.reads.clone(),
                members: body
                    .admissions
                    .iter()
                    .copied()
                    .map(AdmitMember::Admission)
                    .collect(),
            },
            true,
        ),
        Record::Start(body) => (
            RoundFact::Start {
                member: body.member,
                attempt: body.attempt,
            },
            true,
        ),
        Record::Wait(body) => {
            let (output, held, _) = settled_fact(staging, &body.output)?;
            (
                RoundFact::Wait {
                    start: body.start,
                    output,
                },
                held,
            )
        }
        Record::Outcome(body) | Record::FinalOutcome(body) => {
            let (output, held, body_node) = settled_fact(staging, &body.output)?;
            call_body = body_node;
            let fact = if matches!(record, Record::FinalOutcome(_)) {
                RoundFact::FinalOutcome {
                    start: body.start,
                    output,
                    state: body.state.clone(),
                    ended_at_ms: body.ended_at_ms,
                    pages: body.pages,
                }
            } else {
                RoundFact::Outcome {
                    start: body.start,
                    output,
                    state: body.state.clone(),
                    ended_at_ms: body.ended_at_ms,
                    pages: body.pages,
                }
            };
            (
                fact,
                // The heap pages an outcome owns are its candidate's nodes:
                // a fold reads its record, checking them.
                held && body.pages.is_none(),
            )
        }
        Record::Retry(body) => {
            let (output, held, _) = settled_fact(staging, &body.output)?;
            (
                RoundFact::Retry {
                    start: body.start,
                    output,
                    due_at_ms: body.due_at_ms,
                },
                held,
            )
        }
        Record::Cancel {
            start,
            cancellation,
        } => (
            RoundFact::Cancel {
                start: *start,
                cancellation: cancellation.clone(),
            },
            true,
        ),
        Record::Final { start, payload } => (
            RoundFact::Final {
                start: *start,
                payload: payload.clone(),
            },
            true,
        ),
        Record::Present(reads) => (
            RoundFact::Present {
                reads: reads.clone(),
            },
            true,
        ),
        Record::CellReport(report) => (RoundFact::CellReport { report: *report }, true),
    };
    let material = match &fact {
        RoundFact::Outcome { output, .. } | RoundFact::FinalOutcome { output, .. } => {
            output.material
        }
        _ => None,
    };
    let node = fact.stage_settled(staging)?;
    // The fact's own body (its state trees among it) must read back as it
    // was written, or a fold's read refuses it as it would any other.
    let held = held
        && GraphDecoder::new(&*staging, DecodeLimits::within_total(1 << 32))
            .decode::<RoundFact>(node, NodeKind::ROUND_FACT)
            .is_ok_and(|read| read == fact);
    Ok((node, held, material, call_body))
}

/// `row`'s body, read from `facts`: refused unless its RoundFact is of the
/// row's kind and every payload it names hashes to its outcome's digest.
///
/// # Errors
///
/// Why the record does not decode.
pub(super) fn read(facts: &RunFacts, row: &RunFactRow) -> Result<Record, String> {
    #[cfg(any(test, feature = "testing"))]
    crate::testing::fact_decodes::entered();
    let mut decoder = GraphDecoder::new(facts, facts.read_limits);
    let (fact, output) =
        RoundFact::load_settled(&mut decoder, row.record).map_err(|refusal| refusal.to_string())?;
    let output = output.ok_or_else(|| "a settled fact holds no output".to_owned());
    if fact.kind() != row.kind {
        return Err(format!(
            "a `{}` record holds a `{}` fact",
            row.kind.as_str(),
            fact.kind().as_str()
        ));
    }
    let final_outcome = matches!(&fact, RoundFact::FinalOutcome { .. });
    Ok(match fact {
        RoundFact::Admit { members, reads } => {
            let mut pins = Vec::with_capacity(members.len());
            let mut admissions = Vec::new();
            for (index, member) in (0_u64..).zip(members) {
                match member {
                    AdmitMember::Pinned(member) if admissions.is_empty() => pins.push(*member),
                    AdmitMember::Admission(node) if admissions.len() == pins.len() => {
                        let admitted = graph::admission_pin(&mut decoder, node)
                            .map_err(|refusal| format!("admission {node}: {refusal}"))?;
                        if admitted.run != row.run.0
                            || admitted.ordinal != super::member_ordinal(index).0
                        {
                            return Err(
                                "an admission names another member than its round's".to_owned()
                            );
                        }
                        pins.push(admitted.pin);
                        admissions.push(node);
                    }
                    _ => {
                        return Err(
                            "a round pins its members itself or by admission, not both".to_owned()
                        );
                    }
                }
            }
            Record::Admit(AdmitBody {
                members: pins,
                reads,
                admissions,
            })
        }
        RoundFact::Start { member, attempt } => Record::Start(StartBody { member, attempt }),
        RoundFact::Wait { start, .. } => Record::Wait(OutcomeBody {
            start,
            output: output?,
            state: Vec::new(),
            ended_at_ms: None,
            pages: None,
        }),
        RoundFact::Outcome {
            start,
            state,
            ended_at_ms,
            pages,
            ..
        }
        | RoundFact::FinalOutcome {
            start,
            state,
            ended_at_ms,
            pages,
            ..
        } => {
            let body = OutcomeBody {
                start,
                output: output?,
                state,
                ended_at_ms,
                pages,
            };
            if final_outcome {
                Record::FinalOutcome(body)
            } else {
                Record::Outcome(body)
            }
        }
        RoundFact::Retry {
            start, due_at_ms, ..
        } => Record::Retry(RetryBody {
            start,
            output: output?,
            due_at_ms,
        }),
        RoundFact::Cancel {
            start,
            cancellation,
        } => Record::Cancel {
            start,
            cancellation,
        },
        RoundFact::Final { start, payload } => Record::Final { start, payload },
        RoundFact::Present { reads } => Record::Present(reads),
        RoundFact::CellReport { report } => Record::CellReport(report),
    })
}

/// A record its owner staged, and the node its row names: the record that
/// owner's fold takes the row as once the commit that writes it is
/// acknowledged ([`RunRecords::extend`](super::RunRecords::extend)),
/// instead of reading and checking the node again. Held only when the node
/// was checked, as it was staged, to read back as exactly this record.
pub struct StagedRecord {
    pub(super) node: NodeId,
    pub(super) record: Option<Arc<Record>>,
}

impl std::fmt::Debug for StagedRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagedRecord")
            .field("node", &self.node)
            .field("held", &self.record.is_some())
            .finish()
    }
}

/// Append `record` to `owner`'s run `run` at `ordinal` in `tx`, as
/// [`append`] does, and hand it back as staged.
pub(super) fn append_staged(
    tx: &mut ActorTx,
    owner: &OwnerKey,
    run: RunSeq,
    ordinal: Ordinal,
    call: Option<&ToolCallId>,
    record: Record,
) -> StagedRecord {
    let (node, held) = append_held(tx, owner, run, ordinal, call, &record);
    // A park's fact holds its start and output alone: its record reads
    // back without the end the outcome after it records.
    let record = match record {
        Record::Wait(body) => Record::Wait(OutcomeBody {
            state: Vec::new(),
            ended_at_ms: None,
            pages: None,
            ..body
        }),
        record => record,
    };
    StagedRecord {
        node,
        record: held.then(|| Arc::new(record)),
    }
}

/// Append `record` to `owner`'s run `run` at `ordinal` in `tx`: its fact's
/// nodes staged, its row written.
pub(super) fn append(
    tx: &mut ActorTx,
    owner: &OwnerKey,
    run: RunSeq,
    ordinal: Ordinal,
    call: Option<&ToolCallId>,
    record: &Record,
) {
    append_held(tx, owner, run, ordinal, call, record);
}

fn append_held(
    tx: &mut ActorTx,
    owner: &OwnerKey,
    run: RunSeq,
    ordinal: Ordinal,
    call: Option<&ToolCallId>,
    record: &Record,
) -> (NodeId, bool) {
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    #[expect(
        clippy::expect_used,
        reason = "a fact's typed body always encodes, and an unbounded staging area places \
                  every value it holds"
    )]
    let (node, held, material, call_body) = stage(&mut staging, record).expect("a run fact stages");
    if let Some(call) = call {
        use lash_durable::domain::{ToolCallAnswer, ToolCallDecision, ToolCallWrite};
        let decision = match record {
            Record::Cancel { .. } => Some(ToolCallDecision::Cancel),
            Record::Final { .. } => Some(ToolCallDecision::Final),
            _ => None,
        };
        if let Some(decision) = decision {
            tx.write(DomainWrite::ToolCall(ToolCallWrite::Decide {
                owner: owner.clone(),
                call: call.clone(),
                decision,
                fact: node,
            }));
        }
        if let Record::Outcome(body) | Record::FinalOutcome(body) = record {
            let body_node = match material {
                Some(
                    MaterialNode::CallRecord(_)
                    | MaterialNode::Outcome(_)
                    | MaterialNode::CallPayload(_),
                ) => call_body,
                Some(MaterialNode::ProcessReadOutput(node)) => {
                    let mut decoder =
                        GraphDecoder::new(&staging, DecodeLimits::within_total(1 << 32));
                    #[expect(
                        clippy::expect_used,
                        reason = "the process output and reads were just staged"
                    )]
                    for (fact, body) in graph::ProcessReadOutput::bodies(&mut decoder, node)
                        .expect("staged caller reads")
                    {
                        tx.write(DomainWrite::ToolCall(ToolCallWrite::Admit {
                            counted: false,
                            logical: lash_durable::domain::tool_calls::logical_owner(owner),
                            execution: owner.clone(),
                            run,
                            member: fact.ordinal,
                            call: fact.call_id.clone(),
                            cancellable: false,
                        }));
                        tx.write(DomainWrite::ToolCall(ToolCallWrite::Answer {
                            owner: owner.clone(),
                            call: fact.call_id,
                            answer: ToolCallAnswer::Read { body },
                        }));
                    }
                    None
                }
                Some(MaterialNode::Value(_)) | None => None,
            };
            let answer = ToolCallAnswer::terminal(body.output.outcome(), body_node);
            tx.write(DomainWrite::ToolCall(
                if matches!(record, Record::FinalOutcome(_)) {
                    ToolCallWrite::Complete {
                        owner: owner.clone(),
                        call: call.clone(),
                        fact: node,
                        answer,
                    }
                } else {
                    ToolCallWrite::Answer {
                        owner: owner.clone(),
                        call: call.clone(),
                        answer,
                    }
                },
            ));
        }
    }
    tx.stage_all(staging);
    tx.write(DomainWrite::RunFact(RunFactWrite::Append {
        owner: owner.clone(),
        run,
        ordinal,
        kind: record.kind(),
        call: call.cloned(),
        record: node,
    }));
    (node, held)
}

/// Append the admission record of `drafts`, naming each by its Admission
/// in `admissions` when it is not empty.
pub(super) fn admit_record(
    tx: &mut ActorTx,
    owner: &OwnerKey,
    run: RunSeq,
    drafts: &[ExecutionDraft],
    admissions: Vec<NodeId>,
    reads: Vec<super::ModelReadPlan>,
) {
    let mut slot = 0_u64;
    let mut directory_admissions = Vec::new();
    for draft in drafts {
        while reads.iter().any(|read| read.slot == slot) {
            slot += 1;
        }
        directory_admissions.push((
            slot,
            draft.call().clone(),
            true,
            lash_durable::domain::tool_calls::request_owner(owner, draft.request()),
        ));
        slot += 1;
    }
    directory_admissions.extend(reads.iter().map(|read| {
        (
            read.slot,
            read.call_id.clone(),
            false,
            lash_durable::domain::tool_calls::request_owner(owner, &read.request),
        )
    }));
    directory_admissions.sort_by_key(|(slot, _, _, _)| *slot);
    for (member, call, cancellable, logical) in directory_admissions {
        tx.write(DomainWrite::ToolCall(
            lash_durable::domain::ToolCallWrite::Admit {
                counted: matches!(owner, OwnerKey::Turn(..) | OwnerKey::Cell(..)),
                logical,
                execution: owner.clone(),
                run,
                member,
                call,
                cancellable,
            },
        ));
    }
    append(
        tx,
        owner,
        run,
        ADMIT_ORDINAL,
        None,
        &Record::Admit(AdmitBody {
            members: drafts.iter().map(PinnedMember::of).collect(),
            reads,
            admissions,
        }),
    );
}

/// Append the `x_start` of `draft`, member `member`, for `attempt`.
pub(super) fn start_record(
    tx: &mut ActorTx,
    owner: &OwnerKey,
    run: RunSeq,
    ordinal: Ordinal,
    draft: &ExecutionDraft,
    member: u64,
    attempt: u32,
) {
    append(
        tx,
        owner,
        run,
        ordinal,
        Some(draft.call()),
        &Record::Start(StartBody { member, attempt }),
    );
}
