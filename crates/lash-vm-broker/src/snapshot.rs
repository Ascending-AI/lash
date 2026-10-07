//! VM snapshots and broker admission (ADR 0132 §8; S7 of I0, FIG-5194).
//!
//! A VM runs until it blocks on an operation or ends a fuel slice: a quiet
//! point. The host then commits, in one `cell.snapshot+admit` transaction,
//! the next snapshot revision, the [`BrokerLedger`] that matches it, the
//! admission and `x_start` (S4) of the operation the VM stands on, and its
//! waits. The operation's body starts only after that commit, and only
//! while the node still holds its lease; its outcome commits as
//! `round.outcome`. On restore the operation's saved outcome (or
//! `Interrupted` for a started `Once`) is fed back by [`OperationId`];
//! nothing re-dispatches and no earlier host operation re-runs.
//!
//! An operation's identity is minted at admission and stored in the
//! snapshot. It is never a journal position.
//!
//! Owned by L7 (FIG-5177); L7b (FIG-5198) takes the lashlang-process half.

use std::collections::BTreeMap;
use std::sync::Mutex;

use lash_core_execution::ActorContext;
use lash_core_execution::runtime::actor::round::{
    self, AdmittedExecution, BodyOutput, ExecutionDraft, PolicyView, Recovery, RunFold,
};
use lash_core_execution::runtime::actor::waits::{self, PinnedKey, WaitRef, WaitSpec};
use lash_core_store::effect_opener::EffectOpener;
use lash_core_store::tool_run::{
    AttemptOutcome, AvailableEvidence, KnownFailure, KnownFailureReason, MaterialDigest,
    MaterialLocation, MaterialOwner, MaterialRef, MaterialRole,
};
use lash_durable::domain::{
    AdmittedId, ExecKey, Ordinal, RunRecordWrite, RunSeq, SnapshotRev, SnapshotWrite,
};
use lash_durable::{CommitLabel, DomainWrite};
use lash_vm_protocol::{EffectOutcome, FrameEpoch};
use serde::{Deserialize, Serialize};

use crate::authority::{HandleGrant, RequestFingerprint};
use crate::effects::Performed;
use crate::ledger::{Checkpoint, QuietPointRefusal};

/// One admitted VM operation's identity: the run and ordinal of its
/// admission under its execution's owner. Minted at admission, stored in the
/// snapshot, never a journal position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationId {
    /// The run it was admitted in.
    pub run: u64,
    /// Its admission's ordinal.
    pub ordinal: u64,
}

impl OperationId {
    /// The admitted execution this operation is under `exec`.
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

/// How a pending operation was admitted, including an explicit no-execution admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum OperationAdmission {
    /// A started execution, whose run is part of its identity.
    Execution(OperationId),
    /// An operation such as a wait, performed again on restore.
    NoExecution { run: u64 },
}

/// The operation a VM stands on, with its committed admission and pinned waits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingOperation {
    /// The admission it took.
    pub admission: OperationAdmission,
    /// The request it was issued from, resolved again on restore.
    pub kind: lash_vm_protocol::EffectKind,
    /// The request's payload.
    pub request: lash_vm_protocol::EncodedPayload,
    /// What it asked: a resumed run must ask exactly this again.
    pub fingerprint: RequestFingerprint,
    /// The waits its quiet point pinned, by identity.
    pub waits: Vec<[u8; 16]>,
}

impl PendingOperation {
    /// The admission's run, stored once in its admission.
    #[must_use]
    pub fn run(&self) -> u64 {
        match self.admission {
            OperationAdmission::Execution(operation) => operation.run,
            OperationAdmission::NoExecution { run } => run,
        }
    }

    /// Its execution identity, when admitted as an execution.
    #[must_use]
    pub fn operation(&self) -> Option<OperationId> {
        match self.admission {
            OperationAdmission::Execution(operation) => Some(operation),
            OperationAdmission::NoExecution { .. } => None,
        }
    }
}

