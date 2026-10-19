//! A region's context (`docs/design/kernel-regions.md` §3.5): the capacity
//! a region's compiled statements read and build within, admitted at its
//! entry against the capacity meter's headroom (FIG-6791), and the heap
//! paths of a template whose body a region runs. A write the
//! heap paths make to a guest object is held until its statement commits
//! ([`Held`]): a statement that misses drops it, having written nothing.

use std::ops::Range;

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::heap::{HeapFault, LogicalBytes};
use lash_kernel_doc::plan::{Collection, Table, TemplateHeap};
use lash_kernel_doc::{
    Args, KernelDataLimits, Measure, NativeCall, NewObject, Nursery, PreparedAnswer, Value,
    ValueKind, WorkCounter,
};
use lash_kernel_state::arena::Arena;

use super::super::{
    Charge, Entry, Executable, Fast, INNER, Lib, LibId, LibRun, Plan, Source, inner_entry as inner,
};
use super::GuestCall;
use super::nodes::{
    Fresh, Held, Meter, Place, SessionSlot, apply, error_field, prepare, read_index,
};
use crate::data::immediate_measure;
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
    /// The running statement is a linear text accumulator's
    /// (`CompiledStep::Write::accumulates`) whose slot holds a long text.
    pub(crate) accumulator: Option<Accumulator>,
}

/// A running accumulator statement's text (FIG-6795): the long text its
/// slot holds, whether that text's storage may move, and what the
/// statement's concatenation of it made.
pub(crate) struct Accumulator {
    pub(crate) left: ArenaHandle,
    /// The accumulator made `left` and no older version of it is
    /// observable: a concatenation of it may take its storage.
    pub(crate) owned: bool,
    pub(crate) made: Option<Made>,
}

/// What an accumulator statement's concatenation made: a fresh long text
/// and, where it took its operand's storage, the length that operand had.
#[derive(Clone, Copy)]
pub(crate) struct Made {
    pub(crate) text: ArenaHandle,
    pub(crate) moved: Option<usize>,
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

    /// The call of native `function` on `args`, one per parameter, as the
    /// reference's call answers it: by its prepared entry where its guard
    /// holds, and otherwise by its general call.
    pub(super) fn native(&mut self, function: &Lib, args: Range<usize>) -> Option<Value> {
        if let Some((entry, fast)) = &function.prepared
            && entry.depth() == 0
            && let Some(answer) = entry.answer(&self.args[args.clone()], self.charging)
        {
            return self.charged_at(function, fast, args, answer);
        }
        self.general(function, args)
    }

    /// [`Self::native`] on `values`, one per parameter, which the node
    /// evaluated in place: its prepared entry answers them directly, and
    /// only its general call takes an operand window.
    pub(super) fn native_on(
        &mut self,
        function: &Lib,
        values: &[Value],
        fresh: Fresh,
    ) -> Option<Value> {
        if let Some((entry, fast)) = &function.prepared
            && entry.depth() == 0
            && let Some(answer) = entry.answer(values, self.charging)
        {
            return self.charged(function, fast, values, fresh, answer);
        }
        self.windowed(values, |cx, args| cx.general(function, args))
    }

    /// [`Self::operate`] on `values`, one per parameter, which the node
    /// evaluated in place, `fresh` those of them that are constructions of
    /// no members: its prepared entry answers them directly, and only a
    /// general call after a decline takes an operand window.
    pub(super) fn operate_on(
        &mut self,
        function: &Lib,
        values: &[Value],
        fresh: Fresh,
    ) -> Option<Value> {
        let (entry, fast) = function.prepared.as_ref()?;
        match entry.answer(values, self.charging) {
            Some(answer) => self.charged(function, fast, values, fresh, answer),
            None => self.declined(function, entry, fast, values, fresh),
        }
    }

    /// Native `function`'s membership test of `table` on `values`, the
    /// table and the key (FIG-6820): looked up as its call looks the key
    /// up ([`Self::member`]) and charged its formula as [`Self::general`]
    /// charges it; its general call where the lookup declines.
    pub(super) fn contains(
        &mut self,
        function: &Lib,
        table: Table,
        values: [Value; 2],
    ) -> Option<Value> {
        let Some(value) = self.member(table, values[0], values[1]) else {
            return self.windowed(&values, |cx, args| cx.general(function, args));
        };
        if self.charging {
            let units = self.formula(&function.charge, &values, Some(&value))?;
            self.meter.measured(units);
        }
        Some(value)
    }

