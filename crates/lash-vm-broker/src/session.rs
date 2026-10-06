//! One owner's frames, and what opening one does (F5).
//!
//! A session's model code runs frame by frame. When a frame opens (a
//! context-pressure frame, `continue_as`, an administrative compaction), the
//! live execution state of the frame it leaves ends, exactly as its durable
//! state does: a global the old frame set is `undefined` in the new one.
//! Opening a frame, in order:
//!
//! 1. **fences** old responses: the session's [`FrameFence`] advances, so no
//!    message, state or checkpoint of an earlier frame is applied from here
//!    on, and every run of an earlier frame retires where it stands;
//! 2. **resets persisted state** atomically: the checkpoint store drops the
//!    earlier frame's checkpoint in one write;
//! 3. **retires live state**: it waits until every run of an earlier frame
//!    has ended and its worker gone back to the pool, discarded.
//!
//! Each step is idempotent, so a redriven open replays to the same end.

use std::time::Duration;

use lash_vm_protocol::FrameEpoch;

use crate::broker::FrameFence;
use crate::ledger::QuietPointRefusal;
use crate::snapshot::SnapshotStore;

/// Why a frame did not open.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FrameOpenFailure {
    #[error("{0}")]
    Reset(QuietPointRefusal),
    #[error("a run of an earlier frame was still live after {0:?}")]
    RetireTimedOut(Duration),
}

/// One owner's frames.
#[derive(Clone, Debug)]
pub struct VmSession {
    frames: FrameFence,
}

impl VmSession {
    pub fn new(frame_epoch: FrameEpoch) -> Self {
        Self {
            frames: FrameFence::new(frame_epoch),
        }
    }

    /// The fence the owner's brokers run under.
    pub fn frames(&self) -> &FrameFence {
        &self.frames
    }

    pub fn frame_epoch(&self) -> FrameEpoch {
        self.frames.current()
    }

    /// Opens frame `epoch` (see the module docs), waiting at most `retire`
    /// for the earlier frame's runs to end.
    pub async fn open_frame(
        &self,
        epoch: FrameEpoch,
        checkpoints: &dyn SnapshotStore,
        retire: Duration,
    ) -> Result<(), FrameOpenFailure> {
        self.frames.advance(epoch);
        checkpoints
            .open_frame(epoch)
            .await
            .map_err(FrameOpenFailure::Reset)?;
        tokio::time::timeout(retire, self.frames.retired_before(epoch))
            .await
            .map_err(|_| FrameOpenFailure::RetireTimedOut(retire))
    }
}
