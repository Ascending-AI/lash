//! A kernel run's parks on the durable store (ADR 0132 §8).
//!
//! # The park transaction
//!
//! [`DurableSnapshotStore::commit_park`] commits, in one
//! `cell.snapshot+admit` transaction, the execution's next root graph
//! (ADR 0145 §6): the machine's CheckpointRoot at the next revision, its
//! changed heap pages published from the activation's spool as one
//! candidate (S6, FIG-6193), the
//! effect membership that matches its [`EffectLedger`], one Admission per
//! effect requested since the last park (holding its one frozen request),
//! the round admission and `x_start` of every admitted execution (one
//! each, under its own declared policy, limit and completion wait), the
//! timer wait of every sleep, due at the deadline the sleep was admitted
//! with (ADR 0132 §6), the host's own root and envelope, and the prune of
//! the run records no saved state can reach again. No effect's body starts
//! before that commit; each runs through the admitted-execution lifecycle
//! (ADR 0132 §5) and its outcome commits as `round.outcome`. The root
//! depends on no earlier root: a restore reads its header alone, and the
//! machine faults in the pages it touches.
//!
//! # Delivery
//!
//! [`DurableSnapshotStore::settled`] reads, from the run records, every
//! wait the ledger stands on whose outcome has committed. They may be
//! delivered in any order and several before the next save: the state is
//! saved, never replayed, so no order has to be reproduced. After a crash
//! the saved ledger still names every effect whose outcome the saved state
//! has not consumed, so `settled` answers them again from their records. A
//! started `Once` effect without an outcome records `Interrupted`, a
//! `Repeatable` one reruns at its ordinal, and no settled body is entered
//! again.
//!
//! # Settlement (ADR 0065, over tasks)
//!
//! Every effect is its own admitted execution with its own start and
//! outcome rows. Aggregate losers and observers retain their live calls.
//! Explicit task cancellation saves its targeted call identity with the guest
//! state and the actor's cancel decision. The call stays reachable until its
//! cancellation obligation is discharged. A run's end closes every call still
//! open. Losing a worker records no call decision; another node resumes from
//! the committed guest state and call records.
//!
//! # The end
//!
//! A run whose ledger stands on nothing when it ends has nothing to close.
//! When its consumer can hold the end, no checkpoint records it
//! ([`DurableSnapshotStore::held_end`], FIG-5999): the commit that consumes
//! the run's answer retires the parked root, and the store reclaims the
//! run's records and admissions with it. Until then the parked root
//! stands, and the final stretch, which issued no effect, runs again for
//! whoever resumes from it. Every other run that parked records its end
//! ([`DurableSnapshotStore::commit_run_end`]).

use std::collections::{BTreeMap, BTreeSet};

use lash_core_execution::runtime::actor::claim_obligations::ExecutionDebts;
use lash_core_execution::runtime::actor::round::{
    self, AdmittedExecution, Recovery, RoundDraft, RunFold,
};
use lash_core_execution::runtime::actor::waits;
use lash_core_store::checkpoint_graph::bodies::{ExecRoot, ExecStatus};
use lash_core_store::checkpoint_graph::{
    CommitId, EncodeBudget, ExplicitLimit, GraphDecoder, GraphStaging, NodeId, NodeKind,
};
use lash_core_store::tool_run::SettledOutput;
use lash_durable::domain::graph::{HydratedGraph, RootOwner};
use lash_durable::domain::pages::{CandidateOwner, CandidatePublication};
use lash_durable::domain::{ExecKey, RunFactWrite, RunSeq, SnapshotRev};
use lash_durable::{CommitLabel, DomainWrite, DurableError};
use lash_kernel_doc::{Datum, ErrorDatum, KERNEL_VERSION};

use lash_kernel_state::paged::CheckpointHeader;
use lash_kernel_value::names::DocumentId;
use lash_kernel_value::names::OperationUse;
use lash_kernel_value::sites::EffectId;
use lash_kernel_vm::{End, Outcome, WaitId};
use tokio_util::sync::CancellationToken;

use super::closure::Closure;
use super::graph::{
    CellEnd, Disposition, EffectMembership, FrozenAdmission, FrozenRequest, MemberStatus,
    Projections, ResolvedAdmission, RootParts, RunFingerprint, stage_exec_root,
};
use super::ledger::{AdmittedEffect, EffectLedger, PendingEffect, Standing};
use super::pages::{Resolution, SharedPages};
use super::run::KernelEffects;
use super::state::{EndRecord, checkpoint_header};
use super::value::datum_from_json;
use crate::effects::MemberDraft;
use crate::members::{Decide, Driven, Undriven};
use crate::snapshot::{
    BrokerInvariant, DriveRefusal, DurableSnapshotStore, HeldRoot, OperationId, QuietPointRefusal,
    refused,
};
use lash_durable::domain::graph::EffectAdmissionWrite;

/// The error kind of an effect that reported a failure of its own; its
/// `data` is what the effect reported.
pub const EFFECT_FAILED: &str = "effect_failed";
/// The error kind of an effect that started and never settled.
pub const EFFECT_INTERRUPTED: &str = "effect_interrupted";
/// The error kind of an effect that ran past a limit.
pub const EFFECT_TIMED_OUT: &str = "effect_timed_out";
/// The error kind of an effect that was cancelled.
pub const EFFECT_CANCELLED: &str = "effect_cancelled";
/// The kernel's kind for a result that is not what the `perform` can take
/// (`K-EFF-007`): here, a result that is not JSON.
pub const EFFECT_RESULT: &str = "effect_result";

/// How one requested wait is admitted with its park.
#[derive(Clone, Debug, PartialEq)]
pub enum AdmitAs {
    /// As this execution.
    Execution(Box<MemberDraft>),
    /// As a sleep of `duration` that is over at `until`.

    /// Not at all: the parent refuses it, and it is answered with `error`.
    /// The guest's arguments stay with its admission.
    Refused { error: ErrorDatum, args: Vec<Datum> },
}

/// One wait a park admits.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectAdmission {
    pub identity: EffectId,
    pub wait: WaitId,
    /// The effect performed; none for a sleep.
    pub effect: Option<OperationUse>,
    pub admit: AdmitAs,
    /// How far the run had come at the park that requests it.
    pub progress: super::graph::ParkProgress,
}

/// One call a save admitted, as the host records it in its own state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmittedCall {
    pub effect: EffectId,
    pub operation: OperationUse,
    pub call: lash_protocol_types::ToolCallId,
    /// Its Admission.
    pub admission: NodeId,
}

/// The host's nodes a root names beside the machine's state, and what the
/// host's state owes while the execution is parked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostParts {
    /// Caller-owned reads first saved by this checkpoint.
    pub read_calls: Vec<(
        lash_durable::domain::CallOwner,
        lash_protocol_types::ToolCallId,
        u64,
        NodeId,
    )>,
    /// A HostRoot.
    pub host: Option<NodeId>,
    /// A CellEnvelope.
    pub envelope: Option<NodeId>,
    /// The callbacks the host's state owes beyond its admitted calls (S7):
    /// a cell's committed deferred matches' providers and codecs.
    pub owed: Vec<lash_vocabulary::providers::PluginCallbackIdentity>,
    /// What the host's static operation links require of a node: their
    /// providers and codecs (FIG-6985). A parked root's requirements
    /// record holds them beside what its code requires.
    pub links: lash_vocabulary::claims::ClaimRequirements,
}

