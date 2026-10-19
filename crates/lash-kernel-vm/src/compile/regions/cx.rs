//! A region's context (`docs/design/kernel-regions.md` §3.5): the capacity
//! a region's compiled statements read and build within, admitted at its
//! entry against the capacity meter's headroom (FIG-6791), and the heap
//! paths of a template whose body a region runs. A write the
//! heap paths make to a guest object is held until its statement commits
//! ([`Held`]): a statement that misses drops it, having written nothing.

use std::ops::Range;

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::heap::{HeapFault, LogicalBytes};
use lash_kernel_doc::plan::TemplateHeap;
use lash_kernel_doc::{
    Args, KernelDataLimits, Measure, NativeCall, NewObject, Nursery, Value, ValueKind, WorkCounter,
};
use lash_kernel_state::arena::Arena;

use super::super::{
    Charge, Executable, INNER, Lib, LibId, LibRun, Plan, Source, inner_entry as inner,
};
use super::GuestCall;
use super::nodes::{Held, Meter, Place, SessionSlot, apply, error_field, prepare, read_index};
use crate::machine::Occurrences;
use crate::machine::registers::Arguments;
use crate::machine::sites::SiteCaches;

/// The guest calls a region runs through its callees' islands
/// (`docs/design/kernel-regions.md` §5 R4), which the machine answers.
pub(crate) trait Guests {
    /// Runs `call` of `callee` on `args` through its callee's island,
    /// having paid what its statement charges before it: the value the
    /// callee returned. `None` where the island does not run, having
    /// paid nothing, or where it stopped, with [`Cx::called`] set.
    fn call(
        &mut self,
        cx: &mut Cx<'_>,
        call: &GuestCall,
        callee: Option<Value>,
        args: &[Value],
    ) -> Option<Value>;
}

/// What a region's statements read and build with: the meter of the
/// running statement, the executable's libraries, the arena, the
/// machine's property caches and the region's session slots.
pub(crate) struct Cx<'a> {
    pub(crate) meter: Meter,
    /// Whether the run is charged: a paced one measures no formula, and
    /// its prepared entries answer by their value-only guards.
    pub(crate) charging: bool,
    pub(crate) args: Arguments,
    pub(crate) exe: &'a Executable,
    pub(crate) arena: &'a mut Arena,
    pub(crate) sites: &'a mut SiteCaches,
    /// The session bindings the plan names, by [`super::LoopPlan::sessions`]
    /// index.
    pub(crate) sessions: &'a mut [SessionSlot],
    pub(crate) limits: KernelDataLimits,
    /// Whether memory is metered (`MemoryMeter` not off).
    pub(crate) memory: bool,
    /// The capacity the arena may hold before the region exits: what it
    /// held at the region's entry and the nursery the capacity meter's
    /// headroom admitted there (§3.5). Within it no verdict refuses.
    pub(crate) ceiling: u64,
    /// What the running statement sets aside of the room until it ends:
    /// its native calls' work; while a construction collects its members,
    /// the least it will hold; and the bound of the write it holds.
    pub(crate) transient: u64,
    /// The fault a node met, which ends the run: a miss is no fault.
    pub(crate) fault: Option<HeapFault>,
    /// The running statement's expression variables, innermost last: each
    /// frame position and its value, which the reference's frame holds
    /// from its declaration to its end, inside the statement.
    pub(crate) lets: Vec<(u32, Value)>,
    /// The running statement met an operand kind its operator's heap
    /// paths leave to the general call, before they built anything: the
    /// reference's own, as a barrier is.
    pub(crate) declined: bool,
    /// The write the running statement's operator made through its heap
    /// paths, held until the statement commits.
    pub(crate) held: Option<Held>,
    /// The run's charge, which the machine takes back once the region
    /// exits. After a statement's guest call ran, it holds the island's
    /// charge but not what the statement paid before the call, which the
    /// statement's commit pays with its fixed units.
    pub(crate) charged: u64,
    /// The task's occurrence counters, which the region's invokes advance
    /// as they commit and its guest calls as they run (`K-EFF-008`).
    pub(crate) counters: Option<&'a mut Occurrences>,
    /// What runs the plan's guest calls, when it makes any.
    pub(crate) guests: Option<&'a mut (dyn Guests + 'a)>,
    /// The running statement's guest call ran: its value, or `None` where
    /// its island stopped. The statement stands in its call from then on.
    pub(crate) called: Option<Option<Value>>,
}

