//! One affine state publication installs already prepared canonical VM state.
use super::*;
use crate::machine::{
    Frame,
    frames::{Frames, region::PreparedRegionFrames},
    registers::Registers,
    tasks::region::{CounterLease, PreparedCounterExit},
};
use lash_kernel_doc::TaskId;
use lash_kernel_state::kernel_internals::logical::{
    Owner, PreparedStateExit, PublicationValue, StateExit,
};

pub(super) struct Authority<'m> {
    pub(super) frames: &'m mut Frames,
    pub(super) task_state: &'m mut lash_kernel_state::paged::TaskStateRecord,
    pub(super) counters: CounterLease<'m>,
    pub(super) registers: &'m mut Registers,
    pub(super) charged: &'m mut u64,
    pub(super) logical_memory: &'m std::cell::Cell<u64>,
    pub(super) session: &'m mut crate::machine::session::Session,
    pub(super) task: TaskId,
    pub(super) exe: Arc<crate::compile::Executable>,
    pub(super) entry_code: CodeId,
    pub(super) entry_invocation: u64,
    /// The stack index of the lease's root frame: above it stand only the
    /// frames the lease runs, beneath it the frames the machine holds.
    pub(super) entry_base: u64,
    pub(super) entry_frames: usize,
    pub(super) inputs: Box<[Owner]>,
    pub(super) limits: Limits,
}
pub(super) struct Canonical {
    #[expect(clippy::vec_box, reason = "install already allocated canonical frames")]
    pub(super) frames: Vec<Box<Frame>>,
    pub(super) registers: Option<Registers>,
    pub(super) crossing: Option<verified::Crossing>,
    pub(super) incoming: Option<lash_kernel_state::paged::IncomingRecord>,
}
pub(super) struct Prepared<'m> {
    state: PreparedStateExit<'m>,
    frames: PreparedRegionFrames<'m>,
    counters: PreparedCounterExit<'m>,
    registers: &'m mut Registers,
    next_registers: Option<Registers>,
    session: &'m mut crate::machine::session::Session,
    session_writes: Vec<crate::machine::session::PreparedSessionWrite>,
    charged: &'m mut u64,
    logical_memory: &'m std::cell::Cell<u64>,
    task_state: &'m mut lash_kernel_state::paged::TaskStateRecord,
    incoming: Option<lash_kernel_state::paged::IncomingRecord>,
}
impl<'m> Authority<'m> {
    pub(super) fn prepare(
        self,
        image: &mut Reconstruction,
        frozen: FrozenGraph<'m>,
    ) -> Result<Prepared<'m>, RegionError> {
        let FrozenGraph {
            lease,
            graph,
            invocations,
            publications,
        } = frozen;
        let canonical = image
            .canonical
            .take()
            .ok_or_else(|| invalid(Refused::Exit))?;
        // A limiter only grows the limits the run entered under.
        let limits = image.limits;
        if limits.charge < self.limits.charge || limits.memory < self.limits.memory {
            return Err(invalid(Refused::Facts));
        }
        if canonical.frames.len() != image.frames.len()
            || publications.frames.len() != image.frames.len()
            || (image.counters.charge > limits.charge
                && !(image.stop == RegionStop::Refusal
                    && canonical.crossing.as_ref().is_some_and(|crossing| {
                        crossing.authenticates(limits.charge, image.counters.charge)
                    })))
            || image.counters.peak > limits.memory
            || image.counters.depth > self.limits.depth
        {
            return Err(invalid(Refused::Facts));
        }
        if canonical.incoming.is_some() != image.payload.is_some()
            || image.payload.is_some() != (image.stop == RegionStop::Complete)
            || (image.stop == RegionStop::Complete && !image.frames.is_empty())
        {
            return Err(invalid(Refused::Exit));
        }
        let mut outputs = Vec::new();
        for ((frame, actual), published) in canonical
            .frames
            .iter()
            .zip(&image.frames)
            .zip(publications.frames)
        {
            if frame.code != actual.program
                || frame.invocation != actual.invocation
                || published.program != actual.program
                || published.invocation != actual.invocation
                || actual.program.0 as usize >= self.exe.code_count()
            {
                return Err(invalid(Refused::Source));
            }
            if published.incoming {
                if outputs
                    .iter()
                    .any(|(owner, _)| *owner == Owner::Incoming(self.task))
                {
                    return Err(invalid(Refused::Exit));
                }
                outputs.push((
                    Owner::Incoming(self.task),
                    published.pending.ok_or_else(|| invalid(Refused::Exit))?,
                ));
            }
            outputs.push((
                Owner::Frame {
                    task: self.task,
                    invocation: actual.invocation,
                },
                PublicationValue::Frame,
            ));
            outputs.extend(published.roles.into_vec().into_iter().map(|(slot, value)| {
                (
                    Owner::Value {
                        task: self.task,
                        invocation: actual.invocation,
                        slot,
                    },
                    value,
                )
            }));
        }
        if let Some(value) = publications.payload {
            outputs.push((Owner::Incoming(self.task), value));
        }
        for (digest, value) in publications.sessions {
            outputs.push((Owner::Session(digest), value));
        }
        let session_writes = self
            .session
            .prepare_writes(&image.sessions)
            .map_err(RegionError::Heap)?;
        let counters = self
            .counters
            .prepare(&image.source_counts)
            .map_err(RegionError::Heap)?;
        let frames = self
            .frames
            .prepare_region_stack_exit(
                self.entry_code,
                self.entry_invocation,
                self.entry_base,
                self.entry_frames,
                canonical.frames,
            )
            .map_err(RegionError::Heap)?;
        let state = lease
            .prepare(
                StateExit {
                    inputs: self.inputs,
                    outputs: outputs.into_boxed_slice(),
                    graph,
                    invocations,
                    peak: image.counters.peak,
                    delta: i128::from(image.counters.balance)
                        - i128::from(self.limits.entry_balance),
                },
                limits.memory,
            )
            .map_err(RegionError::Heap)?;
        Ok(Prepared {
            state,
            frames,
            counters,
            registers: self.registers,
            next_registers: canonical.registers,
            session: self.session,
            session_writes,
            charged: self.charged,
            logical_memory: self.logical_memory,
            task_state: self.task_state,
            incoming: canonical.incoming,
        })
    }
}
impl Prepared<'_> {
    pub(super) fn commit(self, image: Reconstruction) -> Result<(), RegionError> {
        let Self {
            state,
            frames,
            counters,
            registers,
            next_registers,
            session,
            session_writes,
            charged,
            logical_memory,
            task_state,
            incoming,
        } = self;
        let mut retired = None;
        let mut old_registers = None;
        state
            .commit(|| {
                retired = Some(frames.publish_stack());
                counters.publish();
                for write in session_writes {
                    write.publish(session);
                }
                if let Some(next) = next_registers {
                    old_registers = Some(std::mem::replace(registers, next));
                }
                *charged = image.counters.progress;
                logical_memory.set(image.counters.balance);
                if let Some(incoming) = incoming {
                    *task_state = lash_kernel_state::paged::TaskStateRecord::Resuming {
                        incoming,
                        requeue: false,
                    };
                }
            })
            .map_err(RegionError::Heap)?;
        drop((retired, old_registers));
        Ok(())
    }
}