    /// `<table>.contains(target, key)`, looked up as the native's call
    /// looks it up ([`Self::entry`]).
    fn member(&self, table: Table, target: Value, key: Value) -> Option<Value> {
        Some(Value::Bool(self.entry(table, target, key)?.is_some()))
    }

    /// `target`'s entry under `key`, looked up as `<table>.contains`'s call
    /// looks it up, through the same views, without its call: it builds and
    /// reserves nothing. `Some(None)` where the table holds no `key`, and a
    /// set's member is `true`, as `target[key]` reads them; `None` where
    /// the call raises or must decode first.
    fn entry(&self, table: Table, target: Value, key: Value) -> Option<Option<Value>> {
        if !matches!(
            (table, target),
            (Table::Map, Value::Map(_))
                | (Table::Set, Value::Set(_))
                | (Table::Record, Value::Record(_))
        ) {
            return None;
        }
        let semantics = self.arena.semantics().with_limits(self.limits);
        let args = Args::new(&[], self.arena.objects(), &semantics);
        let view = args.table(&target).ok()??;
        match table {
            Table::Record if !matches!(key, Value::Text(_) | Value::LongText(_)) => return None,
            Table::Record => {}
            Table::Map | Table::Set => legal_key(&args, &key)?,
        }
        let found = args.lookup(view, &key).ok()?;
        Some(found.map(|value| match table {
            Table::Set => Value::Bool(true),
            Table::Map | Table::Record => *value,
        }))
    }

    /// `call` on a window holding `values`, released after it.
    fn windowed(
        &mut self,
        values: &[Value],
        call: impl FnOnce(&mut Self, Range<usize>) -> Option<Value>,
    ) -> Option<Value> {
        let window = self.argument_window(values.len())?;
        for (at, value) in values.iter().enumerate() {
            self.args.put(window.start + at, *value);
        }
        let answer = call(self, window.clone());
        self.args.release(window.start);
        answer
    }

    /// The general call of native `function` on `args`, one per
    /// parameter, as the reference's call answers it: charged its formula
    /// when it answers, a miss when it raises, declines or asks again.
    fn general(&mut self, function: &Lib, args: Range<usize>) -> Option<Value> {
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
        let accumulating = self.accumulating(function, &args);
        if accumulating == Some(true)
            && let Some(extended) = self.extended(args.clone())
        {
            return extended;
        }
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
        if accumulating.is_some()
            && let (Some(accumulator), Value::LongText(text)) = (&mut self.accumulator, value)
        {
            accumulator.made = Some(Made { text, moved: None });
        }
        Some(value)
    }

    /// Whether native `function`, called on `args`, is
    /// the running accumulator statement's concatenation of its text, and
    /// if so whether that text's storage may move.
    #[inline]
    fn accumulating(&self, function: &Lib, args: &Range<usize>) -> Option<bool> {
        let accumulator = self.accumulator.as_ref()?;
        (accumulator.made.is_none()
            && matches!(self.args[args.clone()].first(), Some(Value::LongText(left)) if *left == accumulator.left)
            && function.definition.name.as_str() == "text.concat")
            .then_some(accumulator.owned)
    }

