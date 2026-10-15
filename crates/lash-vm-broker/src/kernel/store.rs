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
//! outcome rows. A list `join` that answers early leaves its other members'
//! effects live under the run, and so does a task's cancellation: the
//! effect a cancelled task stood on is released, not awaited, and settles
//! on its own. Only the run's end closes: it cancels every execution still
//! open and commits once each has settled, so none outlives the checkpoint
//! that records the end. Losing the worker closes nothing; another node
//! resumes the run from its rows.
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
    self, AdmittedExecution, Recovery, RoundDraft, RunFold, SettledOutput,
};
use lash_core_execution::runtime::actor::waits;
use lash_core_store::checkpoint_graph::bodies::{ExecRoot, ExecStatus};
use lash_core_store::checkpoint_graph::{
    CommitId, EncodeBudget, ExplicitLimit, GraphDecoder, GraphStaging, NodeId, NodeKind,
};
use lash_durable::domain::graph::{HydratedGraph, RootOwner};
use lash_durable::domain::pages::{CandidateOwner, CandidatePublication};
use lash_durable::domain::{ExecKey, RunFactWrite, RunSeq, SnapshotRev};
use lash_durable::{CommitLabel, DomainWrite, DurableError, DurableInstant};
use lash_kernel_doc::{Datum, DocumentId, EffectIdentity, EffectName, ErrorDatum, KERNEL_VERSION};
use lash_kernel_state::paged::CheckpointRoot;
use lash_kernel_vm::{End, Outcome, WaitId};
use tokio_util::sync::CancellationToken;

use super::graph::{
    CellEnd, Disposition, EffectMembership, FrozenAdmission, FrozenRequest, MemberStatus,
    Projections, ResolvedAdmission, RootParts, stage_exec_root,
};
use super::ledger::{AdmittedEffect, EffectLedger, PendingEffect, Standing};
use super::pages::{Resolution, SharedPages};
use super::run::KernelEffects;
use super::state::{EndRecord, ReleaseGraph, admission_of, checkpoint_header};
use super::value::datum_from_json;
use crate::effects::MemberDraft;
use crate::members::{Decide, Driven};
use crate::snapshot::{DurableSnapshotStore, HeldRoot, OperationId, QuietPointRefusal, refused};

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
    Sleep {
        duration: std::time::Duration,
        until: DurableInstant,
    },
    /// Not at all: the parent refuses it, and it is answered with `error`.
    /// The guest's arguments stay with its admission.
    Refused { error: ErrorDatum, args: Vec<Datum> },
}

/// One wait a park admits.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectAdmission {
    pub identity: EffectIdentity,
    pub wait: WaitId,
    /// The effect performed; none for a sleep.
    pub effect: Option<EffectName>,
    pub admit: AdmitAs,
}

/// One call a save admitted, as the host records it in its own state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmittedCall {
    pub effect: EffectIdentity,
    pub operation: EffectName,
    pub call: lash_sansio::ToolCallId,
    /// Its Admission.
    pub admission: NodeId,
}

/// The host's nodes a root names beside the machine's state, and what the
/// host's state owes while the execution is parked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostParts {
    /// A HostRoot.
    pub host: Option<NodeId>,
    /// A CellEnvelope.
    pub envelope: Option<NodeId>,
    /// The callbacks the host's state owes beyond its admitted calls (S7):
    /// a cell's committed deferred matches' providers and codecs.
    pub owed: Vec<lash_core_store::store::plugin_writers::PluginCallbackIdentity>,
}

/// A park's sealed checkpoint root: its page, staged in the activation's
/// spool, and its decoded header.
#[derive(Clone, Debug)]
pub struct SealedPark {
    pub root: NodeId,
    pub header: CheckpointRoot,
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
    /// The helper releases the state may pin.
    pub releases: &'a ReleaseGraph,
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

/// How a run's first save that carried its turn's held model response
/// ended (FIG-6000).
#[derive(Debug)]
pub enum FirstPark {
    /// The save committed with the response.
    Saved(Saved),
    /// The turn accepted an `Immediate` cancel (or its session's close)
    /// before the save: either the save's open read it, and nothing
    /// committed, or the commit read it under its fence, and nothing it
    /// admitted starts. The turn stops on the cell.
    Stopped,
}

/// A run's state at its latest park, as a machine resumes from it.
#[derive(Clone, Debug)]
pub struct ParkedState {
    /// The execution root the row holds: what an activation leases.
    pub exec_root: NodeId,
    /// Its CheckpointRoot page and header.
    pub root: NodeId,
    pub header: CheckpointRoot,
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
#[expect(
    clippy::large_enum_variant,
    reason = "read once per claim and matched at once; a parked state is the common case"
)]
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
    pub identity: EffectIdentity,
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
        SettledOutput::Cancelled { .. } => failed(
            EFFECT_CANCELLED,
            "the effect was cancelled".to_owned(),
            Datum::Null,
        ),
        SettledOutput::Waiting(_) => return None,
    })
}

