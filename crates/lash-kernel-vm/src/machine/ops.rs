//! The operations: each kernel operation's one description, which the
//! stream's ops execute (`super::stream`).
//!
//! An operation is one machine method with a register-indexed shape: it
//! reads its operands from registers, writes its result to its result
//! register, and ends in one of four ways ([`Status`]):
//!
//! - `Ok(Flow::Done)`: done, its result, if it has one, in its result
//!   register;
//! - `Ok(Flow::Step)`: it ended the task's step where it left the task: it
//!   entered a block, a loop or a frame, or the task waits;
//! - `Err(Interrupt::raised(_))`: it raised;
//! - `Err(Interrupt::Halt(_))`: it ended the run.
//!
//! An operation that cannot step returns [`Eval`] instead. The registers
//! hold the running statement's values: each expression's value goes to
//! one and each operation reads its operands from them. No register lives
//! past its step: a statement that stands at an action nested in its
//! expression keeps its operands as its continuation's roles
//! (`K-EVAL-008`).
//!
//! The operations are the member reads and writes, the charge anchor, the
//! capacity meter's growth checks and the pin anchor, the library call with its charge (the one
//! entry of every library call, inside an expression or in a frame), and
//! the loop's iteration step, here, and action dispatch,
//! [`KernelMachine::act`].

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::heap::{LogicalBytes, NormalKey};
use lash_kernel_doc::{
    Args, NativeCall, NativeError, NewObject, Nursery, Retry, TaskId, Value, WorkCounter,
};

use lash_kernel_state::arena::{Arena, Key};

use super::exec::Call;
use super::operation_entry::AdmittedBody;
use super::{Eval, Halt, Interrupt, KernelMachine, Refusal, bound, fault, raise};
use crate::compile::{Charge, Executable, Lib, LibId, LibRun};
use crate::functions::MachineFunction;
use crate::interface::Bound;
use crate::observe::Event;
use lash_kernel_state::paged::TaskStateRecord;

/// A register: a position in the machine's register stack.
pub(super) type Reg = usize;

/// How an operation that did not raise or halt ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Flow {
    /// Done: the result, if any, is in the result register.
    Done,
    /// The task's step ends where the operation left the task.
    Step,
}

/// How an operation ends (see the module documentation).
pub(super) type Status = Result<Flow, Interrupt>;

/// The list position a value is: a number with an integral value that is
/// not negative (`K-FORM-006`). `None` is a number that is no position.
fn position(index: &Value) -> Eval<Option<usize>> {
    match index {
        Value::Int(word) => Ok(usize::try_from(*word).ok()),
        Value::BigInt(_) => Ok(None),
        Value::Float(float) => {
            let float = float.get();
            Ok(
                (float.fract() == 0.0 && float >= 0.0 && float < usize::MAX as f64)
                    .then_some(float as usize),
            )
        }
        _ => raise("type_error", "a list is indexed by a number"),
    }
}

