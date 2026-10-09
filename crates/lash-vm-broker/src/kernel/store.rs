//! A kernel run's parks on the durable store (ADR 0132 §8).
//!
//! # The park transaction
//!
//! [`DurableSnapshotStore::commit_park`] commits, in one
//! `cell.snapshot+admit` transaction: the run's saved state at the next
//! snapshot revision, the [`EffectLedger`] that matches it, the admission
//! and `x_start` of every effect requested since the last park (one
//! admitted execution each, under its own declared policy, limit and
//! completion wait), the deadline of every sleep, and the prune of the run
//! records no saved state can reach again. No effect's body starts before
//! that commit; each runs through the admitted-execution lifecycle (ADR
//! 0132 §5) and its outcome commits as `round.outcome`.
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

use lash_core_execution::runtime::actor::round::{
    self, AdmittedExecution, Recovery, RoundDraft, RunFold, SettledOutput,
};
use lash_core_execution::runtime::actor::waits;
use lash_durable::domain::{ExecKey, RunRecordWrite, RunSeq, SnapshotRev, SnapshotWrite};
use lash_durable::{CommitLabel, DomainWrite, DurableInstant};
use lash_kernel_doc::{Datum, DocumentId, EffectIdentity, EffectName, ErrorDatum, KERNEL_VERSION};
use lash_kernel_vm::{Outcome, WaitId};
use lash_vm_protocol::EncodedPayload;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use super::ledger::{
    AdmittedEffect, EffectLedger, ParkedCheckpoint, PendingEffect, RecordedEnd, Standing,
};
use super::value::datum_from_json;
use crate::effects::MemberDraft;
use crate::ledger::QuietPointRefusal;
use crate::members::{Decide, Driven};
use crate::snapshot::{DurableSnapshotStore, OperationId, refused};

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
    /// As a sleep that is over at `until`.
    Sleep { until: DurableInstant },
    /// Not at all: the parent refuses it, and it is answered with `error`.
    Refused(ErrorDatum),
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

/// What one park commits.
#[derive(Clone, Debug, PartialEq)]
pub struct ParkSave<P> {
    /// The document the run executes.
    pub document: DocumentId,
    /// The machine's state at this park.
    pub state: P,
    /// The ledger that matches it, before this park's admissions.
    pub ledger: EffectLedger,
    /// Every wait requested since the last park.
    pub admit: Vec<EffectAdmission>,
    /// The host's own state for the run at this park.
    pub host: Option<EncodedPayload>,
    /// The owner's rows that commit with it.
    pub with: Vec<DomainWrite>,
}

/// What a run's end commits.
#[derive(Clone, Debug, PartialEq)]
pub struct EndSave {
    pub document: DocumentId,
    /// The ledger as the run left it.
    pub ledger: EffectLedger,
    pub end: RecordedEnd,
    pub host: Option<EncodedPayload>,
    pub with: Vec<DomainWrite>,
}

