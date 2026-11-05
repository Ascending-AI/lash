use super::code::Register;
use super::*;
use lash_kernel_state::kernel_internals::logical::Slot;

#[derive(Clone, Debug)]
pub(super) struct FrameImage {
    pub(super) program: CodeId,
    pub(super) invocation: u64,
    pub(super) pc: u32,
    pub(super) continuation: Point,
    pub(super) roles: Box<[(Slot, Value)]>,
    pub(super) operands: Box<[Value]>,
    pub(super) pending: Option<Value>,
}

impl Execution {
    pub(super) fn reconstruct(self) -> Result<Reconstruction, RegionError> {
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
        let frames = self
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
                        Some(
                            self.returning
                                .as_ref()
                                .ok_or_else(|| invalid(Refused::Exit))?
                                .incoming()?,
                        )
                    } else {
                        map.pending.map(read).transpose()?
                    },
                })
            })
            .collect::<Result<Vec<_>, RegionError>>()?
            .into_boxed_slice();
        let pending = frames.last().and_then(|frame| frame.pending);
        Ok(Reconstruction {
            prefix: self.reached,
            frames,
            pending,
            attempted: self.attempted,
            counters: self.counters,
            stop,
            payload: self.payload,
            source_counts: self.source.freeze(),
            entering: self.entering,
            returning: self.returning.as_ref().map(returns::Returning::image),
        })
    }
}
