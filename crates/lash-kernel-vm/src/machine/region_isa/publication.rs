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
    pub(super) counters: CounterLease<'m>,
    pub(super) registers: &'m mut Registers,
    pub(super) charged: &'m mut u64,
    pub(super) logical_memory: &'m std::cell::Cell<u64>,
    pub(super) task: TaskId,
    pub(super) exe: Arc<crate::compile::Executable>,
    pub(super) entry_code: CodeId,
    pub(super) entry_invocation: u64,
    pub(super) inputs: Box<[Owner]>,
    pub(super) limits: Limits,
}
pub(super) struct Canonical {
    #[expect(clippy::vec_box, reason = "install already allocated canonical frames")]
    pub(super) frames: Vec<Box<Frame>>,
    pub(super) registers: Option<Registers>,
}
pub(super) struct Prepared<'m> {
    state: PreparedStateExit<'m>,
    frames: PreparedRegionFrames<'m>,
    counters: PreparedCounterExit<'m>,
    registers: &'m mut Registers,
    next_registers: Option<Registers>,
    charged: &'m mut u64,
    logical_memory: &'m std::cell::Cell<u64>,
}
impl<'m> Authority<'m> {
    pub(super) fn prepare(
        self,
        image: &mut Reconstruction,
        frozen: FrozenGraph<'m>,
    ) -> Result<Prepared<'m>, RegionError> {
        let (lease, graph, invocations, publications) = match frozen {
            FrozenGraph::Machine {
                lease,
                graph,
                invocations,
                publications,
            } => (lease, graph, invocations, publications),
            #[cfg(test)]
            FrozenGraph::Fixture(_) => return Err(invalid(Refused::Facts)),
        };
        let canonical = image
            .canonical
            .take()
            .ok_or_else(|| invalid(Refused::Exit))?;
        if canonical.frames.len() != image.frames.len()
            || publications.frames.len() != image.frames.len()
            || image.counters.charge > self.limits.charge
            || image.counters.peak > self.limits.memory
            || image.counters.depth > self.limits.depth
        {
            return Err(invalid(Refused::Facts));
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
        if let Some(value) = publications.returning {
            if outputs
                .iter()
                .any(|(owner, _)| *owner == Owner::Incoming(self.task))
            {
                return Err(invalid(Refused::Exit));
            }
            outputs.push((Owner::Incoming(self.task), value));
        }
        let counters = self
            .counters
            .prepare(&image.source_counts)
            .map_err(RegionError::Heap)?;
        let frames = self
            .frames
            .prepare_region_exit(self.entry_code, self.entry_invocation, canonical.frames)
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
                self.limits.memory,
            )
            .map_err(RegionError::Heap)?;
        Ok(Prepared {
            state,
            frames,
            counters,
            registers: self.registers,
            next_registers: canonical.registers,
            charged: self.charged,
            logical_memory: self.logical_memory,
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
            charged,
            logical_memory,
        } = self;
        let mut retired = None;
        let mut old_registers = None;
        state
            .commit(|| {
                retired = Some(frames.publish());
                counters.publish();
                if let Some(next) = next_registers {
                    old_registers = Some(std::mem::replace(registers, next));
                }
                *charged = image.counters.progress;
                logical_memory.set(image.counters.balance);
            })
            .map_err(RegionError::Heap)?;
        drop((retired, old_registers));
        Ok(())
    }
}