/// A park's sealed checkpoint root: its page, staged in the activation's
/// spool, and its decoded header.
#[derive(Clone, Debug)]
pub struct SealedPark {
    pub root: NodeId,
    pub header: CheckpointHeader,
}

/// What one park commits.
pub struct ParkSave<'a> {
    /// The document the run executes.
    pub document: DocumentId,
    /// The machine's checkpoint root at this park.
    pub state: SealedPark,
    /// The activation's page service, whose spool holds the root's
    /// changed pages: the candidate the park publishes.
    pub pages: &'a SharedPages,
    /// The execution closure the state's root names: the run's
    /// document's.
    pub closure: &'a Closure,
    /// The closures of the session's earlier cells no commit installed:
    /// each the park's pages name is staged with it.
    pub carried: &'a [Closure],
    /// The ledger that matches it, before this park's admissions.
    pub ledger: EffectLedger,
    /// Every wait requested since the last park.
    pub admit: Vec<EffectAdmission>,
    /// The owner's rows that commit with it.
    pub with: Vec<DomainWrite>,
}

/// What a run's end commits.
pub struct EndSave<'a> {
    pub document: DocumentId,
    /// The ledger as the run left it.
    pub ledger: EffectLedger,
    pub end: End,
    /// The session descriptor page the cell leaves, which the turn adopts:
    /// staged in `pages`' spool, it is the candidate the end publishes.
    pub session: NodeId,
    pub pages: &'a SharedPages,
    /// The closures of the session's earlier cells no commit installed:
    /// each the end's pages name is staged with it.
    pub carried: &'a [Closure],
    pub with: Vec<DomainWrite>,
}

/// A committed save.
#[derive(Clone, Debug, PartialEq)]
pub struct Saved {
    /// The new revision.
    pub rev: SnapshotRev,
    /// The root it published.
    pub root: NodeId,
    /// The ledger as stored: it names this park's admissions.
    pub ledger: EffectLedger,
}

/// A run's state at its latest park, as a machine resumes from it.
#[derive(Clone, Debug)]
pub struct ParkedState {
    /// The execution root the row holds: what an activation leases.
    pub exec_root: NodeId,
    /// Its CheckpointRoot page and header.
    pub root: NodeId,
    pub header: CheckpointHeader,
    /// Each wait's request, taken from the one admission that holds it.
    pub admissions: Vec<lash_kernel_state::AdmittedWait>,
}

/// An execution's latest root as read back: its row, its body and every
/// node it reaches.
#[derive(Clone, Debug)]
pub struct RootRead {
    pub read_limits: lash_core_store::checkpoint_graph::DecodeLimits,
    pub held: HeldRoot,
    pub body: ExecRoot,
    pub graph: HydratedGraph,
}

/// What a run's latest root holds.
#[derive(Clone, Debug)]
pub enum Checkpoint {
    /// A parked state and the ledger its membership names.
    Parked {
        rev: SnapshotRev,
        state: ParkedState,
        ledger: EffectLedger,
    },
    /// The end the run recorded.
    Ended {
        rev: SnapshotRev,
        end: End,
        cell: CellEnd,
    },
}

/// A wait whose outcome is committed and can be delivered.
#[derive(Clone, Debug, PartialEq)]
pub struct Settled {
    pub identity: EffectId,
    pub wait: WaitId,
    pub outcome: Outcome,
}

/// The outcome a `perform` is answered with for `output`, its effect's
/// final record; `None` while the record is not final.
pub fn outcome_of(output: &SettledOutput) -> Option<Outcome> {
    let failed = |kind: &str, message: String, data: Datum| {
        Outcome::Failed(ErrorDatum {
            kind: kind.to_owned(),
            message,
            data,
        })
    };
    Some(match output {
        SettledOutput::Completed(material) => match datum_from_json(material.payload()) {
            Ok(result) => Outcome::Completed(result),
            Err(invalid) => failed(EFFECT_RESULT, invalid.to_string(), Datum::Null),
        },
        SettledOutput::Failed(material) => failed(
            EFFECT_FAILED,
            "the effect reported a failure".to_owned(),
            datum_from_json(material.payload())
                .unwrap_or_else(|_| Datum::Text(material.payload().to_owned())),
        ),
        SettledOutput::Interrupted => failed(
            EFFECT_INTERRUPTED,
            "the effect started and never settled; outside work may already have happened, so check outside state before calling again".to_owned(),
            Datum::Null,
        ),
        SettledOutput::TimedOut { cause, .. } => failed(
            EFFECT_TIMED_OUT,
            format!("the effect ran past its limit: {cause:?}"),
            Datum::Null,
        ),
        SettledOutput::Cancelled { cancellation, .. } => failed(
            EFFECT_CANCELLED,
            cancellation.message.clone(),
            datum_from_json(&cancellation.to_json_value().to_string()).unwrap_or_else(|_| Datum::Text(cancellation.to_json_value().to_string())),
        ),
        SettledOutput::Waiting(_) => return None,
    })
}

/// How the parent reads an admitted effect's final record as the outcome
/// its `perform` is answered with; `None` while the record is not final. It
/// is called on every read of a settled wait, so it is a function of its
/// arguments alone: a resume reads the same records again.
pub type OutcomeOf<'a> = &'a (dyn Fn(&AdmittedEffect, &SettledOutput) -> Option<Outcome> + Sync);

/// Every wait `ledger` stands on that `folded` settles, in
/// identity order.
fn settled_in(
    ledger: &EffectLedger,
    exec: &ExecKey,
    folded: &RunFold,
    outcome: OutcomeOf<'_>,
) -> Vec<Settled> {
    ledger
        .pending()
        .filter_map(|(identity, entry)| {
            let outcome = match &entry.standing {
                Standing::Admitted(admitted) => {
                    let output = final_output(folded, exec, admitted)?;
                    let mut result = outcome(admitted, output)?;
                    if matches!(output, SettledOutput::Interrupted)
                        && let Outcome::Failed(error) = &mut result
                        && error.kind == EFFECT_INTERRUPTED
                    {
                        let name = entry.effect.as_ref().map(ToString::to_string).unwrap_or_else(|| "operation".into());
                        error.message = format!(
                            "effect `{name}` at {identity} started and never settled; outside work may already have happened, so check outside state before calling again",
                        );
                        error.data = Datum::Record(vec![
                            ("effect".to_owned(), Datum::Text(name.to_owned())),
                            ("position".to_owned(), Datum::Text(identity.position().to_string())),
                            ("identity".to_owned(), Datum::Text(identity.to_string())),
                            ("cause".to_owned(), error.data.clone()),
                        ]);
                    }
                    result
                },
                Standing::Refused(error) => Outcome::Failed(error.clone()),
            };
            Some(Settled {
                identity: identity.clone(),
                wait: entry.wait(),
                outcome,
            })
        })
        .collect()
}

