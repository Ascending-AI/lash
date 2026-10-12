//! The run records' bodies and where they go (the layout is in the module
//! documentation of [`super`]).

use lash_core_store::checkpoint_graph::{
    EncodeBudget, ExplicitLimit, GraphDecoder, GraphRefusal, GraphStaging, NodeId,
};
use lash_durable::domain::{
    Ordinal, OwnerKey, RunFactKind, RunFactRow, RunFactWrite, RunFacts, RunSeq,
};
use lash_durable::{ActorTx, DomainWrite};

use super::graph::{self, AdmitMember, Decision, PinnedMember, RoundFact, SettledFact};
use super::{ExecutionDraft, SettledOutput};
use crate::ToolCallId;

/// The format of a run record's body: the RoundFact node (kind 21) each
/// record names, and the admission, start, outcome, retry and presentation
/// bodies it holds. Bodies carry no stamp of their own: the record's actor
/// carries this version in its format set, so only a node that decodes them
/// claims it (ADR 0106 §1).
///
/// version_guard(
///     shapes(
///         path = "crates/lash-core-execution/src/runtime/actor/round/graph.rs",
///         cover(RoundFact, AdmitMember, PinnedMember, SettledFact, MaterialNode, Decision),
///     ),
/// )
/// version_surface = "drain"
/// format_manifest = "RunRecord"
pub const RUN_RECORD_FORMAT_VERSION: u32 = 1;

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
/// completion's plugin-state resolutions commit in it (ADR 0132 §5).
pub(super) struct OutcomeBody {
    pub(super) start: u64,
    pub(super) output: SettledOutput,
    pub(super) state: Vec<lash_core_store::tool_run::StateResolution>,
}

/// A `retry` record's body: the attempt started at `start` failed in a way
/// its pinned `Repeatable` contract repeats, and the next attempt is due at
/// `due_at_ms`.
pub(super) struct RetryBody {
    pub(super) start: u64,
    pub(super) output: SettledOutput,
    pub(super) due_at_ms: i64,
}

/// A `decide` record's body: a decision of the round's coordinator, which
/// no member's recovery depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DecideBody {
    /// The trace admissions the round's admission retained are exported:
    /// the admission's export obligation is discharged (FIG-5452).
    TraceExported,
}

/// One record's typed body, as its RoundFact holds it.
pub(super) enum Record {
    Admit(AdmitBody),
    Start(StartBody),
    Wait(OutcomeBody),
    Outcome(OutcomeBody),
    Retry(RetryBody),
    Present,
    Decide(DecideBody),
    CellReport(NodeId),
}

impl Record {
    fn kind(&self) -> RunFactKind {
        match self {
            Self::Admit(_) => RunFactKind::Admit,
            Self::Start(_) => RunFactKind::XStart,
            Self::Wait(_) => RunFactKind::XWait,
            Self::Outcome(_) => RunFactKind::XOutcome,
            Self::Retry(_) => RunFactKind::Retry,
            Self::Present => RunFactKind::Present,
            Self::Decide(_) => RunFactKind::Decide,
            Self::CellReport(_) => RunFactKind::CellReport,
        }
    }
}

fn settled_fact(
    staging: &mut GraphStaging,
    output: &SettledOutput,
) -> Result<SettledFact, GraphRefusal> {
    Ok(SettledFact {
        outcome: output.outcome(),
        material: output
            .payload()
            .map(|payload| graph::stage_payload(staging, payload))
            .transpose()?,
    })
}

fn settled_output(
    decoder: &mut GraphDecoder<'_>,
    fact: SettledFact,
) -> Result<SettledOutput, String> {
    let payload = fact
        .material
        .map(|material| graph::load_payload(decoder, material))
        .transpose()
        .map_err(|refusal| refusal.to_string())?;
    SettledOutput::new(fact.outcome, payload).map_err(|refusal| refusal.to_string())
}