impl KernelMachine {
    /// Ends the statement's decoding streaks, dropping clean large objects
    /// back to their pages once residency is high, and frees the arena
    /// objects nothing the run holds, once enough wait: at a statement
    /// boundary, where every value the run holds outside the arena's
    /// members is in a frame. Garbage cycles are collected only under
    /// pressure, and only when a count fell without reaching zero since
    /// the last collection (seam D).
    pub(super) fn collect_arena(&mut self) -> Result<(), Halt> {
        self.arena.borrow_mut().statement_boundary();
        let (release, cycles) = {
            let arena = self.arena.borrow();
            (arena.release_due(), arena.cycles_due())
        };
        if !release && !cycles {
            return Ok(());
        }
        let (tasks, session) = (&self.tasks, &self.session);
        let mut roots = |visit: &mut lash_kernel_state::arena::Visit<'_>| {
            tasks.each_value(visit);
            session.each_value(visit);
        };
        let mut arena = self.arena.borrow_mut();
        if cycles {
            arena.collect_cycles(&mut roots);
        }
        arena.release(&mut roots);
        Ok(())
    }

    /// Frees the objects nothing the run holds as it ends (FIG-6791): the
    /// capacity the host reads there is what the run reaches, not where
    /// garbage was last released.
    pub(super) fn release_garbage(&mut self) {
        if self.bounds.memory_meter == crate::MemoryMeter::Off {
            return;
        }
        let (tasks, session) = (&self.tasks, &self.session);
        let mut roots = |visit: &mut lash_kernel_state::arena::Visit<'_>| {
            tasks.each_value(visit);
            session.each_value(visit);
        };
        let mut arena = self.arena.borrow_mut();
        arena.collect_cycles(&mut roots);
        arena.release(&mut roots);
    }

    /// Writes `value` to register `out`, growing the registers to it.
    #[inline(always)]
    pub(super) fn put(&mut self, out: Reg, value: Value) {
        #[cfg(test)]
        {
            self.storage.value_moves += 1;
        }
        self.regs.put(out, value);
    }

    /// Takes the value out of register `r`, which is left null.
    #[inline(always)]
    pub(super) fn take(&mut self, r: Reg) -> Value {
        #[cfg(test)]
        {
            self.storage.value_moves += 1;
        }
        self.regs.take(r, Value::Null)
    }

    /// Runs `op` over the arena. What it reaches cold the machine decodes,
    /// then asks again.
    #[inline]
    pub(super) fn in_arena<T>(
        &mut self,
        mut op: impl FnMut(&mut Arena) -> Result<T, NativeError>,
    ) -> Eval<T> {
        loop {
            let result = op(&mut self.arena.borrow_mut());
            match result {
                Err(NativeError::Retry(Retry::Cold { object, chunk })) => {
                    self.warm(object, chunk)?
                }
                other => return other.map_err(Interrupt::from),
            }
        }
    }

    /// Looks `key` up in the heap's key index of the table `table` when the
    /// arena cannot answer for it, a key only a cold chunk may hold, so a
    /// point read or write of a large table decodes the one chunk holding
    /// its entry (seam D).
    pub(super) fn resolve(&mut self, table: ArenaHandle, key: &Value) -> Eval<()> {
        if let Some(normal) = self.in_arena(|arena| arena.unresolved(table, key))? {
            let resident = self.in_arena(|arena| arena.key(key))?;
            self.resolve_with(table, resident, normal)?;
        }
        Ok(())
    }

    /// [`Self::resolve`] for a field name.
    pub(super) fn resolve_field(
        &mut self,
        table: ArenaHandle,
        field: &crate::compile::Field,
    ) -> Eval<()> {
        if self.in_arena(|arena| arena.unresolved_key(table, &field.key))? {
            self.resolve_with(table, field.key.clone(), field.normal.clone())?;
        }
        Ok(())
    }

    fn resolve_with(&mut self, table: ArenaHandle, key: Key, normal: NormalKey) -> Eval<()> {
        let entity = self
            .arena
            .borrow()
            .facts(table)
            .map_err(Interrupt::from)?
            .entity
            .ok_or(lash_kernel_doc::NativeError::Malformed(
                "a table with cold chunks has no durable id",
            ))
            .map_err(Interrupt::from)?;
        let seq = self
            .heap
            .table_lookup(entity, &normal)
            .map_err(Halt::from)?;
        self.arena
            .borrow_mut()
            .resolved(table, key, seq)
            .map_err(Interrupt::from)
    }

    /// An error value of `kind` and `message`, built in the arena.
    pub(super) fn error_value(&mut self, kind: &'static str, message: impl Into<String>) -> Value {
        *self.thrown(Refusal {
            kind: kind.into(),
            message: message.into(),
            data: None,
        })
    }

    /// A transient census root outside the frame that returned it.
    #[inline]
    pub(super) fn borrow_value(&mut self, value: Value) {
        if self.bounds.memory_meter != crate::MemoryMeter::Off {
            self.pins.borrowed.push(value);
        }
    }

    /// The charge anchor (`K-CHG-002`): `units` more, unless the running
    /// code is not charged (`K-CHG-007`), ending the run past its bound.
    pub(super) fn charge(&mut self, units: u64) -> Result<(), Halt> {
        charge_scheduled(
            &mut self.charged,
            &mut self.bounds.charge_limit,
            &mut self.limiters,
            units,
            self.observer.as_deref(),
        )
    }

    /// A verdict point with `bytes` about to be allocated: a native's
    /// reservation, or what it built beyond what it reserved. What the
    /// run holds and `bytes` that fit pass at once; otherwise the capacity
    /// meter decides (FIG-6791).
    pub(super) fn reserve(&mut self, bytes: u64) -> Result<(), Halt> {
        if self.bounds.memory_meter == crate::MemoryMeter::Off
            || self.capacity_held().saturating_add(bytes) <= self.bounds.memory
        {
            return Ok(());
        }
        self.memory_pressure(bytes)
    }

    /// The capacity meter's check where the arena's storage or the run's
    /// control state grew: one compare, the crossing it makes decided at
    /// the run's next verdict point (FIG-6791).
    #[inline(always)]
    pub(super) fn grown(&mut self) {
        if self.arena.borrow().capacity_due() {
            self.capacity_crossed();
        }
    }

    /// Installs what `new` built. Its mutable objects take durable ids
    /// when a projection saves them.
    fn install(&mut self, new: Vec<(ArenaHandle, NewObject)>) -> Result<(), Halt> {
        if new.is_empty() {
            return Ok(());
        }
        self.arena
            .borrow_mut()
            .install(new)
            .map_err(|error| fault(&error.to_string()))
    }

    /// Allocates `object` outside a native call, counted.
    pub(super) fn new_object(&mut self, object: NewObject) -> Result<Value, Halt> {
        let (value, new) = {
            let arena = self.arena.borrow();
            let mut nursery = Nursery::new(arena.objects(), LogicalBytes(u64::MAX));
            let value = nursery.allocate(object);
            (value, nursery.into_new())
        };
        self.install(new)?;
        self.grown();
        Ok(value)
    }

    /// A fresh shared variable holding `value`, counted.
    pub(super) fn new_cell(&mut self, value: Value) -> ArenaHandle {
        let cell = self.arena.borrow_mut().new_cell(value);
        self.grown();
        cell
    }

    /// The pin anchor: keeps the value in register `r`, which the
    /// statement made, a root of a sweep until the statement ends.
    #[inline(always)]
    pub(super) fn pin(&mut self, r: Reg) -> Result<(), Halt> {
        let value = self.regs[r];
        self.pin_value(&value)
    }

    /// The pin anchor on a value the statement holds outside the
    /// registers.
    pub(super) fn pin_value(&mut self, value: &Value) -> Result<(), Halt> {
        // Immediates own no arena storage: no sweep can free them.
        if self.bounds.memory_meter == crate::MemoryMeter::Off || super::Pins::scalar(value) {
            return Ok(());
        }
        self.pins.root(value);
        Ok(())
    }

    /// Reads field `field` of the record or the error in `target` into
    /// `out` (`K-FORM-009`).
    pub(super) fn read_field(
        &mut self,
        target: Reg,
        field: &crate::compile::Field,
        out: Reg,
    ) -> Eval<()> {
        let value = match self.regs[target] {
            Value::Record(record) => {
                self.resolve_field(record, field)?;
                let slot = field.site.and_then(|site| self.sites.slot(site));
                let (value, found) = self.in_arena(|arena| {
                    crate::compile::find_property(arena, record, slot, &field.key)
                })?;
                if let Some(site) = field.site
                    && found != slot
                {
                    self.sites.keep(site, found);
                }
                value.unwrap_or(Value::Absent)
            }
            Value::Error(error) => {
                let parts = self.in_arena(|arena| match arena.body(error)? {
                    lash_kernel_doc::arena::Body::Error(parts) => Ok(*parts),
                    _ => Err(NativeError::Malformed("an error handle names another body")),
                })?;
                match field.name.as_str() {
                    "kind" => parts.kind,
                    "message" => parts.message,
                    "data" => parts.data,
                    _ => Value::Absent,
                }
            }
            _ => return raise("type_error", "only a record or an error has fields"),
        };
        self.put(out, value);
        Ok(())
    }

    pub(super) fn read_index(
        &mut self,
        target: Reg,
        index: Reg,
        out: Reg,
        site: Option<crate::compile::ReadSite>,
    ) -> Eval<()> {
        let index = self.regs[index];
        let value = match self.regs[target] {
            Value::Tuple(object) | Value::List(object) => {
                let at = position(&index)?;
                let (element, len) = self.in_arena(|arena| {
                    let element = match at {
                        Some(at) => arena.item(object, at as u64)?,
                        None => None,
                    };
                    Ok((element, arena.len(object)?))
                })?;
                match element {
                    Some(value) => value,
                    None => {
                        return raise(
                            "index_out_of_range",
                            format!("no element there; the length is {len}"),
                        );
                    }
                }
            }
            Value::Record(record) => {
                if !matches!(index, Value::Text(_) | Value::LongText(_)) {
                    return raise("type_error", "a record index must be text");
                }
                self.resolve(record, &index)?;
                let Some(site) = site else {
                    let found = self.in_arena(|arena| arena.table_get(record, &index))?;
                    self.put(out, found.unwrap_or(Value::Absent));
                    return Ok(());
                };
                let slot = self.sites.slot(site);
                let (found, at) = self.in_arena(|arena| {
                    crate::compile::find_property(arena, record, slot, &arena.key(&index)?)
                })?;
                if at != slot {
                    self.sites.keep(site, at);
                }
                found.unwrap_or(Value::Absent)
            }
            Value::Map(map) => {
                self.resolve(map, &index)?;
                match self.in_arena(|arena| arena.table_get(map, &index))? {
                    Some(value) => value,
                    None => return raise("key_missing", "the map has no entry under that key"),
                }
            }
            Value::Set(set) => {
                self.resolve(set, &index)?;
                Value::Bool(
                    self.in_arena(|arena| arena.table_get(set, &index))?
                        .is_some(),
                )
            }
            _ => {
                return raise(
                    "type_error",
                    "only a list, a tuple, a record, a map or a set is read by index",
                );
            }
        };
        self.put(out, value);
        Ok(())
    }

    /// Refuses a value that is no legal key (`K-KEY-002`).
    pub(super) fn key(&mut self, value: &Value) -> Eval<()> {
        self.in_arena(|arena| arena.key(value).map(|_| ()))
    }

    /// A record field name's text value.
    fn field_name(&mut self, field: &str) -> Value {
        self.arena.borrow_mut().text(field)
    }

    pub(super) fn write_field(&mut self, target: Reg, field: &str, value: Reg) -> Eval<()> {
        let Value::Record(record) = self.regs[target] else {
            return raise("type_error", "only a record has fields to assign");
        };
        let value = self.take(value);
        let name = self.field_name(field);
        self.resolve(record, &name)?;
        self.in_arena(|arena| arena.table_write(record, name, value))?;
        self.grown();
        Ok(())
    }

    pub(super) fn write_index(&mut self, target: Reg, index: Reg, value: Reg) -> Eval<()> {
        match self.regs[target] {
            Value::List(list) => {
                let length = self.in_arena(|arena| arena.len(list))?;
                let position =
                    position(&self.regs[index])?.filter(|position| *position as u64 <= length);
                let Some(position) = position else {
                    return raise(
                        "index_out_of_range",
                        format!("a list of {length} is assigned at 0 to {length}"),
                    );
                };
                let value = self.take(value);
                self.in_arena(|arena| arena.list_set(list, position as u64, value))?
            }
            Value::Record(record) => {
                let field = self.take(index);
                if !matches!(field, Value::Text(_) | Value::LongText(_)) {
                    return raise("type_error", "a record index must be text");
                }
                self.resolve(record, &field)?;
                let value = self.take(value);
                self.in_arena(|arena| arena.table_write(record, field, value))?
            }
            Value::Map(map) => {
                let key = self.regs[index];
                self.key(&key)?;
                self.resolve(map, &key)?;
                let key = self.take(index);
                let value = self.take(value);
                self.in_arena(|arena| arena.table_write(map, key, value))?
            }
            Value::Set(set) => {
                let member = self.regs[index];
                self.key(&member)?;
                let Value::Bool(present) = self.regs[value] else {
                    return raise(
                        "type_error",
                        "a set's member is assigned `true` to add it or `false` to remove it",
                    );
                };
                self.resolve(set, &member)?;
                let member = self.take(index);
                if present {
                    self.in_arena(|arena| arena.table_write(set, member, Value::Absent))?
                } else {
                    self.in_arena(|arena| arena.table_delete(set, &member))?;
                }
            }
            _ => {
                return raise(
                    "type_error",
                    "only a list, a record, a map or a set is assigned by index",
                );
            }
        }
        self.grown();
        Ok(())
    }

    pub(super) fn remove_field(&mut self, target: Reg, field: &str) -> Eval<()> {
        let Value::Record(record) = self.regs[target] else {
            return raise("type_error", "only a record has fields to remove");
        };
        let name = self.field_name(field);
        self.resolve(record, &name)?;
        self.in_arena(|arena| arena.table_delete(record, &name))
    }

    pub(super) fn remove_index(&mut self, target: Reg, index: Reg) -> Eval<()> {
        let at = self.regs[index];
        match self.regs[target] {
            Value::List(list) => {
                let length = self.in_arena(|arena| arena.len(list))?;
                let position = position(&at)?.filter(|position| (*position as u64) < length);
                let Some(position) = position else {
                    return raise(
                        "index_out_of_range",
                        format!("a list of {length} has no element there to remove"),
                    );
                };
                self.in_arena(|arena| arena.list_remove(list, position as u64))?;
            }
            Value::Record(record) => {
                if !matches!(at, Value::Text(_) | Value::LongText(_)) {
                    return raise("type_error", "a record index must be text");
                }
                self.resolve(record, &at)?;
                self.in_arena(|arena| arena.table_delete(record, &at))?;
            }
            Value::Map(table) | Value::Set(table) => {
                self.key(&at)?;
                self.resolve(table, &at)?;
                self.in_arena(|arena| arena.table_delete(table, &at))?;
            }
            _ => {
                return raise(
                    "type_error",
                    "only a list, a record, a map or a set has an index to remove",
                );
            }
        }
        Ok(())
    }

    /// Calls library function `lib` with the `count` registers from `args`
    /// as its arguments: the one entry of every library call. A native
    /// implementation or a machine function answers at once, charged by
    /// its formula: `Done`, its value in `args`. A function that runs its
    /// kernel body, its native declining or absent, has that body's frame
    /// pushed, its arguments moved in, the caller's statement `awaiting`
    /// waiting in it, if any: `Step`.
    pub(super) fn call_lib_linked(
        &mut self,
        task: TaskId,
        exe: &Executable,
        (lib, link): (LibId, lash_kernel_value::content_ids::LinkId),
        invocation: super::exec::Invocation<'_>,
    ) -> Status {
        let caller = self.active_link;
        self.active_link = link;
        let result = self.call_lib(task, exe, lib, invocation);
        self.active_link = caller;
        result
    }

    pub(super) fn call_lib(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        invocation: super::exec::Invocation<'_>,
    ) -> Status {
        let super::exec::Invocation { args, count, .. } = invocation;
        let code = match self
            .answer(task, exe, lib, args, count, args)
            .map_err(|interrupt| interrupt.answering(exe.lib(lib)))?
        {
            Answer::Done => return Ok(Flow::Done),
            Answer::Body(code) => code,
        };
        self.enter_body(task, exe, lib, code, invocation)
    }

    /// Pushes the frame of library function `lib`'s kernel body `code`,
    /// its arguments the registers from `args`, the caller's statement
    /// `awaiting` waiting in it, if any: `Step`.
    pub(super) fn enter_body(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        admitted: AdmittedBody,
        invocation: super::exec::Invocation<'_>,
    ) -> Status {
        let super::exec::Invocation {
            args,
            awaiting,
            export,
            ..
        } = invocation;
        let function = exe.lib(lib);
        let AdmittedBody { code, entry } = admitted;
        // An argument the call leaves out is absent: the body binds every
        // parameter.
        let call = Call::new(code, args, function.arity)
            .in_link(self.current_link())
            .of_library(lib)
            .with_boundary(export);
        self.push_operation_frame(task, exe, call.awaited_by(awaiting), entry)?;
        Ok(Flow::Step)
    }

    /// Calls `lib`, which answers without a frame, inside an expression,
    /// with the `count` arguments from `args`, its value there.
    pub(super) fn call_answered(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        args: Reg,
        count: usize,
    ) -> Eval<()> {
        match self
            .answer(task, exe, lib, args, count, args)
            .map_err(|interrupt| interrupt.answering(exe.lib(lib)))?
        {
            Answer::Done => Ok(()),
            Answer::Body(_) => Err(fault(&format!(
                "a call of {} that runs its body was lowered as one that answers",
                exe.lib(lib).header.name
            ))
            .into()),
        }
    }

    /// Answers a call of a library function that needs no frame: a native
    /// implementation or one of the machine's own, its value in `out`.
    /// [`Answer::Body`] leaves the arguments from `args`, one per parameter,
    /// for the kernel body that runs the call: the function has no native,
    /// or its native declined.
    pub(super) fn answer(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        args: Reg,
        given: usize,
        out: Reg,
    ) -> Eval<Answer> {
        let function = exe.lib(lib);
        if given > function.arity {
            return raise(
                "arity",
                format!(
                    "`{}` takes {} argument(s); {} given",
                    function.header.name, function.arity, given
                ),
            );
        }
        for absent in args + given..args + function.arity {
            self.put(absent, Value::Absent);
        }
        // An operation's admission refusal names it, whichever
        // implementation would have answered (`K-LIB-006`).
        let entry = function
            .operation()
            .then(|| {
                self.operation_entry(task, exe, function, args)
                    .map_err(|interrupt| interrupt.in_function(&function.header.name))
            })
            .transpose()?;
        let result =
            self.answer_admitted(task, exe, lib, function, args, out, entry.as_ref(), true);
        match result {
            Ok(Answer::Body(mut body)) => {
                body.entry = entry;
                Ok(Answer::Body(body))
            }
            other => other,
        }
    }

    /// An operator's call (`Op::Operator`, or a nested call lowered as
    /// one): its arguments are its parameters', one each, and its value
    /// goes to `out`. Its depth is checked first; its prepared entry
    /// answers when its guard holds, and otherwise its native's general
    /// call, then its plan, then its body runs the call as [`Self::answer`]
    /// would, from the same registers.
    #[inline(always)]
    pub(super) fn operator(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        args: Reg,
        out: Reg,
    ) -> Eval<Answer> {
        let function = exe.lib(lib);
        let entry = if function.operation() {
            Some(
                self.operation_entry(task, exe, function, args)
                    .map_err(|interrupt| interrupt.in_function(&function.header.name))?,
            )
        } else {
            None
        };
        if let Some((prepared, _)) = &function.specialization.prepared
            && self.template_depth_fits(task, prepared.depth(), entry.as_ref())?
            && let Some((value, charges)) = prepared.metered(
                &self.regs[args..args + function.arity],
                exe,
                self.bounds.charge == crate::Charge::Exact,
                self.bounds.charge == crate::Charge::Exact,
            )
        {
            self.observe(Event::NativeEnter { task, lib });
            self.metered_answered(exe, lib, args, value, charges, out)?;
            return Ok(Answer::Done);
        }
        if let Some((prepared, fast)) = &function.specialization.prepared
            && (prepared.depth() <= u32::from(entry.is_some())
                || self.template_depth_fits(task, prepared.depth(), entry.as_ref())?)
            && let Some(value) =
                prepared.answer(&self.regs[args..args + function.arity], self.charging)
        {
            self.observe(Event::NativeEnter { task, lib });
            self.prepared_answered(exe, lib, function, args, out, value, fast)?;
            return Ok(Answer::Done);
        }
        self.operator_missed(task, exe, lib, args, out, entry)
    }

    /// An operator whose prepared entry declined, or that has none.
    #[inline(never)]
    fn operator_missed(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        args: Reg,
        out: Reg,
        entry: Option<super::operation_entry::OperationEntry>,
    ) -> Eval<Answer> {
        let function = exe.lib(lib);
        match self.answer_admitted(task, exe, lib, function, args, out, entry.as_ref(), false)? {
            Answer::Body(mut body) => {
                body.entry = entry;
                Ok(Answer::Body(body))
            }
            other => Ok(other),
        }
    }

    /// What the general call does with a prepared entry's value: keep it,
    /// charge the formula and put it in `out`. `function` is `lib`'s.
    #[expect(
        clippy::too_many_arguments,
        reason = "the caller's lookup of the function travels with its id"
    )]
    #[inline(always)]
    fn prepared_answered(
        &mut self,
        exe: &Executable,
        lib: LibId,
        function: &Lib,
        args: Reg,
        out: Reg,
        answer: lash_kernel_doc::PreparedAnswer,
        fast: &crate::compile::Fast,
    ) -> Eval<()> {
        let end = args + function.arity;
        self.observe(Event::NativeFast);
        let value = answer.value;
        self.pin_value(&value)?;
        if self.charging {
            let units =
                if let Some(units) = answer.scalar_sizes.and_then(|mask| fast.scalar_units(mask)) {
                    units
                } else {
                    match fast.charge(&function.charge, &self.regs[args..end], &value) {
                        Charge::Units(units) => units,
                        Charge::Plan(plan) => {
                            self.warm_formula(plan, args, Some(&value))?;
                            self.in_formula(lib, true, |machine| {
                                machine.formula(plan, &machine.regs[args..end], Some(&value))
                            })?
                        }
                    }
                };
            self.charge_call(exe, lib, units)?;
        }
        self.observe(Event::NativeExit);
        self.put(out, value);
        Ok(())
    }

    fn metered_answered(
        &mut self,
        exe: &Executable,
        lib: LibId,
        args: Reg,
        value: Value,
        charges: crate::compile::MeteredCharges,
        out: Reg,
    ) -> Eval<()> {
        self.observe(Event::NativeFast);
        self.pin_value(&value)?;
        for event in charges.events {
            let lash_kernel_doc::plan::ChargeEvent { units, function } = event;
            self.charge(units).map_err(|halt| {
                let interrupt = Interrupt::from(halt);
                match function.and_then(|function| exe.library_lib(&function)) {
                    Some(id) => interrupt.in_function(&exe.lib(id).header.name),
                    None => interrupt,
                }
            })?;
        }
        if self.bounds.charge == crate::Charge::Off {
            self.charged = self.charged.saturating_add(charges.measurement.pace);
        }
        let function = exe.lib(lib);
        if self.charging && function.header.charge().is_some() {
            self.warm_formula(&function.charge, args, Some(&value))?;
            let units = self.in_formula(lib, true, |machine| {
                machine.call_units(
                    exe,
                    lib,
                    &machine.regs[args..args + function.arity],
                    Some(&value),
                )
            })?;
            self.charge_call(exe, lib, units)?;
        }
        self.observe(Event::NativeExit);
        self.put(out, value);
        Ok(())
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "an operator's miss enters past its prepared entry"
    )]
    #[inline(always)]
    fn answer_admitted(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        function: &Lib,
        args: Reg,
        out: Reg,
        admission: Option<&super::operation_entry::OperationEntry>,
        prepared: bool,
    ) -> Eval<Answer> {
        let end = args + function.arity;
        // A function that runs its body is observed by its frame; one that
        // does not, from here to its charge. A halt ends the run and its
        // observation with it.
        if self.observer.is_some() && !matches!(function.run, LibRun::Body(_)) {
            self.observe(Event::NativeEnter { task, lib });
        }
        if prepared
            && let Some((entry, _)) = &function.specialization.prepared
            && self.template_depth_fits(task, entry.depth(), admission)?
            && let Some((value, charges)) = entry.metered(
                &self.regs[args..end],
                exe,
                self.bounds.charge == crate::Charge::Exact,
                self.bounds.charge == crate::Charge::Exact,
            )
        {
            self.metered_answered(exe, lib, args, value, charges, out)?;
            return Ok(Answer::Done);
        }
        if prepared
            && let Some((entry, fast)) = &function.specialization.prepared
            && (entry.depth() <= u32::from(function.operation())
                || self.template_depth_fits(task, entry.depth(), admission)?)
            && let Some(value) = entry.answer(&self.regs[args..end], self.charging)
        {
            self.prepared_answered(exe, lib, function, args, out, value, fast)?;
            return Ok(Answer::Done);
        }
        let result = match &function.run {
            LibRun::Declaration => {
                return Err(super::fault(
                    "a native-only declaration has no executable implementation",
                )
                .into());
            }
            LibRun::Body(code) => match self.prepared_heap(task, exe, lib, args)? {
                Some(value) => {
                    self.pin_value(&value)?;
                    self.capacity_check()?;
                    Ok(value)
                }
                None => return Ok(Answer::Body((*code).into())),
            },
            LibRun::Machine(MachineFunction::Deref) => match self.regs.get(args).copied() {
                Some(Value::TaskRef(task)) => Ok(Value::Task(task)),
                Some(Value::ObjectRef(object)) => {
                    match self.in_arena(|arena| arena.object_value(object))? {
                        Some(value) => Ok(value),
                        None => Err(self.error_value("type_error", "the ref names no object")),
                    }
                }
                _ => Err(self.error_value("type_error", "`deref` takes a ref")),
            },
            LibRun::Machine(MachineFunction::TasksUnfinished) => {
                let mut unfinished = Vec::new();
                self.task_table()?.visit(&mut |id, other| {
                    if id != task && !matches!(other.state, TaskStateRecord::Ended(_)) {
                        unfinished.push(Value::Task(id));
                    }
                    true
                })?;
                Ok(self.new_object(NewObject::List(unfinished))?)
            }
            // Lowering answers a call written by name in an expression; an
            // action's call reaches no body it is written in.
            LibRun::Machine(MachineFunction::BrandBuilt) => Err(self.error_value(
                "type_error",
                "`brand.built` is called by name in an expression",
            )),
            LibRun::Native { native, body, .. } => {
                let limit = match &function.limit {
                    Some(limit) => {
                        self.warm_formula(limit, args, None)?;
                        Some(self.in_formula(lib, false, |machine| {
                            machine.formula(limit, &machine.regs[args..end], None)
                        })?)
                    }
                    None => None,
                };
                #[cfg(test)]
                if self.pins.force_cold {
                    let given = self.regs[args..end].to_vec();
                    self.chill_args(&given)?;
                }
                let mut granted = 0u64;
                let mut spent;
                let answer = loop {
                    let room = granted.max(self.capacity_headroom());
                    let mut counter = WorkCounter::new(limit);
                    let (answer, built) = {
                        let arena = self.arena.borrow();
                        let semantics = arena.semantics().with_limits(self.limits);
                        let mut nursery = if self.bounds.memory_meter == crate::MemoryMeter::Off {
                            Nursery::unaccounted(arena.objects())
                        } else {
                            Nursery::new(arena.objects(), LogicalBytes(room))
                        };
                        let answer = native.call(NativeCall {
                            args: Args::new(&self.regs[args..end], arena.objects(), &semantics),
                            new: &mut nursery,
                            counter: &mut counter,
                        });
                        match &answer {
                            Ok(_) | Err(NativeError::Raised(_)) => {
                                let held = nursery.held().0;
                                (answer, Some((held, nursery.into_new())))
                            }
                            Err(_) => {
                                nursery.discard();
                                (answer, None)
                            }
                        }
                    };
                    // A call run again does what it did: its last run's work
                    // is the call's.
                    spent = counter.spent();
                    match answer {
                        Err(NativeError::Retry(Retry::Cold { object, chunk })) => {
                            #[cfg(test)]
                            {
                                self.pins.cold_reads += 1;
                            }
                            self.warm(object, chunk)?;
                        }
                        Err(NativeError::Retry(Retry::Room { wanted })) => {
                            if wanted.0 <= granted {
                                return Err(fault("a native asked again for room it holds").into());
                            }
                            self.reserve(wanted.0 - granted)?;
                            granted = wanted.0;
                        }
                        answer => {
                            if let Some((held, new)) = built {
                                self.reserve(held.saturating_sub(granted))?;
                                self.install(new)?;
                            }
                            break answer;
                        }
                    }
                };
                self.observe(Event::GuardWork(spent));
                match answer {
                    Ok(value) => {
                        let depth = self.arena.borrow().summary_of(&value).depth;
                        if depth as usize > self.limits.value_depth() {
                            Err(self.error_value(
                                "too_deep",
                                format!(
                                    "a value exceeds the configured {}-level value depth",
                                    self.limits.value_depth()
                                ),
                            ))
                        } else {
                            // What the call built is counted before its
                            // value is published: pinned, it is a root, and
                            // the meter decides here, in the call.
                            self.pin_value(&value)?;
                            self.capacity_check()?;
                            Ok(value)
                        }
                    }
                    Err(NativeError::Raised(error)) => Err(error),
                    Err(NativeError::Refused { kind, message }) => {
                        Err(self.error_value(kind, message))
                    }
                    Err(NativeError::Guard(guard)) => {
                        return Err(bound(
                            Bound::Guard {
                                function: function.specialization.function,
                            },
                            guard.limit,
                        )
                        .into());
                    }
                    Err(NativeError::Malformed(problem)) => return Err(fault(problem).into()),
                    Err(NativeError::Retry(_)) => {
                        return Err(fault("a native's retry left its call").into());
                    }
                    // The body runs the call, charged by the formula when
                    // its frame is left (`K-CHG-007`).
                    Err(NativeError::Decline) => {
                        let Some(body) = body else {
                            return Err(fault("a native with no kernel body declined").into());
                        };
                        match self.prepared_heap(task, exe, lib, args)? {
                            Some(value) => {
                                self.pin_value(&value)?;
                                self.capacity_check()?;
                                Ok(value)
                            }
                            None => {
                                self.observe(Event::NativeExit);
                                return Ok(Answer::Body((*body).into()));
                            }
                        }
                    }
                }
            }
        };
        if self.charging {
            self.warm_formula(&function.charge, args, result.as_ref().ok())?;
            let units = self.in_formula(lib, true, |machine| {
                machine.call_units(exe, lib, &machine.regs[args..end], result.as_ref().ok())
            })?;
            self.charge_call(exe, lib, units)?;
        }
        self.observe(Event::NativeExit);
        match result {
            Ok(value) => {
                self.put(out, value);
                Ok(Answer::Done)
            }
            Err(value) => Err(Interrupt::raised(value)),
        }
    }
}

