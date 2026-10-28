//! Keep CPU receipts unsigned integers, with a constant MessagePack width.

use crate::{BorrowedParentFrame, ParentFrame, PoolStarted, WorkerFrame, WorkerMessage};

pub(super) trait FrameEncoding: serde::Serialize {
    fn cpu_counter(&self) -> Option<u64> {
        None
    }
}

impl FrameEncoding for ParentFrame {}
impl FrameEncoding for BorrowedParentFrame<'_> {}
impl FrameEncoding for PoolStarted {}
impl FrameEncoding for WorkerFrame {
    fn cpu_counter(&self) -> Option<u64> {
        match &self.message {
            WorkerMessage::Progress { cpu_nanos, .. } | WorkerMessage::ResetDone { cpu_nanos } => {
                Some(*cpu_nanos)
            }
            _ => None,
        }
    }
}

/// The first named CPU integer precedes all user-controlled payloads in
/// Progress/ResetDone. The numeric header cannot contain this ten-byte
/// string marker and name, and phase is a fixed enum spelling.
pub(super) fn integer(bytes: &[u8]) -> Option<std::ops::Range<usize>> {
    const NAME: &[u8] = b"\xa9cpu_nanos";
    let start = bytes.windows(NAME.len()).position(|part| part == NAME)? + NAME.len();
    let width = match *bytes.get(start)? {
        0x00..=0x7f => 1,
        0xcc => 2,
        0xcd => 3,
        0xce => 5,
        0xcf => 9,
        _ => return None,
    };
    bytes.get(start..start + width)?;
    Some(start..start + width)
}
