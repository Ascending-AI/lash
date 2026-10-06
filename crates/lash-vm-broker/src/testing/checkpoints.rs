//! An in-memory checkpoint store that counts what it commits.

use std::sync::Mutex;

use lash_vm_protocol::FrameEpoch;

use crate::ledger::{Checkpoint, QuietPointRefusal};
use crate::snapshot::{QuietPoint, SnapshotStore};
use lash_durable::domain::SnapshotRev;

struct Held {
    latest: Option<Checkpoint>,
    frame_epoch: FrameEpoch,
    commits: Vec<Checkpoint>,
}

/// One owner's checkpoints, in memory. It refuses a checkpoint of a frame
/// older than the one it was opened to, as a durable store's fenced write
/// does, and takes the checkpoint it already holds again as a no-op.
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
    /// Every checkpoint committed, in order.
    pub fn commits(&self) -> Vec<Checkpoint> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .commits
            .clone()
    }
}

#[async_trait::async_trait]
impl SnapshotStore for MemoryCheckpoints {
    async fn commit_quiet_point(
        &self,
        point: QuietPoint,
    ) -> Result<SnapshotRev, QuietPointRefusal> {
        let checkpoint = &point.checkpoint;
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if checkpoint.frame_epoch != held.frame_epoch {
            return Err(QuietPointRefusal(format!(
                "the checkpoint belongs to frame {:?}, and the owner is in frame {:?}",
                checkpoint.frame_epoch, held.frame_epoch
            )));
        }
        // A redriven invocation that replays to the same state commits it
        // again: the write is idempotent, as a durable store's keyed write
        // is.
        if held.latest.as_ref() == Some(checkpoint) {
            return Ok(revision(&held.commits));
        }
        held.latest = Some(checkpoint.clone());
        held.commits.push(checkpoint.clone());
        Ok(revision(&held.commits))
    }

    async fn latest(&self) -> Result<Option<(SnapshotRev, Checkpoint)>, QuietPointRefusal> {
        let held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(held
            .latest
            .clone()
            .map(|latest| (revision(&held.commits), latest)))
    }

    async fn open_frame(&self, frame_epoch: FrameEpoch) -> Result<(), QuietPointRefusal> {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if frame_epoch > held.frame_epoch {
            held.frame_epoch = frame_epoch;
            held.latest = None;
        }
        Ok(())
    }
}

/// The revision of the latest of `commits`: one per commit, from 1.
fn revision(commits: &[Checkpoint]) -> SnapshotRev {
    SnapshotRev(u64::try_from(commits.len()).unwrap_or(u64::MAX))
}
