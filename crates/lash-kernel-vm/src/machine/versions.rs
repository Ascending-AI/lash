//! Guarded native instructions of the selected numeric block version.

use lash_kernel_doc::TaskId;

use super::{Eval, KernelMachine};
use crate::compile::{Executable, LibId};
use crate::heap::within_depth;
use crate::observe::Event;

impl KernelMachine {
    /// A native's record-read capability, guarded at each expression site.
    /// Neither the hint nor its hit retains an object or enters a checkpoint.
    pub(super) fn record_answer(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        args: usize,
        given: usize,
        hint: &crate::compile::ReadHint,
    ) -> Eval<bool> {
        let function = exe.lib(lib);
        let code = match function.run {
            crate::compile::LibRun::Body(code)
            | crate::compile::LibRun::Native {
                body: Some(code), ..
            } => exe.code(code),
            _ => return Ok(false),
        };
        if code.params.iter().any(|param| param.shared) {
            return Ok(false);
        }
        let Some((marker, units)) = function.record_read else {
            return Ok(false);
        };
        if given != function.arity {
            return Ok(false);
        }
        let entry = self.operation_entry(task, exe, lib, args)?;
        let result = self.record_admitted(task, exe, lib, args, given, hint, marker, units);
        self.release_operation(entry);
        result
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "guarded call descriptor is already validated at operation admission"
    )]
    fn record_admitted(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        args: usize,
        given: usize,
        hint: &crate::compile::ReadHint,
        marker: &str,
        units: u64,
    ) -> Eval<bool> {
        use lash_kernel_doc::heap::FieldName;
        use lash_kernel_doc::{NativeHeap, Value};
        let [Value::Record(record), Value::Text(key)] = &self.regs[args..args + given] else {
            return Ok(false);
        };
        if matches!(self.heap.record_get(*record, FieldName::Static(marker))?,
            Some(Value::Tuple(tuple)) if tuple.summary.len == 0)
        {
            return Ok(false);
        }
        let value = hint
            .read(&self.heap, *record, FieldName::Text(key))?
            .unwrap_or(Value::Absent);
        if !within_depth(&value, self.limits.value_depth()) {
            return Ok(false);
        }
        self.observe(Event::NativeEnter { task, lib });
        self.observe(Event::NativeFast);
        self.charge_call(exe, lib, if self.charging { units } else { 0 })?;
        self.observe(Event::NativeExit);
        self.put(args, value);
        Ok(true)
    }

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
        let code = match function.run {
            crate::compile::LibRun::Body(code)
            | crate::compile::LibRun::Native {
                body: Some(code), ..
            } => exe.code(code),
            _ => return Ok(false),
        };
        if code.params.iter().any(|param| param.shared) {
            return Ok(false);
        }
        if !self.scalar_blocks || given != function.arity {
            return Ok(false);
        }
        let Some(scalar) = function.scalar else {
            return Ok(false);
        };
        let entry = self.operation_entry(task, exe, lib, args)?;
        let result = self.scalar_admitted(task, exe, lib, args, given, out, scalar);
        self.release_operation(entry);
        result
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "guarded call descriptor is already validated at operation admission"
    )]
    fn scalar_admitted(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        args: usize,
        given: usize,
        out: usize,
        scalar: crate::compile::ScalarCall,
    ) -> Eval<bool> {
        let function = exe.lib(lib);
        let Some(value) = scalar
            .answer(&self.regs[args..args + given])
            .filter(|value| within_depth(value, self.limits.value_depth()))
        else {
            return Ok(false);
        };
        self.observe(Event::NativeEnter { task, lib });
        self.observe(Event::NativeFast);
        if !function.existing_result {
            self.pin_value(&value)?;
        }
        let units = if self.charging && function.native {
            scalar.units
        } else {
            0
        };
        self.charge_call(exe, lib, units)?;
        self.observe(Event::NativeExit);
        self.put(out, value);
        Ok(true)
    }
}