/// Stages `record`'s RoundFact and returns its node.
fn stage(staging: &mut GraphStaging, record: &Record) -> Result<NodeId, GraphRefusal> {
    let fact = match record {
        Record::Admit(body) if body.admissions.is_empty() => RoundFact::Admit {
            members: body
                .members
                .iter()
                .cloned()
                .map(|member| AdmitMember::Pinned(Box::new(member)))
                .collect(),
        },
        Record::Admit(body) => RoundFact::Admit {
            members: body
                .admissions
                .iter()
                .copied()
                .map(AdmitMember::Admission)
                .collect(),
        },
        Record::Start(body) => RoundFact::Start {
            member: body.member,
            attempt: body.attempt,
        },
        Record::Wait(body) => RoundFact::Wait {
            start: body.start,
            output: settled_fact(staging, &body.output)?,
        },
        Record::Outcome(body) => RoundFact::Outcome {
            start: body.start,
            output: settled_fact(staging, &body.output)?,
            state: body.state.clone(),
        },
        Record::Retry(body) => RoundFact::Retry {
            start: body.start,
            output: settled_fact(staging, &body.output)?,
            due_at_ms: body.due_at_ms,
        },
        Record::Present => RoundFact::Present,
        Record::Decide(DecideBody::TraceExported) => RoundFact::Decide(Decision::TraceExported),
        Record::CellReport(report) => RoundFact::CellReport { report: *report },
    };
    fact.stage(staging)
}

/// `row`'s body, read from `facts`: refused unless its RoundFact is of the
/// row's kind and every payload it names hashes to its outcome's digest.
///
/// # Errors
///
/// Why the record does not decode.
pub(super) fn read(facts: &RunFacts, row: &RunFactRow) -> Result<Record, String> {
    let mut decoder = GraphDecoder::new(facts, facts.read_limits);
    let fact = RoundFact::load(&mut decoder, row.record).map_err(|refusal| refusal.to_string())?;
    if fact.kind() != row.kind {
        return Err(format!(
            "a `{}` record holds a `{}` fact",
            row.kind.as_str(),
            fact.kind().as_str()
        ));
    }
    Ok(match fact {
        RoundFact::Admit { members } => {
            let mut pins = Vec::with_capacity(members.len());
            let mut admissions = Vec::new();
            for (index, member) in (0_u64..).zip(members) {
                match member {
                    AdmitMember::Pinned(member) if admissions.is_empty() => pins.push(*member),
                    AdmitMember::Admission(node) if admissions.len() == pins.len() => {
                        let admitted = graph::admission_pin(&mut decoder, node)
                            .map_err(|refusal| refusal.to_string())?;
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
                admissions,
            })
        }
        RoundFact::Start { member, attempt } => Record::Start(StartBody { member, attempt }),
        RoundFact::Wait { start, output } => Record::Wait(OutcomeBody {
            start,
            output: settled_output(&mut decoder, output)?,
            state: Vec::new(),
        }),
        RoundFact::Outcome {
            start,
            output,
            state,
        } => Record::Outcome(OutcomeBody {
            start,
            output: settled_output(&mut decoder, output)?,
            state,
        }),
        RoundFact::Retry {
            start,
            output,
            due_at_ms,
        } => Record::Retry(RetryBody {
            start,
            output: settled_output(&mut decoder, output)?,
            due_at_ms,
        }),
        RoundFact::Present => Record::Present,
        RoundFact::Decide(Decision::TraceExported) => Record::Decide(DecideBody::TraceExported),
        RoundFact::CellReport { report } => Record::CellReport(report),
    })
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
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    #[expect(
        clippy::expect_used,
        reason = "a fact's typed body always encodes, and an unbounded staging area places \
                  every value it holds"
    )]
    let node = stage(&mut staging, record).expect("a run fact stages");
    tx.stage_all(staging);
    tx.write(DomainWrite::RunFact(RunFactWrite::Append {
        owner: owner.clone(),
        run,
        ordinal,
        kind: record.kind(),
        call: call.cloned(),
        record: node,
    }));
}

/// Append the admission record of `drafts`, naming each by its Admission
/// in `admissions` when it is not empty.
pub(super) fn admit_record(
    tx: &mut ActorTx,
    owner: &OwnerKey,
    run: RunSeq,
    drafts: &[ExecutionDraft],
    admissions: Vec<NodeId>,
) {
    append(
        tx,
        owner,
        run,
        ADMIT_ORDINAL,
        None,
        &Record::Admit(AdmitBody {
            members: drafts.iter().map(PinnedMember::of).collect(),
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

/// Append the `decide` record of `decision`, at `ordinal` of `owner`'s run
/// `run`.
pub(super) fn decide_record(
    tx: &mut ActorTx,
    owner: &OwnerKey,
    run: RunSeq,
    ordinal: Ordinal,
    decision: DecideBody,
) {
    append(tx, owner, run, ordinal, None, &Record::Decide(decision));
}