/// The broker's state that commits with the VM: the operations whose
/// outcomes a snapshot can still feed back, by identity, the next
/// admission's sequence, the frame they belong to, the handles the parent
/// granted and the operation the VM stands on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerLedger {
    /// The admitted operations still reachable, by identity, with the call
    /// each settles.
    #[serde(with = "operation_entries")]
    pub operations: BTreeMap<OperationId, lash_sansio::ToolCallId>,
    /// The sequence the next admission takes.
    pub next_admission: u64,
    /// The frame the ledger belongs to.
    pub frame_epoch: FrameEpoch,
    /// The handles the parent granted, by handle.
    pub grants: BTreeMap<String, HandleGrant>,
    /// The operation the VM stands on.
    pub pending: Option<PendingOperation>,
}

/// The operations map as a list of entries: its keys are not strings, so a
/// JSON snapshot carries it as pairs.
mod operation_entries {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::OperationId;

    pub(super) fn serialize<S: Serializer>(
        operations: &BTreeMap<OperationId, lash_sansio::ToolCallId>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        operations.iter().collect::<Vec<_>>().serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<OperationId, lash_sansio::ToolCallId>, D::Error> {
        Ok(
            Vec::<(OperationId, lash_sansio::ToolCallId)>::deserialize(deserializer)?
                .into_iter()
                .collect(),
        )
    }
}

/// What one quiet point commits: the checkpoint (VM bytes, the ledger that
/// matches them, the host's state), the admission of the operation the VM
/// stands on, the waits it pins, and the owner's own rows that ride with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuietPoint {
    /// The VM bytes, their ledger and the host's state.
    pub checkpoint: Checkpoint,
    /// The execution to admit for the pending operation, if it is one.
    pub admit: Option<ExecutionDraft>,
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
            admit: None,
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
    /// operation's minted identity.
    pub checkpoint: Checkpoint,
    /// The waits pinned with it, in order.
    pub waits: Vec<(WaitRef, Option<PinnedKey>)>,
}

/// What restoring the operation a VM stands on feeds back: its outcome as
/// the broker answers it ([`Performed`]), or as its run records hold it
/// ([`BodyOutput`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recovered<T = Performed> {
    /// Its body ended: the VM is answered with what it performed.
    Settled(T),
    /// A started `Once` without an outcome: its `Interrupted` outcome is
    /// committed, and its body is never entered again.
    Interrupted,
    /// A started `Repeatable` without an outcome, or an operation admitted
    /// as no execution: its body runs again under the same identity, over
    /// the waits its quiet point pinned.
    Rerun {
        /// The pinned waits. Their keys were handed out when they were
        /// pinned and are not minted again.
        waits: Vec<WaitRef>,
    },
}

/// Where an execution's quiet points commit.
#[async_trait::async_trait]
pub trait SnapshotStore: Send + Sync {
    /// Commit `point` in one transaction: the next snapshot revision, its
    /// ledger, the pending operation's admission and its waits.
    async fn commit_quiet_point(&self, point: QuietPoint) -> Result<Committed, QuietPointRefusal>;

    /// Record what admitted `operation`'s body performed (`round.outcome`).
    async fn settle(
        &self,
        operation: OperationId,
        performed: &Performed,
    ) -> Result<(), QuietPointRefusal>;

    /// What restoring `pending`, the operation the latest snapshot's VM
    /// stands on, feeds back. A started `Once` without an outcome is settled
    /// `Interrupted` here (`cell.inject`) before anything acts on it.
    async fn recover(&self, pending: &PendingOperation) -> Result<Recovered, QuietPointRefusal>;

    /// The execution's latest snapshot, if any.
    async fn latest(&self) -> Result<Option<(SnapshotRev, Checkpoint)>, QuietPointRefusal>;

    /// Open frame `frame` (F5): drop every earlier frame's snapshot and
    /// records, so nothing an earlier frame left is restored into the new
    /// one.
    async fn open_frame(&self, frame: FrameEpoch) -> Result<(), QuietPointRefusal>;
}

