//! VM snapshots and broker admission (ADR 0132 §8; S7 of I0, FIG-5194).
//!
//! A VM runs until it blocks on an operation or ends a fuel slice: a quiet
//! point. The host then commits, in one `cell.snapshot+admit` transaction,
//! the next snapshot revision, the [`BrokerLedger`] that matches it, the
//! admission and `x_start` (S4) of every member execution of the operation
//! the VM stands on (each tool call it makes, under its own declared
//! policy, limit and completion wait), and its waits. No member's body
//! starts before that commit; each runs through the admitted-execution
//! lifecycle (ADR 0132 §5), and its outcome commits as `round.outcome`.
//!
//! On restore the operation is performed again over the same pinned waits:
//! its host answers from its members' committed outcomes, a started `Once`
//! member without one records `Interrupted`, and a `Repeatable` one reruns
//! at its ordinal. No settled body is entered again, and no earlier
//! operation re-runs.
//!
//! A member's identity is minted at admission and stored in the snapshot.
//! It is never a journal position. A member the operation's answer did not
//! wait for (a race's loser) stays in the ledger until it settles on its
//! own, and the execution's end settles every member still open, so no
//! member outlives the snapshot that records it.
//!
//! A quiet point deletes the records no snapshot can reach again. A
//! settled member's plugin-state resolutions ride its `x_outcome`, and its
//! lifecycle publishes them from that committed record (ADR 0132 §5); once
//! a quiet point prunes the record, its ledger carries them
//! ([`BrokerLedger::state`]), so the snapshot that drops the record holds
//! what it changed, and a restore publishes them again.
//!
//! Owned by L7 (FIG-5177); L7b (FIG-5198) takes the lash-vm-process half.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use lash_core_execution::ActorContext;
use lash_core_execution::runtime::actor::round::lifecycle::MemberBodies;
use lash_core_execution::runtime::actor::round::{
    self, AdmittedExecution, PolicyView, RoundDraft, RoundError,
};
use lash_core_execution::runtime::actor::waits::{self, PinnedKey, WaitRef, WaitSpec};
use lash_durable::domain::{
    AdmittedId, ExecKey, Ordinal, RunRecordWrite, RunSeq, SnapshotRev, SnapshotWrite,
};
use lash_durable::{CommitLabel, DomainWrite};
use lash_vm_protocol::{EncodedPayload, FrameEpoch};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::authority::{HandleGrant, RequestFingerprint};
use crate::effects::MemberDraft;
use crate::ledger::{Checkpoint, QuietPointRefusal};
use crate::members::{Decide, Driven, MemberEnd, Members};

/// One admitted member's identity: the run of its operation's admission
/// and its ordinal there, under its execution's owner. Minted at admission,
/// stored in the snapshot, never a journal position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationId {
    /// The run it was admitted in.
    pub run: u64,
    /// Its admission's ordinal.
    pub ordinal: u64,
}

impl OperationId {
    /// The admitted execution this member is under `exec`.
    #[must_use]
    pub fn admitted(&self, exec: &ExecKey) -> AdmittedId {
        AdmittedId {
            owner: exec.owner(),
            run: RunSeq(self.run),
            ordinal: Ordinal(self.ordinal),
        }
    }

    /// The identity of an admitted execution.
    #[must_use]
    pub fn of(admitted: &AdmittedId) -> Self {
        Self {
            run: admitted.run.0,
            ordinal: admitted.ordinal.0,
        }
    }
}

/// The operation a VM stands on, with its committed admission and pinned
/// waits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingOperation {
    /// The admission it took: its calls' identities derive from it.
    pub run: u64,
    /// Its member executions, minted when its admission committed; none
    /// for an operation that only waits on rows, which a restore performs
    /// again.
    pub members: Vec<OperationId>,
    /// The request it was issued from, resolved again on restore.
    pub kind: lash_vm_protocol::EffectKind,
    /// The request's payload.
    pub request: lash_vm_protocol::EncodedPayload,
    /// What it asked: a resumed run must ask exactly this again.
    pub fingerprint: RequestFingerprint,
    /// The waits its quiet point pinned, by identity.
    pub waits: Vec<[u8; 16]>,
}

/// An admitted member the ledger keeps until it settles: its call, and the
/// request its body is built from again on any owner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenMember {
    /// Its call.
    pub call: lash_sansio::ToolCallId,
    /// Its request, as its host encoded it.
    pub request: EncodedPayload,
}

