//! All call edges index one immutable code registry; recursive edges own no Arc.
use super::*;
use crate::compile::CodeId;

pub(super) struct Programs {
    codes: Box<[Arc<Code>]>,
    pub(super) registers: usize,
    pub(super) arguments: usize,
}
impl Programs {
    pub(super) fn bind(codes: Box<[Arc<Code>]>) -> Result<Arc<Self>, RegionError> {
        let first = codes.first().ok_or_else(|| invalid(Refused::Source))?;
        let registers = codes
            .iter()
            .map(|code| code.registers)
            .max()
            .ok_or_else(|| invalid(Refused::Facts))?;
        for (index, code) in codes.iter().enumerate() {
            if code.id.0 as usize != index
                || code.source_initial != first.source_initial
                || code.instructions.get(code.entry as usize).is_none()
                || code.recovery.get(code.entry_prefix as usize).is_none()
            {
                return Err(invalid(Refused::Facts));
            }
            for call in &code.calls {
                let target = codes
                    .get(call.target.0 as usize)
                    .ok_or_else(|| invalid(Refused::Source))?;
                if target.instructions.get(call.entry as usize).is_none()
                    || target.recovery.get(call.prefix as usize).is_none()
                    || [
                        call.before_prefix,
                        call.leave_prefix,
                        call.completion_prefix,
                        call.restore_prefix,
                    ]
                    .iter()
                    .any(|prefix| code.recovery.get(*prefix as usize).is_none())
                {
                    return Err(invalid(Refused::Facts));
                }
            }
        }
        let arguments = codes
            .iter()
            .flat_map(|code| code.heap.iter())
            .map(|action| action.args.len())
            .max()
            .unwrap_or(0);
        Ok(Arc::new(Self {
            codes,
            registers,
            arguments,
        }))
    }
    pub(super) fn code(&self, code: CodeId) -> Result<&Arc<Code>, RegionError> {
        self.codes
            .get(code.0 as usize)
            .ok_or_else(|| invalid(Refused::Source))
    }
}
