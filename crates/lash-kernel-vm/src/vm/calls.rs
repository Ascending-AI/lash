//! Calls (cutover contract §1, §2, §3): frame calls of units and values,
//! the native-call ABI and the machine's own functions.
//!
//! A native reads its arguments through zero-copy [`Args`] views over the
//! heap's bodies and builds into a [`Nursery`] the machine installs after
//! it returns; what it built is admitted under the memory schedule, after
//! one collection, or the run ends at the memory bound. A Decline runs the
//! definition's body (`K-LIB-006`), covered, its formula billed when its
//! frame is left; it creates no step.
//!
//! Completion formulas are dynamic fees (`K-CHG-003`): a call bills its
//! formula once, at return, or at raise with a result of size 0.

use lash_kernel_doc::Value;
use lash_kernel_doc::WorkCounter;
use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::arena::native::{
    Args, InlineOperation, IntegerTextDomain, NativeCall, NativeError, NewObject, Nursery, Retry,
    ScalarDomain,
};
use lash_kernel_doc::heap::LogicalBytes;

use super::frames::{Bill, Entry};
use super::{Exceeded, Flow, Restart, Run, Stop, Vm, fault, read};
use crate::Bound;
use crate::bytecode::op::Op;
use crate::bytecode::program::{Adapt, Callee, Checked, NativeEntry, Program, Unit};
use crate::functions::MachineFunction;
use crate::heap::Heap;

/// How a native call that ran ended, before its completion bills.
#[derive(Debug)]
pub(crate) enum Done {
    Value(Value),
    Raised(Value),
    Refused(&'static str, String),
}

impl Done {
    /// The value it holds, for the collector.
    pub(super) fn roots(&self, visit: &mut dyn FnMut(&Value)) {
        match self {
            Self::Value(value) | Self::Raised(value) => visit(value),
            Self::Refused(..) => {}
        }
    }
}

/// The direct-call guard: what the last closure a value call entered
/// resolved to. It holds while the arena has freed nothing, so no handle
/// was freed or given to another object and a closure's identity is
/// the one read. Globals are bound before any call can populate the
/// memo. The program's tables are the instance's for its life.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Direct {
    closure: ArenaHandle,
    freed: u64,
    globals: usize,
    params: u16,
    entry: Entry,
    leaf: Leaf,
}

#[cfg(test)]
thread_local! {
    static PLAIN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static RETRIES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static PROJECTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static LEAVES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static HITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn without_memo<R>(f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            PLAIN.with(|plain| plain.set(self.0));
        }
    }
    let _restore = Restore(PLAIN.with(|plain| plain.replace(true)));
    f()
}