fn final_output<'a>(
    folded: &'a RunFold,
    exec: &ExecKey,
    admitted: &AdmittedEffect,
) -> Option<&'a SettledOutput> {
    match folded.recovery(&admitted.operation.admitted(exec)) {
        Some(Recovery::Settled(output)) => Some(output),
        _ => None,
    }
}

enum SavePhase<'a> {
    Parked {
        state: SealedPark,
        closure: &'a Closure,
    },
    Ended {
        end: End,
        session: NodeId,
    },
}

/// The ledger a stored membership names, from the admissions it resolved.
fn ledger_of(
    membership: &EffectMembership,
    resolved: &BTreeMap<EffectId, ResolvedAdmission>,
) -> Result<EffectLedger, QuietPointRefusal> {
    let mut pending = BTreeMap::new();
    let mut cancelling = Vec::new();
    for entry in &membership.entries {
        let admitted = resolved
            .get(&entry.effect)
            .ok_or_else(|| refused("a membership entry resolved no admission"))?;
        let execution = match &admitted.admission.disposition {
            Disposition::Execution { operation, pin } => Some(AdmittedEffect {
                operation: *operation,
                call: pin.call.clone(),
            }),
            Disposition::Refused => None,
        };
        let standing = match (entry.status, execution, &admitted.request) {
            (MemberStatus::Cancelling, Some(execution), _) => {
                cancelling.push(execution);
                continue;
            }
            (MemberStatus::Pending | MemberStatus::CancellingObserved, Some(execution), _) => {
                Standing::Admitted(execution)
            }
            (MemberStatus::Pending, None, FrozenRequest::Refused { error, .. }) => {
                Standing::Refused(error.clone())
            }
            _ => {
                return Err(refused(
                    "a membership entry stands as its admission does not",
                ));
            }
        };
        pending.insert(
            entry.effect.clone(),
            PendingEffect {
                wait: entry.wait,
                effect: admitted.admission.effect_name,
                standing,
                cancellation_requested: entry.status == MemberStatus::CancellingObserved,
            },
        );
    }
    EffectLedger::restored(pending, cancelling, membership.next_park, membership.growth)
        .map_err(refused)
}

fn staging() -> GraphStaging {
    GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)))
}

/// Whether a refused publication is ambiguous: its acknowledgement was
/// lost, so the owner's row decides it (D-SPOOL).
fn ambiguous(error: &DurableError) -> bool {
    matches!(error, DurableError::AckLost { .. })
}

/// One wait a park requested, sorted for its admission.
struct Requested {
    identity: EffectId,
    wait: WaitId,
    effect: Option<OperationUse>,
    standing: Option<Standing>,
    request: Option<FrozenRequest>,
    progress: super::graph::ParkProgress,
}

/// A park's requests: the waits it stands on, the drafts its round
/// admits, and which waits those drafts are, by index.
struct Requests {
    requested: Vec<Requested>,
    drafts: Vec<lash_core_execution::runtime::actor::round::ExecutionDraft>,
    executions: Vec<(usize, FrozenRequest)>,
}

impl Requests {
    fn of(admit: Vec<EffectAdmission>) -> Self {
        let mut drafts = Vec::new();
        let mut executions = Vec::new();
        let mut requested = Vec::with_capacity(admit.len());
        for admission in admit {
            let EffectAdmission {
                identity,
                wait,
                effect,
                admit,
                progress,
            } = admission;
            let (standing, request) = match admit {
                AdmitAs::Execution(draft) => {
                    let MemberDraft { draft, request } = *draft;
                    executions.push((requested.len(), request));
                    drafts.push(draft);
                    (None, None)
                }
                AdmitAs::Refused { error, args } => (
                    Some(Standing::Refused(error.clone())),
                    Some(FrozenRequest::Refused { error, args }),
                ),
            };
            requested.push(Requested {
                identity,
                wait,
                effect,
                standing,
                request,
                progress,
            });
        }
        Self {
            requested,
            drafts,
            executions,
        }
    }

    fn is_empty(&self) -> bool {
        self.requested.is_empty()
    }
}

/// What an in-turn run's commits recorded (FIG-7585): every effect it
/// admitted, and the batches its outcomes were delivered in. A run that
/// resumes starts again and follows it: each park requests what these
/// admissions froze, and each wait takes the next batch whole.
#[derive(Clone, Debug, Default)]
pub struct CellJournal {
    /// Every admission, by effect identity.
    pub admitted: BTreeMap<EffectId, ResolvedAdmission>,
    /// The delivery batches in order, each its effects in delivery order.
    pub batches: Vec<Vec<EffectId>>,
}

/// How an in-turn park's commit ended.
#[derive(Debug)]
pub enum Journaled {
    /// It committed, or had nothing to commit: the ledger as it stands.
    Committed(EffectLedger),
    /// The turn accepted an `Immediate` cancel (or its session's close)
    /// before the commit that carried its held response: nothing it
    /// admitted starts, and the turn stops on the cell.
    Stopped,
}

/// What one in-turn park commits: no state, only the journal's facts.
pub struct JournaledPark<'a> {
    pub document: DocumentId,
    /// The execution closure the run's document links against.
    pub closure: &'a Closure,
    /// The ledger before this park's admissions.
    pub ledger: EffectLedger,
    /// Every wait requested since the last park.
    pub admit: Vec<EffectAdmission>,
    /// The deliveries since the last commit, in order.
    pub delivered: Vec<(EffectId, lash_durable::domain::Delivery)>,
    pub with: Vec<DomainWrite>,
}