/// The broker's state that commits with the VM: the members still open, by
/// identity, the next admission's sequence, the frame they belong to, the
/// handles the parent granted and the operation the VM stands on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerLedger {
    /// The admitted members not known to have settled, by identity.
    #[serde(with = "operation_entries")]
    pub operations: BTreeMap<OperationId, OpenMember>,
    /// The sequence the next admission takes.
    pub next_admission: u64,
    /// The frame the ledger belongs to.
    pub frame_epoch: FrameEpoch,
    /// The handles the parent granted, by handle.
    pub grants: BTreeMap<String, HandleGrant>,
    /// The operation the VM stands on.
    pub pending: Option<PendingOperation>,
}

/// The members map as a list of entries: its keys are not strings, so a
/// JSON snapshot carries it as pairs.
mod operation_entries {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::{OpenMember, OperationId};

    pub(super) fn serialize<S: Serializer>(
        operations: &BTreeMap<OperationId, OpenMember>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        operations.iter().collect::<Vec<_>>().serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<OperationId, OpenMember>, D::Error> {
        Ok(Vec::<(OperationId, OpenMember)>::deserialize(deserializer)?
            .into_iter()
            .collect())
    }
}

/// What one quiet point commits: the checkpoint (VM bytes, the ledger that
/// matches them, the host's state), the admission of the members of the
/// operation the VM stands on, the waits it pins, and the owner's own rows
/// that ride with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuietPoint {
    /// The VM bytes, their ledger and the host's state.
    pub checkpoint: Checkpoint,
    /// The member executions to admit for the pending operation.
    pub members: Vec<MemberDraft>,
    /// The waits to pin with it.
    pub waits: Vec<WaitSpec>,
    /// The owner's rows that commit with it: a turn's checkpoint advance that
    /// names the cell, so the turn never restores to a point before a
    /// snapshot that exists.
    pub with: Vec<DomainWrite>,
}

impl QuietPoint {
    /// A quiet point that commits `checkpoint` alone.
    #[must_use]
    pub fn bare(checkpoint: Checkpoint) -> Self {
        Self {
            checkpoint,
            members: Vec::new(),
            waits: Vec::new(),
            with: Vec::new(),
        }
    }
}

/// A committed quiet point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Committed {
    /// The new revision.
    pub rev: SnapshotRev,
    /// The checkpoint as it was stored: its ledger names the pending
    /// operation's minted members.
    pub checkpoint: Checkpoint,
    /// The waits pinned with it, in order.
    pub waits: Vec<(WaitRef, Option<PinnedKey>)>,
}

/// Where an execution's quiet points commit.
#[async_trait::async_trait]
pub trait SnapshotStore: Send + Sync {
    /// Commit `point` in one transaction: the next snapshot revision, its
    /// ledger, the pending operation's member admissions and its waits.
    async fn commit_quiet_point(&self, point: QuietPoint) -> Result<Committed, QuietPointRefusal>;

    /// The waits `pending`, the operation the latest snapshot's VM stands
    /// on, pinned with its quiet point: a restore performs it again over
    /// these same rows.
    async fn recover(&self, pending: &PendingOperation) -> Result<Vec<WaitRef>, QuietPointRefusal>;

    /// The execution's latest snapshot, if any.
    async fn latest(&self) -> Result<Option<(SnapshotRev, Checkpoint)>, QuietPointRefusal>;

    /// Open frame `frame` (F5): drop every earlier frame's snapshot and
    /// records, so nothing an earlier frame left is restored into the new
    /// one.
    async fn open_frame(&self, frame: FrameEpoch) -> Result<(), QuietPointRefusal>;
}

/// A checkpoint as its row stores it: JSON, whose VM bytes the protocol
/// spells in base64.
fn encode_checkpoint(checkpoint: &Checkpoint) -> Result<String, QuietPointRefusal> {
    serde_json::to_string(checkpoint).map_err(refused)
}

fn decode_checkpoint(stored: &str) -> Result<Checkpoint, QuietPointRefusal> {
    serde_json::from_str(stored).map_err(refused)
}

/// The durable [`SnapshotStore`] of one execution, over an actor's context:
/// rows in `lash_exec_snapshots` and `lash_run_records`, committed under the
/// actor's epoch. It holds the execution's admitted members on this
/// activation and runs their bodies from the host's [`MemberBodies`].
pub struct DurableSnapshotStore {
    pub(crate) cx: ActorContext,
    pub(crate) exec: ExecKey,
    /// The revision it last read or wrote.
    rev: Mutex<Option<Option<SnapshotRev>>>,
    pub(crate) members: tokio::sync::Mutex<Members>,
}

impl std::fmt::Debug for DurableSnapshotStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableSnapshotStore")
            .field("exec", &self.exec)
            .finish_non_exhaustive()
    }
}

