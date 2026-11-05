//! Canonical frame replacement reserved before the single region publication.
#![allow(
    dead_code,
    reason = "canonical region installation is connected at machine cutover"
)]
use super::*;

pub(in crate::machine) struct PreparedRegionFrames<'a> {
    frames: &'a mut Frames,
    #[expect(
        clippy::vec_box,
        reason = "move existing frame allocations into the canonical stack"
    )]
    replacements: Vec<Box<Frame>>,
    base: u64,
    len: u64,
}

impl Frames {
    /// The entry frame must already be resident. Preparing an exit never
    /// restores a cold frame or borrows the arena held by the state lease.
    #[expect(
        clippy::vec_box,
        reason = "move existing frame allocations into the canonical stack"
    )]
    pub(in crate::machine) fn prepare_region_exit(
        &mut self,
        entry_code: crate::compile::CodeId,
        entry_invocation: u64,
        mut replacements: Vec<Box<Frame>>,
    ) -> Result<PreparedRegionFrames<'_>, HeapFault> {
        let base = self
            .len
            .checked_sub(1)
            .ok_or_else(|| malformed("region entry frame is absent"))?;
        let (index, entry) = self
            .cache
            .last()
            .ok_or_else(|| malformed("region entry frame is not resident"))?;
        if *index != base || entry.code != entry_code || entry.invocation != entry_invocation {
            return Err(malformed(
                "region exit does not replace its actual entry frame",
            ));
        }
        let count = u64::try_from(replacements.len())
            .map_err(|_| malformed("region frame count overflow"))?;
        let len = base
            .checked_add(count)
            .ok_or_else(|| malformed("region frame depth overflow"))?;
        self.cache
            .try_reserve(replacements.len().saturating_sub(1))
            .map_err(|_| HeapFault::Exhausted {
                resource: lash_kernel_doc::heap::HostResource::StagingBytes,
            })?;
        for frame in &mut replacements {
            frame.owned = false;
        }
        Ok(PreparedRegionFrames {
            frames: self,
            replacements,
            base,
            len,
        })
    }
}

impl PreparedRegionFrames<'_> {
    /// Return the old frame for retirement after the state publication closure.
    /// Every bound, capacity and entry check has already completed.
    pub(in crate::machine) fn publish(self) -> Box<Frame> {
        let Some((_, retired)) = self.frames.cache.pop() else {
            unreachable!("prepared resident region entry")
        };
        let empty = self.replacements.is_empty();
        for (offset, frame) in self.replacements.into_iter().enumerate() {
            self.frames.cache.push((self.base + offset as u64, frame));
        }
        self.frames.len = self.len;
        let changed = if empty {
            self.len.saturating_sub(1)
        } else {
            self.base
        };
        self.frames.held = self.frames.held.min(changed);
        self.frames.placed = self.frames.placed.min(changed);
        self.frames.active = if empty {
            None
        } else {
            Some(self.frames.cache.len() - 1)
        };
        self.frames.arrival = if empty {
            Arrival::Returned
        } else {
            Arrival::Fresh
        };
        retired
    }
}

#[cfg(test)]
mod laws {
    use super::*;
    use crate::laws::embedder::Embedder;
    use crate::{Layout, Step};
    use lash_kernel_doc::TaskId;

    /// K-EVAL-008: an exit must replace only its entered activation and must
    /// not restore a cold caller while the exclusive state lease is held.
    #[test]
    fn region_frame_exit_preserves_cold_callers_and_an_abandoned_entry() {
        let mut run = Embedder::new(
            "definition \"function work() -> Any\\nkernel 1\\nmetering statements\\nbody { return 1 }\"\nmain { let a = call work() return a }",
        );
        run.machine.force_source_execution();
        for attempt in 0..256 {
            assert!(
                attempt < 255,
                "the named call must be reached before the fixed test bound"
            );
            assert!(matches!(
                run.machine.run_slice(&mut run.world, 1).unwrap(),
                Step::Slice
            ));
            if run.machine.tasks.get_mut(0).unwrap().unwrap().frames.len() == 2 {
                break;
            }
        }
        run.reopen(Layout::default(), false);
        let (code, invocation) = {
            let task = run
                .machine
                .tasks
                .get_mut(TaskId::MAIN.0 as usize)
                .unwrap()
                .unwrap();
            let top = task.frames.last_mut().unwrap().unwrap();
            (top.code, top.invocation)
        };
        let exe = Arc::clone(&run.machine.exe);
        let mut first = run.machine.fresh_frame(&exe, code).unwrap();
        first.invocation = 100;
        let mut second = run.machine.fresh_frame(&exe, code).unwrap();
        second.invocation = 101;
        let lease = lash_kernel_state::kernel_internals::logical::StateLease::acquire_shared(
            &mut run.machine.heap,
            &run.machine.arena,
        )
        .unwrap();
        let frames = &mut run.machine.tasks.get_mut(0).unwrap().unwrap().frames;
        assert_eq!(
            frames
                .cache
                .iter()
                .map(|(index, _)| *index)
                .collect::<Vec<_>>(),
            [1]
        );
        let before = row(&frames.rows, 0, &frames.access).unwrap();
        assert!(
            frames
                .prepare_region_exit(code, invocation + 1, Vec::new())
                .is_err()
        );
        drop(
            frames
                .prepare_region_exit(code, invocation, Vec::new())
                .unwrap(),
        );
        assert_eq!(frames.len(), 2);
        assert_eq!(frames.cache[0].1.invocation, invocation);
        assert!(
            row(&frames.rows, 0, &frames.access)
                .unwrap()
                .same_version(&before)
        );
        let prepared = frames
            .prepare_region_exit(code, invocation, vec![first, second])
            .unwrap();
        let retired = prepared.publish();
        assert_eq!(retired.invocation, invocation);
        assert_eq!(frames.len(), 3);
        assert_eq!(
            frames
                .cache
                .iter()
                .map(|(index, frame)| (*index, frame.invocation))
                .collect::<Vec<_>>(),
            [(1, 100), (2, 101)]
        );
        assert_eq!((frames.active, frames.held, frames.placed), (Some(1), 1, 1));
        assert!(
            row(&frames.rows, 0, &frames.access)
                .unwrap()
                .same_version(&before)
        );
        drop(retired);
        let retired = frames
            .prepare_region_exit(code, 101, Vec::new())
            .unwrap()
            .publish();
        assert_eq!(retired.invocation, 101);
        assert_eq!((frames.len(), frames.active), (2, None));
        let retired = frames
            .prepare_region_exit(code, 100, Vec::new())
            .unwrap()
            .publish();
        assert_eq!(retired.invocation, 100);
        assert_eq!(
            (frames.len(), frames.cache.len(), frames.active),
            (1, 0, None)
        );
        assert!(
            frames
                .prepare_region_exit(code, invocation, Vec::new())
                .is_err(),
            "an exit must never restore the remaining cold caller under a state lease"
        );
        assert!(
            row(&frames.rows, 0, &frames.access)
                .unwrap()
                .same_version(&before)
        );
        drop(lease);
        let expected_code = code_of(&before, &exe, &frames.access).unwrap();
        let caller = frames.last_mut().unwrap().unwrap();
        assert_eq!(caller.invocation, before.invocation);
        assert_eq!(caller.code, expected_code);
    }
}
