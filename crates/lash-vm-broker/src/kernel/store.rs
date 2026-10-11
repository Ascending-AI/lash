//! A kernel run's parks on the durable store (ADR 0132 §8).
//!
//! # The park transaction
//!
//! [`DurableSnapshotStore::commit_park`] commits, in one
//! `cell.snapshot+admit` transaction, the execution's next root graph
//! (FIG-5887 §5): the machine's KernelWhole at the next revision, the
//! effect membership that matches its [`EffectLedger`], one Admission per
//! effect requested since the last park (holding its one frozen request),
//! the round admission and `x_start` of every admitted execution (one
//! each, under its own declared policy, limit and completion wait), the
//! timer wait of every sleep, due at the deadline the sleep was admitted
//! with (ADR 0132 §6), the host's own root and envelope, and the prune of
//! the run records no saved state can reach again. No effect's body starts
//! before that commit; each runs through the admitted-execution lifecycle
//! (ADR 0132 §5) and its outcome commits as `round.outcome`. The root
//! depends on no earlier root: a restore reads it alone.
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

use std::collections::{BTreeMap, BTreeSet};

use lash_core_execution::runtime::actor::round::{
    self, AdmittedExecution, Recovery, RoundDraft, RunFold, SettledOutput,
};
use lash_core_execution::runtime::actor::waits;
use lash_core_store::checkpoint_graph::bodies::{ExecRoot, ExecStatus};
use lash_core_store::checkpoint_graph::{
    CommitId, EncodeBudget, EncodedNode, ExplicitLimit, GraphDecoder, GraphStaging, NodeId,
    NodeKind,
};
use lash_durable::domain::graph::{HydratedGraph, ReleasePinWrite, RootOwner};
use lash_durable::domain::{ExecKey, RunFactWrite, RunSeq, SnapshotRev};
use lash_durable::{CommitLabel, DomainWrite, DurableInstant};
use lash_kernel_doc::{Datum, DocumentId, EffectIdentity, EffectName, ErrorDatum, KERNEL_VERSION};
use lash_kernel_vm::{End, Outcome, WaitId};
use tokio_util::sync::CancellationToken;

use super::graph::{
    CellEnd, Disposition, EffectMembership, FrozenAdmission, FrozenRequest, MemberStatus,
    Projections, ResolvedAdmission, RootParts, stage_exec_root,
};
use super::ledger::{AdmittedEffect, EffectLedger, PendingEffect, Standing};
use super::run::KernelEffects;
use super::state::{ReleaseGraph, WholeState};
use super::value::datum_from_json;
use crate::effects::MemberDraft;
use crate::members::{Decide, Driven};
use crate::snapshot::{
    DurableSnapshotStore, HeldRoot, OperationId, QuietPointRefusal, READ_LIMITS, refused,
};

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

/// The host's nodes a root names beside the machine's state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostParts {
    /// A HostRoot.
    pub host: Option<NodeId>,
    /// A CellEnvelope.
    pub envelope: Option<NodeId>,
}

/// What one park commits.
pub struct ParkSave<'a> {
    /// The document the run executes.
    pub document: DocumentId,
    /// The machine's state at this park.
    pub state: WholeState,
    /// The helper releases the state may pin.
    pub releases: &'a ReleaseGraph,
    /// The ledger that matches it, before this park's admissions.
    pub ledger: EffectLedger,
    /// Every wait requested since the last park.
    pub admit: Vec<EffectAdmission>,
    /// The owner's rows that commit with it.
    pub with: Vec<DomainWrite>,
}

/// A SessionExecution graph: its root and the nodes a capture staged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionGraph {
    pub root: NodeId,
    pub nodes: BTreeMap<NodeId, EncodedNode>,
}