impl DurableSnapshotStore {
    /// The snapshot store of `exec`, owned by `cx`'s actor.
    #[must_use]
    pub fn new(cx: &ActorContext, exec: ExecKey) -> Self {
        Self {
            members: tokio::sync::Mutex::new(Members::new(cx, exec.owner())),
            cx: cx.clone(),
            exec,
            rev: Mutex::new(None),
        }
    }

    /// Run its members' bodies from `bodies`, vetoing a stored `Repeatable`
    /// rerun against the policies `current` declares. Bound before the
    /// execution runs.
    pub async fn bind_members(&self, bodies: Arc<dyn MemberBodies>, current: PolicyView) {
        self.members.lock().await.with_bodies(bodies, current);
    }

    /// The context it commits through.
    #[must_use]
    pub fn context(&self) -> &ActorContext {
        &self.cx
    }

    /// The execution.
    #[must_use]
    pub fn exec(&self) -> &ExecKey {
        &self.exec
    }

    pub(crate) fn held_rev(&self) -> std::sync::MutexGuard<'_, Option<Option<SnapshotRev>>> {
        self.rev
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The revision a write replaces: the one last read or written, or the
    /// stored one.
    pub(crate) async fn revision(&self) -> Result<Option<SnapshotRev>, QuietPointRefusal> {
        if let Some(rev) = *self.held_rev() {
            return Ok(rev);
        }
        let rev = self.read().await?.map(|row| row.rev);
        *self.held_rev() = Some(rev);
        Ok(rev)
    }

    pub(crate) async fn read(
        &self,
    ) -> Result<Option<lash_durable::domain::SnapshotRow>, QuietPointRefusal> {
        self.cx
            .durable_reads()
            .map_err(refused)?
            .snapshot(&self.exec)
            .await
            .map_err(refused)
    }

    /// Commit `tx` under `label`. A refused or unacknowledged commit forgets
    /// the revision and the records the members read: the next write reads
    /// them back.
    pub(crate) async fn commit(
        &self,
        tx: lash_durable::ActorTx,
        label: CommitLabel,
        members: &mut Members,
    ) -> Result<(), QuietPointRefusal> {
        members.forget();
        match self.cx.commit(tx, label).await {
            Ok(_) => Ok(()),
            Err(error) => {
                *self.held_rev() = None;
                Err(refused(error))
            }
        }
    }

    /// Run the members of `run`, the operation the VM stands on, until
    /// `decide` answers it from their committed outcomes; `cancel` (the
    /// turn's cancel) cancels every open member. Answers
    /// [`Driven::Suspended`] once nothing runs and only rows are left to
    /// wait on: the operation stays open beyond this activation.
    ///
    /// # Errors
    ///
    /// [`QuietPointRefusal`]: ownership lost or another store failure, the
    /// activation stopping, or an operation nothing can answer.
    pub async fn drive<T>(
        &self,
        run: u64,
        cancel: &CancellationToken,
        decide: &mut (dyn FnMut(&[MemberEnd], lash_durable::DurableInstant) -> Decide<T> + Send),
    ) -> Result<Driven<T>, QuietPointRefusal> {
        self.members
            .lock()
            .await
            .drive(RunSeq(run), cancel, decide)
            .await
            .map_err(refused)
    }
}

/// The oldest run a snapshot of `ledger` can reach: that of an open member
/// or of the pending operation, or the next admission's.
fn reachable_from(ledger: &BrokerLedger) -> u64 {
    ledger
        .operations
        .keys()
        .map(|operation| operation.run)
        .chain(ledger.pending.as_ref().map(|pending| pending.run))
        .min()
        .unwrap_or(ledger.next_admission)
}

pub(crate) fn refused(error: impl std::fmt::Display) -> QuietPointRefusal {
    QuietPointRefusal(error.to_string())
}

fn round_refused(error: RoundError) -> QuietPointRefusal {
    refused(error)
}

