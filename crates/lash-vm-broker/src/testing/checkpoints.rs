//! An in-memory snapshot store that counts what it commits.

use std::sync::Mutex;

use lash_vm_protocol::FrameEpoch;

use crate::ledger::{Checkpoint, QuietPointRefusal};
use crate::snapshot::{
    Committed, OpenMember, OperationId, PendingOperation, QuietPoint, SnapshotStore,
};
use lash_durable::domain::SnapshotRev;

struct Held {
    latest: Option<Checkpoint>,
    frame_epoch: FrameEpoch,
    commits: Vec<QuietPoint>,
}

/// One execution's snapshots, in memory. It refuses a checkpoint of a frame
/// other than the one it was opened to, as a durable store's fenced write
/// does. An admitted operation's members take ordinals 1, 2, ... of its run,
/// as a durable admission's first starts do; a restore performs the
/// operation again, with no waits pinned.
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
        if !point.members.is_empty() {
            let ledger = &mut point.checkpoint.ledger;
            let pending = ledger.pending.as_mut().ok_or_else(|| {
                QuietPointRefusal("an admission needs the operation the VM stands on".into())
            })?;
            for (ordinal, member) in (1_u64..).zip(&point.members) {
                let operation = OperationId {
                    run: pending.run,
                    ordinal,
                };
                pending.members.push(operation);
                ledger.operations.insert(
                    operation,
                    OpenMember {
                        call: member.draft.call().clone(),
                        request: member.request.clone(),
                    },
                );
            }
        }
        held.latest = Some(point.checkpoint.clone());
        held.commits.push(point.clone());
        Ok(Committed {
            rev: SnapshotRev(u64::try_from(held.commits.len()).unwrap_or(u64::MAX)),
            checkpoint: point.checkpoint,
            waits: Vec::new(),
        })
    }

    async fn recover(
        &self,
        _pending: &PendingOperation,
    ) -> Result<Vec<crate::WaitRef>, QuietPointRefusal> {
        Ok(Vec::new())
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