/// How the parent reads an admitted effect's final record as the outcome
/// its `perform` is answered with; `None` while the record is not final. It
/// is called on every read of a settled wait, so it is a function of its
/// arguments alone: a resume reads the same records again.
pub type OutcomeOf<'a> = &'a (dyn Fn(&AdmittedEffect, &SettledOutput) -> Option<Outcome> + Sync);

/// Every wait `ledger` stands on that `folded` and `now` settle, in
/// identity order.
fn settled_in(
    ledger: &EffectLedger,
    exec: &ExecKey,
    folded: &RunFold,
    now: DurableInstant,
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
                    {
                        let name = entry.effect.as_ref().map_or("effect", EffectName::as_str);
                        error.message = format!(
                            "effect `{name}` at site {:?}, occurrence {} started and never settled; outside work may already have happened, so check outside state before calling again",
                            identity.site, identity.occurrence,
                        );
                        error.data = Datum::Record(vec![
                            ("effect".to_owned(), Datum::Text(name.to_owned())),
                            ("site".to_owned(), Datum::Record(vec![
                                ("unit".to_owned(), Datum::Text(match &identity.site.unit {
                                    lash_kernel_doc::Unit::Main => "main".to_owned(),
                                    lash_kernel_doc::Unit::Function(name) => format!("fn:{name}"),
                                    lash_kernel_doc::Unit::Library(function) => format!("lib:{function}"),
                                })),
                                ("path".to_owned(), Datum::List(identity.site.path.iter()
                                    .map(|part| Datum::Int(i64::from(*part).into())).collect())),
                            ])),
                            ("occurrence".to_owned(), Datum::Int(lash_kernel_doc::Integer::new(identity.occurrence))),
                            ("cause".to_owned(), error.data.clone()),
                        ]);
                    }
                    result
                },
                Standing::Sleeping { until_ms } => {
                    (now.0 >= *until_ms).then_some(Outcome::Elapsed)?
                }
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

#[expect(
    clippy::large_enum_variant,
    reason = "one per park transaction, consumed by its commit"
)]
enum SavePhase<'a> {
    Parked {
        state: SealedPark,
        releases: &'a ReleaseGraph,
        /// The model response a fresh run's first park commits (FIG-6000).
        response: Option<&'a lash_core_execution::HeldResponse>,
    },
    Ended {
        end: End,
        session: NodeId,
    },
}

/// The ledger a stored membership names, from the admissions it resolved.
fn ledger_of(
    membership: &EffectMembership,
    resolved: &BTreeMap<EffectIdentity, ResolvedAdmission>,
) -> Result<EffectLedger, QuietPointRefusal> {
    let mut pending = BTreeMap::new();
    let mut released = Vec::new();
    for entry in &membership.entries {
        let admitted = resolved
            .get(&entry.effect)
            .ok_or_else(|| refused("a membership entry resolved no admission"))?;
        let execution = match &admitted.admission.disposition {
            Disposition::Execution { operation, pin } => Some(AdmittedEffect {
                operation: *operation,
                call: pin.call.clone(),
            }),
            Disposition::Sleep | Disposition::Refused => None,
        };
        let standing = match (entry.status, execution, &admitted.request) {
            (MemberStatus::Released, Some(execution), _) => {
                released.push(execution);
                continue;
            }
            (MemberStatus::Pending, Some(execution), _) => Standing::Admitted(execution),
            (MemberStatus::Pending, None, FrozenRequest::Sleep { until_ms, .. }) => {
                Standing::Sleeping {
                    until_ms: *until_ms,
                }
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
                effect: admitted.admission.effect_name.clone(),
                standing,
            },
        );
    }
    EffectLedger::restored(pending, released, membership.next_park).map_err(refused)
}

/// The save a commit that carried no held response made.
fn saved(park: FirstPark) -> Result<Saved, QuietPointRefusal> {
    match park {
        FirstPark::Saved(saved) => Ok(saved),
        FirstPark::Stopped => Err(refused("a save that carries no response stopped")),
    }
}

fn staging() -> GraphStaging {
    GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)))
}

