//! VM snapshots and broker admission (ADR 0132 §8; S7 of I0, FIG-5194).
//!
//! A VM runs until it blocks on an await or ends a fuel slice: a quiet
//! point. The host then commits, in one `cell.snapshot+admit` transaction,
//! the next snapshot revision, the [`BrokerLedger`] that matches it, the
//! admission and `x_start` (S4) of every operation the VM issued since the
//! last snapshot, and any new waits. Bodies start only after that commit.
//! On restore each admitted operation's saved outcome (or `Interrupted` for a
//! started `Once`) is fed back by [`OperationId`]; nothing re-dispatches and
//! no earlier host operation re-runs.
//!
//! An operation's identity is minted at admission and stored in the
//! snapshot. It is never a journal position.
//!
//! Owned by V0 (FIG-5170), then L7 (FIG-5177); L7b (FIG-5198) takes the
//! lashlang-process half.

use std::collections::BTreeMap;

use lash_core_execution::ActorContext;
use lash_core_execution::runtime::actor::round::{
    self, AdmittedExecution, ExecutionDraft, Recovery, RunFold,
};
use lash_core_execution::runtime::actor::waits::WaitSpec;
use lash_core_store::tool_run::AttemptOutcome;
use lash_durable::domain::{
    AdmittedId, DomainRefusal, ExecKey, Ordinal, RunSeq, SnapshotRev, SnapshotWrite,
};
use lash_durable::{CommitLabel, DomainWrite, DurableError};
use lash_vm_protocol::FrameEpoch;
use serde::{Deserialize, Serialize};

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
}

/// The broker's state that commits with the VM: the operations it admitted,
/// by identity, the next admission's sequence and the frame they belong to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerLedger {
    /// The admitted operations, by identity, with the call each settles.
    #[serde(with = "operation_entries")]
    pub operations: BTreeMap<OperationId, lash_sansio::ToolCallId>,
    /// The sequence the next admission takes.
    pub next_admission: u64,
    /// The frame the ledger belongs to.
    pub frame_epoch: FrameEpoch,
}

/// The operations map as a list of entries: an identity is a pair, and JSON
/// keys are strings.
mod operation_entries {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::OperationId;

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Entry {
        operation: OperationId,
        call: lash_sansio::ToolCallId,
    }

    pub(super) fn serialize<S: Serializer>(
        operations: &BTreeMap<OperationId, lash_sansio::ToolCallId>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(operations.iter().map(|(operation, call)| Entry {
            operation: *operation,
            call: call.clone(),
        }))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<OperationId, lash_sansio::ToolCallId>, D::Error> {
        let entries = Vec::<Entry>::deserialize(deserializer)?;
        Ok(entries
            .into_iter()
            .map(|entry| (entry.operation, entry.call))
            .collect())
    }
}

/// One operation the VM issued since the last snapshot, admitted with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuedOperation {
    /// Its identity.
    pub operation: OperationId,
    /// What to admit.
    pub draft: ExecutionDraft,
}

/// What one quiet point commits: the checkpoint (VM bytes and the ledger
/// that matches them), the operations issued since the last snapshot, and
/// the waits they pinned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuietPoint {
    /// The VM bytes and their ledger.
    pub checkpoint: Checkpoint,
    /// The broker's admitted operations, the issued ones included.
    pub broker: BrokerLedger,
    /// The executable the VM state resumes.
    pub executable_identity: String,
    /// The operations to admit with it.
    pub issued: Vec<IssuedOperation>,
    /// The waits to pin with it.
    pub waits: Vec<WaitSpec>,
    /// The owner's own rows that commit with it: the turn's checkpoint
    /// advance that names the cell, so the turn never restores to a point
    /// before a snapshot that exists.
    pub with: Vec<DomainWrite>,
}

impl QuietPoint {
    /// A quiet point that commits `checkpoint` alone: no operation, no wait,
    /// no owner row, and an empty broker ledger in the checkpoint's frame.
    #[must_use]
    pub fn bare(checkpoint: Checkpoint) -> Self {
        Self {
            broker: BrokerLedger {
                operations: BTreeMap::new(),
                next_admission: 0,
                frame_epoch: checkpoint.frame_epoch,
            },
            checkpoint,
            executable_identity: String::new(),
            issued: Vec::new(),
            waits: Vec::new(),
            with: Vec::new(),
        }
    }
}

