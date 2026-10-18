//! A region's context (`docs/design/kernel-regions.md` §3.5): the census
//! and admission a region's compiled statements read, build and hold with,
//! as the reference's anchors hold and reserve what its statements make,
//! and the heap paths of a template whose body a region runs.

use std::ops::Range;

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::heap::{HeapFault, LogicalBytes};
use lash_kernel_doc::plan::TemplateHeap;
use lash_kernel_doc::{
    Args, KernelDataLimits, Measure, NativeCall, NewObject, Nursery, Value, ValueKind, WorkCounter,
};
use lash_kernel_state::arena::Arena;
use lash_kernel_state::kernel_internals::PagedHeap;

use super::super::{
    Charge, Executable, INNER, Lib, LibId, LibRun, Plan, Source, inner_entry as inner,
};
use super::nodes::{Meter, Place, SessionSlot, error_field, read_index, scalar, store};
use crate::machine::registers::Arguments;
use crate::machine::sites::SiteCaches;

/// What a region's statements read, build and hold with: the meter of
/// the running statement, the executable's libraries, the arena, the
/// heap's census, the machine's property caches and the region's session
/// slots.
pub(crate) struct Cx<'a> {
    pub(crate) meter: Meter,
    /// Whether the run is charged: a paced one measures no formula, and
    /// its prepared entries answer by their value-only guards.
    pub(crate) charging: bool,
    pub(crate) args: Arguments,
    pub(crate) exe: &'a Executable,
    pub(crate) arena: &'a mut Arena,
    pub(crate) heap: &'a mut PagedHeap,
    pub(crate) sites: &'a mut SiteCaches,
    /// The session bindings the plan names, by [`super::LoopPlan::sessions`]
    /// index.
    pub(crate) sessions: &'a mut [SessionSlot],
    pub(crate) limits: KernelDataLimits,
    /// Whether memory is accounted (`MemoryMeter` not off).
    pub(crate) memory: bool,
    /// The room the nursery admitted at the region's entry still has: the
    /// growth the region may hold before it exits (§3.5).
    pub(crate) room: u64,
    /// What the running statement reserves until it ends, out of the
    /// room: its pins and its native calls' work, as the reference's
    /// statement credit; and, while a construction collects its members,
    /// the least it will hold.
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
}

impl Cx<'_> {
    /// A window of `count` operands, which takes none of the room: what
    /// it holds is held already, as the reference's registers are. A
    /// window past the inline capacity exits to the reference.
    pub(super) fn argument_window(&mut self, count: usize) -> Option<Range<usize>> {
        self.args.window(count)
    }

    /// Collects the members of a new `kind` of `members` members with the
    /// least it will hold set aside of the room, so that what is admitted
    /// while they are collected leaves room for it; `None` before anything
    /// is collected where the room does not admit that least, which leaves
    /// the statement to the reference.
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

    /// The room the running statement may still take.
    #[inline]
    fn free(&self) -> u64 {
        self.room.saturating_sub(self.transient)
    }

    /// Whether `bytes` of growth fit the room, so that holding them after
    /// they are made cannot refuse.
    #[inline]
    pub(crate) fn fits(&self, bytes: u64) -> bool {
        !self.memory || bytes <= self.free()
    }

    /// Holds `bytes` of growth the room admitted (`fits`), as the
    /// reference's growth anchor holds it; `false` with a fault when the
    /// census refuses what the room admitted.
    pub(crate) fn hold(&mut self, bytes: u64) -> bool {
        if !self.memory || bytes == 0 {
            return true;
        }
        match self.heap.hold_growth_metered(bytes) {
            Ok(true) => {
                self.room = self.room.saturating_sub(bytes);
                true
            }
            Ok(false) => {
                self.fault = Some(HeapFault::Malformed(
                    "the census refused growth a region's nursery admitted".into(),
                ));
                false
            }
            Err(fault) => {
                self.fault = Some(fault);
                false
            }
        }
    }

    /// Reserves `bytes` until the statement ends, as the reference's
    /// memory anchor reserves them: statement credit, which the
    /// statement's growth spends first; `false` where the room does not
    /// admit them, or with a fault where the census refuses what it did.
    fn reserve(&mut self, bytes: u64) -> bool {
        if !self.memory || bytes == 0 {
            return true;
        }
        if bytes > self.free() {
            return false;
        }
        match self.heap.hold_metered(bytes) {
            Ok(true) => {
                self.transient += bytes;
                true
            }
            Ok(false) => {
                self.fault = Some(HeapFault::Malformed(
                    "the census refused a reservation a region's nursery admitted".into(),
                ));
                false
            }
            Err(fault) => {
                self.fault = Some(fault);
                false
            }
        }
    }

    /// The reference's pin anchor: a value other than a scalar the
    /// statement still uses is reserved at its cost until it ends.
    pub(super) fn pin(&mut self, value: &Value) -> bool {
        scalar(value) || self.reserve(self.arena.cost_of(value))
    }

    /// The statement ended, as the reference's step ends it: its
    /// reservations are released, what it made is held by a root or
    /// garbage (its kill set), and the arena is at a statement boundary.
    pub(crate) fn statement_ended(&mut self) {
        self.transient = 0;
        if self.memory
            && let Err(fault) = self.heap.release_statement_roots()
        {
            self.fault = Some(fault);
        }
        self.arena.statement_ended();
        self.arena.statement_boundary();
    }

    /// Installs what a nursery built and holds its growth, which
    /// [`Self::admitted`] admitted.
    fn install(&mut self, new: Vec<(ArenaHandle, NewObject)>) -> Option<()> {
        if new.is_empty() {
            return Some(());
        }
        match self.arena.install(new) {
            Ok(grown) => self.hold(grown).then_some(()),
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
        let (value, new) = {
            let mut nursery = Nursery::new(self.arena.objects(), LogicalBytes(u64::MAX));
            let value = nursery.allocate(object);
            if !self.admitted(&nursery, 0) {
                nursery.discard();
                return None;
            }
            (value, nursery.into_new())
        };
        self.install(new)?;
        Some(value)
    }

    fn formula(&self, plan: &Plan, args: &[Value], result: Option<&Value>) -> Option<u64> {
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
    /// installed, its work and its value reserved until the statement
    /// ends; a miss when it raises, declines or asks again.
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
        if self.arena.summary_of(&value).depth as usize > self.limits.value_depth()
            || !self.pin(&value)
        {
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
                    // What the path's plan variables grew by is held, its
                    // reservation the credit the growth spends.
                    let bound = paths.bound;
                    if !self.hold(bound) {
                        return None;
                    }
                    value?.into()
                }
                None => return self.native(function, args),
            },
        };
        let value = answer.value;
        if !self.pin(&value) {
            return None;
        }
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