/// A committed save.
#[derive(Clone, Debug, PartialEq)]
pub struct Saved<P> {
    /// The new revision.
    pub rev: SnapshotRev,
    /// The checkpoint as it was stored: its ledger names this park's
    /// admissions.
    pub checkpoint: ParkedCheckpoint<P>,
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
            "the effect started and never settled".to_owned(),
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

/// Every wait `ledger` stands on that `folded` and `now` settle, in
/// identity order.
fn settled_in(
    ledger: &EffectLedger,
    exec: &ExecKey,
    folded: &RunFold,
    now: DurableInstant,
) -> Vec<Settled> {
    ledger
        .pending()
        .filter_map(|(identity, entry)| {
            let outcome = match &entry.standing {
                Standing::Admitted(admitted) => {
                    final_output(folded, exec, admitted).and_then(outcome_of)?
                }
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

fn encode<P: Serialize>(checkpoint: &ParkedCheckpoint<P>) -> Result<String, QuietPointRefusal> {
    serde_json::to_string(checkpoint).map_err(refused)
}

impl DurableSnapshotStore {
    /// Commit `save` in one transaction (see the module docs).
    ///
    /// # Errors
    ///
    /// [`QuietPointRefusal`]: an identity the ledger already stands on,
    /// ownership lost or another store failure. Nothing is admitted.
    pub async fn commit_park<P: Serialize>(
        &self,
        save: ParkSave<P>,
    ) -> Result<Saved<P>, QuietPointRefusal> {
        let ParkSave {
            document,
            state,
            ledger,
            admit,
            host,
            with,
        } = save;
        let checkpoint = ParkedCheckpoint {
            state: Some(state),
            ledger,
            host,
            end: None,
        };
        self.commit_checkpoint(&document, checkpoint, admit, with)
            .await
    }

    /// Commit a run's end: every execution still open is cancelled and
    /// settles first, so the checkpoint that records the end names none.
    ///
    /// # Errors
    ///
    /// As [`commit_park`](Self::commit_park).
    pub async fn commit_run_end<P: Serialize>(
        &self,
        save: EndSave,
    ) -> Result<Saved<P>, QuietPointRefusal> {
        let checkpoint = ParkedCheckpoint {
            state: None,
            ledger: save.ledger,
            host: save.host,
            end: Some(save.end),
        };
        self.commit_checkpoint(&save.document, checkpoint, Vec::new(), save.with)
            .await
    }

    async fn commit_checkpoint<P: Serialize>(
        &self,
        document: &DocumentId,
        mut checkpoint: ParkedCheckpoint<P>,
        admit: Vec<EffectAdmission>,
        with: Vec<DomainWrite>,
    ) -> Result<Saved<P>, QuietPointRefusal> {
        let mut members = self.members.lock().await;
        if checkpoint.end.is_some() {
            if checkpoint.ledger.executions().next().is_some() {
                members.close().await.map_err(refused)?;
            }
            checkpoint.ledger.close();
        }
        // A released execution whose outcome committed is no longer open.
        if !checkpoint.ledger.released().is_empty() {
            let folded = members.fold().await.map_err(refused)?;
            checkpoint
                .ledger
                .drop_released(|admitted| final_output(&folded, &self.exec, admitted).is_some());
        }
        let mut drafts = Vec::new();
        let mut executions = Vec::new();
        let mut standings = Vec::with_capacity(admit.len());
        for admission in admit {
            let EffectAdmission {
                identity,
                wait,
                effect,
                admit,
            } = admission;
            let standing = match admit {
                AdmitAs::Execution(draft) => {
                    let MemberDraft { draft, request } = *draft;
                    executions.push((standings.len(), request));
                    drafts.push(draft);
                    None
                }
                AdmitAs::Sleep { until } => Some(Standing::Sleeping { until_ms: until.0 }),
                AdmitAs::Refused(error) => Some(Standing::Refused(error)),
            };
            standings.push((identity, wait, effect, standing));
        }
        let expected = self.revision().await?;
        let mut tx = self.cx.begin().await.map_err(refused)?;
        let mut admitted: Vec<AdmittedExecution> = Vec::new();
        if !drafts.is_empty() {
            let park = checkpoint.ledger.take_park();
            admitted = round::admit_round(
                &mut tx,
                &waits::wait_scope(&self.cx).map_err(refused)?,
                RoundDraft {
                    owner: self.exec.owner(),
                    run: RunSeq(park),
                    members: drafts,
                },
            )
            .map_err(refused)?
            .members()
            .to_vec();
            for (execution, (index, request)) in admitted.iter().zip(executions) {
                standings[index].3 = Some(Standing::Admitted(AdmittedEffect {
                    operation: OperationId::of(execution.id()),
                    call: execution.call().clone(),
                    request,
                }));
            }
        }
        for (identity, wait, effect, standing) in standings {
            let Some(standing) = standing else {
                return Err(refused("an execution's admission minted no identity"));
            };
            let entry = PendingEffect {
                wait: wait.0,
                effect,
                standing,
            };
            if !checkpoint.ledger.stand(identity.clone(), entry) {
                return Err(refused(format!(
                    "the run already stands on effect {identity:?}"
                )));
            }
        }
        // Records no saved state can reach again go, in the transaction
        // that saves the state that consumed them. What their settled
        // members changed commits with the prune as the turn's run
        // namespaces (FIG-5301): a pruned outcome is never the only copy of
        // a value.
        let oldest = checkpoint.ledger.oldest_reachable();
        let changes = match (&self.exec, members.bodies()) {
            (ExecKey::Cell(session, run, _), Some(bodies)) if oldest > 0 => {
                Some((session.clone(), run.clone(), bodies.run_changes(), bodies))
            }
            _ => None,
        };
        if let Some((session, run, namespaces, _)) = &changes
            && !namespaces.is_empty()
        {
            tx.write(DomainWrite::Turn(
                lash_durable::domain::TurnWrite::Namespaces {
                    session: session.clone(),
                    run: run.clone(),
                    namespaces: namespaces.clone(),
                },
            ));
        }
        for write in with {
            tx.write(write);
        }
        if oldest > 0 {
            tx.write(DomainWrite::RunRecord(RunRecordWrite::Prune {
                owner: self.exec.owner(),
                before: RunSeq(oldest),
            }));
        }
        tx.write(DomainWrite::Snapshot(SnapshotWrite::Put {
            exec: self.exec.clone(),
            expected,
            snapshot_ref: encode(&checkpoint)?,
            executable_identity: document.to_string(),
            format_version: KERNEL_VERSION,
        }));
        let label = if admitted.is_empty() {
            CommitLabel::CELL_SNAPSHOT
        } else {
            CommitLabel::CELL_SNAPSHOT_ADMIT
        };
        self.commit(tx, label, &mut members).await?;
        if let Some((_, _, namespaces, bodies)) = &changes {
            bodies.run_changes_committed(namespaces);
        }
        if !admitted.is_empty() {
            members.admitted(&admitted).map_err(refused)?;
        }
        let rev = SnapshotRev(expected.map_or(1, |rev| rev.0 + 1));
        *self.held_rev() = Some(Some(rev));
        Ok(Saved { rev, checkpoint })
    }

    /// The run's latest checkpoint, if any.
    ///
    /// # Errors
    ///
    /// [`QuietPointRefusal`]: a store failure, or a row that is not a
    /// kernel run's checkpoint.
    pub async fn latest_park<P: DeserializeOwned>(
        &self,
    ) -> Result<Option<(SnapshotRev, ParkedCheckpoint<P>)>, QuietPointRefusal> {
        let row = self.read().await?;
        *self.held_rev() = Some(row.as_ref().map(|row| row.rev));
        row.map(|row| {
            Ok((
                row.rev,
                serde_json::from_str(&row.snapshot_ref).map_err(refused)?,
            ))
        })
        .transpose()
    }

    /// Every wait `ledger` stands on whose outcome is committed now: read
    /// from the run records, so a restore answers the same ones again.
    ///
    /// # Errors
    ///
    /// [`QuietPointRefusal`]: a store failure, or records that do not fold.
    pub async fn settled(&self, ledger: &EffectLedger) -> Result<Vec<Settled>, QuietPointRefusal> {
        let folded = self.members.lock().await.fold().await.map_err(refused)?;
        let now = self.cx.durable_now().await.map_err(refused)?;
        Ok(settled_in(ledger, &self.exec, &folded, now))
    }

    /// Run the run's open executions until at least one wait `ledger`
    /// stands on is settled, and answer every one that is; `cancel` cancels
    /// every open execution. Answers [`Driven::Suspended`] once nothing
    /// runs and only rows or sleeps are left to wait on: the run stays
    /// parked beyond this activation.
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
        let exec = &self.exec;
        self.members
            .lock()
            .await
            .drive_by(cancel, &mut |_, folded, now| {
                let settled = settled_in(ledger, exec, folded, now);
                if settled.is_empty() {
                    Decide::Wait {
                        until: ledger.next_wake(),
                    }
                } else {
                    Decide::Answer(settled)
                }
            })
            .await
            .map_err(refused)
    }
}