/// What an execution's latest snapshot holds: a VM parked at a quiet point
/// with the broker's ledger, or the end the execution reached.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StoredSnapshot {
    /// Parked at a quiet point.
    Parked {
        /// The VM bytes and their ledger.
        checkpoint: Checkpoint,
        /// The broker's admitted operations.
        broker: BrokerLedger,
    },
    /// Ended: the execution's result, encoded by its runner.
    Ended {
        /// The result.
        result: serde_json::Value,
        /// The broker's admitted operations.
        broker: BrokerLedger,
    },
}

impl StoredSnapshot {
    /// The broker ledger either form carries.
    #[must_use]
    pub fn broker(&self) -> &BrokerLedger {
        match self {
            Self::Parked { broker, .. } | Self::Ended { broker, .. } => broker,
        }
    }
}

/// An outcome restored into the VM for one admitted operation: its saved
/// outcome, or `Interrupted` for a started `Once`.
pub type InjectedOutcome = AttemptOutcome;

/// Where an execution's quiet points commit.
#[async_trait::async_trait]
pub trait SnapshotStore: Send + Sync {
    /// Commit `point` in one transaction: the next snapshot revision, its
    /// ledger, the admissions and waits. Answers the new revision.
    async fn commit_quiet_point(&self, point: QuietPoint)
    -> Result<SnapshotRev, QuietPointRefusal>;

    /// The execution's latest snapshot, if any.
    async fn latest(&self) -> Result<Option<(SnapshotRev, Checkpoint)>, QuietPointRefusal>;

    /// Open frame `frame` (F5): drop every earlier frame's snapshot, so
    /// nothing an earlier frame left is restored into the new one.
    async fn open_frame(&self, frame: FrameEpoch) -> Result<(), QuietPointRefusal>;
}

/// The outcomes to feed back into a restored VM, by operation: each
/// admitted operation's saved outcome under `fold`, or `Interrupted` for a
/// started `Once`. Nothing re-dispatches.
///
/// An operation whose fold says it runs again at its ordinal (a started
/// `Repeatable`), or that was never started, has no outcome to inject and is
/// left out: its caller runs it under its admitted identity.
#[must_use]
pub fn outcomes_to_inject(
    ledger: &BrokerLedger,
    fold: &RunFold,
) -> Vec<(OperationId, InjectedOutcome)> {
    ledger
        .operations
        .keys()
        .filter_map(|operation| {
            let recovery = fold.recoveries().iter().find_map(|(id, recovery)| {
                (id.run.0 == operation.run && id.ordinal.0 == operation.ordinal).then_some(recovery)
            })?;
            match recovery {
                Recovery::Settled(outcome) => Some((*operation, outcome.clone())),
                Recovery::Interrupt => Some((*operation, AttemptOutcome::Interrupted)),
                Recovery::RerunAtOrdinal(_) | Recovery::RetryDue { .. } | Recovery::NotStarted => {
                    None
                }
            }
        })
        .collect()
}

fn refused(error: impl std::fmt::Display) -> QuietPointRefusal {
    QuietPointRefusal(error.to_string())
}

/// The durable [`SnapshotStore`] of one execution, over an actor's context:
/// rows in `lash_exec_snapshots` and `lash_run_records`, committed under the
/// actor's epoch.
#[derive(Clone, Debug)]
pub struct DurableSnapshotStore {
    cx: ActorContext,
    exec: ExecKey,
}

impl DurableSnapshotStore {
    /// The snapshot store of `exec`, owned by `cx`'s actor.
    #[must_use]
    pub fn new(cx: &ActorContext, exec: ExecKey) -> Self {
        Self {
            cx: cx.clone(),
            exec,
        }
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
}

impl DurableSnapshotStore {
    /// The execution's latest stored snapshot, if any.
    ///
    /// # Errors
    ///
    /// The store's refusal, or a stored snapshot that does not decode.
    pub async fn stored(&self) -> Result<Option<(SnapshotRev, StoredSnapshot)>, QuietPointRefusal> {
        let Some(row) = self
            .cx
            .durable_reads()
            .map_err(refused)?
            .snapshot(&self.exec)
            .await
            .map_err(refused)?
        else {
            return Ok(None);
        };
        let stored = serde_json::from_str(&row.snapshot_ref).map_err(refused)?;
        Ok(Some((row.rev, stored)))
    }

    /// Commit `point` in one transaction and answer its revision and the
    /// executions it admitted, whose bodies may run now. The transaction is
    /// labelled `cell.snapshot+admit` when it admits an operation and
    /// `cell.snapshot` otherwise.
    ///
    /// # Errors
    ///
    /// [`QuietPointRefusal`]: the store's refusal (a lost epoch, a moved
    /// revision), waits (L5's), or an issued operation whose identity is not
    /// the one its admission takes.
    pub async fn commit_admitting(
        &self,
        point: QuietPoint,
    ) -> Result<(SnapshotRev, Vec<AdmittedExecution>), QuietPointRefusal> {
        let stored = StoredSnapshot::Parked {
            checkpoint: point.checkpoint,
            broker: point.broker,
        };
        self.commit(
            stored,
            point.executable_identity,
            point.issued,
            point.waits,
            point.with,
        )
        .await
    }

