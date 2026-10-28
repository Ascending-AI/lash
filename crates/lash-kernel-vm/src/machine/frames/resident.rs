//! A task's frames as a resident sweep reads them (seam I, FIG-6748): what
//! their rows hold, read where they are. A row that holds what its frame
//! holds is read as it is; a frame that changed since its row was written
//! is walked through what it holds in memory, nothing projected. The
//! capacity meter recounts the resident frames here (FIG-6791).

use lash_kernel_doc::heap::{HeapFault, PageValue};
use lash_kernel_state::arena::{Arena, Reach, Reached, VALUE_BYTES};
use lash_kernel_state::kernel_internals::resident::frame_values;
use lash_kernel_state::kernel_internals::tree::Cell;
use lash_kernel_state::paged::FrameRecord;

use super::{Frames, row};

impl Frames {
    /// Gives `out` what the frames hold: a row's values by their page
    /// forms, a changed frame's through `arena`'s walk.
    pub(in crate::machine) fn reach(
        &self,
        arena: &mut Arena,
        reach: &mut Reach,
        out: &mut dyn FnMut(Reached<'_>) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        let rows = self.rows.len();
        // The rows a projection would write: past the stack it removes
        // them, a changed frame's it writes again.
        let mut written = vec![false; rows as usize];
        for index in self.len..rows {
            written[index as usize] = true;
        }
        let mut values = Vec::new();
        for (index, frame) in &self.cache {
            if *index < self.held || (frame.owned && frame.holds_as_projected()) {
                continue;
            }
            if *index < rows {
                written[*index as usize] = true;
            }
            values.clear();
            frame.each_value(&mut |value| values.push(*value));
            for value in &values {
                arena.reach(reach, value, out)?;
            }
        }
        for (index, written) in written.into_iter().enumerate() {
            if written {
                continue;
            }
            let frame = row::<FrameRecord>(&self.rows, index as u64, &self.access)?;
            frame_values(&frame, &self.access, &mut |value: &PageValue| {
                out(Reached::Page(value))
            })?;
        }
        Ok(())
    }

    /// Prices cached frame holdings and cold frame summaries for normalization.
    pub(in crate::machine) fn logical_control_bytes(
        &self,
        arena: &Arena,
    ) -> Result<u64, HeapFault> {
        let mut bytes = 0_u64;
        for index in 0..self.len {
            let local = match self.cache.iter().find(|(at, _)| *at == index) {
                Some((_, frame)) => {
                    let mut local = lash_kernel_doc::LogicalHeldV1::FRAME;
                    let mut problem = None;
                    frame.each_value(&mut |value| match arena.logical_local(value).and_then(|n| {
                        local.checked_add(n).ok_or_else(|| {
                            HeapFault::Malformed("frame holding cost overflow".into())
                        })
                    }) {
                        Ok(n) => local = n,
                        Err(e) => problem = Some(e),
                    });
                    if let Some(e) = problem {
                        return Err(e);
                    }
                    local
                }
                None => row::<FrameRecord>(&self.rows, index, &self.access)?.cost(),
            };
            bytes = bytes
                .checked_add(local)
                .ok_or_else(|| HeapFault::Malformed("frame stack holding cost overflow".into()))?;
        }
        Ok(bytes)
    }

    /// The bytes the capacity meter counts for the resident frames
    /// (FIG-6791): what each added as it came into memory.
    pub(in crate::machine) fn control_bytes(&self) -> u64 {
        self.cache
            .iter()
            .map(|(_, frame)| frame_bytes(frame))
            .fold(0, u64::saturating_add)
    }

    /// Gives back what the resident frames count, as the task they belong
    /// to leaves memory.
    pub(in crate::machine) fn release_control(&mut self) {
        let bytes = self.control_bytes();
        self.arena.borrow_mut().shrink_control(bytes);
    }
}

/// The bytes the capacity meter counts for `frame` while it is resident:
/// its [`FRAME_BYTES`](crate::machine::FRAME_BYTES) and its slots.
pub(super) fn frame_bytes(frame: &super::Frame) -> u64 {
    (frame.slots.values.len() as u64)
        .saturating_mul(VALUE_BYTES)
        .saturating_add(crate::machine::FRAME_BYTES)
}
