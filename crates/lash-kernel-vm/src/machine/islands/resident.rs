//! A helper's existing heap template may stage one resident record write.
//! Its body still supplies every guard and result; unsupported work declines
//! before any mutation, and the caller admits the formula before publishing.

use super::*;
use lash_kernel_doc::plan::{Collection, FormCharge, Table, TemplateHeap, TemplateMeter};
use lash_kernel_value::names::FunctionId;

struct Write<'a> {
    arena: &'a Arena,
    limits: KernelDataLimits,
    absent: &'a mut Option<cx::Absent>,
    pending: Option<(ArenaHandle, Value, Value)>,
    declined: bool,
}

impl Write<'_> {
    fn decline<T>(&mut self) -> Option<T> {
        self.declined = true;
        None
    }
}

impl TemplateMeter for Write<'_> {
    fn mark(&self) -> usize {
        usize::from(self.declined || self.pending.is_some())
    }
    fn rewind(&mut self, mark: usize) {
        self.pending = None;
        self.declined = mark != 0;
    }
    fn form(&mut self, _: FunctionId, _: FormCharge) {
        self.declined = true;
    }
    fn bill(&mut self, _: FunctionId, _: &[Value], _: Value) {
        self.declined = true;
    }
}

impl TemplateHeap for Write<'_> {
    fn call(&mut self, _: usize, _: &[Value]) -> Option<Value> {
        self.decline()
    }
    fn bind(&mut self, _: Value, _: Option<Value>) -> Option<()> {
        if self.pending.is_some() {
            return self.decline();
        }
        Some(())
    }
    fn path(&mut self) {
        self.pending = None;
        self.declined = false;
    }
    fn index(&mut self, _: Value, _: Value) -> Option<Value> {
        self.decline()
    }
    fn field(&mut self, _: Value, _: &str) -> Option<Value> {
        self.decline()
    }
    fn tuple(&mut self, _: &[Value]) -> Option<Value> {
        self.decline()
    }
    fn set_index(&mut self, target: Value, index: Value, value: Value) -> Option<()> {
        if self.pending.is_some() {
            return self.decline();
        }
        let (Value::Record(record), Value::Text(_)) = (target, index) else {
            return self.decline();
        };
        self.pending = Some((record, index, value));
        Some(())
    }
    fn len(&mut self, _: Collection, _: Value) -> Option<Value> {
        self.decline()
    }
    fn lookup(&mut self, table: Table, target: Value, key: Value) -> Option<Option<Value>> {
        if self.pending.is_some() {
            return self.decline();
        }
        cx::probe(self.arena, self.limits, self.absent, table, target, key)
    }
}

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    /// Run the template's guards into a staged write, with no heap context,
    /// argument window or event vector. Only an arena-independent formula
    /// qualifies: the pending write cannot change any of its measurements.
    pub(super) fn resident_write(
        &mut self,
        function: &Lib,
        args: &[Value],
        before: u64,
        checked: bool,
    ) -> Option<super::heap::HeapAnswer> {
        if !matches!(function.run, LibRun::Body(_))
            || self.deferred_end.is_some()
            || self.arena.any_cold()
            || !self.resident_room(2 * lash_kernel_state::arena::VALUE_BYTES)
        {
            return None;
        }
        let (entry, fast) = function.prepared.as_ref()?;
        let (paths, _, natives) = entry.heap(&|id| self.exe.library_lib(id))?;
        if !paths.writes || !matches!(args.first(), Some(Value::Record(_))) {
            return None;
        }
        let mut write = Write {
            arena: self.arena,
            limits: self.limits,
            absent: &mut self.scratch.absent,
            pending: None,
            declined: false,
        };
        let value = (paths.entry)(&mut write, args, natives)?;
        if write.declined {
            return None;
        }
        let (record, key, written) = write.pending?;
        let units = if self.charging {
            fast.immediate(args, Fresh::default(), &value)?
        } else {
            0
        };
        let end = self.charged.checked_add(before)?.checked_add(units)?;
        if end > self.bound
            || end >= self.slice_end
            || (checked
                && !crate::compile::import_result_fits(&function.header.signature.result, value))
        {
            return None;
        }
        self.arena.table_write(record, key, written).ok()?;
        Some(super::heap::HeapAnswer::resident(value, units))
    }
}