impl DurableSnapshotStore {
    /// Admit `requests` in `tx`: one round of the executions, pinned at the
    /// ledger's next park, and an admission of every wait, each standing in
    /// `ledger` as admitted. Answers the calls the host records and the
    /// round's members.
    #[expect(clippy::too_many_arguments, reason = "one admission, in its parts")]
    fn admit_requests(
        &self,
        tx: &mut lash_durable::ActorTx,
        staging: &mut GraphStaging,
        book: &mut super::graph::AdmissionBook,
        ledger: &mut EffectLedger,
        requests: Requests,
        run: Option<RunFingerprint>,
    ) -> Result<(Vec<AdmittedCall>, Vec<AdmittedExecution>), QuietPointRefusal> {
        let Requests {
            mut requested,
            drafts,
            executions,
        } = requests;
        if requested.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let run = run.ok_or_else(|| refused("a run's end admits nothing"))?;
        let mut calls = Vec::new();
        let mut admitted: Vec<AdmittedExecution> = Vec::new();
        if !drafts.is_empty() {
            let park = ledger.take_park();
            let pinned = round::pin_round(
                tx,
                &waits::wait_scope(&self.cx).map_err(refused)?,
                RoundDraft {
                    owner: self.exec.owner(),
                    run: RunSeq(park),
                    members: drafts,
                },
            )
            .map_err(refused)?;
            let mut nodes = Vec::with_capacity(executions.len());
            for ((ordinal, pin), (index, request)) in pinned.pins().into_iter().zip(executions) {
                let entry = &mut requested[index];
                let operation = OperationId {
                    run: park,
                    ordinal: ordinal.0,
                };
                let call = pin.call.clone();
                let (node, write) = book
                    .admit(
                        staging,
                        &self.exec,
                        FrozenAdmission {
                            run: run.clone(),
                            effect: entry.identity.clone(),
                            wait: entry.wait,
                            effect_name: entry.effect,
                            request,
                            execution: Some((operation, pin)),
                            progress: entry.progress,
                        },
                    )
                    .map_err(refused)?;
                tx.write(write);
                nodes.push(node);
                if let Some(operation) = &entry.effect {
                    calls.push(AdmittedCall {
                        effect: entry.identity.clone(),
                        operation: *operation,
                        call: call.clone(),
                        admission: node,
                    });
                }
                entry.standing = Some(Standing::Admitted(AdmittedEffect { operation, call }));
            }
            admitted = pinned.admit(tx, nodes).map_err(refused)?.members().to_vec();
        }
        for entry in requested {
            let Some(standing) = entry.standing else {
                return Err(refused("an execution's admission minted no identity"));
            };
            if let Some(request) = entry.request {
                let (_, write) = book
                    .admit(
                        staging,
                        &self.exec,
                        FrozenAdmission {
                            run: run.clone(),
                            effect: entry.identity.clone(),
                            wait: entry.wait,
                            effect_name: entry.effect,
                            request,
                            execution: None,
                            progress: entry.progress,
                        },
                    )
                    .map_err(refused)?;
                tx.write(write);
            }
            let pending = PendingEffect {
                wait: entry.wait,
                effect: entry.effect,
                standing,
                cancellation_requested: false,
            };
            if !ledger.stand(entry.identity.clone(), pending) {
                return Err(refused(format!(
                    "the run already stands on effect {:?}",
                    entry.identity
                )));
            }
        }
        Ok((calls, admitted))
    }

    /// Write in `tx` the cancel decision of every cancelling execution of
    /// `ledger` whose call has none committed.
    async fn cancel_in(
        &self,
        tx: &mut lash_durable::ActorTx,
        members: &mut crate::members::Members,
        ledger: &EffectLedger,
    ) -> Result<(), QuietPointRefusal> {
        if !ledger.has_cancellations() {
            return Ok(());
        }
        let folded = members.fold().await.map_err(refused)?;
        for cancelled in ledger.cancelling() {
            for view in folded.rounds() {
                if let Some(member) = view.members().iter().find(|member| {
                    member.call() == &cancelled.call
                        && member.outcome().is_none()
                        && member.cancellation().is_none()
                        && member.final_payload().is_none()
                }) {
                    let _ = round::cancel_call(
                        tx,
                        &view.execution(member),
                        lash_protocol_types::ToolCancellation::runtime(
                            "the guest task cancelled its call",
                        )
                        .with_origin(lash_protocol_types::CancelOrigin::ModelRequested),
                    );
                }
            }
        }
        Ok(())
    }

    /// The admission of `effect` this activation knows, with its request.
    #[must_use]
    pub fn resolved(&self, effect: &EffectId) -> Option<ResolvedAdmission> {
        self.book
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .resolved(effect)
    }

    /// The journal this in-turn execution's commits recorded: every
    /// admission, each request read through the projection of
    /// `projections` it was frozen under, and the delivery batches. The
    /// activation's admission book holds them from here on.
    ///
    /// # Errors
    ///
    /// [`QuietPointRefusal`]: a store failure, an admission of another
    /// execution, or deliveries that do not form whole batches.
    pub async fn journal(
        &self,
        projections: &dyn Projections,
    ) -> Result<CellJournal, QuietPointRefusal> {
        let reads = self.cx.durable_reads().map_err(refused)?;
        let rows = reads.effect_admissions(&self.exec).await.map_err(refused)?;
        let mut admitted = BTreeMap::new();
        let mut book = BTreeMap::new();
        let mut delivered = BTreeMap::new();
        for row in rows {
            let graph = reads
                .hydrate(
                    RootOwner::EffectAdmission {
                        exec: self.exec.clone(),
                        effect: row.effect.clone(),
                    },
                    Some(row.admission),
                    self.read_limits(),
                )
                .await
                .map_err(refused)?;
            let mut decoder = GraphDecoder::new(&graph, self.read_limits());
            let resolved = super::graph::Admission::load(&mut decoder, row.admission, projections)
                .map_err(refused)?;
            if resolved.admission.exec != self.exec
                || super::graph::effect_key(&resolved.admission.effect) != row.effect
            {
                return Err(refused("an admission row names another execution's effect"));
            }
            // A covered admission is the last checkpoint's: the book
            // resolves it, the journal since that checkpoint does not hold it.
            if !row.covered {
                if let Some(delivery) = row.delivered {
                    delivered.insert(delivery, resolved.admission.effect.clone());
                }
                admitted.insert(resolved.admission.effect.clone(), resolved.clone());
            }
            book.insert(resolved.admission.effect.clone(), resolved);
        }
        let mut batches: Vec<Vec<EffectId>> = Vec::new();
        for (delivery, effect) in delivered {
            let batch = usize::try_from(delivery.batch).unwrap_or(usize::MAX);
            if batch == batches.len() {
                batches.push(Vec::new());
            }
            let last = batches.len().checked_sub(1);
            match batches.get_mut(batch) {
                Some(effects)
                    if Some(batch) == last
                        && usize::try_from(delivery.position).ok() == Some(effects.len()) =>
                {
                    effects.push(effect);
                }
                _ => return Err(refused("the journal's deliveries are not whole batches")),
            }
        }
        *self
            .book
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            super::graph::AdmissionBook::restored(book);
        Ok(CellJournal { admitted, batches })
    }

