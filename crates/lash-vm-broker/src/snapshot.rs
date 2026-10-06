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
use lash_core_execution::runtime::actor::round::{ExecutionDraft, RunFold};
use lash_core_execution::runtime::actor::waits::WaitSpec;
use lash_core_store::tool_run::AttemptOutcome;
use lash_durable::domain::{AdmittedId, ExecKey, Ordinal, RunSeq, SnapshotRev};
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
    pub operations: BTreeMap<OperationId, lash_sansio::ToolCallId>,
    /// The sequence the next admission takes.
    pub next_admission: u64,
    /// The frame the ledger belongs to.
    pub frame_epoch: FrameEpoch,
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
    /// The operations to admit with it.
    pub issued: Vec<IssuedOperation>,
    /// The waits to pin with it.
    pub waits: Vec<WaitSpec>,
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
#[must_use]
pub fn outcomes_to_inject(
    _ledger: &BrokerLedger,
    _fold: &RunFold,
) -> Vec<(OperationId, InjectedOutcome)> {
    todo!("V0 (FIG-5170): match a restored ledger's operations to their folded outcomes")
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

#[async_trait::async_trait]
impl SnapshotStore for DurableSnapshotStore {
    async fn commit_quiet_point(
        &self,
        _point: QuietPoint,
    ) -> Result<SnapshotRev, QuietPointRefusal> {
        todo!(
            "V0 (FIG-5170): commit a snapshot with its ledger, admissions and waits in one transaction"
        )
    }

    async fn latest(&self) -> Result<Option<(SnapshotRev, Checkpoint)>, QuietPointRefusal> {
        todo!("V0 (FIG-5170): read an execution's latest snapshot")
    }

    async fn open_frame(&self, _frame: FrameEpoch) -> Result<(), QuietPointRefusal> {
        todo!("L7 (FIG-5177): open a frame, dropping earlier frames' snapshots")
    }
}