/// What the fold says to feed back for the operation `ledger`'s VM stands
/// on, by identity: its settled outcome, `Interrupted` for a started `Once`
/// (committed already, or still to be), or a rerun. `None` when the VM
/// stands on no admitted operation.
///
/// # Errors
///
/// [`QuietPointRefusal`] when the rows do not hold the admitted operation,
/// or its saved outcome does not decode.
pub fn outcomes_to_inject(
    ledger: &BrokerLedger,
    fold: &RunFold,
) -> Result<Option<(OperationId, Recovered)>, QuietPointRefusal> {
    let Some(operation) = ledger
        .pending
        .as_ref()
        .and_then(|pending| pending.operation())
    else {
        return Ok(None);
    };
    let recovered = match recovery_of(operation, fold)? {
        Recovery::Settled(AttemptOutcome::Interrupted) | Recovery::Interrupt => {
            Recovered::Interrupted
        }
        Recovery::Settled(outcome) | Recovery::Vetoed(outcome) => {
            Recovered::Settled(performed_of(fold, outcome)?)
        }
        Recovery::RerunAtOrdinal(_) => Recovered::Rerun { waits: Vec::new() },
        Recovery::RetryDue { .. } | Recovery::NotStarted => {
            return Err(QuietPointRefusal(format!(
                "the snapshot's operation {operation:?} was admitted without starting"
            )));
        }
        // A VM operation is admitted with no completion wait, so it never
        // parks.
        Recovery::Waiting(_) => {
            return Err(QuietPointRefusal(format!(
                "the snapshot's operation {operation:?} parked without a completion wait"
            )));
        }
    };
    Ok(Some((operation, recovered)))
}

/// What `operation`'s rows fold to.
fn recovery_of(operation: OperationId, fold: &RunFold) -> Result<&Recovery, QuietPointRefusal> {
    fold.recoveries()
        .iter()
        .find(|(id, _)| OperationId::of(id) == operation)
        .map(|(_, recovery)| recovery)
        .ok_or_else(|| {
            QuietPointRefusal(format!(
                "the snapshot's operation {operation:?} has no admission in its run records"
            ))
        })
}

/// The settled outcome as the VM is answered with it.
fn performed_of(fold: &RunFold, outcome: &AttemptOutcome) -> Result<Performed, QuietPointRefusal> {
    let material = match outcome {
        AttemptOutcome::Completed(material) => material,
        AttemptOutcome::Failed(failure) => &failure.output,
        AttemptOutcome::Cancelled { .. } => {
            return Ok(Performed::outcome(EffectOutcome::Cancelled));
        }
        AttemptOutcome::Interrupted
        | AttemptOutcome::TimedOut { .. }
        | AttemptOutcome::Waiting(_) => {
            return Err(QuietPointRefusal(format!(
                "a VM operation never settles as {outcome:?}"
            )));
        }
    };
    let payload = fold.material(material).ok_or_else(|| {
        QuietPointRefusal("a VM operation's outcome lost its material".to_owned())
    })?;
    decode_performed(payload)
}

/// A checkpoint as its row stores it: JSON, whose VM bytes the protocol
/// spells in base64.
fn encode_checkpoint(checkpoint: &Checkpoint) -> Result<String, QuietPointRefusal> {
    serde_json::to_string(checkpoint).map_err(refused)
}

fn decode_checkpoint(stored: &str) -> Result<Checkpoint, QuietPointRefusal> {
    serde_json::from_str(stored).map_err(refused)
}

/// What an operation performed, as its outcome's material stores it: its
/// MessagePack bytes in hexadecimal.
fn encode_performed(performed: &Performed) -> Result<String, QuietPointRefusal> {
    let bytes = rmp_serde::to_vec_named(performed).map_err(refused)?;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    Ok(text)
}

