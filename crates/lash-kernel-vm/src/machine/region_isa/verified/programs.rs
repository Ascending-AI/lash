//! Compile each reachable callee once. Recursive edges retain code indexes.
use super::super::*;
use super::Code;
use crate::compile::region_isa::{compile_region, verify_region};
use crate::compile::room::region::calls::region_isa::compile_calls;

pub(super) struct Programs {
    codes: Box<[Option<Arc<Code>>]>,
    calls: Box<[Option<super::call_bind::Calls>]>,
    pub(super) registers: usize,
    pub(super) arguments: usize,
    pub(super) transfer_width: usize,
    pub(super) quotes: usize,
    pub(super) loops: usize,
    pub(super) parameters: usize,
    pub(super) captures: usize,
}

impl Programs {
    pub(super) fn bind(entry: &Arc<Code>) -> Result<Arc<Self>, RegionError> {
        let exe = &entry.exe;
        let mut codes = Vec::new();
        codes
            .try_reserve_exact(exe.code_count())
            .map_err(|_| invalid(Refused::Scratch))?;
        codes.resize_with(exe.code_count(), || None);
        let slot = codes
            .get_mut(entry.program.code().0 as usize)
            .ok_or_else(|| invalid(Refused::Source))?;
        *slot = Some(Arc::clone(entry));
        let mut pending = Vec::new();
        pending
            .try_reserve_exact(exe.code_count())
            .map_err(|_| invalid(Refused::Scratch))?;
        pending.push(Arc::clone(entry));
        let mut result = Self {
            codes: Box::new([]),
            calls: Box::new([]),
            registers: 0,
            arguments: 0,
            transfer_width: 0,
            quotes: 0,
            loops: 0,
            parameters: 0,
            captures: 0,
        };
        while let Some(code) = pending.pop() {
            result.registers = result.registers.max(code.program.registers().len());
            result.arguments = result.arguments.max(code.arguments);
            result.captures = result.captures.max(code.captures);
            result.transfer_width = result.transfer_width.max(code.transfer_width);
            result.quotes = result.quotes.max(code.quotes);
            let mut loops = 0;
            while code.program.loop_info(loops).is_some() {
                loops = loops
                    .checked_add(1)
                    .ok_or_else(|| invalid(Refused::Facts))?;
            }
            result.loops = result.loops.max(loops as usize);
            for call in code.calls.iter().flatten() {
                for target in call.plan.targets() {
                    let slot = codes
                        .get_mut(target.code().0 as usize)
                        .ok_or_else(|| invalid(Refused::Source))?;
                    if slot.is_some() {
                        continue;
                    }
                    let draft = compile_region(exe, target.code()).map_err(invalid)?;
                    let calls = compile_calls(exe, &draft).map_err(invalid)?;
                    let program =
                        verify_region(exe, target.code(), draft, &calls).map_err(invalid)?;
                    let bound = Code::bind_executable(exe, &program)?;
                    *slot = Some(Arc::clone(&bound));
                    pending.push(bound);
                }
            }
        }
        result.calls = codes
            .iter()
            .map(|code| {
                code.as_ref()
                    .map(|code| super::call_bind::Calls::bind(code, &codes))
                    .transpose()
            })
            .collect::<Result<_, RegionError>>()?;
        result.parameters = result
            .calls
            .iter()
            .flatten()
            .map(super::call_bind::Calls::width)
            .max()
            .unwrap_or(0);
        result.captures = result.captures.max(
            result
                .calls
                .iter()
                .flatten()
                .map(super::call_bind::Calls::captures)
                .max()
                .unwrap_or(0),
        );
        result.codes = codes.into();
        Ok(Arc::new(result))
    }

    pub(super) fn code(&self, id: CodeId) -> Result<&Arc<Code>, RegionError> {
        self.codes
            .get(id.0 as usize)
            .and_then(Option::as_ref)
            .ok_or_else(|| invalid(Refused::Source))
    }

    pub(super) fn target(
        &self,
        caller: CodeId,
        site: u32,
        target: CodeId,
    ) -> Result<&super::call_bind::Target, RegionError> {
        self.calls
            .get(caller.0 as usize)
            .and_then(Option::as_ref)
            .ok_or_else(|| invalid(Refused::Source))?
            .target(site, target)
    }
}
