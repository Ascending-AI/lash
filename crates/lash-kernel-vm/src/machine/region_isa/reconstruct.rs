use super::code::Register;
use super::storage::{Graph, View};
use super::*;
use lash_kernel_state::kernel_internals::logical::Slot;

#[derive(Clone, Debug)]
pub(super) struct FrameImage<T = Value> {
    pub(super) program: CodeId,
    pub(super) invocation: u64,
    pub(super) pc: u32,
    pub(super) continuation: Point,
    pub(super) roles: Box<[(Slot, T)]>,
    pub(super) operands: Box<[T]>,
    pub(super) pending: Option<T>,
}

pub(super) struct Finished<V: View> {
    pub(super) image: Reconstruction,
    pub(super) live: super::live::Live<V::Value>,
    pub(super) view: V,
    pub(super) graph: V::Graph,
    pub(super) invocations: Option<lash_kernel_state::kernel_internals::logical::InvocationRange>,
}

impl<V: View> Execution<V> {
    pub(super) fn reconstruct(mut self) -> Result<Finished<V>, RegionError> {
        if self.faulted {
            return Err(invalid(Refused::Facts));
        }
        let stop = self.stop.ok_or_else(|| invalid(Refused::Exit))?;
        let current = self.frames.last().ok_or_else(|| invalid(Refused::Facts))?;
        if current.program != self.reached.program
            || current.invocation != self.reached.invocation
            || current.prefix != self.reached.prefix
            || self.counters.debt != self.reached.debt
        {
            return Err(invalid(Refused::Facts));
        }
        let graph = self.view.freeze_graph().map_err(RegionError::Heap)?;
        let live_frames = self
            .frames
            .iter()
            .enumerate()
            .map(|(index, frame)| {
                let map = self
                    .programs
                    .code(frame.program)?
                    .recovery
                    .get(frame.prefix as usize)
                    .ok_or_else(|| invalid(Refused::Exit))?;
                let read = |reg: Register| self.registers.read(index, frame.invocation, reg);
                let roles = map
                    .roles
                    .iter()
                    .map(|(slot, reg)| Ok((*slot, read(*reg)?)))
                    .collect::<Result<Vec<_>, RegionError>>()?
                    .into_boxed_slice();
                let operands = map
                    .operands
                    .iter()
                    .map(|reg| read(*reg))
                    .collect::<Result<Vec<_>, RegionError>>()?
                    .into_boxed_slice();
                Ok(FrameImage {
                    program: frame.program,
                    invocation: frame.invocation,
                    pc: frame.pc,
                    continuation: map.continuation.clone(),
                    roles,
                    operands,
                    pending: if map.incoming {
                        if map.pending.is_some() || index + 1 != self.frames.len() {
                            return Err(invalid(Refused::Exit));
                        }
                        Some(super::operands::Operand::Dynamic(
                            self.returning
                                .as_ref()
                                .ok_or_else(|| invalid(Refused::Exit))?
                                .incoming()?,
                        ))
                    } else {
                        map.pending.map(read).transpose()?
                    },
                })
            })
            .collect::<Result<Vec<_>, RegionError>>()?
            .into_boxed_slice();
        let live = super::live::Live {
            frames: live_frames,
            payload: self.payload,
            returning: self.returning.as_ref().map(|returning| returning.image()),
        };
        let frames = live
            .frames
            .iter()
            .map(|frame| frame.materialize(&graph))
            .collect::<Result<Box<[_]>, RegionError>>()?;
        let pending = frames.last().and_then(|frame| frame.pending);
        let returning = self
            .returning
            .as_ref()
            .map(|returning| {
                let image = returning.image();
                Ok::<_, RegionError>(returns::ReturnImage {
                    callee_invocation: image.callee_invocation,
                    completion: image.completion,
                    caller_program: image.caller_program,
                    site: image.site,
                    value: graph.value(image.value).map_err(RegionError::Heap)?,
                    phase: image.phase,
                    resume_pc: image.resume_pc,
                })
            })
            .transpose()?;
        let (issued_invocations, invocations) = self.invocations.finish()?;
        let reconstruction = Reconstruction {
            issued_invocations,
            prefix: self.reached,
            frames,
            pending,
            attempted: self.attempted,
            counters: self.counters,
            stop,
            payload: self
                .payload
                .map(|value| graph.value(value).map_err(RegionError::Heap))
                .transpose()?,
            source_counts: self.source.freeze(),
            entering: self.entering,
            returning,
            canonical: None,
        };
        Ok(Finished {
            image: reconstruction,
            live,
            view: self.view,
            graph,
            invocations,
        })
    }
}

impl KernelMachine {
    /// Rebind ephemeral ISA state from the restored canonical frames only.
    pub(in crate::machine) fn reconstruct_canonical_region(
        &mut self,
    ) -> Result<(), lash_kernel_doc::heap::HeapFault> {
        Err(lash_kernel_doc::heap::HeapFault::Malformed(
            "canonical ISA frame reconstruction is not implemented".into(),
        ))
    }
}
