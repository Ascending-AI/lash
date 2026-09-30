//! An in-memory checkpoint store that counts what it commits.

use std::sync::Mutex;

use lash_vm_protocol::FrameEpoch;

use crate::ledger::{Checkpoint, CheckpointRefusal, CheckpointStore};

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
impl CheckpointStore for MemoryCheckpoints {
    async fn commit(&self, checkpoint: &Checkpoint) -> Result<(), CheckpointRefusal> {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if checkpoint.frame_epoch != held.frame_epoch {
            return Err(CheckpointRefusal(format!(
                "the checkpoint belongs to frame {:?}, and the owner is in frame {:?}",
                checkpoint.frame_epoch, held.frame_epoch
            )));
        }
        // A re-driven invocation that replays to the same state commits it
        // again: the write is idempotent, as a durable store's keyed write
        // is.
        if held.latest.as_ref() == Some(checkpoint) {
            return Ok(());
        }
        held.latest = Some(checkpoint.clone());
        held.commits.push(checkpoint.clone());
        Ok(())
    }

    async fn latest(&self) -> Result<Option<Checkpoint>, CheckpointRefusal> {
        Ok(self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .latest
            .clone())
    }

    async fn open_frame(&self, frame_epoch: FrameEpoch) -> Result<(), CheckpointRefusal> {
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