/// What a run's end commits.
#[derive(Clone, Debug, PartialEq)]
pub struct EndSave {
    pub document: DocumentId,
    /// The ledger as the run left it.
    pub ledger: EffectLedger,
    pub end: End,
    /// The session state the cell leaves, which the turn adopts.
    pub session: SessionGraph,
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
#[derive(Clone, Debug, PartialEq)]
pub struct ParkedState {
    /// The KernelWhole and every node it reaches but its release's.
    pub whole: WholeState,
    /// The document it was written under.
    pub document: String,
    /// Each wait's request, taken from the one admission that holds it.
    pub admissions: Vec<lash_kernel_state::AdmittedWait>,
}

impl ParkedState {
    /// The state as a worker resumes it, sealed under `owner`.
    #[must_use]
    pub fn sealed(&self, owner: lash_vm_protocol::VmOwner) -> lash_vm_protocol::ParkedGraph {
        lash_vm_protocol::ParkedGraph {
            state: self.whole.sealed(owner, &self.document),
            admissions: self
                .admissions
                .iter()
                .map(super::state::admission)
                .collect(),
        }
    }
}

/// An execution's latest root as read back: its row, its body and every
/// node it reaches.
#[derive(Clone, Debug)]
pub struct RootRead {
    pub held: HeldRoot,
    pub body: ExecRoot,
    pub graph: HydratedGraph,
}

/// What a run's latest root holds.
#[derive(Clone, Debug, PartialEq)]
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

enum SavePhase<'a> {
    Parked {
        state: WholeState,
        releases: &'a ReleaseGraph,
    },
    Ended {
        end: End,
        session: SessionGraph,
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

fn staging() -> GraphStaging {
    GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)))
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
        let ParkSave {
            document,
            state,
            releases,
            ledger,
            admit,
            with,
        } = save;
        self.commit_checkpoint(
            host,
            &document,
            ledger,
            SavePhase::Parked { state, releases },
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
        save: EndSave,
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
            Vec::new(),
            save.with,
        )
        .await
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one park transaction, in its commit order"
    )]
    async fn commit_checkpoint(
        &self,
        host: &dyn KernelEffects,
        document: &DocumentId,
        mut ledger: EffectLedger,
        phase: SavePhase<'_>,
        admit: Vec<EffectAdmission>,
        with: Vec<DomainWrite>,
    ) -> Result<Saved, QuietPointRefusal> {
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
        let mut parked = BTreeSet::new();
        let (status, state, format, membership) = match phase {
            SavePhase::Parked { state, releases } => {
                let membership = book.membership(&ledger).map_err(refused)?;
                for write in book.retire_unnamed(&self.exec, &membership) {
                    tx.write(write);
                }
                self.pin_release(&mut tx, &state, releases, expected.is_none())?;
                let (root, kernel) = (state.root, state.kernel);
                // The park exports the whole state; a node the latest root
                // reaches is stored, so only the others are staged and a
                // large value an earlier park kept is never carried again.
                let stored = self.stored_nodes();
                parked = state.nodes.keys().copied().collect();
                tx.stage(
                    state
                        .nodes
                        .into_values()
                        .filter(|node| !stored.contains(&node.id)),
                );
                (ExecStatus::Parked, root, kernel, Some(membership))
            }
            SavePhase::Ended { end, session } => {
                for write in book.retire_unnamed(&self.exec, &EffectMembership::default()) {
                    tx.write(write);
                }
                let terminal = super::state::export_end_state(&end, KERNEL_VERSION, u64::MAX)
                    .map_err(refused)?
                    .ok_or_else(|| refused("a cancelled run records no end"))?;
                let (Some(host_root), Some(envelope)) = (parts.host, parts.envelope) else {
                    return Err(refused("a cell's end names its host state and envelope"));
                };
                let end = CellEnd {
                    terminal: terminal.root,
                    session: session.root,
                    host: host_root,
                    envelope,
                }
                .stage(&mut staging)
                .map_err(refused)?;
                // The terminal export is whole, and the session's state
                // shares the run's values: what the latest root reaches is
                // stored, and the end's root reaches it in turn.
                let stored = self.stored_nodes();
                tx.stage(
                    terminal
                        .nodes
                        .into_values()
                        .chain(session.nodes.into_values())
                        .filter(|node| !stored.contains(&node.id)),
                );
                (ExecStatus::Ended, end, KERNEL_VERSION, None)
            }
        };
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
            },
            (
                expected.map(|held| held.rev),
                expected.map(|held| held.root),
            ),
            CommitId::mint(),
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
        self.commit(tx, label, &mut members).await?;
        *self
            .stored
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = parked;
        host.host_committed();
        *self
            .book
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = book;
        if let Some((_, _, namespaces, bodies)) = &changes {
            bodies.run_changes_committed(namespaces);
        }
        if !admitted.is_empty() {
            members.admitted(&admitted).map_err(refused)?;
        }
        let rev = SnapshotRev(expected.map_or(1, |held| held.rev.0 + 1));
        *self.held() = Some(Some(HeldRoot { rev, root }));
        Ok(Saved { rev, root, ledger })
    }

    /// Pins the helper release `state` pins: on the execution's first root
    /// its manifest's nodes are installed with the release's pin, and every
    /// later root requires them.
    fn pin_release(
        &self,
        tx: &mut lash_durable::ActorTx,
        state: &WholeState,
        releases: &ReleaseGraph,
        first: bool,
    ) -> Result<(), QuietPointRefusal> {
        // The state omits the release manifest it pins; a whole that places
        // a large value is read over the release's nodes, which tell its
        // typed children from its leaves.
        let source = super::state::Layered {
            state: &state.nodes,
            release: &releases.nodes,
        };
        let whole: lash_kernel_state::KernelWhole = GraphDecoder::new(&source, READ_LIMITS)
            .decode(state.root, NodeKind::KERNEL_WHOLE)
            .map_err(refused)?;
        let release = NodeId(whole.release.0);
        if !first {
            tx.require(release);
            return Ok(());
        }
        let (name, nodes) = releases
            .pinned(release)
            .ok_or_else(|| refused("the state pins a release this build does not hold"))?;
        tx.stage(nodes.into_values());
        tx.write(DomainWrite::ReleasePin(ReleasePinWrite::Install {
            release: name,
            root: release,
        }));
        Ok(())
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
                READ_LIMITS,
            )
            .await
            .map_err(refused)?;
        let mut decoder = GraphDecoder::new(&graph, READ_LIMITS);
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
        let read = std::sync::Arc::new(RootRead { held, body, graph });
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
        let mut decoder = GraphDecoder::new(&read.graph, READ_LIMITS);
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
                let whole = WholeState::parked_in(
                    &read.graph,
                    read.body.state,
                    read.body.format,
                    &READ_LIMITS,
                )
                .map_err(refused)?;
                Ok(Some(Checkpoint::Parked {
                    rev: read.held.rev,
                    state: ParkedState {
                        whole,
                        document: read.body.executable_identity.clone(),
                        admissions,
                    },
                    ledger,
                }))
            }
            (ExecStatus::Ended, None) => {
                let cell = CellEnd::load(&mut decoder, read.body.state).map_err(refused)?;
                let end = super::state::read_end_in(&read.graph, cell.terminal, READ_LIMITS)
                    .map_err(refused)?;
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