    /// `text.concat` of the accumulator's text and the text after it, in
    /// the storage the accumulator's text gives (FIG-6795), admitted as
    /// the native's call is and its storage's growth before it grows:
    /// `Some(None)` where the room refuses, as the native's call misses;
    /// `None`, having changed nothing, where the operand is not a resident
    /// text, for the native's call to answer.
    fn extended(&mut self, args: Range<usize>) -> Option<Option<Value>> {
        let (Some(&Value::LongText(from)), Some(&right)) = (
            self.args[args.clone()].first(),
            self.args[args.clone()].get(1),
        ) else {
            return None;
        };
        let short;
        let long;
        let part = match right {
            Value::Text(text) => {
                short = text;
                short.as_str()
            }
            Value::LongText(_) => {
                let semantics = self.arena.semantics().with_limits(self.limits);
                let view = Args::new(&self.args[args.clone()], self.arena.objects(), &semantics);
                long = view.text(&right).ok()??.as_str()?.to_owned();
                &long
            }
            _ => return None,
        };
        let bytes = self.arena.body(from).ok().and_then(|body| match body {
            lash_kernel_doc::arena::Body::LongText(text) => Some(text.summary.utf8_bytes),
            _ => None,
        })?;
        let len = bytes.checked_add(part.len() as u64)?;
        let free = self.free();
        let held = if self.memory {
            // The native holds its result's bytes as its work and builds
            // the result: both must fit the room.
            let bound = self.arena.text_install_bound(len).checked_add(len)?;
            if bound > free {
                return Some(None);
            }
            len
        } else {
            0
        };
        let memory = self.memory;
        let (text, moved) = self
            .arena
            .extend_text(from, part, |grown| !memory || grown <= free)?;
        self.transient += held;
        if let Some(accumulator) = &mut self.accumulator {
            accumulator.made = Some(Made {
                text,
                moved: Some(moved),
            });
        }
        Some(Some(Value::LongText(text)))
    }

    /// Whether the running accumulator statement's concatenation answered:
    /// a heap path reads and builds nothing after it, so that the text it
    /// made is the statement's value and nothing else holds it.
    #[inline]
    fn concatenated(&self) -> bool {
        self.accumulator
            .as_ref()
            .is_some_and(|accumulator| accumulator.made.is_some())
    }

    /// Operator `function`'s call on `args`: its prepared entry, charged
    /// as the reference's prepared answer is; where the entry declines,
    /// its native's general call, or the heap paths of a template whose
    /// body runs.
    pub(super) fn operate(&mut self, function: &Lib, args: Range<usize>) -> Option<Value> {
        let values = self.args[args].to_vec();
        self.operate_on(function, &values, Fresh::default())
    }

    /// Operator `function`'s call on `values` where its prepared `entry`
    /// declined: its native's general call, or the heap paths of a
    /// template whose body runs.
    fn declined(
        &mut self,
        function: &Lib,
        entry: &Entry,
        fast: &Fast,
        values: &[Value],
        fresh: Fresh,
    ) -> Option<Value> {
        // An installed native answers first, as the reference's call.
        if matches!(function.run, LibRun::Native { .. }) {
            return self.windowed(values, |cx, args| cx.general(function, args));
        }
        let Some((heap, callees, natives)) = entry.heap(&|function| self.exe.library_lib(function))
        else {
            return self.windowed(values, |cx, args| cx.general(function, args));
        };
        // A heap path that declines has written nothing; what it built is
        // a miss's garbage, so the statement misses. One that built nothing
        // leaves its kind to the reference. The paths hold the context, so
        // they read a copy of the operands.
        let mut operands = [Value::Absent; HEAP_ARITY];
        let operands = operands.get_mut(..values.len())?;
        operands.copy_from_slice(values);
        let mut paths = HeapCx::new(self, callees, 0);
        let value = (heap.entry)(&mut paths, operands, natives);
        if paths.spoiled || value.is_none() {
            let declined = !paths.built;
            paths.release();
            self.declined = declined;
            return None;
        }
        self.charged(function, fast, values, fresh, value?.into())
    }

    /// `function`'s prepared `answer` on `args`, charged as the
    /// reference's prepared answer is ([`Self::units`]).
    #[inline(always)]
    fn charged(
        &mut self,
        function: &Lib,
        fast: &Fast,
        args: &[Value],
        fresh: Fresh,
        answer: PreparedAnswer,
    ) -> Option<Value> {
        if self.charging {
            let units = self.units(function, fast, args, fresh, &answer)?;
            self.meter.measured(units);
        }
        Some(answer.value)
    }

    /// [`Self::charged`] of the answer on the operands at `args`.
    #[inline(always)]
    fn charged_at(
        &mut self,
        function: &Lib,
        fast: &Fast,
        args: Range<usize>,
        answer: PreparedAnswer,
    ) -> Option<Value> {
        if self.charging {
            let units = self.units(function, fast, &self.args[args], Fresh::default(), &answer)?;
            self.meter.measured(units);
        }
        Some(answer.value)
    }