impl Cx<'_> {
    /// Runs guest call `call` of `callee` on `args` through its callee's
    /// island ([`Guests::call`]).
    pub(super) fn guest(
        &mut self,
        call: &GuestCall,
        callee: Option<Value>,
        args: &[Value],
    ) -> Option<Value> {
        let guests = self.guests.take()?;
        let value = guests.call(self, call, callee, args);
        self.guests = Some(guests);
        value
    }

    /// A window of `count` operands, which takes none of the room: what
    /// it holds is held already, as the reference's registers are. A
    /// window past the inline capacity exits to the reference.
    pub(super) fn argument_window(&mut self, count: usize) -> Option<Range<usize>> {
        self.args.window(count)
    }

    /// Collects the members of a new `kind` of `members` members with the
    /// least capacity it will hold set aside of the room, so that what is
    /// built while they are collected leaves room for it; `None` before
    /// anything is collected where the room does not admit that least,
    /// which leaves the statement to the reference, whose verdict decides
    /// (FIG-6791).
    pub(super) fn collecting<T>(
        &mut self,
        kind: ValueKind,
        members: usize,
        collect: impl FnOnce(&mut Self) -> Option<T>,
    ) -> Option<T> {
        let floor = if self.memory {
            self.arena.construction_floor(kind, members)
        } else {
            0
        };
        if floor > self.free() {
            return None;
        }
        self.transient += floor;
        let collected = collect(self);
        self.transient -= floor;
        collected
    }

    /// The room the running statement may still take: what the arena may
    /// grow by before the ceiling, less what the statement set aside.
    #[inline]
    fn free(&self) -> u64 {
        self.ceiling
            .saturating_sub(self.arena.held())
            .saturating_sub(self.transient)
    }

    /// Applies the write the committing statement holds, if any, as the
    /// reference's member write makes it; `false`, having written nothing,
    /// where the arena must decode first, so that the statement misses.
    #[inline]
    pub(crate) fn commit_write(&mut self) -> bool {
        match self.held.take() {
            Some(held) => {
                self.transient = self.transient.saturating_sub(held.bound);
                apply(self, held.write).is_some()
            }
            None => true,
        }
    }

    /// Whether `bytes` of capacity fit the room, so that holding them after
    /// they are made passes the bound.
    #[inline]
    pub(crate) fn fits(&self, bytes: u64) -> bool {
        !self.memory || bytes <= self.free()
    }

    /// Sets `bytes` aside until the statement ends, as the reference's
    /// native call reserves its work; `false` where the room does not
    /// admit them.
    fn reserve(&mut self, bytes: u64) -> bool {
        if !self.fits(bytes) {
            return false;
        }
        if self.memory {
            self.transient += bytes;
        }
        true
    }

    /// The statement ended, as the reference's step ends it: what it set
    /// aside is released and the arena is at a statement boundary. A region
    /// publishes no statement roots, so it has none to release.
    pub(crate) fn statement_ended(&mut self) {
        self.transient = 0;
        self.arena.statement_boundary();
    }

    /// Installs what a nursery built, which [`Self::admitted`] admitted.
    fn install(&mut self, new: Vec<(ArenaHandle, NewObject)>) -> Option<()> {
        if new.is_empty() {
            return Some(());
        }
        match self.arena.install(new) {
            Ok(()) => Some(()),
            Err(error) => {
                self.fault = Some(HeapFault::Malformed(error.to_string()));
                None
            }
        }
    }

    /// Whether what `nursery` built, with `held` for its work, fits the
    /// room.
    fn admitted(&self, nursery: &Nursery<'_>, held: u64) -> bool {
        !self.memory
            || self
                .arena
                .install_bound(nursery.built())
                .checked_add(held)
                .is_some_and(|bytes| bytes <= self.free())
    }

    /// Builds `object`, as the reference's construction does.
    pub(super) fn build(&mut self, object: NewObject) -> Option<Value> {
        if self.memory && self.arena.object_bound(&object) > self.free() {
            return None;
        }
        match self.arena.create(object) {
            Ok(value) => Some(value),
            Err(error) => {
                self.fault = Some(HeapFault::Malformed(error.to_string()));
                None
            }
        }
    }

    /// What `plan` charges a call of `args` that gave `result`, measured
    /// as the reference measures it. With a write held, a formula that
    /// measures an object other than a long text, which the write may
    /// change or reach, is the reference's to measure after the write.
    fn formula(&self, plan: &Plan, args: &[Value], result: Option<&Value>) -> Option<u64> {
        if self.held.is_some() && measures_object(plan, args, result) {
            return None;
        }
        measured_formula(self.arena, plan, args, result)
    }

    /// The general call of native `function` on `args`, one per
    /// parameter, as the reference's call answers it: charged its formula
    /// when it answers, a miss when it raises, declines or asks again.
    pub(super) fn native(&mut self, function: &Lib, args: Range<usize>) -> Option<Value> {
        let value = self.answered(function, args.clone())?;
        if self.charging {
            let units = self.formula(&function.charge, &self.args[args], Some(&value))?;
            self.meter.measured(units);
        }
        Some(value)
    }

    /// The general call of native `function` on `args`, one per
    /// parameter, uncharged, as a helper body's own call is: what it built
    /// installed, its work set aside until the statement ends; a miss when
    /// it raises, declines, asks again or builds past the room.
    fn answered(&mut self, function: &Lib, args: Range<usize>) -> Option<Value> {
        let LibRun::Native { native, .. } = &function.run else {
            return None;
        };
        let limit = match &function.limit {
            Some(limit) => Some(self.formula(limit, &self.args[args.clone()], None)?),
            None => None,
        };
        let mut counter = WorkCounter::new(limit);
        let (value, held, new) = {
            let semantics = self.arena.semantics().with_limits(self.limits);
            let objects = self.arena.objects();
            let mut nursery = if self.memory {
                Nursery::new(objects, LogicalBytes(self.free()))
            } else {
                Nursery::unaccounted(objects)
            };
            let answer = native.call(NativeCall {
                args: Args::new(&self.args[args.clone()], objects, &semantics),
                new: &mut nursery,
                counter: &mut counter,
            });
            let held = nursery.held().0;
            match answer {
                Ok(value) if self.admitted(&nursery, held) => (value, held, nursery.into_new()),
                _ => {
                    nursery.discard();
                    return None;
                }
            }
        };
        if !self.reserve(held) {
            return None;
        }
        self.install(new)?;
        if self.arena.summary_of(&value).depth as usize > self.limits.value_depth() {
            return None;
        }
        Some(value)
    }

    /// Operator `function`'s call on `args`: its prepared entry, charged
    /// as the reference's prepared answer is; where the entry declines,
    /// its native's general call, or the heap paths of a template whose
    /// body runs.
    pub(super) fn operate(&mut self, function: &Lib, args: Range<usize>) -> Option<Value> {
        let (entry, fast) = function.prepared.as_ref()?;
        let answer = match entry.answer(&self.args[args.clone()], self.charging) {
            Some(answer) => answer,
            // An installed native answers first, as the reference's call.
            None if matches!(function.run, LibRun::Native { .. }) => {
                return self.native(function, args);
            }
            None => match entry.heap(&|function| self.exe.library_lib(function)) {
                // A heap path that declines has written nothing; what it
                // built is a miss's garbage, so the statement misses. One
                // that built nothing leaves its kind to the reference.
                Some((heap, callees, natives)) => {
                    // The paths hold the context, so they read a copy of
                    // the operands.
                    let mut operands = [Value::Absent; HEAP_ARITY];
                    let operands = operands.get_mut(..args.len())?;
                    operands.copy_from_slice(&self.args[args.clone()]);
                    let mut paths = HeapCx::new(self, callees, 0);
                    let value = (heap.entry)(&mut paths, operands, natives);
                    if paths.spoiled || value.is_none() {
                        let declined = !paths.built;
                        paths.release();
                        self.declined = declined;
                        return None;
                    }
                    value?.into()
                }
                None => return self.native(function, args),
            },
        };
        let value = answer.value;
        if !self.charging {
            return Some(value);
        }
        let units = match answer.scalar_sizes.and_then(|mask| fast.scalar_units(mask)) {
            Some(units) => units,
            None => match fast.charge(&function.charge, &self.args[args.clone()], &value) {
                Charge::Units(units) => units,
                Charge::Plan(plan) => self.formula(plan, &self.args[args], Some(&value))?,
            },
        };
        self.meter.measured(units);
        Some(value)
    }
}

