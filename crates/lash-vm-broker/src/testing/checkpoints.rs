//! An in-memory snapshot store that counts what it commits.

use std::collections::BTreeMap;
use std::sync::Mutex;

use lash_vm_protocol::FrameEpoch;

use crate::effects::Performed;
use crate::ledger::{Checkpoint, QuietPointRefusal};
use crate::snapshot::{
    Committed, OperationId, PendingOperation, QuietPoint, Recovered, SnapshotStore,
};
use lash_durable::domain::SnapshotRev;

struct Held {
    latest: Option<Checkpoint>,
    frame_epoch: FrameEpoch,
    commits: Vec<QuietPoint>,
    settled: BTreeMap<OperationId, Performed>,
}

/// One execution's snapshots, in memory. It refuses a checkpoint of a frame
/// other than the one it was opened to, as a durable store's fenced write
/// does. An admitted operation takes ordinal 1 of its run; a restore feeds
/// back its settled outcome, and reruns one that never settled.
pub struct MemoryCheckpoints {
    held: Mutex<Held>,
}

impl Default for MemoryCheckpoints {
    fn default() -> Self {
        Self {
            held: Mutex::new(Held {
                latest: None,
                frame_epoch: FrameEpoch(0),
                commits: Vec::new(),
                settled: BTreeMap::new(),
            }),
        }
    }
}

impl MemoryCheckpoints {
    fn held(&self) -> std::sync::MutexGuard<'_, Held> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Every quiet point committed, in order, as it was stored.
    pub fn commits(&self) -> Vec<QuietPoint> {
        self.held().commits.clone()
    }

    /// Every outcome settled, by identity.
    pub fn settled(&self) -> BTreeMap<OperationId, Performed> {
        self.held().settled.clone()
    }
}

#[async_trait::async_trait]
impl SnapshotStore for MemoryCheckpoints {
    async fn commit_quiet_point(&self, point: QuietPoint) -> Result<Committed, QuietPointRefusal> {
        let mut held = self.held();
        let mut point = point;
        if point.checkpoint.frame_epoch() != held.frame_epoch {
            return Err(QuietPointRefusal(format!(
                "the checkpoint belongs to frame {:?}, and the owner is in frame {:?}",
                point.checkpoint.frame_epoch(),
                held.frame_epoch
            )));
        }
        if let Some(draft) = &point.admit {
            let ledger = &mut point.checkpoint.ledger;
            let pending = ledger.pending.as_mut().ok_or_else(|| {
                QuietPointRefusal("an admission needs the operation the VM stands on".into())
            })?;
            let operation = OperationId {
                run: pending.run(),
                ordinal: 1,
            };
            pending.admission = crate::snapshot::OperationAdmission::Execution(operation);
            ledger.operations.insert(operation, draft.call().clone());
        }
        held.latest = Some(point.checkpoint.clone());
        held.commits.push(point.clone());
        Ok(Committed {
            rev: SnapshotRev(u64::try_from(held.commits.len()).unwrap_or(u64::MAX)),
            checkpoint: point.checkpoint,
            waits: Vec::new(),
        })
    }

    async fn settle(
        &self,
        operation: OperationId,
        performed: &Performed,
    ) -> Result<(), QuietPointRefusal> {
        let mut held = self.held();
        if held.settled.contains_key(&operation) {
            return Err(QuietPointRefusal(format!(
                "operation {operation:?} already has an outcome"
            )));
        }
        held.settled.insert(operation, performed.clone());
        Ok(())
    }

    async fn recover(&self, pending: &PendingOperation) -> Result<Recovered, QuietPointRefusal> {
        let held = self.held();
        Ok(
            match pending
                .operation()
                .and_then(|operation| held.settled.get(&operation))
            {
                Some(performed) => Recovered::Settled(performed.clone()),
                None => Recovered::Rerun { waits: Vec::new() },
            },
        )
    }

    async fn latest(&self) -> Result<Option<(SnapshotRev, Checkpoint)>, QuietPointRefusal> {
        let held = self.held();
        let rev = SnapshotRev(u64::try_from(held.commits.len()).unwrap_or(u64::MAX));
        Ok(held.latest.clone().map(|latest| (rev, latest)))
    }

    async fn open_frame(&self, frame_epoch: FrameEpoch) -> Result<(), QuietPointRefusal> {
        let mut held = self.held();
        if frame_epoch > held.frame_epoch {
            held.frame_epoch = frame_epoch;
            held.latest = None;
        }
        Ok(())
    }
}