    /// What `function`'s prepared `answer` on `args` charges: the units of
    /// the scalar sizes its guard certified, or its formula over the
    /// values, taken from the values themselves where each carries its
    /// measurements, those `fresh` names included while no write is held
    /// that may change them.
    #[inline(always)]
    fn units(
        &self,
        function: &Lib,
        fast: &Fast,
        args: &[Value],
        fresh: Fresh,
        answer: &PreparedAnswer,
    ) -> Option<u64> {
        if let Some(units) = answer.scalar_sizes.and_then(|mask| fast.scalar_units(mask)) {
            return Some(units);
        }
        let fresh = if self.held.is_some() {
            Fresh::default()
        } else {
            fresh
        };
        if let Some(units) = fast.immediate(args, fresh, &answer.value) {
            return Some(units);
        }
        match fast.charge(&function.charge, args, &answer.value) {
            Charge::Units(units) => Some(units),
            Charge::Plan(plan) => self.formula(plan, args, Some(&answer.value)),
        }
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
        let Some(value) = value else {
            return 0;
        };
        // A value the arena holds nothing of carries its measurements.
        if let Some(units) = immediate_measure(value, measure) {
            return units;
        }
        let measured = match measure {
            Measure::Size => crate::data::size(arena, value),
            Measure::DeepSize => crate::data::deep_size(arena, value),
            Measure::NestedSize => crate::data::nested_size(arena, value),
            Measure::Magnitude => Ok(crate::data::magnitude(arena, value)),
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
        if self.cx.held.is_some() || self.cx.concatenated() {
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
        (self.cx.held.is_none() && !self.cx.concatenated()).then_some(())
    }

    fn path(&mut self) {
        self.release();
    }

    fn index(&mut self, target: Value, index: Value) -> Option<Value> {
        if self.cx.held.is_some() || self.cx.concatenated() {
            return None;
        }
        read_index(self.cx, None, target, index)
    }

    fn field(&mut self, target: Value, field: &str) -> Option<Value> {
        if self.cx.held.is_some() || self.cx.concatenated() {
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
        if self.cx.held.is_some() || self.cx.concatenated() {
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
        if self.cx.held.is_some() || self.cx.concatenated() {
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

    /// Reads the collection as the native's call reads it, through the
    /// same views, without its call: it builds and reserves nothing.
    fn len(&mut self, collection: Collection, target: Value) -> Option<Value> {
        if self.cx.held.is_some() || self.cx.concatenated() {
            return None;
        }
        if !matches!(
            (collection, target),
            (Collection::List, Value::List(_))
                | (Collection::Tuple, Value::Tuple(_))
                | (Collection::Map, Value::Map(_))
                | (Collection::Set, Value::Set(_))
                | (Collection::Record, Value::Record(_))
        ) {
            return None;
        }
        let semantics = self.cx.arena.semantics().with_limits(self.cx.limits);
        let args = Args::new(&[], self.cx.arena.objects(), &semantics);
        let length = match args.sequence(&target).ok()? {
            Some(sequence) => sequence.len(),
            None => args.table(&target).ok()??.len(),
        };
        Some(Value::Int(length as i64))
    }

    /// Looks the key up once, as the native's call does ([`Cx::entry`]).
    fn lookup(&mut self, table: Table, target: Value, key: Value) -> Option<Option<Value>> {
        if self.cx.held.is_some() || self.cx.concatenated() {
            return None;
        }
        self.cx.entry(table, target, key)
    }
}

/// `key` where it is a legal key as `map.contains` and `set.contains`
/// check one: `None` where their call raises (`invalid_key`, `too_deep`).
fn legal_key(args: &Args<'_>, key: &Value) -> Option<()> {
    // An inline scalar or short text is a legal key by its kind.
    if matches!(
        key,
        Value::Null
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Text(_)
            | Value::Timestamp(_)
            | Value::Function(_)
            | Value::TaskRef(_)
    ) {
        return Some(());
    }
    if matches!(key, Value::Tuple(_))
        && args.depth(key) as usize > args.limits().value_depth().min(128)
    {
        return None;
    }
    args.normal_key(key).ok()?.map(|_| ())
}