fn decode_performed(stored: &str) -> Result<Performed, QuietPointRefusal> {
    let digits = stored.as_bytes();
    if !digits.len().is_multiple_of(2) {
        return Err(QuietPointRefusal(
            "an operation's outcome is not hexadecimal".into(),
        ));
    }
    let bytes = digits
        .chunks(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(|| {
                    QuietPointRefusal("an operation's outcome is not hexadecimal".into())
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    rmp_serde::from_slice(&bytes).map_err(refused)
}

/// The durable [`SnapshotStore`] of one execution, over an actor's context:
/// rows in `lash_exec_snapshots` and `lash_run_records`, committed under the
/// actor's epoch.
#[derive(Debug)]
pub struct DurableSnapshotStore {
    cx: ActorContext,
    exec: ExecKey,
    policies: PolicyView,
    held: Mutex<Held>,
}

/// What the store holds between its commits: the revision it last read or
/// wrote, and the executions whose bodies may still settle.
#[derive(Debug, Default)]
struct Held {
    rev: Option<Option<SnapshotRev>>,
    admitted: BTreeMap<OperationId, AdmittedExecution>,
}

impl DurableSnapshotStore {
    /// The snapshot store of `exec`, owned by `cx`'s actor.
    #[must_use]
    pub fn new(cx: &ActorContext, exec: ExecKey) -> Self {
        Self {
            cx: cx.clone(),
            exec,
            policies: PolicyView::default(),
            held: Mutex::new(Held::default()),
        }
    }

    /// This store folding a restored operation under the policies `current`
    /// declares: a current `Once` vetoes a stored `Repeatable` rerun.
    #[must_use]
    pub fn with_policies(mut self, current: PolicyView) -> Self {
        self.policies = current;
        self
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

    fn held(&self) -> std::sync::MutexGuard<'_, Held> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The revision a write replaces: the one last read or written, or the
    /// stored one.
    async fn revision(&self) -> Result<Option<SnapshotRev>, QuietPointRefusal> {
        if let Some(rev) = self.held().rev {
            return Ok(rev);
        }
        let rev = self.read().await?.map(|row| row.rev);
        self.held().rev = Some(rev);
        Ok(rev)
    }

    async fn read(&self) -> Result<Option<lash_durable::domain::SnapshotRow>, QuietPointRefusal> {
        self.cx
            .durable_reads()
            .map_err(refused)?
            .snapshot(&self.exec)
            .await
            .map_err(refused)
    }

    /// Commit `tx` under `label`. A refused or unacknowledged commit forgets
    /// the revision and the held executions: the next write reads them back.
    async fn commit(
        &self,
        tx: lash_durable::ActorTx,
        label: CommitLabel,
    ) -> Result<(), QuietPointRefusal> {
        match self.cx.commit(tx, label).await {
            Ok(_) => Ok(()),
            Err(error) => {
                let mut held = self.held();
                held.rev = None;
                held.admitted.clear();
                Err(refused(error))
            }
        }
    }

    /// Refuse to hand an execution to its body once the node's lease
    /// lapsed. An admission's commit can be acknowledged after the node
    /// paused past its self-stop deadline: by then it may be reaped and the
    /// actor's new owner may have restored from this snapshot and settled
    /// the started `Once` `Interrupted`. The body never runs on this owner;
    /// the store forgets what it held, and the run stops.
    fn lease_held(&self) -> Result<(), QuietPointRefusal> {
        if self.cx.lease_held() {
            return Ok(());
        }
        let mut held = self.held();
        held.rev = None;
        held.admitted.clear();
        Err(QuietPointRefusal(format!(
            "the node's lease lapsed before {:?}'s admitted body could start",
            self.exec
        )))
    }

    /// The owner's run records, folded.
    async fn fold(&self) -> Result<RunFold, QuietPointRefusal> {
        let rows = self
            .cx
            .durable_reads()
            .map_err(refused)?
            .run_records(&self.exec.owner())
            .await
            .map_err(refused)?;
        round::fold(&rows, &self.policies).map_err(|error| QuietPointRefusal(error.to_string()))
    }

    /// The material a VM operation's outcome is stored under: its run's.
    fn material(&self, payload: &str) -> Result<MaterialRef, QuietPointRefusal> {
        let opener = match &self.exec {
            ExecKey::Cell(session, turn, _) => EffectOpener::turn(session.clone(), turn.clone()),
            ExecKey::Process(process) => EffectOpener::process(process.clone()),
        };
        Ok(MaterialRef {
            owner: MaterialOwner::Run { opener },
            role: MaterialRole::AttemptOutput,
            location: MaterialLocation::JournalLocal,
            digest: MaterialDigest::parse(blake3::hash(payload.as_bytes()).to_hex().as_str())
                .map_err(|error| QuietPointRefusal(error.to_string()))?,
        })
    }
    /// The execution admitted, or restored to run again, as `operation`:
    /// what a host that runs the body itself runs it under.
    #[must_use]
    pub fn admitted(&self, operation: OperationId) -> Option<AdmittedExecution> {
        self.held().admitted.get(&operation).cloned()
    }

    /// Record `output`, what admitted `operation`'s body produced, as its
    /// outcome (`round.outcome`).
    ///
    /// # Errors
    ///
    /// [`QuietPointRefusal`]: an operation this store did not admit or
    /// restore, or the store's refusal.
    pub async fn settle_output(
        &self,
        operation: OperationId,
        output: BodyOutput,
    ) -> Result<(), QuietPointRefusal> {
        let execution = self.admitted(operation).ok_or_else(|| {
            QuietPointRefusal(format!(
                "operation {operation:?} is not admitted on this store"
            ))
        })?;
        let mut tx = self.cx.begin().await.map_err(refused)?;
        round::settle(&mut tx, &execution, output, None).map_err(refused)?;
        self.commit(tx, CommitLabel::ROUND_OUTCOME).await?;
        self.held().admitted.remove(&operation);
        Ok(())
    }

    /// What restoring `pending` feeds back, as its run records hold it. A
    /// started `Once` without an outcome is settled `Interrupted` here
    /// (`cell.inject`); a started `Repeatable` is held to run again under its
    /// identity ([`Self::admitted`]).
    ///
    /// # Errors
    ///
    /// [`QuietPointRefusal`]: rows that do not hold the operation, or the
    /// store's refusal.
    pub async fn recover_output(
        &self,
        pending: &PendingOperation,
    ) -> Result<Recovered<BodyOutput>, QuietPointRefusal> {
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
        let Some(operation) = pending.operation() else {
            return Ok(Recovered::Rerun { waits });
        };
        let fold = self.fold().await?;
        let id = operation.admitted(&self.exec);
        match recovery_of(operation, &fold)?.clone() {
            Recovery::Interrupt => {
                let mut tx = self.cx.begin().await.map_err(refused)?;
                round::settle_interrupted(&mut tx, &fold, &id).map_err(refused)?;
                self.commit(tx, CommitLabel::CELL_INJECT).await?;
                Ok(Recovered::Interrupted)
            }
            Recovery::Settled(AttemptOutcome::Interrupted) => Ok(Recovered::Interrupted),
            Recovery::Settled(outcome) | Recovery::Vetoed(outcome) => {
                let material = round::outcome_material(&outcome)
                    .and_then(|material| fold.material(material))
                    .map(str::to_owned);
                Ok(Recovered::Settled(BodyOutput { outcome, material }))
            }
            Recovery::RerunAtOrdinal(_) => {
                let execution = fold.admitted(&id).ok_or_else(|| {
                    QuietPointRefusal(format!("operation {operation:?} has no started execution"))
                })?;
                self.lease_held()?;
                self.held().admitted.insert(operation, execution);
                Ok(Recovered::Rerun { waits })
            }
            Recovery::RetryDue { .. } | Recovery::NotStarted => Err(QuietPointRefusal(format!(
                "the snapshot's operation {operation:?} was admitted without starting"
            ))),
            Recovery::Waiting(_) => Err(QuietPointRefusal(format!(
                "the snapshot's operation {operation:?} parked without a completion wait"
            ))),
        }
    }
}

fn refused(error: impl std::fmt::Display) -> QuietPointRefusal {
    QuietPointRefusal(error.to_string())
}

#[async_trait::async_trait]
impl SnapshotStore for DurableSnapshotStore {
    async fn commit_quiet_point(&self, point: QuietPoint) -> Result<Committed, QuietPointRefusal> {
        let QuietPoint {
            mut checkpoint,
            admit,
            waits: specs,
            with,
        } = point;
        let expected = self.revision().await?;
        let mut tx = self.cx.begin().await.map_err(refused)?;
        for write in with {
            tx.write(write);
        }
        let mut admitted = None;
        if let Some(draft) = admit {
            let pending = checkpoint.ledger.pending.as_mut().ok_or_else(|| {
                QuietPointRefusal("an admission needs the operation the VM stands on".to_owned())
            })?;
            let execution = round::admit(
                &mut tx,
                &self.exec.owner(),
                RunSeq(pending.run()),
                vec![draft],
            )
            .map_err(refused)?
            .pop()
            .ok_or_else(|| QuietPointRefusal("the admission admitted nothing".to_owned()))?;
            let operation = OperationId::of(execution.id());
            pending.admission = OperationAdmission::Execution(operation);
            checkpoint
                .ledger
                .operations
                .insert(operation, execution.call().clone());
            admitted = Some((operation, execution));
        }
        let mut pinned = Vec::with_capacity(specs.len());
        for spec in specs {
            let (wait, key) = waits::pin(&mut tx, spec).map_err(refused)?;
            if let Some(pending) = checkpoint.ledger.pending.as_mut() {
                pending.waits.push(wait.id().0);
            }
            pinned.push((wait, key));
        }
        // Records no snapshot can feed back again go: every run before the
        // oldest one the ledger still reaches.
        let ledger = &checkpoint.ledger;
        let oldest = ledger
            .operations
            .keys()
            .map(|operation| operation.run)
            .chain(ledger.pending.as_ref().map(|pending| pending.run()))
            .min()
            .unwrap_or(ledger.next_admission);
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
        let label = if admitted.is_some() {
            CommitLabel::CELL_SNAPSHOT_ADMIT
        } else {
            CommitLabel::CELL_SNAPSHOT
        };
        self.commit(tx, label).await?;
        if admitted.is_some() {
            self.lease_held()?;
        }
        let rev = SnapshotRev(expected.map_or(1, |rev| rev.0 + 1));
        let mut held = self.held();
        held.rev = Some(Some(rev));
        if let Some((operation, execution)) = admitted {
            held.admitted.insert(operation, execution);
        }
        Ok(Committed {
            rev,
            checkpoint,
            waits: pinned,
        })
    }

    async fn settle(
        &self,
        operation: OperationId,
        performed: &Performed,
    ) -> Result<(), QuietPointRefusal> {
        let payload = encode_performed(performed)?;
        let material = self.material(&payload)?;
        let outcome = match &performed.outcome {
            EffectOutcome::Failed(_) => AttemptOutcome::Failed(KnownFailure {
                output: material,
                reason: KnownFailureReason::Reported,
                suggested_delay_ms: None,
            }),
            EffectOutcome::Cancelled => AttemptOutcome::Cancelled {
                evidence: AvailableEvidence::default(),
            },
            EffectOutcome::Value(_)
            | EffectOutcome::Unit
            | EffectOutcome::HandedOver
            | EffectOutcome::Checkpoint { .. } => AttemptOutcome::Completed(material),
        };
        self.settle_output(
            operation,
            BodyOutput {
                outcome,
                material: Some(payload),
            },
        )
        .await
    }

    async fn recover(&self, pending: &PendingOperation) -> Result<Recovered, QuietPointRefusal> {
        Ok(match self.recover_output(pending).await? {
            Recovered::Settled(BodyOutput {
                outcome: AttemptOutcome::Cancelled { .. },
                ..
            }) => Recovered::Settled(Performed::outcome(EffectOutcome::Cancelled)),
            Recovered::Settled(output) => {
                let payload = output.material.as_deref().ok_or_else(|| {
                    QuietPointRefusal("a VM operation's outcome lost its material".to_owned())
                })?;
                Recovered::Settled(decode_performed(payload)?)
            }
            Recovered::Interrupted => Recovered::Interrupted,
            Recovered::Rerun { waits } => Recovered::Rerun { waits },
        })
    }

    async fn latest(&self) -> Result<Option<(SnapshotRev, Checkpoint)>, QuietPointRefusal> {
        let row = self.read().await?;
        self.held().rev = Some(row.as_ref().map(|row| row.rev));
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
        let mut tx = self.cx.begin().await.map_err(refused)?;
        tx.write(DomainWrite::Snapshot(SnapshotWrite::Delete {
            exec: self.exec.clone(),
        }));
        tx.write(DomainWrite::RunRecord(RunRecordWrite::Prune {
            owner: self.exec.owner(),
            before: RunSeq(checkpoint.ledger.next_admission),
        }));
        self.commit(tx, CommitLabel::CELL_SNAPSHOT).await?;
        let mut held = self.held();
        held.rev = Some(None);
        held.admitted.clear();
        Ok(())
    }
}