/// How [`KernelMachine::answer`] left a library call.
pub(super) enum Answer {
    /// Its value is in the call's result register.
    Done,
    /// The kernel body runs the call.
    Body(AdmittedBody),
}

/// A declared schedule event is independent of ordinary body-form charging.
#[inline(always)]
pub(super) fn charge_scheduled(
    charged: &mut u64,
    limit: &mut u64,
    limiters: &mut super::Limiters,
    units: u64,
    observer: Option<&crate::observe::Observer>,
) -> Result<(), Halt> {
    if let Some(observer) = observer {
        observe_scheduled(observer, units);
    }
    if units == 0 {
        return Ok(());
    }
    *charged = charged.saturating_add(units);
    if *charged > *limit {
        return limiters.charge_reached(*charged, limit);
    }
    Ok(())
}

/// Charge diagnostics are entirely cold for an unobserved run.
#[cold]
#[inline(never)]
fn observe_scheduled(observer: &crate::observe::Observer, units: u64) {
    #[cfg(any(test, feature = "perf-witness"))]
    if observer.work_witness {
        crate::perf_witness::record(|work| work.charge_work += 1);
    }
    if units != 0
        && let Some(tape) = observer.charges.as_ref()
    {
        tape.push(lash_kernel_exec::Weight::Atomic(units));
    }
}