/// Whether a refused publication is ambiguous: its acknowledgement was
/// lost, so the owner's row decides it (D-SPOOL).
fn ambiguous(error: &DurableError) -> bool {
    matches!(error, DurableError::AckLost { .. })
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
        self.commit_parked(save, host, None).await.and_then(saved)
    }

    /// Commit `save`, a fresh run's first park, with the model response
    /// its turn holds for it (FIG-6000), in one transaction labelled
    /// `model.done+cell.snapshot+admit`. The save's open reads the turn's
    /// accepted cancel: an `Immediate` one, or the session's close, stops
    /// the cell with nothing committed. One its commit reads under its
    /// fence leaves the save committed and starts nothing it admitted.
    ///
    /// # Errors
    ///
    /// As [`commit_park`](Self::commit_park); the response stays held.
    pub async fn commit_first_park(
        &self,
        save: ParkSave<'_>,
        host: &dyn KernelEffects,
        response: &lash_core_execution::HeldResponse,
    ) -> Result<FirstPark, QuietPointRefusal> {
        self.commit_parked(save, host, Some(response)).await
    }

    async fn commit_parked(
        &self,
        save: ParkSave<'_>,
        host: &dyn KernelEffects,
        response: Option<&lash_core_execution::HeldResponse>,
    ) -> Result<FirstPark, QuietPointRefusal> {
        let ParkSave {
            document,
            state,
            pages,
            releases,
            ledger,
            admit,
            with,
        } = save;
        self.commit_checkpoint(
            host,
            &document,
            ledger,
            SavePhase::Parked {
                state,
                releases,
                response,
            },
            pages,
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
            save.pages,
            Vec::new(),
            save.with,
        )
        .await
        .and_then(saved)
    }

    /// The parked root of a run whose end its consumer commits (FIG-5999):
    /// `ledger`, as the run left it, stands on nothing, so nothing is left
    /// to close, and the commit that consumes the run's answer retires
    /// this root with every record and admission under it. A successor
    /// that finds the root resumes from it, is answered the same committed
    /// outcomes and runs the final stretch again.
    ///
    /// `None` when the ledger still names a wait or a released execution.
    /// Closing an execution is a commit of its own, and a successor that
    /// resumed from the parked root would read the closing's cancel as the
    /// execution's own outcome; a sleep still pending could be over by
    /// then and answer a race its call had won. Such a run closes and
    /// records its end ([`commit_run_end`](Self::commit_run_end)).
    ///
    /// # Errors
    ///
    /// [`QuietPointRefusal`]: a store failure, or a run with no root.
    pub async fn held_end(
        &self,
        ledger: &EffectLedger,
    ) -> Result<Option<HeldRoot>, QuietPointRefusal> {
        if ledger.pending().next().is_some() || !ledger.released().is_empty() {
            return Ok(None);
        }
        self.revision()
            .await?
            .map(Some)
            .ok_or_else(|| refused("a run that parked holds its root"))
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
        pages: &SharedPages,
        admit: Vec<EffectAdmission>,
        with: Vec<DomainWrite>,
    ) -> Result<FirstPark, QuietPointRefusal> {
        let response = match phase {
            SavePhase::Parked { response, .. } => response,
            SavePhase::Ended { .. } => None,
        };
        let mut members = self.members.lock().await;
        if matches!(phase, SavePhase::Ended { .. }) {
            if ledger.executions().next().is_some() {
                members.close().await.map_err(refused)?;
            }
            ledger.close();
        }
        // A released execution whose outcome committed is no longer open.
        if !ledger.released().is_empty() {
            let folded = members.fold().await.map_err(refused)?;
            ledger.drop_released(|admitted| final_output(&folded, &self.exec, admitted).is_some());
        }
        struct Requested {
            identity: EffectIdentity,
            wait: WaitId,
            effect: Option<EffectName>,
            standing: Option<Standing>,
            request: Option<FrozenRequest>,
        }
        let mut drafts = Vec::new();
        let mut executions = Vec::new();
        let mut requested = Vec::with_capacity(admit.len());
        let mut timers = Vec::new();
        for admission in admit {
            let EffectAdmission {
                identity,
                wait,
                effect,
                admit,
            } = admission;
            let (standing, request) = match admit {
                AdmitAs::Execution(draft) => {
                    let MemberDraft { draft, request } = *draft;
                    executions.push((requested.len(), request));
                    drafts.push(draft);
                    (None, None)
                }
                AdmitAs::Sleep { duration, until } => {
                    timers.push(until);
                    (
                        Some(Standing::Sleeping { until_ms: until.0 }),
                        Some(FrozenRequest::Sleep {
                            duration,
                            until_ms: until.0,
                        }),
                    )
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
            });
        }
        let expected = self.revision().await?;
        let mut tx = self.cx.begin().await.map_err(refused)?;
        // The response the turn held commits first, as `model.done` would
        // have, in this transaction: unless the turn stops at once.
        if let Some(response) = response {
            if response.stops_at(&tx) {
                return Ok(FirstPark::Stopped);
            }
            let held = response
                .writes()
                .ok_or_else(|| refused("a first save carries a response its turn holds"))?;
            tx.append(held);
        }
        // A sleep is a timer wait of the run's scope, pinned with the park
        // that admits it: the owner that holds only sleeps releases until
        // the earliest, and the scope's end revokes them.
        if !timers.is_empty() {
            let scope = waits::wait_scope(&self.cx).map_err(refused)?;
            for deadline in timers {
                waits::pin(
                    &mut tx,
                    waits::WaitSpec {
                        scope: scope.clone(),
                        purpose: waits::WaitPurpose::Timer { deadline },
                    },
                );
            }
        }
        let mut staging = staging();
        let mut book = self
            .book
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut calls = Vec::new();
        let mut admitted: Vec<AdmittedExecution> = Vec::new();
        if !drafts.is_empty() {
            let park = ledger.take_park();
            let pinned = round::pin_round(
                &mut tx,
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
                        &mut staging,
                        &self.exec,
                        FrozenAdmission {
                            effect: entry.identity.clone(),
                            wait: entry.wait.0,
                            effect_name: entry.effect.clone(),
                            request,
                            execution: Some((operation, pin)),
                        },
                    )
                    .map_err(refused)?;
                tx.write(write);
                nodes.push(node);
                if let Some(operation) = &entry.effect {
                    calls.push(AdmittedCall {
                        effect: entry.identity.clone(),
                        operation: operation.clone(),
                        call: call.clone(),
                        admission: node,
                    });
                }
                entry.standing = Some(Standing::Admitted(AdmittedEffect { operation, call }));
            }
            admitted = pinned
                .admit(&mut tx, nodes)
                .map_err(refused)?
                .members()
                .to_vec();
        }
        for entry in requested {
            let Some(standing) = entry.standing else {
                return Err(refused("an execution's admission minted no identity"));
            };
            if let Some(request) = entry.request {
                let (_, write) = book
                    .admit(
                        &mut staging,
                        &self.exec,
                        FrozenAdmission {
                            effect: entry.identity.clone(),
                            wait: entry.wait.0,
                            effect_name: entry.effect.clone(),
                            request,
                            execution: None,
                        },
                    )
                    .map_err(refused)?;
                tx.write(write);
            }
            let pending = PendingEffect {
                wait: entry.wait.0,
                effect: entry.effect,
                standing,
            };
            if !ledger.stand(entry.identity.clone(), pending) {
                return Err(refused(format!(
                    "the run already stands on effect {:?}",
                    entry.identity
                )));
            }
        }
        // Records no saved state can reach again go, in the transaction
        // that saves the state that consumed them. What their settled
        // members changed commits with the prune as the turn's run
        // namespaces (FIG-5301): a pruned outcome is never the only copy of
        // a value. The host's state, staged below, holds each consumed
        // outcome first.
        let oldest = ledger.oldest_reachable();
        let changes = match (&self.exec, members.bodies()) {
            (ExecKey::Cell(session, run, _), Some(bodies)) if oldest > 0 => {
                Some((session.clone(), run.clone(), bodies.run_changes(), bodies))
            }
            _ => None,
        };
        if let Some((session, run, namespaces, _)) = &changes
            && !namespaces.is_empty()
        {
            tx.write_namespaces(session, run, namespaces)
                .map_err(refused)?;
        }
        for write in with {
            tx.write(write);
        }
        if oldest > 0 {
            tx.write(DomainWrite::RunFact(RunFactWrite::Prune {
                owner: self.exec.owner(),
                before: RunSeq(oldest),
            }));
        }
        let mut reads = BTreeSet::new();
        let parts = host
            .stage_host(&mut staging, &mut reads, &calls)
            .map_err(refused)?;
        // The candidate the commit publishes: the park's checkpoint root, or
        // the session descriptor an end hands the turn.
        let (status, state, format, membership, candidate) = match phase {
            SavePhase::Parked {
                state, releases, ..
            } => {
                let membership = book.membership(&ledger).map_err(refused)?;
                for write in book.retire_unnamed(&self.exec, &membership) {
                    tx.write(write);
                }
                releases
                    .pin(
                        self.cx.durable_reads().map_err(refused)?,
                        &mut tx,
                        NodeId(state.header.release.0),
                        expected.is_none(),
                    )
                    .await
                    .map_err(refused)?;
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
            },
            (
                expected.map(|held| held.rev),
                expected.map(|held| held.root),
            ),
            commit,
        )
        .map_err(refused)?;
        let manifest = pages
            .lock()
            .await
            .candidate(candidate, expected.map(|held| held.root), root, commit)
            .map_err(refused)?;
        tx.stage_all(staging);
        for read in reads {
            tx.require(read);
        }
        tx.write(write);
        let label = if response.is_some() {
            CommitLabel::MODEL_DONE_CELL_SNAPSHOT_ADMIT
        } else if admitted.is_empty() {
            CommitLabel::CELL_SNAPSHOT
        } else {
            CommitLabel::CELL_SNAPSHOT_ADMIT
        };
        let committed = self
            .commit_candidate(
                tx,
                CandidatePublication {
                    owner: CandidateOwner::ExecRootPut(self.exec.clone()),
                    manifest,
                    admission: admission_of(candidate, commit),
                },
                label,
                &owing,
                &mut members,
                pages,
                (root, commit),
            )
            .await?;
        // What the cell published while the response was held is
        // published now, before anything it admitted starts.
        if let Some(response) = response {
            response.committed(label);
        }
        let stopped = response.is_some_and(|response| response.stops_in(&committed));
        host.host_committed();
        *self
            .book
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = book;
        if let Some((_, _, namespaces, bodies)) = &changes {
            bodies.run_changes_committed(namespaces);
        }
        if !admitted.is_empty() && !stopped {
            members.admitted(&admitted).map_err(refused)?;
        }
        let rev = SnapshotRev(expected.map_or(1, |held| held.rev.0 + 1));
        *self.held() = Some(Some(HeldRoot { rev, root }));
        if stopped {
            return Ok(FirstPark::Stopped);
        }
        Ok(FirstPark::Saved(Saved { rev, root, ledger }))
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
        members.forget();
        *self
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        match self
            .cx
            .commit_candidate_owing(tx, candidate, label, Some(owing))
            .await
        {
            Ok(committed) => {
                pages.lock().await.settle(published.1).map_err(refused)?;
                Ok(committed)
            }
            Err(error) => {
                self.forget_root();
                if ambiguous(&error) {
                    let row = self
                        .cx
                        .pages()
                        .map_err(refused)?
                        .candidate_state(&self.exec)
                        .await
                        .map_err(refused)?;
                    let resolved = pages.lock().await.resolve(row).map_err(refused)?;
                    let committed = matches!(resolved, Resolution::Committed);
                    return Err(refused(format!(
                        "the publication's acknowledgement was lost; the row {} it",
                        if committed { "holds" } else { "does not hold" }
                    )));
                }
                Err(refused(error))
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
                Standing::Sleeping { .. } | Standing::Refused(_) => None,
            });
        pending
            .filter_map(|(identity, admitted)| Some((admitted, book.request(&identity)?.clone())))
            .chain(ledger.released().iter().filter_map(|admitted| {
                Some((
                    admitted.clone(),
                    book.released_request(admitted.operation)?.clone(),
                ))
            }))
            .collect()
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
        let now = self.cx.durable_now().await.map_err(refused)?;
        Ok(settled_in(ledger, &self.exec, &folded, now, outcome))
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
    /// [`QuietPointRefusal`]: ownership lost or another store failure, the
    /// activation stopping, or a ledger that stands on nothing that can
    /// settle.
    pub async fn drive_effects(
        &self,
        ledger: &EffectLedger,
        cancel: &CancellationToken,
    ) -> Result<Driven<Vec<Settled>>, QuietPointRefusal> {
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
    ) -> Result<Driven<Vec<Settled>>, QuietPointRefusal> {
        let exec = &self.exec;
        let driven = self
            .members
            .lock()
            .await
            .drive_by(cancel, quiet, &mut |_, folded, now| {
                let settled = settled_in(ledger, exec, folded, now, outcome);
                if !settled.is_empty() || (cancel.is_cancelled() && !ledger.awaits_execution()) {
                    Decide::Answer(settled)
                } else {
                    Decide::Wait {
                        until: ledger.next_wake(),
                    }
                }
            })
            .await
            .map_err(refused)?;
        // A sleep that is over settles its timer row, so the owner's next
        // release is not due at a deadline already answered.
        if let Driven::Answered(settled) = &driven
            && settled
                .iter()
                .any(|settled| settled.outcome == Outcome::Elapsed)
        {
            waits::settle_due(&self.cx).await.map_err(refused)?;
        }
        Ok(driven)
    }
}