/// What `plan` charges a call of `args` that gave `result`, measured in
/// `arena` as the reference measures it; `None` when a value cannot be
/// measured where it is.
pub(crate) fn measured_formula(
    arena: &Arena,
    plan: &Plan,
    args: &[Value],
    result: Option<&Value>,
) -> Option<u64> {
    let mut failed = false;
    let units = plan.evaluate(|source, measure| {
        if failed {
            return 0;
        }
        let value = match source {
            Source::Arg(index) => args.get(index),
            Source::Result => result,
            Source::Nothing => None,
        };
        let measured = match (value, measure) {
            (None, _) => Ok(0),
            (Some(value), Measure::Size) => crate::data::size(arena, value),
            (Some(value), Measure::DeepSize) => crate::data::deep_size(arena, value),
            (Some(value), Measure::NestedSize) => crate::data::nested_size(arena, value),
            (Some(value), Measure::Magnitude) => Ok(crate::data::magnitude(arena, value)),
        };
        measured.unwrap_or_else(|_| {
            failed = true;
            0
        })
    });
    (!failed).then_some(units)
}

/// Whether `plan`, charging a call of `args` that gave `result`, measures
/// an object other than a long text: one a held write may change or reach.
#[inline]
fn measures_object(plan: &Plan, args: &[Value], result: Option<&Value>) -> bool {
    plan.measurements().iter().any(|(source, _)| {
        let value = match *source {
            Source::Arg(index) => args.get(index),
            Source::Result => result,
            Source::Nothing => None,
        };
        value.is_some_and(|value| value.handle().is_some() && !matches!(value, Value::LongText(_)))
    })
}