    /// Commit the execution's end as its last snapshot, with the owner's
    /// rows `with`, under `cell.snapshot`.
    ///
    /// # Errors
    ///
    /// The store's refusal.
    pub async fn commit_end(
        &self,
        result: serde_json::Value,
        broker: BrokerLedger,
        executable_identity: String,
        with: Vec<DomainWrite>,
    ) -> Result<SnapshotRev, QuietPointRefusal> {
        self.commit(
            StoredSnapshot::Ended { result, broker },
            executable_identity,
            Vec::new(),
            Vec::new(),
            with,
        )
        .await
        .map(|(rev, _)| rev)
    }

    async fn commit(
        &self,
        stored: StoredSnapshot,
        executable_identity: String,
        issued: Vec<IssuedOperation>,
        waits: Vec<WaitSpec>,
        with: Vec<DomainWrite>,
    ) -> Result<(SnapshotRev, Vec<AdmittedExecution>), QuietPointRefusal> {
        if !waits.is_empty() {
            return Err(QuietPointRefusal(
                "a quiet point that pins waits is L5's (FIG-5173)".to_owned(),
            ));
        }
        let expected = self
            .cx
            .durable_reads()
            .map_err(refused)?
            .snapshot(&self.exec)
            .await
            .map_err(refused)?
            .map(|row| row.rev);
        let format_version = match &stored {
            StoredSnapshot::Parked { checkpoint, .. } => checkpoint.vm.format_version(),
            StoredSnapshot::Ended { .. } => lashlang::vm_contract_versions().continuation,
        };
        let mut tx = self.cx.begin().await.map_err(refused)?;
        for write in with {
            tx.write(write);
        }
        tx.write(DomainWrite::Snapshot(SnapshotWrite::Put {
            exec: self.exec.clone(),
            expected,
            snapshot_ref: serde_json::to_string(&stored).map_err(refused)?,
            executable_identity,
            format_version,
        }));
        let label = if issued.is_empty() {
            CommitLabel::CELL_SNAPSHOT
        } else {
            CommitLabel::CELL_SNAPSHOT_ADMIT
        };
        let admitted = match issued.first() {
            None => Vec::new(),
            Some(first) => {
                let run = RunSeq(first.operation.run);
                let operations: Vec<OperationId> =
                    issued.iter().map(|issued| issued.operation).collect();
                let admitted = round::admit(
                    &mut tx,
                    &self.exec.owner(),
                    run,
                    issued.into_iter().map(|issued| issued.draft).collect(),
                )
                .map_err(refused)?;
                for (operation, admitted) in operations.iter().zip(&admitted) {
                    if operation.admitted(&self.exec) != *admitted.id() {
                        return Err(QuietPointRefusal(format!(
                            "operation {operation:?} is not the identity its admission takes"
                        )));
                    }
                }
                admitted
            }
        };
        let rev = expected.map_or(SnapshotRev(1), |rev| SnapshotRev(rev.0 + 1));
        match self.cx.commit(tx, label).await {
            Ok(_) => Ok((rev, admitted)),
            Err(DurableError::Domain(DomainRefusal::SnapshotRevConflict { found, .. })) => Err(
                QuietPointRefusal(format!("the snapshot moved to {found:?} under this owner")),
            ),
            Err(error) => Err(refused(error)),
        }
    }
}

#[async_trait::async_trait]
impl SnapshotStore for DurableSnapshotStore {
    async fn commit_quiet_point(
        &self,
        point: QuietPoint,
    ) -> Result<SnapshotRev, QuietPointRefusal> {
        self.commit_admitting(point).await.map(|(rev, _)| rev)
    }

    async fn latest(&self) -> Result<Option<(SnapshotRev, Checkpoint)>, QuietPointRefusal> {
        Ok(self.stored().await?.and_then(|(rev, stored)| match stored {
            StoredSnapshot::Parked { checkpoint, .. } => Some((rev, checkpoint)),
            StoredSnapshot::Ended { .. } => None,
        }))
    }

    async fn open_frame(&self, _frame: FrameEpoch) -> Result<(), QuietPointRefusal> {
        todo!("L7 (FIG-5177): open a frame, dropping earlier frames' snapshots")
    }
}
