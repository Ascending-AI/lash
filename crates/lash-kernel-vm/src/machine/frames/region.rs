//! Canonical frame replacement reserved before the single region publication.
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
    #[expect(clippy::vec_box, reason = "retire frame allocations after commit")]
    retired: Vec<Box<Frame>>,
}

impl Frame {
    /// Copy only the entered resident frame into private exit storage. Cold
    /// callers remain in their canonical rows and no source value is evaluated.
    pub(in crate::machine) fn region_image(
        &self,
        logical: std::rc::Rc<
            std::cell::RefCell<lash_kernel_state::kernel_internals::logical::Ledger>,
        >,
        task: lash_kernel_doc::TaskId,
        exe: &crate::compile::Executable,
    ) -> Result<Box<Self>, HeapFault> {
        let roots = self
            .logical
            .as_ref()
            .ok_or_else(|| malformed("region entry has no invocation"))?;
        let mut slots = Slots::default();
        slots.prepare_fresh(self.slot_states().len())?;
        slots.fresh(self.slot_states().len());
        let mut control = crate::machine::controls::Controls::default();
        for value in self.control.iter() {
            control.push(value.clone());
        }
        let mut pending = Continuation::default();
        if let Some(value) = self.pending.peek() {
            pending.set(value.clone());
        }
        pending.source = self.pending.source.clone();
        let mut image = Box::new(Self {
            source_action: self.source_action,
            texts: Vec::new(),
            source_frontier: self
                .source_frontier
                .as_ref()
                .map(|frontier| frontier.region_successor(frontier.position().clone()))
                .transpose()?,
            invocation: self.invocation,
            logical: Some(crate::machine::logical_held::FrameRoots::prepared(
                logical,
                task,
                roots.invocation().clone(),
                Arc::clone(&exe.code(self.code).source_room),
                Arc::clone(&exe.code(self.code).slots_by_at),
            )?),
            convention: self.convention.clone(),
            public_result: self.public_result.clone(),
            link: self.link,
            code: self.code,
            charging: self.charging,
            paced: self.paced,
            slots,
            statement: self.statement.clone(),
            pending,
            control,
            awaiting: self.awaiting,
            nested: self.nested,
            library: self
                .library
                .as_ref()
                .map(|call| crate::machine::LibraryCall {
                    lib: call.lib,
                    args: call.args.clone(),
                }),
            cleaning: self.cleaning,
            held: self.held.clone(),
            owned: false,
        });
        for (at, state) in self.slot_states().iter().enumerate() {
            image.restore_slot(at as u32, *state);
        }
        Ok(image)
    }
}

impl Frames {
    /// Snapshot the active source stack the region runs, before taking the
    /// exclusive lease: the top frame and each caller suspended at a source
    /// call beneath it, down to a caller the machine holds (one awaiting a
    /// return policy or a run-root cleanup), which the lease leaves into.
    /// Gives the index of the lowest snapshot frame. Cold frames are
    /// admitted once at entry, never during private execution.
    #[expect(clippy::vec_box, reason = "canonical frame allocations")]
    pub(in crate::machine) fn region_stack(&mut self) -> Result<(u64, Vec<Box<Frame>>), HeapFault> {
        self.last_mut()?;
        let (task, logical) = self
            .logical
            .as_ref()
            .ok_or_else(|| malformed("region stack has no source context"))?;
        let mut images = Vec::new();
        let mut base = 0;
        for index in (0..self.len).rev() {
            let mut image = if let Some((_, frame)) = self.cache.iter().find(|(at, _)| *at == index)
            {
                frame.region_image(logical.clone(), *task, &self.exe)?
            } else {
                let record = row(&self.rows, index, &self.access)?;
                let mut frame = restore_frame(
                    &record,
                    Arrival::Returned,
                    &self.access,
                    &self.exe,
                    &self.bound,
                    &mut self.arena.borrow_mut(),
                )?;
                frame.logical = Some(crate::machine::logical_held::FrameRoots::restore(
                    logical.clone(),
                    *task,
                    &frame,
                    &self.exe,
                    &self.access,
                )?);
                frame
            };
            // A caller suspended at a region call joins the lease; any other
            // caller (an awaiting statement, a cleanup, a start adapter) is
            // the machine's, and the region's root returns into it there.
            if index + 1 < self.len
                && !image
                    .pending
                    .source
                    .as_ref()
                    .is_some_and(|source| source.progress == 3)
            {
                base = index + 1;
                break;
            }
            image.owned = false;
            images.push(image);
        }
        images.reverse();
        Ok((base, images))
    }