/// The most parameters a heap path's callee takes.
const HEAP_ARITY: usize = 8;

/// A template's heap paths over a region's context: their general calls
/// by the template's resolved callees, a body-run helper's answered by its
/// entry, `level` helpers below the operator's. A form that declines
/// after the arena grew spoils the paths: what it built would outlive a
/// later alternative, so the statement misses. `bound` is what the running
/// path's plan variables grew by, reserved until the statement ends; what a
/// declined path reserved is released to `mark`, the statement's credit and
/// the region's reservations where the paths began. `built` is whether a
/// call answered an object, or a tuple or a store was made.
struct HeapCx<'c, 'a> {
    cx: &'c mut Cx<'a>,
    callees: &'c [LibId],
    level: u32,
    spoiled: bool,
    built: bool,
    bound: u64,
    mark: (u64, u64),
}

impl<'c, 'a> HeapCx<'c, 'a> {
    fn new(cx: &'c mut Cx<'a>, callees: &'c [LibId], level: u32) -> Self {
        let mark = (cx.heap.statement_reserved(), cx.transient);
        Self {
            cx,
            callees,
            level,
            spoiled: false,
            built: false,
            bound: 0,
            mark,
        }
    }

    /// What the paths reserved is released: a declined path changed
    /// nothing a run sees.
    fn release(&mut self) {
        if !self.cx.memory {
            return;
        }
        if let Err(fault) = self.cx.heap.release_reserved_to(self.mark.0) {
            self.cx.fault = Some(fault);
        }
        self.cx.transient = self.mark.1;
    }

    /// `form`'s value, the paths spoiled where it declines after growth.
    fn grown<T>(&mut self, form: impl FnOnce(&mut Self) -> Option<T>) -> Option<T> {
        let room = self.cx.room;
        let value = form(self);
        self.spoiled |= value.is_none() && self.cx.room != room;
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
                self.bound += paths.bound;
                value
            }
            None => return None,
        };
        self.cx.pin(&value).then_some(value)
    }
}

impl TemplateHeap for HeapCx<'_, '_> {
    fn call(&mut self, callee: usize, args: &[Value]) -> Option<Value> {
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

    fn bind(&mut self, value: Value, previous: Option<Value>) -> Option<()> {
        if !self.cx.memory {
            return Some(());
        }
        let arena = &*self.cx.arena;
        let grows = arena
            .cost_of_metered(&value)
            .saturating_sub(previous.map_or(0, |previous| arena.cost_of_metered(&previous)));
        if !self.cx.reserve(grows) {
            return None;
        }
        self.bound += grows;
        Some(())
    }

    fn path(&mut self) {
        self.release();
        self.bound = 0;
    }

    fn index(&mut self, target: Value, index: Value) -> Option<Value> {
        read_index(self.cx, None, target, index)
    }

    fn field(&mut self, target: Value, field: &str) -> Option<Value> {
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
        self.grown(|paths| {
            let cx = &mut *paths.cx;
            let tuple = cx
                .arena
                .tuple(items.to_vec(), cx.limits.value_depth())
                .ok()?;
            cx.pin(&tuple).then_some(tuple)
        })
        .inspect(|_| self.built = true)
    }

    fn set_index(&mut self, target: Value, index: Value, value: Value) -> Option<()> {
        self.grown(|paths| store(paths.cx, &Place::Index, target, index, value))
            .inspect(|_| self.built = true)
    }
}