    /// Commit `save`, an in-turn park (FIG-7585), with the model response
    /// its turn holds for the run, when this is its first commit: the
    /// admission of every wait it requested, the cancel decisions it made,
    /// the reads and deliveries since its last commit. No state is saved:
    /// a run that resumes starts again and follows these facts.
    ///
    /// # Errors
    ///
    /// As [`commit_park`](Self::commit_park).
    pub async fn commit_journaled(
        &self,
        save: JournaledPark<'_>,
        host: &dyn KernelEffects,
        response: Option<&lash_core_execution::HeldResponse>,
    ) -> Result<Journaled, QuietPointRefusal> {
        let JournaledPark {
            document,
            closure,
            mut ledger,
            admit,
            delivered,
            with,
        } = save;
        let mut members = self.members.lock().await;
        if ledger.has_cancellations() {
            let folded = members.fold().await.map_err(refused)?;
            ledger.drop_cancelling(|admitted| {
                let Some(output) = final_output(&folded, &self.exec, admitted) else {
                    return false;
                };
                let _ = host.outcome(admitted, output);
                true
            });
        }
        let requests = Requests::of(admit);
        let mut tx = self.cx.begin().await.map_err(refused)?;
        self.cancel_in(&mut tx, &mut members, &ledger).await?;
        // Another commit of the turn, a deferred match's resolution, may
        // have carried the response first: only one the turn still holds
        // rides this commit.
        let response = response.filter(|response| response.is_held());
        if let Some(response) = response {
            if response.stops_at(&tx) {
                return Ok(Journaled::Stopped);
            }
            let held = response
                .writes()
                .ok_or_else(|| refused("a first save carries a response its turn holds"))?;
            tx.append(held)?;
        }
        let mut staging = staging();
        let admits = !requests.is_empty();
        // The book takes this park's admissions in place, not a copy of
        // every admission so far: a commit that does not land takes them
        // out again, so each park's work is its own (FIG-7585).
        let (calls, admitted, mut booked) = {
            let mut book = self
                .book
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let booked = Booked {
                book: &self.book,
                added: Some(
                    requests
                        .requested
                        .iter()
                        .filter(|requested| book.get(&requested.identity).is_none())
                        .map(|requested| requested.identity.clone())
                        .collect(),
                ),
            };
            let (calls, admitted) = self.admit_requests(
                &mut tx,
                &mut staging,
                &mut book,
                &mut ledger,
                requests,
                Some(RunFingerprint {
                    document,
                    closure: closure.reference(),
                }),
            )?;
            (calls, admitted, booked)
        };
        for (effect, delivery) in delivered {
            tx.write(DomainWrite::EffectAdmission(
                EffectAdmissionWrite::Deliver {
                    exec: self.exec.clone(),
                    effect: super::graph::effect_key(&effect),
                    delivery,
                },
            ));
        }
        for write in with {
            tx.write(write);
        }
        for (logical, call, ordinal, body) in host.stage_reads(&mut staging).map_err(refused)? {
            tx.write(DomainWrite::ToolCall(
                lash_durable::domain::ToolCallWrite::Admit {
                    counted: false,
                    logical,
                    execution: self.exec.owner(),
                    run: RunSeq(0),
                    member: ordinal,
                    call: call.clone(),
                    cancellable: false,
                },
            ));
            tx.write(DomainWrite::ToolCall(
                lash_durable::domain::ToolCallWrite::Answer {
                    owner: self.exec.owner(),
                    call,
                    answer: lash_durable::domain::ToolCallAnswer {
                        outcome: lash_durable::domain::tool_calls::ToolCallSettlement::Read,
                        body: Some(body),
                    },
                },
            ));
        }
        tx.stage_all(staging);
        let label = if response.is_some() {
            CommitLabel::MODEL_DONE_CELL_ADMIT
        } else {
            CommitLabel::CELL_ADMIT
        };
        if tx.domain().is_empty() {
            return Ok(Journaled::Committed(ledger));
        }
        // What running the cell requires of a node, stated from the records
        // it runs under: its code's natives and kernel, and its
        // environment's and resolved matches' providers and codecs. The
        // commit that consumes its answer ends the execution, and the debt.
        let (owed, links) = host.debts();
        let mut requirements = lash_vocabulary::claims::ClaimRequirements {
            machine: super::loader::requirements(&closure, KERNEL_VERSION, std::iter::empty())
                .machine,
            ..lash_vocabulary::claims::ClaimRequirements::default()
        };
        requirements.extend(&links);
        let owing = ExecutionDebts {
            exec: self.exec.clone(),
            owed,
            requirements,
        };
        let horizon = RunSeq(ledger.oldest_reachable());
        let update = members
            .prepare_update(&tx, Some(horizon))
            .await
            .map_err(refused)?;
        let committed = match self.cx.commit_owing(tx, label, &owing).await {
            Ok(committed) => committed,
            Err(error) => {
                members.forget();
                return Err(error.into());
            }
        };
        booked.keep();
        members.apply_update(update).map_err(refused)?;
        if let Some(response) = response {
            response.committed(label);
        }
        let stopped = response.is_some_and(|response| response.stops_in(&committed));
        host.host_committed();
        host.journaled(
            &calls
                .iter()
                .map(|call| (call.call.clone(), call.admission))
                .collect::<Vec<_>>(),
        );
        if admits && !admitted.is_empty() && !stopped {
            members.admitted(&admitted).map_err(refused)?;
        }
        if stopped {
            return Ok(Journaled::Stopped);
        }
        Ok(Journaled::Committed(ledger))
    }
}

/// The admissions one in-turn park added to its execution's book: unless
/// its commit lands ([`keep`](Self::keep)), they leave the book again.
struct Booked<'a> {
    book: &'a std::sync::Mutex<super::graph::AdmissionBook>,
    added: Option<Vec<EffectId>>,
}

impl Booked<'_> {
    fn keep(&mut self) {
        self.added = None;
    }
}

impl Drop for Booked<'_> {
    fn drop(&mut self) {
        if let Some(added) = self.added.take() {
            self.book
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .forget(&added);
        }
    }
}

impl DurableSnapshotStore {
    /// Commit `save` in one transaction (see the module docs), with the
    /// host's own state `host` stages.
    ///
    /// # Errors
    ///
    /// [`QuietPointRefusal`]: an identity the ledger already stands on,
    /// ownership lost or another store failure. Nothing is admitted.
    pub async fn commit_park(
        &self,
        save: ParkSave<'_>,
        host: &dyn KernelEffects,
    ) -> Result<Saved, QuietPointRefusal> {
        self.commit_parked(save, host).await
    }

    async fn commit_parked(
        &self,
        save: ParkSave<'_>,
        host: &dyn KernelEffects,
    ) -> Result<Saved, QuietPointRefusal> {
        let ParkSave {
            document,
            state,
            pages,
            closure,
            carried,
            ledger,
            admit,
            with,
        } = save;
        self.commit_checkpoint(
            host,
            &document,
            ledger,
            SavePhase::Parked { state, closure },
            (pages, carried),
            admit,
            with,
        )
        .await
    }

    /// Commit a run's end: every execution still open is cancelled and
    /// settles first, so the root that records the end names none. A
    /// cancelled run records no end, and is refused here.
    ///
    /// # Errors
    ///
    /// As [`commit_park`](Self::commit_park).
    pub async fn commit_run_end(
        &self,
        save: EndSave<'_>,
        host: &dyn KernelEffects,
    ) -> Result<Saved, QuietPointRefusal> {
        self.commit_checkpoint(
            host,
            &save.document,
            save.ledger,
            SavePhase::Ended {
                end: save.end,
                session: save.session,
            },
            (save.pages, save.carried),
            Vec::new(),
            save.with,
        )
        .await
    }

    /// The root the execution's row holds, if any.
    ///
    /// # Errors
    ///
    /// [`QuietPointRefusal`]: a store failure.
    pub async fn root(&self) -> Result<Option<HeldRoot>, QuietPointRefusal> {
        self.revision().await
    }