    #[expect(clippy::vec_box, reason = "canonical frame allocations")]
    pub(in crate::machine) fn prepare_region_stack_exit(
        &mut self,
        entry_code: crate::compile::CodeId,
        entry_invocation: u64,
        entry_base: u64,
        entry_frames: usize,
        replacements: Vec<Box<Frame>>,
    ) -> Result<PreparedRegionFrames<'_>, HeapFault> {
        if self.len() != entry_frames || self.len <= entry_base {
            return Err(malformed("region entry stack changed"));
        }
        let invocation = match self.cache.iter().find(|(at, _)| *at == entry_base) {
            Some((_, frame)) => frame.invocation,
            None => row(&self.rows, entry_base, &self.access)?.invocation,
        };
        if self.code_at(entry_base)? != entry_code || invocation != entry_invocation {
            return Err(malformed("region entry invocation changed"));
        }
        self.reserve_region_exit(entry_base, replacements)
    }

    /// The entry frame must already be resident. Preparing an exit never
    /// restores a cold frame or borrows the arena held by the state lease.
    #[expect(
        clippy::vec_box,
        reason = "move existing frame allocations into the canonical stack"
    )]
    #[cfg(test)]
    pub(in crate::machine) fn prepare_region_exit(
        &mut self,
        entry_code: crate::compile::CodeId,
        entry_invocation: u64,
        replacements: Vec<Box<Frame>>,
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
        self.reserve_region_exit(base, replacements)
    }

    #[expect(clippy::vec_box, reason = "canonical frame allocations")]
    fn reserve_region_exit(
        &mut self,
        base: u64,
        mut replacements: Vec<Box<Frame>>,
    ) -> Result<PreparedRegionFrames<'_>, HeapFault> {
        let count = u64::try_from(replacements.len())
            .map_err(|_| malformed("region frame count overflow"))?;
        let len = base
            .checked_add(count)
            .ok_or_else(|| malformed("region frame depth overflow"))?;
        self.cache
            .try_reserve(replacements.len())
            .map_err(|_| HeapFault::Exhausted {
                resource: lash_kernel_doc::heap::HostResource::StagingBytes,
            })?;
        let mut retired = Vec::new();
        retired
            .try_reserve_exact(self.cache.len())
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
            retired,
        })
    }
}

impl PreparedRegionFrames<'_> {
    /// Return the old frame for retirement after the state publication closure.
    /// Every bound, capacity and entry check has already completed.
    #[cfg(test)]
    pub(in crate::machine) fn publish(self) -> Box<Frame> {
        let mut retired = self.publish_stack();
        debug_assert_eq!(retired.len(), 1);
        match retired.pop() {
            Some(frame) => frame,
            None => unreachable!("prepared resident region entry"),
        }
    }

    #[expect(clippy::vec_box, reason = "retire existing allocations after commit")]
    pub(in crate::machine) fn publish_stack(mut self) -> Vec<Box<Frame>> {
        while self
            .frames
            .cache
            .last()
            .is_some_and(|(index, _)| *index >= self.base)
        {
            self.retired
                .extend(self.frames.cache.pop().map(|(_, frame)| frame));
        }
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
        self.retired
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