#[cfg(test)]
pub(crate) fn hits() -> u64 {
    HITS.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn leaves() -> u64 {
    LEAVES.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn retries() -> u64 {
    RETRIES.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn projections() -> u64 {
    PROJECTS.with(std::cell::Cell::get)
}

fn small_leaf(program: &Program, unit: &Unit) -> bool {
    let end = unit
        .inlines
        .iter()
        .map(|site| site.entry as usize)
        .min()
        .unwrap_or(unit.ops.len());
    end <= 64
        && unit.boxed.is_empty()
        && unit.cleanup.is_none()
        && unit.ops[..end].iter().all(|op| match *op {
            Op::Block(_)
            | Op::Trace(_)
            | Op::Local(_)
            | Op::SetLocal(_)
            | Op::Pop
            | Op::CaptureGet(_)
            | Op::CaptureSet(_)
            | Op::CellGet(_)
            | Op::CellSet(_)
            | Op::Return
            | Op::Fall
            | Op::Nop => true,
            Op::Const(n) => program.readable(n),
            Op::Inline(n) => {
                let site = &unit.inlines[n as usize];
                site.folds.0 < site.folds.1
            }
            _ => false,
        })
}

#[derive(Clone, Copy, Debug)]
enum Leaf {
    Plain,
    Ops,
    Project {
        block: u32,
        value: crate::bytecode::program::Operand,
    },
}

fn leaf_of(program: &Program, unit: &Unit) -> Leaf {
    use crate::bytecode::program::Operand;
    if !small_leaf(program, unit) {
        return Leaf::Plain;
    }
    match unit.ops.as_slice() {
        [Op::Block(block), Op::Const(n), Op::Return, ..] => Leaf::Project {
            block: *block,
            value: Operand::Const(*n),
        },
        [Op::Block(block), Op::Local(n), Op::Return, ..] => Leaf::Project {
            block: *block,
            value: Operand::Local(*n),
        },
        _ => Leaf::Ops,
    }
}

impl<H: Heap> Vm<H> {
    /// Runs a bounded leaf on its original ops and real frame. An unpaid
    /// op goes back to dispatch before its start; a cold read leaves pc
    /// past the op, as dispatch does, so resume suppresses a second start.
    fn leaf_run(&mut self, program: &Program, entry: Entry, checked: bool) -> Run<()> {
        if self.watch.is_some() || self.sites {
            return Ok(());
        }
        #[cfg(test)]
        if self.chill.is_some() || self.collecting {
            return Ok(());
        }
        let unit = program.unit(entry.unit);
        let top = self.cx.frames.len() - 1;
        let base = self.cx.frames[top].base as usize;
        let mut pc = 0;
        loop {
            let op = unit.ops[pc];
            if self.exact && !matches!(op, Op::Block(_) | Op::Trace(_)) {
                self.cx.frames[top].pc = pc as u32;
                return Ok(());
            }
            match op {
                Op::Block(block) => {
                    self.meter.leave();
                    self.exact = !self.meter.enter(unit.blocks[block as usize].charge);
                }
                Op::Trace(n) => match self.trace(program, unit, top, n, base)? {
                    Some(next) => {
                        pc = next;
                        continue;
                    }
                    None => return Ok(()),
                },
                Op::Inline(n) => {
                    self.cx.frames[top].pc = pc as u32 + 1;
                    #[cfg(test)]
                    if unit.steps && unit.step[pc] {
                        crate::perf_witness::record(|work| work.vm_steps += 1);
                    }
                    let site = &unit.inlines[n as usize];
                    let call = &unit.calls[site.call as usize];
                    if let Some(entry) = call.entry {
                        self.tasks.all[self.tasks.running].count(entry)?;
                    }
                    if let Some(checked) = &call.checked {
                        self.check_arguments(checked, usize::from(call.argc))?;
                    }
                    // Meter::enter prepays only before the slice ends.
                    debug_assert!(!self.meter.spent());
                    let (start, end) = site.folds;
                    let folds = &unit.folds[start as usize..end as usize];
                    if !self.inline_fast(unit, n, folds)? {
                        // Dispatch resumes this original op, whose start and
                        // entry were counted, with its arguments still in place.
                        self.retrying = true;
                        #[cfg(test)]
                        RETRIES.with(|retries| retries.set(retries.get() + 1));
                        return Ok(());
                    }
                }
                _ => {
                    #[cfg(test)]
                    if unit.steps && unit.step[pc] {
                        crate::perf_witness::record(|work| work.vm_steps += 1);
                    }
                    self.cx.frames[top].pc = pc as u32 + 1;
                    match op {
                        Op::Nop => {}
                        Op::Const(n) => self.push(self.konst(n)),
                        Op::Local(n) => self.push(self.cx.stack[base + usize::from(n)]),
                        Op::SetLocal(n) => {
                            let value = self.pop();
                            self.cx.stack[base + usize::from(n)] = value;
                        }
                        Op::Pop => {
                            self.pop();
                        }
                        Op::CaptureGet(_) | Op::CaptureSet(_) | Op::CellGet(_) | Op::CellSet(_) => {
                            self.cell_op(op)?;
                        }
                        Op::Return | Op::Fall => {
                            let value = if op == Op::Return {
                                self.cx.stack[self.cx.stack.len() - 1]
                            } else {
                                Value::Null
                            };
                            self.meter.leave();
                            self.exact = true;
                            let unfit = if checked {
                                self.unfit_result(value)?
                            } else {
                                None
                            };
                            let Some(frame) = self.cx.frames.pop() else {
                                return Err(fault("a leaf returned without a frame"));
                            };
                            self.heap.release(entry.cost);
                            self.cx.stack.truncate(frame.base as usize);
                            if let Some(unfit) = unfit {
                                return Err(self.typed_error("entry_type_mismatch", &unfit));
                            }
                            self.push(value);
                            #[cfg(test)]
                            LEAVES.with(|leaves| leaves.set(leaves.get() + 1));
                            return Ok(());
                        }
                        _ => {
                            self.cx.frames[top].pc = pc as u32;
                            return Ok(());
                        }
                    }
                }
            }
            pc += 1;
        }
    }
}

impl<H: Heap> Vm<H> {
    /// Call site `site` of `unit`, its arguments on the stack.
    pub(super) fn call(&mut self, program: &Program, unit: &Unit, site: u32) -> Run<()> {
        let site = &unit.calls[site as usize];
        let argc = usize::from(site.argc);
        match site.callee {
            Callee::Unit {
                unit: callee,
                charge,
            } => {
                if let Some(checked) = &site.checked {
                    self.check_arguments(checked, argc)?;
                }
                let bill = self.owed(charge, argc);
                self.meter.leave();
                self.exact = true;
                let link = self.running_link();
                self.enter(callee, argc, None, bill, link)
            }
            Callee::Entry {
                unit: callee,
                adapt,
            } => self.enter_entry(callee, adapt, site.checked.as_deref(), argc),
            Callee::Value => self.call_value(program, argc, site.checked.is_some()),
            Callee::Native { native, charged } => {
                if let Some(checked) = &site.checked {
                    self.check_arguments(checked, argc)?;
                }
                self.call_native(program, native, argc, charged)
            }
            Callee::Machine {
                function, charge, ..
            } => self.call_machine(function, charge, argc),
        }
    }

    /// A call through an import entering export entry `callee` with the
    /// top `argc` arguments, adapted by `adapt` after `checked`.
    #[cold]
    #[inline(never)]
    fn enter_entry(
        &mut self,
        callee: crate::bytecode::program::UnitId,
        adapt: Adapt,
        checked: Option<&Checked>,
        argc: usize,
    ) -> Run<()> {
        let argc = self.adapted(checked, adapt, argc)?;
        self.meter.leave();
        self.exact = true;
        let link = self.running_link();
        self.enter(callee, argc, None, None, link)
    }

    /// The top `argc` arguments of a call through an import, held to
    /// `checked` and made the export body's by its start convention as a
    /// start makes them (`K-DOC-003`): how many the body is passed. A
    /// TypeScript export's callers may pass fewer or more than it
    /// declares. What the adaptation builds is built over copies of the
    /// arguments, which stay rooted below until it is placed.
    #[cold]
    #[inline(never)]
    fn adapted(&mut self, checked: Option<&Checked>, adapt: Adapt, argc: usize) -> Run<usize> {
        let Some(checked) = checked else {
            return Err(fault("an export's entry is called without its contract"));
        };
        match adapt {
            Adapt::Python { .. } => self.check_arguments(checked, argc)?,
            Adapt::Slots(_) | Adapt::Listed => self.check_fits(checked, argc)?,
        }
        let from = self.cx.stack.len() - argc;
        let copy = |vm: &mut Self, range: std::ops::Range<usize>| {
            for at in range {
                vm.push(vm.cx.stack[from + at]);
            }
        };
        match adapt {
            Adapt::Slots(arity) => {
                let arity = usize::from(arity);
                self.cx.stack.truncate(from + argc.min(arity));
                for _ in argc..arity {
                    self.push(Value::Absent);
                }
                return Ok(arity);
            }
            Adapt::Listed => {
                self.push(Value::Absent);
                copy(self, 0..argc);
                let values = self.top(argc).to_vec();
                self.place(argc, NewObject::List(values))?;
            }
            Adapt::Python { positional } => {
                let positional = usize::from(positional).min(argc);
                copy(self, 0..positional);
                let values = self.top(positional).to_vec();
                self.place(positional, NewObject::List(values))?;
                for (at, param) in checked.signature.params.iter().enumerate().take(argc) {
                    if at >= positional {
                        let name = self.text(param.name.as_str())?;
                        self.push(name);
                        self.push(self.cx.stack[from + at]);
                    }
                }
                let named = 2 * (argc - positional);
                let pairs = self
                    .top(named)
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&[key, value]| (key, value))
                    .collect();
                self.place(named, NewObject::Map(pairs))?;
                self.push(Value::Absent);
            }
        }
        let adapted = self.cx.stack.len() - from - argc;
        self.cx.stack.drain(from..from + argc);
        Ok(adapted)
    }

    /// The bill a call of formula `charge` owes, over its top `argc`
    /// operands.
    pub(super) fn owed(&mut self, charge: Option<u32>, argc: usize) -> Option<Bill> {
        let charge = charge.filter(|_| !self.off)?;
        Some(self.owe(charge, argc))
    }

    /// Calls the closure or function value below the top `argc` operands
    /// (`K-FN-004`, `K-CLO-001`). The callee is read where it stands and
    /// removed once read. A closure the last call entered, still the same
    /// object, enters as it resolved then ([`Direct`]).
    fn call_value(&mut self, program: &Program, argc: usize, checked: bool) -> Run<()> {
        #[cfg(test)]
        if PLAIN.with(std::cell::Cell::get) {
            self.direct = None;
        }
        // durable::session::bind binds globals only during start, before this memo exists.
        debug_assert!(
            self.direct
                .is_none_or(|direct| direct.globals == self.globals.len())
        );
        let at = self.cx.stack.len() - argc - 1;
        let callee = self.cx.stack[at];
        if let Value::Closure(handle) = callee
            && let Some(direct) = self.direct
            && direct.closure == handle
            && direct.freed == self.heap.arena().freed()
        {
            #[cfg(test)]
            HITS.with(|hits| hits.set(hits.get() + 1));
            self.enter_value(at, argc, direct.params, direct.entry, Some(handle))?;
            return match direct.leaf {
                Leaf::Plain => Ok(()),
                Leaf::Ops => self.leaf_run(program, direct.entry, checked),
                Leaf::Project { block, value } => {
                    if self.watch.is_some() {
                        return Ok(());
                    }
                    #[cfg(test)]
                    if self.chill.is_some() || self.collecting {
                        return Ok(());
                    }
                    let unit = program.unit(direct.entry.unit);
                    let top = self.cx.frames.len() - 1;
                    self.meter.leave();
                    self.exact = !self.meter.enter(unit.blocks[block as usize].charge);
                    if self.exact {
                        self.cx.frames[top].pc = 1;
                        return Ok(());
                    }
                    #[cfg(test)]
                    for pc in [1, 2] {
                        if unit.steps && unit.step[pc] {
                            crate::perf_witness::record(|work| work.vm_steps += 1);
                        }
                    }
                    let base = self.cx.frames[top].base as usize;
                    let value = match value {
                        crate::bytecode::program::Operand::Local(n) => {
                            self.cx.stack[base + usize::from(n)]
                        }
                        crate::bytecode::program::Operand::Const(n) => self.konst(n),
                    };
                    self.cx.frames[top].pc = 3;
                    // A checked return may read cold and retry Return. Its operand
                    // must then stand exactly where ordinary dispatch leaves it.
                    if checked {
                        self.push(value);
                    }
                    self.meter.leave();
                    self.exact = true;
                    let unfit = if checked {
                        self.unfit_result(value)?
                    } else {
                        None
                    };
                    let Some(frame) = self.cx.frames.pop() else {
                        return Err(fault("a leaf returned without a frame"));
                    };
                    self.heap.release(direct.entry.cost);
                    self.cx.stack.truncate(frame.base as usize);
                    if let Some(unfit) = unfit {
                        return Err(self.typed_error("entry_type_mismatch", &unfit));
                    }
                    self.push(value);
                    #[cfg(test)]
                    LEAVES.with(|leaves| leaves.set(leaves.get() + 1));
                    #[cfg(test)]
                    PROJECTS.with(|projects| projects.set(projects.get() + 1));
                    Ok(())
                }
            };
        }
        let (unit, closure, callable) = match callee {
            Value::Closure(handle) => {
                let callable = {
                    let (identity, _, _) = self.heap.arena().closure(handle).map_err(read)?;
                    identity.callable
                };
                let unit = self
                    .closure_unit(&callable)
                    .ok_or_else(|| fault("a closure of a body the program lacks"))?;
                // A saved function runs over a fresh copy of its frozen
                // captures (FIG-6193).
                let handle = match self.saved_function(handle) {
                    true => self.fresh_closure(handle)?,
                    false => handle,
                };
                (unit, Some(handle), callable)
            }
            Value::Function(handle) => {
                let callable = self.heap.arena().callable(handle).map_err(read)?;
                let unit = self
                    .function_unit(&callable)
                    .ok_or_else(|| fault("a function value the program cannot call"))?;
                (unit, None, callable)
            }
            _ => {
                return Err(self.error(
                    "type_error",
                    "only a closure or a function reference can be called",
                ));
            }
        };
        // Library code runs as the Link the value names; an own unit runs
        // as its own whatever the caller passes (`Self::link_of`).
        let link = match self.program.own_index(unit) {
            Some(_) => 0,
            None => self.link_index(&callable.link).unwrap_or(0),
        };
        let params = self.program.unit(unit).params;
        let entry = self.entry(unit, link);
        // A saved call enters a fresh handle the next call cannot match;
        // caching it would evict the useful memo without a possible hit.
        if let (Value::Closure(handle), Some(closure)) = (callee, closure)
            && handle == closure
        {
            self.direct = Some(Direct {
                closure,
                freed: self.heap.arena().freed(),
                globals: self.globals.len(),
                params,
                entry,
                leaf: leaf_of(program, program.unit(unit)),
            });
        }
        self.enter_value(at, argc, params, entry, closure)
    }

    /// Enters a frame of a called value's `entry` over the top `argc`
    /// operands, removing the callee at `at` below them.
    #[inline(always)]
    fn enter_value(
        &mut self,
        at: usize,
        argc: usize,
        params: u16,
        entry: Entry,
        closure: Option<ArenaHandle>,
    ) -> Run<()> {
        let stack = &mut self.cx.stack;
        match argc {
            0 => stack.truncate(at),
            1 => {
                stack[at] = stack[at + 1];
                stack.truncate(at + 1);
            }
            2 => {
                stack[at] = stack[at + 1];
                stack[at + 1] = stack[at + 2];
                stack.truncate(at + 2);
            }
            3 => {
                stack[at] = stack[at + 1];
                stack[at + 1] = stack[at + 2];
                stack[at + 2] = stack[at + 3];
                stack.truncate(at + 3);
            }
            _ => {
                stack.remove(at);
            }
        }
        let params = usize::from(params);
        if argc > params {
            return Err(self.error(
                "arity",
                &format!("the function takes {params} argument(s); {argc} given"),
            ));
        }
        self.meter.leave();
        self.exact = true;
        self.push_frame(entry, argc, closure, None)
    }

    /// The frame holds its closure after entry. Until then only an
    /// admission that collects needs a temporary root; a successful
    /// admission reads no heap object and calls nobody.
    #[inline(always)]
    pub(super) fn admit_frame(&mut self, cost: u64, closure: Option<ArenaHandle>) -> Run<()> {
        if self.heap.admit(cost).is_ok() {
            return Ok(());
        }
        let mark = self.held.len();
        if let Some(closure) = closure {
            self.held.push(Value::Closure(closure));
        }
        let admitted = self.admit_collected(cost);
        self.held.truncate(mark);
        admitted
    }

    /// Keeps the emptied list a call's nursery built into for the next
    /// call. The list it replaces is the empty one `std::mem::take` left,
    /// which owns no storage, so it is forgotten rather than dropped.
    #[inline(always)]
    fn keep_built(&mut self, built: Vec<(ArenaHandle, NewObject)>) {
        debug_assert!(built.is_empty() && self.built.capacity() == 0);
        std::mem::forget(std::mem::replace(&mut self.built, built));
    }

    /// Calls native `native` over its top `argc` operands; `charged` when
    /// the call bills the definition's formula (`K-CHG-007`).
    fn call_native(
        &mut self,
        program: &Program,
        native: u32,
        argc: usize,
        charged: bool,
    ) -> Run<()> {
        let len = self.cx.stack.len();
        if let Some(answer) = self.answer(program, native, charged, &self.cx.stack[len - argc..]) {
            let (value, fee) = answer?;
            if let Some(unfit) = self.native_unfit(value)? {
                self.cx.stack.truncate(len - argc);
                return Err(self.typed_error("entry_type_mismatch", &unfit));
            }
            self.cx.stack.truncate(len - argc);
            self.push(value);
            if let Some(units) = fee
                && let Err(flow) = self.fee(units)
            {
                return Err(self.billed_by(flow, native));
            }
            return Ok(());
        }
        let entry: &NativeEntry = program.native(native);
        if entry.elided != 0
            && let Some(body) = entry.body
            && self.short_of(entry.elided)
        {
            // Near the bound a generated plan's elided helper frames are
            // the body's to count: it runs, and refuses where it does.
            let bill = (charged && !self.off).then(|| self.owe(entry.charge, argc));
            self.cancel();
            let link = self.running_link();
            return self.enter(body, argc, None, bill, link);
        }
        let implementation = match self.implementation(native) {
            Ok(implementation) => implementation,
            Err(flow) => {
                self.cx.stack.truncate(len);
                return Err(flow);
            }
        };
        let params = usize::from(entry.params);
        // The missing parameters are padded absent, and the padding is
        // taken back when a cold read runs the call again.
        for _ in argc..params {
            self.push(Value::Absent);
        }
        let from = self.cx.stack.len() - params.max(argc);
        let charged = charged && !self.off;
        let limit = match entry.guard {
            Some(guard) => match self.formula(guard, &self.cx.stack[from..], None) {
                Ok(limit) => Some(limit),
                Err(flow) => {
                    self.cx.stack.truncate(len);
                    return Err(flow);
                }
            },
            None => None,
        };
        // The call builds into a nursery with the room the bound leaves.
        // What it built is admitted at its peak, the larger of what it
        // reserved and what its objects hold, before any of it is placed;
        // its inputs are still held on the stack (contract §2). Short of
        // room, the run collects and calls the native once more: it is
        // pure, so the second call does what the first would have.
        let mut collected = false;
        let (answer, built, peak) = loop {
            let attempt = {
                let objects = self.heap.objects();
                let semantics = self.heap.semantics();
                let room = self.heap.bound().saturating_sub(self.heap.held());
                let mut nursery =
                    Nursery::reusing(objects, LogicalBytes(room), std::mem::take(&mut self.built));
                let mut counter = WorkCounter::new(limit);
                let answer = implementation.call(NativeCall {
                    args: Args::new(&self.cx.stack[from..], objects, &semantics),
                    new: &mut nursery,
                    counter: &mut counter,
                });
                match &answer {
                    Ok(_) | Err(NativeError::Raised(_)) => {
                        let placed: u64 = nursery
                            .built()
                            .iter()
                            .map(|(_, object)| crate::heap::meter::new_object(object))
                            .fold(0, u64::saturating_add);
                        let peak = placed.max(nursery.held().0);
                        if peak <= room {
                            Ok((answer, nursery.into_new(), peak))
                        } else {
                            nursery.discard();
                            Err(peak)
                        }
                    }
                    Err(NativeError::Retry(Retry::Room { wanted })) => {
                        let wanted = wanted.0;
                        nursery.discard();
                        Err(wanted)
                    }
                    Err(_) => {
                        nursery.discard();
                        Ok((answer, Vec::new(), 0))
                    }
                }
            };
            match attempt {
                Ok(done) => break done,
                Err(_) if !collected => {
                    collected = true;
                    self.collect();
                }
                Err(wanted) => {
                    let held = self.heap.held();
                    // The host decides once a collection leaves too little
                    // room (FIG-6768): what it grows, the call runs in.
                    let used = held.saturating_add(wanted);
                    let bound = self.heap.bound();
                    if let Some(limiter) = self.memory_limiter.as_mut()
                        && let crate::LimitDecision::Grow(more) = limiter.limit_reached(used, bound)
                        && more > 0
                    {
                        self.heap.grow(more);
                        continue;
                    }
                    // The library call that would have exceeded the bound
                    // is named, as a guard that ends the run is.
                    self.refused_memory = Some(used);
                    return Err(Flow::from(Stop::bound(Exceeded {
                        function: Some(self.library_call(&entry.name)),
                        ..self.exceeded(Bound::Memory, bound)
                    })));
                }
            }
        };
        let mut built = built;
        if peak > 0 {
            // Admitted within the room just measured.
            let _fits = self.heap.admit(peak);
            debug_assert!(_fits.is_ok());
            let placed: u64 = built
                .iter()
                .map(|(_, object)| crate::heap::meter::new_object(object))
                .fold(0, u64::saturating_add);
            let installed = self.heap.install(&mut built);
            self.keep_built(built);
            installed.map_err(|error| self.native(error))?;
            // The transient part of the peak ends with the call.
            self.heap.release(peak - placed);
        } else if built.is_empty() {
            self.keep_built(built);
        }
        // What the call built is installed: from here a cold read resumes
        // its completion, never the native.
        let charge = charged.then_some(entry.charge);
        match answer {
            Ok(value) => {
                // An immediate answer holds no other value.
                let limit = self.value_depth;
                if crate::data::immediate_size(&value).is_none()
                    && self.heap.arena().summary_of(&value).depth as usize > limit
                {
                    return Err(self.error(
                        "too_deep",
                        &format!("a value exceeds the configured {limit}-level value depth"),
                    ));
                }
                self.complete_native(from, native, charge, Done::Value(value))
            }
            Err(NativeError::Raised(value)) => {
                self.complete_native(from, native, charge, Done::Raised(value))
            }
            Err(NativeError::Refused { kind, message }) => {
                self.complete_native(from, native, charge, Done::Refused(kind, message))
            }
            Err(NativeError::Guard(guard)) => Err(Flow::from(Stop::bound(Exceeded {
                bound: Bound::Guard {
                    function: entry.function,
                },
                limit: guard.limit,
                function: Some(self.library_call(&entry.name)),
                site: None,
            }))),
            Err(NativeError::Decline) => {
                let Some(body) = entry.body else {
                    return Err(fault("a native with no kernel body declined"));
                };
                let argc = self.cx.stack.len() - from;
                let bill = charged.then(|| self.owe(entry.charge, argc));
                // The body is a frame: the caller's block is left at the
                // call, and the caller continues event by event.
                self.cancel();
                let link = self.running_link();
                self.enter(body, argc, None, bill, link)
            }
            // It read a cold chunk and did nothing a run sees: the call
            // runs again, unpadded, once the chunk is loaded.
            Err(NativeError::Retry(Retry::Cold { object, chunk })) => {
                self.cx.stack.truncate(len);
                Err(Flow::Cold { object, chunk })
            }
            Err(NativeError::Retry(Retry::Room { .. })) => {
                Err(fault("a native asked for room the core gives at once"))
            }
            Err(NativeError::Host(error)) => Err(fault(error.to_string())),
            Err(NativeError::Malformed(problem)) => Err(fault(problem)),
        }
    }

    /// The inline answer of native `native` over `args`, and the fee the
    /// call bills when `charged`: the native's own answer, with no work or
    /// allocation to count (R8, `K-LIB-011`). `None` where the native
    /// declares no inline answer over them. An operation, a native with a
    /// body beside it, is one unit of call depth whichever implementation
    /// answers: at the bound it is refused here, before any is offered the
    /// call (`K-BND-002`, `K-LIB-012`), and a Decline into its body enters
    /// that same unit.
    #[inline(always)]
    pub(super) fn answer(
        &self,
        program: &Program,
        native: u32,
        charged: bool,
        args: &[Value],
    ) -> Option<Run<(Value, Option<u64>)>> {
        let entry = program.native(native);
        if entry.body.is_some() {
            if entry.elided != 0 {
                if self.short_of(entry.elided) {
                    return None;
                }
            } else if self.depth_full() {
                return Some(Err(Flow::from(Stop::bound(
                    self.exceeded(Bound::CallDepth, u64::from(self.call_depth)),
                ))));
            }
        }
        let inline = entry.scalar?;
        if args.len() != usize::from(entry.params) {
            return None;
        }
        let value = self.immediate(inline, args)?;
        if !charged || self.off {
            return Some(Ok((value, None)));
        }
        let units = match inline {
            InlineOperation::Kind
            | InlineOperation::Not
            | InlineOperation::FloatIdentity
            | InlineOperation::FloatUnary(_)
            | InlineOperation::ToFloat
            | InlineOperation::Length(_)
            | InlineOperation::Membership(_)
            | InlineOperation::IntegerText(_) => self.formula(entry.charge, args, Some(&value)),
            _ => match program
                .charge(entry.charge)
                .pair([&args[0], &args[1], &value])
            {
                Some(units) => Ok(units),
                None => self.formula(entry.charge, args, Some(&value)),
            },
        };
        Some(units.map(|units| (value, Some(units))))
    }

    /// What inline operation `inline` answers over `args`, with no effect:
    /// `None` outside the domain the core answers it in, where the
    /// native's ordinary call runs.
    #[inline(always)]
    pub(super) fn immediate(&self, inline: InlineOperation, args: &[Value]) -> Option<Value> {
        match inline {
            // The declared domain is read here, as `InlineOperation::accepts`
            // reads it, so the hot scalar answer makes no call.
            InlineOperation::Scalar { operation, domain } if args.len() == 2 => match *args {
                [Value::Int(a), Value::Int(b)] if domain != ScalarDomain::Floats => {
                    lash_kernel_doc::scalar::ints(operation, a, b)
                }
                [Value::Float(a), Value::Float(b)] if domain != ScalarDomain::Words => {
                    lash_kernel_doc::scalar::floats(operation, a, b)
                }
                [Value::Text(a), Value::Text(b)] if domain == ScalarDomain::WordsFloatsOrTexts => {
                    lash_kernel_doc::scalar::byte_texts(operation, &a, &b)
                }
                [a, b] if domain.mixed() => lash_kernel_doc::scalar::mixed(operation, a, b),
                _ => None,
            },
            InlineOperation::Kind
            | InlineOperation::Not
            | InlineOperation::FloatIdentity
            | InlineOperation::FloatUnary(_)
            | InlineOperation::ToFloat
            | InlineOperation::IntegerText(
                IntegerTextDomain::Decimal | IntegerTextDomain::WholeFloat,
            ) if args.len() == 1 => inline.answer(args),
            InlineOperation::IntegerText(IntegerTextDomain::RadixArgument) if args.len() == 2 => {
                inline.answer(args)
            }
            InlineOperation::Length(_) if args.len() == 1 => self.read_answer(inline, args),
            InlineOperation::Membership(_) if args.len() == 2 => self.read_answer(inline, args),
            InlineOperation::Same if args.len() == 2 => inline.answer(args),
            _ => None,
        }
    }

    /// An answer `inline` reads through the native views: `None` where a
    /// read is cold or malformed, so that the native's call runs.
    fn read_answer(&self, inline: InlineOperation, args: &[Value]) -> Option<Value> {
        let semantics = self.heap.semantics();
        inline
            .answer_read(Args::new(args, self.heap.objects(), &semantics))
            .ok()
            .flatten()
    }

    /// Completes a native call that answered or raised: bills formula
    /// `charge` over its arguments at `from` and its result, of size 0
    /// when it raised, drops the arguments and gives the result or the
    /// raise. A cold read of the formula holds the completion, so that it
    /// resumes once the chunk is loaded.
    pub(super) fn complete_native(
        &mut self,
        from: usize,
        native: u32,
        charge: Option<u32>,
        done: Done,
    ) -> Run<()> {
        // The answer is held to the call's result before anything
        // commits: a cold read resumes this completion, never the native.
        let unfit = match &done {
            Done::Value(value) => match self.native_unfit(*value) {
                Ok(unfit) => unfit,
                Err(flow) => {
                    if let Flow::Cold { .. } = flow {
                        self.restart = Some(Restart::Native {
                            from: from as u32,
                            native,
                            charge,
                            done,
                        });
                    }
                    return Err(flow);
                }
            },
            Done::Raised(_) | Done::Refused(..) => None,
        };
        let fee = match charge {
            Some(charge) => {
                let result = match &done {
                    Done::Value(value) => Some(value),
                    Done::Raised(_) | Done::Refused(..) => None,
                };
                match self.formula(charge, &self.cx.stack[from..], result) {
                    Ok(units) => Some(units),
                    Err(flow) => {
                        if let Flow::Cold { .. } = flow {
                            self.restart = Some(Restart::Native {
                                from: from as u32,
                                native,
                                charge: Some(charge),
                                done,
                            });
                        }
                        return Err(flow);
                    }
                }
            }
            None => None,
        };
        self.cx.stack.truncate(from);
        let raised = match done {
            Done::Value(value) => {
                if unfit.is_none() {
                    self.push(value);
                }
                None
            }
            Done::Raised(value) => Some(Ok(value)),
            Done::Refused(kind, message) => Some(Err((kind, message))),
        };
        if let Some(units) = fee
            && let Err(flow) = self.fee(units)
        {
            return Err(self.billed_by(flow, native));
        }
        if let Some(unfit) = unfit {
            return Err(self.typed_error("entry_type_mismatch", &unfit));
        }
        match raised {
            None => Ok(()),
            Some(Ok(value)) => Err(Flow::Raise(value)),
            Some(Err((kind, message))) => Err(self.error(kind, &message)),
        }
    }

    /// A function the machine answers itself (`K-LIB-010`).
    fn call_machine(
        &mut self,
        function: MachineFunction,
        charge: Option<u32>,
        argc: usize,
    ) -> Run<()> {
        let from = self.cx.stack.len() - argc;
        let value = match function {
            MachineFunction::Deref => {
                match self.cx.stack.get(from).copied().unwrap_or(Value::Absent) {
                    Value::TaskRef(task) => Value::Task(task),
                    Value::ObjectRef(object) => {
                        let found = self
                            .heap
                            .arena()
                            .object_value(object)
                            .map_err(|error| self.native(error))?;
                        match found {
                            Some(value) => value,
                            None => return Err(self.error("type_error", "the ref names no object")),
                        }
                    }
                    _ => return Err(self.error("type_error", "`deref` takes a ref")),
                }
            }
            MachineFunction::TasksUnfinished => {
                let tasks = self.tasks.unfinished();
                self.alloc(NewObject::List(tasks))?
            }
            MachineFunction::BrandBuilt => {
                return Err(self.error(
                    "type_error",
                    "`brand.built` is called by name in an expression",
                ));
            }
        };
        let fee = match charge.filter(|_| !self.off) {
            Some(charge) => Some(self.formula(charge, &self.cx.stack[from..], Some(&value))?),
            None => None,
        };
        self.cx.stack.truncate(from);
        self.push(value);
        if let Some(units) = fee {
            self.fee(units)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod memo_laws {
    use super::*;
    use crate::laws::embedder::{ROOMY, World, core_machine, int};
    use crate::{Machine, Step};

    /// K-CLO-001 / K-FN-004: after a slot's generation wraps, its
    /// complete handle can equal a long-dead memo key. The arena's u64
    /// freed epoch must still resolve the new closure's body and arity.
    #[test]
    fn a_direct_memo_rejects_a_reused_slot_after_generation_wrap() {
        const SOURCE: &str = "main { let cells = [100] let f = fn() { return 7 } let g = fn(x) { set cells[0] = x return cells[0] } do apply f() return apply f() }";
        let prepare = || {
            let mut machine = core_machine(SOURCE, ROOMY, None).unwrap();
            loop {
                assert_eq!(machine.run(&mut World::default(), 1).unwrap(), Step::Slice);
                let vm = machine.call_law_vm();
                if vm.direct.is_some() && vm.cx.frames.len() == 1 {
                    break;
                }
            }
            let vm = machine.call_law_vm();
            let old = vm.direct.unwrap().closure;
            let other = vm
                .cx
                .stack
                .iter()
                .find_map(|value| match value {
                    Value::Closure(handle) if *handle != old => Some(*handle),
                    _ => None,
                })
                .expect("the other closure is still a local");
            let (identity, cells, _) = vm.heap.arena().closure(other).unwrap();
            let identity = std::sync::Arc::new(identity.clone());
            let cells = cells.to_vec();
            vm.held.extend(cells.iter().copied().map(Value::Handle));
            for value in &mut vm.cx.stack {
                if matches!(value, Value::Closure(_)) {
                    *value = Value::Null;
                }
            }
            let caller = vm.cx.frames.last_mut().unwrap();
            let code = vm.program.unit(caller.unit);
            let call = code
                .ops
                .iter()
                .rposition(|op| matches!(op, crate::bytecode::op::Op::Call(_)))
                .unwrap();
            caller.pc = call as u32 + 1;
            let before = vm.heap.arena().freed();
            vm.collect();
            assert!(vm.heap.arena().objects().get(old).is_none());
            assert!(vm.heap.arena().freed() > before);
            // Keep earlier free-list entries occupied, so the target
            // slot is the next allocation during every omitted cycle.
            let probe = loop {
                vm.heap
                    .admit(crate::heap::meter::closure(cells.len()))
                    .unwrap();
                let replacement = vm
                    .heap
                    .closure(std::sync::Arc::clone(&identity), cells.clone());
                if replacement.index() == old.index() {
                    break replacement;
                }
                vm.held.push(Value::Closure(replacement));
            };
            vm.collect();
            assert!(vm.heap.arena().objects().get(probe).is_none());
            // Two real releases, then 2^32-2 complete empty-closure
            // cycles: the same generation, but a different freed epoch.
            vm.heap.skip_free_slot_cycles(old.index(), u32::MAX - 1);
            vm.heap
                .admit(crate::heap::meter::closure(cells.len()))
                .unwrap();
            let replacement = vm.heap.closure(identity, cells);
            vm.held.push(Value::Closure(replacement));
            assert_eq!(
                replacement, old,
                "handle equality alone cannot reject this ABA"
            );
            assert_ne!(vm.direct.unwrap().freed, vm.heap.arena().freed());
            machine
        };

        // The new body takes and stores its argument, unlike the old
        // zero-argument body. Charge and refusal attribution are those
        // of resolving that same replacement without the memo.
        for argc in [0, 1, 2] {
            let run = |plain: bool| {
                let mut machine = prepare();
                let vm = machine.call_law_vm();
                let closure = vm.direct.unwrap().closure;
                if plain {
                    vm.direct = None;
                }
                vm.cx.stack.push(Value::Closure(closure));
                vm.cx.stack.extend(std::iter::repeat_n(Value::Int(9), argc));
                let program = std::sync::Arc::clone(&vm.program);
                let entry = vm.call_value(&program, argc, false);
                let (flow, answer) = match entry {
                    Ok(()) => {
                        // This is the new closure's real frame. Let its
                        // existing dispatcher return its value through
                        // the caller's original return-call continuation.
                        assert_eq!(vm.cx.frames.last().unwrap().closure, Some(closure));
                        let stop = loop {
                            let stop = vm.run(&mut World::default(), 1);
                            if !matches!(stop, Stop::Slice) {
                                break stop;
                            }
                        };
                        let flow = format!("{stop:?}");
                        let answer = match stop {
                            Stop::Returned(value) => Some(value),
                            _ => None,
                        };
                        (flow, answer)
                    }
                    Err(flow) => (format!("{flow:?}"), None),
                };
                let (_, cells, _) = vm.heap.arena().closure(closure).unwrap();
                let captured = vm.heap.cell_get(cells[0]).unwrap();
                let value = vm
                    .heap
                    .arena()
                    .sequence(captured.handle().unwrap())
                    .unwrap()
                    .iter()
                    .map(|value| match value {
                        Value::Int(value) => Some(*value),
                        Value::Absent => None,
                        other => panic!("unexpected captured member: {other:?}"),
                    })
                    .collect::<Vec<_>>();
                (flow, value, vm.charged(), vm.raise_site(), answer)
            };
            let fast = run(false);
            let plain = run(true);
            assert_eq!(fast, plain, "arity {argc}");
            if argc == 1 {
                assert_eq!(fast.4, Some(int(9)));
            }
            if argc == 2 {
                assert_eq!(fast.3.unwrap().unit, lash_kernel_value::sites::Unit::Main);
            }
            assert_eq!(
                fast.1,
                match argc {
                    0 => vec![None],
                    1 => vec![Some(9)],
                    _ => vec![Some(100)],
                }
            );
        }
    }
}
