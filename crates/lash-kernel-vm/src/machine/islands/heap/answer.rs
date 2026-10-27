//! Immediate partial calls carry a value; buffering and heap work keep
//! their own physical frames. Source admission and completion are shared.
use super::super::compiled::CallSite;
use super::*;
use crate::compile::LibId;

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    #[inline]
    pub(in crate::machine::islands) fn call_answer(
        &mut self,
        site: &CallSite,
        (args, inputs): (&[u32], Option<&[Value]>),
        regs: &mut [Value],
        before: u64,
        root_occurrence: Option<usize>,
    ) -> Result<bool, Stop> {
        let (Some(lib), Some(extra)) = (site.library, site.answers) else {
            return Ok(false);
        };
        let occurrence = site.occurrence.or(root_occurrence);
        // Refuse the source entry before a partial answer can allocate.
        if (self.accounting && !self.charging)
            || !self.root_fits(before, 0, self.accounting)
            || args.len() > MAX_ARGS
            || occurrence.is_some_and(|at| !self.counters.can_advance(&[at]))
        {
            return Ok(false);
        }
        if let Some(values) = inputs {
            self.call_answer_values(
                site,
                (lib, extra),
                &values[..args.len()],
                regs,
                (before, occurrence),
            )
        } else {
            self.gathered_call_answer(site, (lib, extra), args, regs, (before, occurrence))
        }
    }

    #[inline(never)]
    fn gathered_call_answer(
        &mut self,
        site: &CallSite,
        library: (LibId, u32),
        args: &[u32],
        regs: &mut [Value],
        edge: (u64, Option<usize>),
    ) -> Result<bool, Stop> {
        let mut values = [Value::Absent; MAX_ARGS];
        for (value, reg) in values.iter_mut().zip(args) {
            *value = regs[*reg as usize];
        }
        self.call_answer_values(site, library, &values[..args.len()], regs, edge)
    }

    // FIG-7441: keep the scalar answer in the caller; heap owners stay outlined.
    #[inline(always)]
    fn call_answer_values(
        &mut self,
        site: &CallSite,
        (lib, extra): (LibId, u32),
        args: &[Value],
        regs: &mut [Value],
        edge: (u64, Option<usize>),
    ) -> Result<bool, Stop> {
        if !self.accounting {
            let function = self.exe.lib(lib);
            let Some((prepared, _)) = function.specialization.prepared.as_ref() else {
                return Ok(false);
            };
            let running = self.base + u64::from(self.depth);
            if (function.operation() && running > self.call_depth)
                || running.saturating_add(u64::from(extra)) > self.call_depth
            {
                return Ok(false);
            }
            if let Some(value) = prepared.call(args)
                && (!site.checked
                    || crate::compile::import_result_fits(&function.header.signature.result, value))
            {
                return self.finish_call_answer(site, regs, edge, |_| value);
            }
        }
        self.heap_call_answer_values(site, (lib, extra), args, regs, edge)
    }

    #[inline(never)]
    fn heap_call_answer_values(
        &mut self,
        site: &CallSite,
        (lib, extra): (LibId, u32),
        args: &[Value],
        regs: &mut [Value],
        (before, occurrence): (u64, Option<usize>),
    ) -> Result<bool, Stop> {
        let answer = if self.accounting {
            self.heap_answer(
                lib,
                args,
                Some(extra),
                if site.checked {
                    Check::Result
                } else {
                    Check::None
                },
                (before, false),
                occurrence,
            )
        } else {
            let function = self.exe.lib(lib);
            let Some((prepared, _)) = function.specialization.prepared.as_ref() else {
                return Ok(false);
            };
            let Some((heap, callees, natives)) = prepared.heap(&|id| self.exe.library_lib(id))
            else {
                return Ok(false);
            };
            let recording = self.recorder.is_some();
            self.heap_at(occurrence, 0, true, |cx| {
                let (value, charges) = cx.heap_paths(heap, callees, natives, args)?;
                if site.checked
                    && !crate::compile::import_result_fits(&function.header.signature.result, value)
                {
                    return None;
                }
                // The enclosing formula covers library calls, but ordinary
                // closures and declared loop tests still bill their work.
                // A recording retains their individual source events.
                if recording && (!charges.events.is_empty() || charges.pace != 0) {
                    return None;
                }
                cx.body_paths = charges.pace != 0;
                cx.meter.charges(charges, false, None);
                Some(value)
            })
        };
        let Some(answer) = answer else {
            return Ok(false);
        };
        self.finish_call_answer(site, regs, (before, occurrence), |island| {
            island.commit_heap(answer)
        })
    }

    #[inline]
    fn finish_call_answer(
        &mut self,
        site: &CallSite,
        regs: &mut [Value],
        (before, occurrence): (u64, Option<usize>),
        finish: impl FnOnce(&mut Self) -> Value,
    ) -> Result<bool, Stop> {
        if let Some(at) = occurrence {
            self.count(at)?;
        }
        self.root_arguments(before);
        regs[site.reg as usize] = finish(self);
        if site.after != 0 {
            self.admit(site.after);
        }
        Ok(true)
    }
}