/// The most parameters a heap path's callee takes.
const HEAP_ARITY: usize = 8;

/// A template's heap paths over a region's context: their general calls
/// by the template's resolved callees, a body-run helper's answered by its
/// entry, `level` helpers below the operator's. A form that declines
/// after the arena grew spoils the paths: what it built would outlive a
/// later alternative, so the statement misses. What a declined path set
/// aside is released to `mark`, the statement's where the paths began, and
/// the write it held is dropped. `built` is whether a call answered an object, or a tuple or
/// a store was made. Once a write is held, every further form declines: it
/// is a path's last, and a caller's path reads nothing after it.
struct HeapCx<'c, 'a> {
    cx: &'c mut Cx<'a>,
    callees: &'c [LibId],
    level: u32,
    spoiled: bool,
    built: bool,
    mark: u64,
    /// Whether a write was held where the paths began.
    held: bool,
}

impl<'c, 'a> HeapCx<'c, 'a> {
    fn new(cx: &'c mut Cx<'a>, callees: &'c [LibId], level: u32) -> Self {
        let mark = cx.transient;
        let held = cx.held.is_some();
        Self {
            cx,
            callees,
            level,
            spoiled: false,
            built: false,
            mark,
            held,
        }
    }

    /// What the paths set aside is released and what they wrote dropped:
    /// a declined path changed nothing a run sees.
    fn release(&mut self) {
        if !self.held {
            self.cx.held = None;
        }
        self.cx.transient = self.mark;
    }

