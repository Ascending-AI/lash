//! Guarded native instructions of the selected numeric block version.

use lash_kernel_doc::TaskId;

use super::{Eval, KernelMachine};
use crate::compile::{Executable, LibId};
use crate::heap::within_depth;
use crate::observe::Event;

impl KernelMachine {
    pub(super) fn scalar_answer(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        args: usize,
        given: usize,
        out: usize,
    ) -> Eval<bool> {
        let function = exe.lib(lib);
        if !self.scalar_blocks || given != function.arity {
            return Ok(false);
        }
        let Some(scalar) = function.scalar else {
            return Ok(false);
        };
        let Some(value) = scalar
            .answer(&self.regs[args..args + given])
            .filter(|value| within_depth(value, self.limits.value_depth()))
        else {
            return Ok(false);
        };
        self.observe(Event::NativeEnter { task, lib });
        self.observe(Event::NativeFast);
        self.pin_value(&value)?;
        let units = if self.charging && function.native {
            scalar.units
        } else {
            0
        };
        self.charge_call(exe, lib, units)?;
        self.observe(Event::NativeExit);
        self.put(out, value);
        #[cfg(test)]
        {
            self.storage.scalar_calls += 1;
        }
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn scalar_version_calls(&self) -> u64 {
        self.storage.scalar_calls
    }
    #[cfg(test)]
    pub(crate) fn fused_list_items(&self) -> u64 {
        self.storage.fused_list_items
    }
}