#[async_trait::async_trait]
impl SnapshotStore for DurableSnapshotStore {
    async fn commit_quiet_point(&self, point: QuietPoint) -> Result<Committed, QuietPointRefusal> {
        let QuietPoint {
            mut checkpoint,
            members: drafts,
            waits: specs,
            with,
        } = point;
        let mut members = self.members.lock().await;
        // An execution's end records no open member: each settles first.
        if checkpoint.end.is_some() && !checkpoint.ledger.operations.is_empty() {
            members.close().await.map_err(round_refused)?;
        }
        // A member whose outcome committed is no longer open.
        if !checkpoint.ledger.operations.is_empty() {
            let folded = members.fold().await.map_err(round_refused)?;
            checkpoint.ledger.operations.retain(|operation, _| {
                folded
                    .recovery(&operation.admitted(&self.exec))
                    .is_none_or(|recovery| !matches!(recovery, round::Recovery::Settled(_)))
            });
        }
        // Records no snapshot can reach again go: every run before the
        // oldest one an open member or the pending operation is in. What
        // their settled members changed, published once their outcomes
        // committed, commits with the prune as the turn's run namespaces
        // (FIG-5301): a pruned outcome is never the only copy of a value.
        let oldest = reachable_from(&checkpoint.ledger);
        let changes = match (&self.exec, members.bodies()) {
            (ExecKey::Cell(session, run, _), Some(bodies)) if oldest > 0 => {
                Some((session.clone(), run.clone(), bodies.run_changes(), bodies))
            }
            _ => None,
        };
        let expected = self.revision().await?;
        let mut tx = self.cx.begin().await.map_err(refused)?;
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
        let mut admitted: Vec<AdmittedExecution> = Vec::new();
        if !drafts.is_empty() {
            let pending = checkpoint.ledger.pending.as_mut().ok_or_else(|| {
                QuietPointRefusal("an admission needs the operation the VM stands on".to_owned())
            })?;
            let requests: Vec<EncodedPayload> =
                drafts.iter().map(|draft| draft.request.clone()).collect();
            admitted = round::admit_round(
                &mut tx,
                &waits::wait_scope(&self.cx).map_err(refused)?,
                RoundDraft {
                    owner: self.exec.owner(),
                    run: RunSeq(pending.run),
                    members: drafts.into_iter().map(|draft| draft.draft).collect(),
                },
            )
            .map_err(refused)?
            .members()
            .to_vec();
            for (execution, request) in admitted.iter().zip(requests) {
                let operation = OperationId::of(execution.id());
                pending.members.push(operation);
                checkpoint.ledger.operations.insert(
                    operation,
                    OpenMember {
                        call: execution.call().clone(),
                        request,
                    },
                );
            }
        }
        let mut pinned = Vec::with_capacity(specs.len());
        for spec in specs {
            let (wait, key) = waits::pin(&mut tx, spec);
            if let Some(pending) = checkpoint.ledger.pending.as_mut() {
                pending.waits.push(wait.id().0);
            }
            pinned.push((wait, key));
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
            snapshot_ref: encode_checkpoint(&checkpoint)?,
            executable_identity: format!(
                "{}:{:?}",
                checkpoint.vm.owner(),
                checkpoint.vm.vm_contract()
            ),
            format_version: checkpoint.vm.format_version(),
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
            members.admitted(&admitted).map_err(round_refused)?;
        }
        let rev = SnapshotRev(expected.map_or(1, |rev| rev.0 + 1));
        *self.held_rev() = Some(Some(rev));
        Ok(Committed {
            rev,
            checkpoint,
            waits: pinned,
        })
    }

    async fn recover(&self, pending: &PendingOperation) -> Result<Vec<WaitRef>, QuietPointRefusal> {
        let mut waits = Vec::with_capacity(pending.waits.len());
        for id in &pending.waits {
            let id = lash_durable::domain::WaitId(*id);
            let row = self
                .cx
                .durable_reads()
                .map_err(refused)?
                .wait(&id)
                .await
                .map_err(refused)?
                .ok_or_else(|| QuietPointRefusal(format!("pinned wait {id:?} is gone")))?;
            waits.push(WaitRef::new(id, row.purpose.kind()));
        }
        Ok(waits)
    }

    async fn latest(&self) -> Result<Option<(SnapshotRev, Checkpoint)>, QuietPointRefusal> {
        let row = self.read().await?;
        *self.held_rev() = Some(row.as_ref().map(|row| row.rev));
        row.map(|row| Ok((row.rev, decode_checkpoint(&row.snapshot_ref)?)))
            .transpose()
    }

    async fn open_frame(&self, frame: FrameEpoch) -> Result<(), QuietPointRefusal> {
        let Some((_, checkpoint)) = self.latest().await? else {
            return Ok(());
        };
        if checkpoint.frame_epoch() >= frame {
            return Ok(());
        }
        let mut members = self.members.lock().await;
        members.reset();
        let mut tx = self.cx.begin().await.map_err(refused)?;
        tx.write(DomainWrite::Snapshot(SnapshotWrite::Delete {
            exec: self.exec.clone(),
        }));
        tx.write(DomainWrite::RunRecord(RunRecordWrite::Prune {
            owner: self.exec.owner(),
            before: RunSeq(checkpoint.ledger.next_admission),
        }));
        self.commit(tx, CommitLabel::CELL_SNAPSHOT, &mut members)
            .await?;
        *self.held_rev() = Some(None);
        Ok(())
    }
}