    /// `form`'s value, the paths spoiled where it declines after growth.
    fn grown<T>(&mut self, form: impl FnOnce(&mut Self) -> Option<T>) -> Option<T> {
        let held = self.cx.arena.held();
        let value = form(self);
        self.spoiled |= value.is_none() && self.cx.arena.held() != held;
        value
    }

    /// Body-run helper `function`'s answer on `args` by its entry: its
    /// immediate paths, then, above [`INNER`], its heap paths.
    fn inner(&mut self, function: &Lib, args: &[Value]) -> Option<Value> {
        let entry = inner(function)?;
        let value = match entry.call(args) {
            Some(value) => value,
            None if self.level < INNER => {
                let exe = self.cx.exe;
                let (heap, callees, natives) = entry.heap(&|function| exe.library_lib(function))?;
                let mut paths = HeapCx::new(self.cx, callees, self.level + 1);
                let value = (heap.entry)(&mut paths, args, natives);
                self.spoiled |= paths.spoiled;
                let Some(value) = value else {
                    paths.release();
                    return None;
                };
                value
            }
            None => return None,
        };
        Some(value)
    }
}

impl TemplateHeap for HeapCx<'_, '_> {
    fn call(&mut self, callee: usize, args: &[Value]) -> Option<Value> {
        if self.cx.held.is_some() {
            return None;
        }
        let exe = self.cx.exe;
        let function = exe.lib(*self.callees.get(callee)?);
        // One argument per parameter, absent past those given; a callee
        // with more parameters than a heap path passes is the reference's.
        let mut given = [Value::Absent; HEAP_ARITY];
        let given = given.get_mut(..function.arity)?;
        given.get_mut(..args.len())?.copy_from_slice(args);
        let given = &*given;
        let value = self.grown(|paths| match inner(function) {
            Some(_) => paths.inner(function, given),
            None => {
                let cx = &mut *paths.cx;
                let window = cx.argument_window(given.len())?;
                for (index, value) in given.iter().enumerate() {
                    cx.args.put(window.start + index, *value);
                }
                let value = cx.answered(function, window.clone());
                cx.args.release(window.start);
                value
            }
        });
        self.built |= value.as_ref().and_then(Value::handle).is_some();
        value
    }

    fn bind(&mut self, _value: Value, _previous: Option<Value>) -> Option<()> {
        // A plan variable is a register: the capacity meter counts no
        // value it holds again.
        self.cx.held.is_none().then_some(())
    }

    fn path(&mut self) {
        self.release();
    }

    fn index(&mut self, target: Value, index: Value) -> Option<Value> {
        if self.cx.held.is_some() {
            return None;
        }
        read_index(self.cx, None, target, index)
    }

    fn field(&mut self, target: Value, field: &str) -> Option<Value> {
        if self.cx.held.is_some() {
            return None;
        }
        match target {
            Value::Record(_) => read_index(
                self.cx,
                None,
                target,
                Value::Text(lash_kernel_doc::arena::ShortText::new(field)?),
            ),
            Value::Error(error) => error_field(self.cx, error, field),
            _ => None,
        }
    }

    fn tuple(&mut self, items: &[Value]) -> Option<Value> {
        if self.cx.held.is_some() {
            return None;
        }
        self.grown(|paths| {
            let cx = &mut *paths.cx;
            if !cx.fits(cx.arena.construction_floor(ValueKind::Tuple, items.len())) {
                return None;
            }
            cx.arena.tuple(items.to_vec(), cx.limits.value_depth()).ok()
        })
        .inspect(|_| self.built = true)
    }

    /// Checks the write as the reference's member write does and holds it
    /// until the statement commits.
    fn set_index(&mut self, target: Value, index: Value, value: Value) -> Option<()> {
        if self.cx.held.is_some() {
            return None;
        }
        let (write, bound) = prepare(self.cx, &Place::Index, target, index, value)?;
        if self.cx.memory {
            self.cx.transient += bound;
        }
        self.cx.held = Some(Held { write, bound });
        self.built = true;
        Some(())
    }
}