    #[expect(
        clippy::too_many_lines,
        clippy::too_many_arguments,
        reason = "one park transaction, in its commit order"
    )]
    async fn commit_checkpoint(
        &self,
        host: &dyn KernelEffects,
        document: &DocumentId,
        mut ledger: EffectLedger,
        phase: SavePhase<'_>,
        (pages, carried): (&SharedPages, &[Closure]),
        admit: Vec<EffectAdmission>,
        with: Vec<DomainWrite>,
    ) -> Result<Saved, QuietPointRefusal> {
        let own = match &phase {
            SavePhase::Parked { closure, .. } => Some(closure.reference()),
            SavePhase::Ended { .. } => None,
        };
        let mut members = self.members.lock().await;
        if matches!(phase, SavePhase::Ended { .. }) {
            if ledger.executions().next().is_some() {
                members.close().await.map_err(refused)?;
                let folded = members.fold().await.map_err(refused)?;
                for admitted in ledger.executions() {
                    let output = final_output(&folded, &self.exec, admitted)
                        .ok_or_else(|| refused("a closing call has no committed outcome"))?;
                    // Closing settles observation too. Retain the committed
                    // record in the host ledger without delivering to the guest.
                    let _ = host.outcome(admitted, output);
                }
            }
            ledger.close();
        }
        // A cancelling execution whose outcome committed is no longer open.
        if ledger.has_cancellations() {
            let folded = members.fold().await.map_err(refused)?;
            ledger.drop_cancelling(|admitted| {
                let Some(output) = final_output(&folded, &self.exec, admitted) else {
                    return false;
                };
                let _ = host.outcome(admitted, output);
                true
            });
        }
        let requests = Requests::of(admit);
        let expected = self.revision().await?;
        let mut tx = self.cx.begin().await.map_err(refused)?;
        if matches!(phase, SavePhase::Ended { .. }) {
            let execution = match &self.exec {
                ExecKey::Cell(session, turn, cell) => {
                    lash_core_store::resolutions::ExecutionId::cell(session, turn, cell.as_str())
                }
                ExecKey::Process(process) => {
                    lash_core_store::resolutions::ExecutionId::process(process)
                }
            };
            tx.end_execution(execution);
        }

        // Guest state and target identities commit together. No cleanup body
        // can be entered until the owner observes these decisions committed.
        self.cancel_in(&mut tx, &mut members, &ledger).await?;
        // A sleep is a timer wait of the run's scope, pinned with the park
        // that admits it: the owner that holds only sleeps releases until
        // the earliest, and the scope's end revokes them.

        let mut staging = staging();
        let mut book = self
            .book
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let (calls, admitted) = self.admit_requests(
            &mut tx,
            &mut staging,
            &mut book,
            &mut ledger,
            requests,
            own.map(|closure| RunFingerprint {
                document: *document,
                closure,
            }),
        )?;
        // Records no saved state can reach again go, in the transaction
        // that saves the state that consumed them. An in-turn cell keeps
        // every record: they are its journal, which a run that starts again
        // follows and the commit that consumes its answer reports and
        // retires (FIG-7585).
        for write in with {
            tx.write(write);
        }
        let oldest = ledger.oldest_reachable();
        if oldest > 0 && !matches!(self.exec, ExecKey::Cell(..)) {
            tx.write(DomainWrite::RunFact(RunFactWrite::Prune {
                owner: self.exec.owner(),
                before: RunSeq(oldest),
            }));
        }
        let mut reads = BTreeSet::new();
        let parts = host
            .stage_host(&mut staging, &mut reads, &calls)
            .map_err(refused)?;
        for (logical, call, ordinal, body) in &parts.read_calls {
            tx.write(DomainWrite::ToolCall(
                lash_durable::domain::ToolCallWrite::Admit {
                    counted: false,
                    logical: logical.clone(),
                    execution: self.exec.owner(),
                    run: RunSeq(0),
                    member: *ordinal,
                    call: call.clone(),
                    cancellable: false,
                },
            ));
            tx.write(DomainWrite::ToolCall(
                lash_durable::domain::ToolCallWrite::Answer {
                    owner: self.exec.owner(),
                    call: call.clone(),
                    answer: lash_durable::domain::ToolCallAnswer {
                        outcome: lash_durable::domain::tool_calls::ToolCallSettlement::Read,
                        body: Some(*body),
                    },
                },
            ));
        }
        // The candidate the commit publishes: the park's checkpoint root, or
        // the session descriptor an end hands the turn.
        let mut requirements = lash_vocabulary::claims::ClaimRequirements::default();
        let (status, state, format, membership, candidate) = match phase {
            SavePhase::Parked { state, closure, .. } => {
                // What running the parked execution requires: its code's
                // natives and kernel version, and its links' providers and
                // codecs (FIG-6985). An ended one requires nothing.
                requirements.machine =
                    super::loader::requirements(closure, state.header.kernel, std::iter::empty())
                        .machine;
                requirements.extend(&parts.links);
                let membership = book.membership(&ledger).map_err(refused)?;
                // A cell's admissions stay until its turn ends: the commit
                // that consumes its answer reports its calls from them.
                if !matches!(self.exec, ExecKey::Cell(..)) {
                    for write in book.retire_unnamed(&self.exec, &membership) {
                        tx.write(write);
                    }
                }
                // `sealed` checked the root names this closure.
                closure.stage(&mut tx, expected.is_none());
                (
                    ExecStatus::Parked,
                    state.root,
                    state.header.kernel,
                    Some(membership),
                    state.root,
                )
            }
            SavePhase::Ended { end, session } => {
                for write in book.retire_unnamed(&self.exec, &EffectMembership::default()) {
                    tx.write(write);
                }
                let end = EndRecord::stage(&end, &mut staging)
                    .map_err(refused)?
                    .ok_or_else(|| refused("a cancelled run records no end"))?;
                let (Some(host_root), Some(envelope)) = (parts.host, parts.envelope) else {
                    return Err(refused("a cell's end names its host state and envelope"));
                };
                let end = CellEnd {
                    end,
                    session,
                    host: host_root,
                    envelope,
                }
                .stage(&mut staging)
                .map_err(refused)?;
                (ExecStatus::Ended, end, KERNEL_VERSION, None, session)
            }
        };
        let owing = ExecutionDebts {
            exec: self.exec.clone(),
            // An ended execution owes nothing more.
            owed: if status == ExecStatus::Parked {
                parts.owed
            } else {
                Vec::new()
            },
            // The record the root states, joined to the actor's claim
            // requirements in this same commit (FIG-6993).
            requirements: requirements.clone(),
        };
        let commit = CommitId::mint();
        let (root, write) = stage_exec_root(
            &mut staging,
            &self.exec,
            membership.as_ref(),
            RootParts {
                state,
                status,
                executable_identity: document.to_string(),
                format,
                host: parts.host,
                envelope: parts.envelope,
                owed: owing.owed.clone(),
                requirements,
            },
            (
                expected.map(|held| held.rev),
                expected.map(|held| held.root),
            ),
            commit,
        )
        .map_err(refused)?;
        let mut manifest = {
            let mut page_owner = pages.lock().await;
            // Every root a live activation publishes stays leased until
            // its worker and carry end (FIG-6629), starting atomically
            // with publication under the existing owner fence.
            if status == ExecStatus::Parked {
                tx.lease_root(
                    RootOwner::Exec(self.exec.clone()),
                    root,
                    page_owner.lease_ttl_millis(),
                );
            }
            page_owner
                .candidate(candidate, expected.map(|held| held.root), root, commit)
                .map_err(refused)?
        };
        // The run's own closure is staged with its park below.
        super::closure::stage_named(
            manifest.as_mut(),
            carried
                .iter()
                .filter(|closure| Some(closure.reference()) != own),
            &mut tx,
        )
        .map_err(refused)?;
        tx.stage_all(staging);
        for read in reads {
            tx.require(read);
        }
        tx.write(write);
        let label = if admitted.is_empty() {
            CommitLabel::CELL_SNAPSHOT
        } else {
            CommitLabel::CELL_SNAPSHOT_ADMIT
        };
        self.commit_candidate(
            tx,
            CandidatePublication {
                owner: CandidateOwner::ExecRootPut(self.exec.clone()),
                manifest,
            },
            label,
            &owing,
            &mut members,
            pages,
            (root, commit),
        )
        .await?;
        host.host_committed();
        *self
            .book
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = book;
        if !admitted.is_empty() {
            members.admitted(&admitted).map_err(refused)?;
        }
        let rev = SnapshotRev(expected.map_or(1, |held| held.rev.0 + 1));
        *self.held() = Some(Some(HeldRoot { rev, root }));
        Ok(Saved { rev, root, ledger })
    }

    /// Commits `tx` with `candidate`, which publishes the execution's root
    /// `published` (its wrapper and commit) owing `owing`, and settles the
    /// candidate in `pages`' spool. A publication whose acknowledgement was
    /// lost is resolved against the execution's row (D-SPOOL): the spool
    /// settles when the row holds `published`, and either way the save is
    /// refused here: this activation ends, and the next resumes from
    /// whichever root the row holds, its admissions committed with it.
    #[expect(clippy::too_many_arguments, reason = "one publication, in its parts")]
    async fn commit_candidate(
        &self,
        tx: lash_durable::ActorTx,
        candidate: CandidatePublication,
        label: CommitLabel,
        owing: &ExecutionDebts,
        members: &mut crate::members::Members,
        pages: &SharedPages,
        published: (NodeId, CommitId),
    ) -> Result<lash_durable::ActorCommit, QuietPointRefusal> {
        let update = members.prepare_update(&tx, None).await.map_err(refused)?;
        *self
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        let mut page_owner = pages.lock().await;
        match self
            .cx
            .commit_candidate_owing(tx, candidate, label, Some(owing))
            .await
        {
            Ok(mut committed) => {
                for lease in std::mem::take(&mut committed.root_leases) {
                    if page_owner.lease().is_some() {
                        page_owner.protect_published(lease);
                    } else {
                        page_owner.lease_on(lease);
                    }
                }
                page_owner.settle(published.1).map_err(refused)?;
                members.apply_update(update).map_err(refused)?;
                Ok(committed)
            }
            Err(error) => {
                members.forget();
                self.forget_root();
                if ambiguous(&error) {
                    let row = self
                        .cx
                        .pages()
                        .map_err(refused)?
                        .candidate_state(&self.exec)
                        .await
                        .map_err(refused)?;
                    let resolved = page_owner.resolve(row).map_err(refused)?;
                    let committed = matches!(resolved, Resolution::Committed);
                    return Err(refused(format!(
                        "the publication's acknowledgement was lost; the row {} it",
                        if committed { "holds" } else { "does not hold" }
                    )));
                }
                Err(error.into())
            }
        }
    }

    /// Forgets the root and the nodes this activation knew stored: the
    /// next write reads them back.
    fn forget_root(&self) {
        *self.held() = None;
        self.stored
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// The execution's latest root, read whole under one protection; none
    /// when it has none. Read once, until a commit replaces it.
    ///
    /// # Errors
    ///
    /// [`QuietPointRefusal`]: a store failure, or a root that is not one of
    /// this execution: its owner binding, its children's kinds, or a phase
    /// that names a membership when ended or none when parked.
    pub async fn latest_root(&self) -> Result<Option<std::sync::Arc<RootRead>>, QuietPointRefusal> {
        if *self.held() == Some(None) {
            return Ok(None);
        }
        if let Some(read) = self
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            return Ok(Some(read));
        }
        let Some(row) = self.read().await? else {
            *self.held() = Some(None);
            return Ok(None);
        };
        let graph = self
            .cx
            .durable_reads()
            .map_err(refused)?
            .hydrate(
                RootOwner::Exec(self.exec.clone()),
                Some(row.root),
                self.read_limits(),
            )
            .await
            .map_err(refused)?;
        let mut decoder = GraphDecoder::new(&graph, self.read_limits());
        let body: ExecRoot = decoder
            .decode(row.root, NodeKind::EXEC_ROOT)
            .map_err(refused)?;
        body.check_children(row.root, &mut decoder)
            .map_err(refused)?;
        let binding: lash_core_store::checkpoint_graph::bodies::OwnerBinding = decoder
            .decode(body.owner, NodeKind::OWNER_BINDING)
            .map_err(refused)?;
        if binding != super::graph::owner_binding(&self.exec) {
            return Err(refused("the root belongs to another execution"));
        }
        if (body.status == ExecStatus::Parked) != body.effect_membership.is_some() {
            return Err(refused(
                "a parked root names its membership, an ended one none",
            ));
        }
        let held = HeldRoot {
            rev: row.rev,
            root: row.root,
        };
        *self.held() = Some(Some(held));
        *self
            .stored
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            graph.nodes.keys().copied().collect();
        let read = std::sync::Arc::new(RootRead {
            held,
            body,
            graph,
            read_limits: self.read_limits(),
        });
        *self
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(read.clone());
        Ok(Some(read))
    }

    /// The run's latest checkpoint, if any: a parked state with the ledger
    /// and admissions its membership names, each tool request read through
    /// the projection of `projections` it was frozen under, or the end it
    /// recorded.
    ///
    /// # Errors
    ///
    /// [`QuietPointRefusal`]: a store failure, a root of another document,
    /// or one that does not read back whole.
    pub async fn checkpoint(
        &self,
        document: &DocumentId,
        projections: &dyn Projections,
    ) -> Result<Option<Checkpoint>, QuietPointRefusal> {
        let Some(read) = self.latest_root().await? else {
            return Ok(None);
        };
        if read.body.executable_identity != document.to_string() {
            return Err(refused("the root holds another document's run"));
        }
        let mut decoder = GraphDecoder::new(&read.graph, self.read_limits());
        match (read.body.status, read.body.effect_membership) {
            (ExecStatus::Parked, Some(membership)) => {
                let (membership, resolved) =
                    EffectMembership::load(&mut decoder, membership, &self.exec, projections)
                        .map_err(refused)?;
                let ledger = ledger_of(&membership, &resolved)?;
                let book = super::graph::AdmissionBook::restored(resolved);
                let admissions = book.admitted_waits(&ledger).map_err(refused)?;
                *self
                    .book
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = book;
                // The hydrated root holds its CheckpointRoot page as a leaf:
                // the header, and nothing of the heap.
                let page = read
                    .graph
                    .nodes
                    .get(&read.body.state)
                    .ok_or_else(|| refused("the root omits its checkpoint root"))?;
                let header = checkpoint_header(read.body.state, &page.bytes).map_err(refused)?;
                if header.kernel != read.body.format {
                    return Err(refused("the root states another kernel than its header"));
                }
                Ok(Some(Checkpoint::Parked {
                    rev: read.held.rev,
                    state: ParkedState {
                        exec_root: read.held.root,
                        root: read.body.state,
                        header,
                        admissions,
                    },
                    ledger,
                }))
            }
            (ExecStatus::Ended, None) => {
                let cell = CellEnd::load(&mut decoder, read.body.state).map_err(refused)?;
                let end = cell.end.end(&mut decoder).map_err(refused)?;
                Ok(Some(Checkpoint::Ended {
                    rev: read.held.rev,
                    end,
                    cell,
                }))
            }
            _ => Err(refused(
                "a parked root names its membership, an ended one none",
            )),
        }
    }

    /// Each wait `ledger` stands on with the request its admission froze:
    /// what the state a park saved resumes with.
    ///
    /// # Errors
    ///
    /// [`QuietPointRefusal`] for a wait no admission holds.
    pub fn admitted_waits(
        &self,
        ledger: &EffectLedger,
    ) -> Result<Vec<lash_kernel_state::AdmittedWait>, QuietPointRefusal> {
        self.book
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .admitted_waits(ledger)
            .map_err(refused)
    }

    /// The frozen request of each execution `ledger` names, by its
    /// admission: what a resumed host rebuilds its open calls from.
    #[must_use]
    pub fn open_requests(&self, ledger: &EffectLedger) -> Vec<(AdmittedEffect, FrozenRequest)> {
        let book = self
            .book
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pending = ledger
            .pending()
            .filter_map(|(identity, entry)| match &entry.standing {
                Standing::Admitted(admitted) => Some((identity.clone(), admitted.clone())),
                Standing::Refused(_) => None,
            });
        pending
            .filter_map(|(identity, admitted)| Some((admitted, book.request(&identity)?.clone())))
            .chain(ledger.detached_cancellations().filter_map(|admitted| {
                Some((
                    admitted.clone(),
                    book.cancelling_request(admitted.operation)?.clone(),
                ))
            }))
            .collect()
    }
    /// Read the owner's records from the store again: a run that starts
    /// again follows records its commits left out of the cache.
    pub async fn reload_records(&self) {
        self.members.lock().await.forget();
    }

    /// Observe the saved cancellation decisions and signal their bodies before
    /// the guest is allowed to run its cleanup stretch.
    pub async fn apply_cancellations(&self) -> Result<(), QuietPointRefusal> {
        self.members.lock().await.activate().await.map_err(refused)
    }

    /// Every wait `ledger` stands on whose outcome is committed now: read
    /// from the run records, so a restore answers the same ones again.
    ///
    /// # Errors
    ///
    /// [`QuietPointRefusal`]: a store failure, or records that do not fold.
    pub async fn settled(&self, ledger: &EffectLedger) -> Result<Vec<Settled>, QuietPointRefusal> {
        self.settled_as(ledger, &|_, output| outcome_of(output))
            .await
    }

    /// [`settled`](Self::settled), with the parent reading each final record
    /// through `outcome`.
    ///
    /// # Errors
    ///
    /// [`QuietPointRefusal`]: a store failure, or records that do not fold.
    pub async fn settled_as(
        &self,
        ledger: &EffectLedger,
        outcome: OutcomeOf<'_>,
    ) -> Result<Vec<Settled>, QuietPointRefusal> {
        let folded = self.members.lock().await.fold().await.map_err(refused)?;
        Ok(settled_in(ledger, &self.exec, &folded, outcome))
    }

    /// Run the run's open executions until at least one wait `ledger`
    /// stands on is settled, and answer every one that is; `cancel` cancels
    /// every open execution and cuts every sleep: a cancelled run that
    /// awaits no execution is answered with nothing, at once. Answers
    /// [`Driven::Suspended`] once nothing runs and only rows or sleeps are
    /// left to wait on: the run stays parked beyond this activation.
    ///
    /// # Errors
    ///
    /// [`DriveRefusal`]: ownership lost or another store failure, or the
    /// activation stopping; or a [`BrokerInvariant`] that failed, a ledger
    /// that stands on nothing that can settle among them.
    pub async fn drive_effects(
        &self,
        ledger: &EffectLedger,
        cancel: &CancellationToken,
    ) -> Result<Driven<Vec<Settled>>, DriveRefusal> {
        self.drive_effects_as(ledger, cancel, &|_, output| outcome_of(output), None)
            .await
    }

    /// [`drive_effects`](Self::drive_effects), with the parent reading each
    /// final record through `outcome`. `quiet` is notified once nothing
    /// runs and only rows or sleeps are left to wait on.
    ///
    /// # Errors
    ///
    /// As [`drive_effects`](Self::drive_effects).
    pub async fn drive_effects_as(
        &self,
        ledger: &EffectLedger,
        cancel: &CancellationToken,
        outcome: OutcomeOf<'_>,
        quiet: Option<&tokio::sync::Notify>,
    ) -> Result<Driven<Vec<Settled>>, DriveRefusal> {
        let exec = &self.exec;
        let driven = self
            .members
            .lock()
            .await
            .drive_by(cancel, quiet, &mut |_, folded, _| {
                let settled = settled_in(ledger, exec, folded, outcome);
                if !settled.is_empty() || (cancel.is_cancelled() && !ledger.awaits_execution()) {
                    Decide::Answer(settled)
                } else {
                    Decide::Wait { until: None }
                }
            })
            .await
            .map_err(|undriven| match undriven {
                Undriven::Round(
                    lash_core_execution::runtime::actor::round::RoundError::Durable(error),
                ) => DriveRefusal::Durable(error),
                Undriven::Round(error) => DriveRefusal::from(refused(error)),
                Undriven::Unbound => BrokerInvariant::Unbound.into(),
                Undriven::Unanswerable => BrokerInvariant::Unanswerable {
                    calls: ledger
                        .pending()
                        .filter_map(|(_, entry)| match &entry.standing {
                            Standing::Admitted(admitted) => Some(admitted.call.clone()),
                            Standing::Refused(_) => None,
                        })
                        .collect(),
                }
                .into(),
            })?;
        Ok(driven)
    }
}
