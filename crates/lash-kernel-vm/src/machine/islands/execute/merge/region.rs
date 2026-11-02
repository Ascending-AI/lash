//! Finite collection kernels. Evaluation borrows the entry state and produces
//! one live-out transfer, an optional member edit, and an exact source exit.
//! Core authenticates the selected path, charges, peak and activation before
//! admitting this transaction. None of these preparations grants acceptance.

#![allow(
    dead_code,
    reason = "staged collection ABI awaits core packet registration"
)]

use super::*;
use crate::compile::{Executable, room};
use lash_kernel_doc::{arena::ArenaHandle, heap::HeapFault};
use lash_kernel_state::{
    arena::{Arena, PreparedListWrite},
    kernel_internals::{
        PagedHeap,
        logical::{
            BoundaryTransaction, Dependencies, Invocation, Ledger, Member, MemberEdit, MemberFault,
            Owner, RootValue,
        },
    },
};
use std::cell::RefCell;

#[cfg(test)]
#[path = "region/law.rs"]
mod law;

// Unpublished consumers cannot be enabled before core's authenticated exits.
pub(in crate::machine::islands) struct CollectionRegion<'a> {
    exe: &'a Executable,
    compiled: &'a Compiled,
    exits: &'a CollectionExits,
    ledger: &'a Ledger,
    arena: &'a Arena,
    slots: &'a [SlotState],
    value_depth: usize,
    call_depth: u32,
    handles: RefCell<Vec<ArenaHandle>>,
    branches: RefCell<Vec<(lash_kernel_value::sites::Site, bool)>>,
}

type SourceExit = (room::Point, room::Point);

/// Compiled once with the direct packet site. Runtime selection reads this
/// finite table; it never walks Source or reconstructs a completed prefix.
pub(in crate::machine::islands) struct CollectionExits {
    source: std::sync::Arc<room::Source>,
    pub(in crate::machine::islands) commit: Option<SourceExit>,
    pub(in crate::machine::islands) callback: Option<SourceExit>,
    pub(in crate::machine::islands) hole: [Option<SourceExit>; 2],
}

impl CollectionExits {
    pub(in crate::machine::islands) fn compile(
        step: &CollectionStep,
        exe: &Executable,
        compiled: &Compiled,
    ) -> Option<Self> {
        let source = &compiled.source_room;
        let site = |block: usize, statement: usize| {
            let block = exe.block(compiled.blocks.get(block)?.id);
            Some(exe.stmt(*block.stmts.get(statement)?).site.clone())
        };
        let committed = |site: lash_kernel_value::sites::Site| {
            let next = source.next_after_commit(&site).ok()?;
            Some((
                room::Point {
                    site,
                    phase: room::Phase::Commit,
                },
                next,
            ))
        };
        let mut exits = Self {
            source: std::sync::Arc::clone(source),
            commit: None,
            callback: None,
            hole: [None, None],
        };
        match step {
            CollectionStep::Append(append) => {
                exits.commit = Some(committed(site(append.body, 0)?)?)
            }
            CollectionStep::Reduce(reduce) => {
                exits.commit = Some(committed(site(reduce.body, 0)?)?)
            }
            CollectionStep::Place(place) => {
                exits.commit = Some(committed(site(
                    place.body,
                    place.statements.checked_sub(1)?,
                )?)?)
            }
            CollectionStep::ReadSlot(read) => {
                exits.commit = Some(committed(exe.stmt(read.completed).site.clone())?)
            }
            CollectionStep::Visit(visit) => {
                let site = site(visit.body, 1)?;
                for taken in [false, true] {
                    exits.hole[usize::from(taken)] = Some((
                        room::Point {
                            site: site.clone(),
                            phase: room::Phase::Test,
                        },
                        source.next_after_if_test(&site, taken).ok()?,
                    ));
                }
            }
            CollectionStep::Merge(merge) => {
                exits.commit = Some(committed(site(merge.body, 4)?)?);
                exits.callback = Some(committed(site(merge.compared[1], 1)?)?);
            }
        }
        Some(exits)
    }
}

/// Physical placement is recovery data only. Every ledger owner is resolved
/// separately through the original Source's owner table and Invocation.
pub(in crate::machine::islands) struct CollectionFacts {
    source: std::sync::Arc<room::Source>,
    writes: Vec<(usize, Value)>,
    member: Option<(ArenaHandle, u64, Value)>,
    dependencies: Dependencies,
    pub(in crate::machine::islands) exit: room::publication::Position,
    pub(in crate::machine::islands) kind: room::ExitKind,
    pub(in crate::machine::islands) transfer: CollectionTransfer,
    pub(in crate::machine::islands) branches: Vec<(lash_kernel_value::sites::Site, bool)>,
}

/// Recover the source stack after the accepted exit, including the exact
/// callback prefix. This contains no callback execution or cursor advancement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::machine::islands) enum CollectionContinuation {
    Commit,
    Callback,
}

pub(in crate::machine::islands) struct CollectionTransfer {
    pub(in crate::machine::islands) continuation: CollectionContinuation,
    pub(in crate::machine::islands) writes: Vec<(usize, Value)>,
    pub(in crate::machine::islands) leave: Option<[usize; 2]>,
    pub(in crate::machine::islands) clears: Vec<usize>,
    pub(in crate::machine::islands) resume: Option<(usize, usize)>,
    pub(in crate::machine::islands) blocks: Vec<(usize, usize)>,
    pub(in crate::machine::islands) pc: usize,
}

pub(in crate::machine::islands) struct CollectionStorage(Option<PreparedListWrite>);

pub(in crate::machine::islands) struct CollectionBoundary<'a> {
    pub(in crate::machine::islands) transaction: BoundaryTransaction<'a>,
    pub(in crate::machine::islands) storage: CollectionStorage,
    pub(in crate::machine::islands) transfer: CollectionTransfer,
    pub(in crate::machine::islands) exit: room::publication::Position,
    pub(in crate::machine::islands) kind: room::ExitKind,
    pub(in crate::machine::islands) branches: Vec<(lash_kernel_value::sites::Site, bool)>,
}

fn malformed(reason: &'static str) -> HeapFault {
    HeapFault::Malformed(reason.into())
}

impl<'a> CollectionRegion<'a> {
    /// Limits name value nesting and the remaining logical call frames.
    pub(in crate::machine::islands) fn new(
        exe: &'a Executable,
        compiled: &'a Compiled,
        exits: &'a CollectionExits,
        ledger: &'a Ledger,
        arena: &'a Arena,
        slots: &'a [SlotState],
        limits: (usize, u32),
    ) -> Self {
        Self {
            exe,
            compiled,
            exits,
            ledger,
            arena,
            slots,
            value_depth: limits.0,
            call_depth: limits.1,
            handles: RefCell::new(Vec::new()),
            branches: RefCell::new(Vec::new()),
        }
    }

    fn observe(&self, value: Value) -> Value {
        if let Some(handle) = value.handle() {
            let mut handles = self.handles.borrow_mut();
            if !handles.contains(&handle) {
                handles.push(handle);
            }
        }
        value
    }

    fn value(&self, at: usize) -> Option<Value> {
        match self.slots.get(at)? {
            SlotState::Value(value) => Some(self.observe(*value)),
            SlotState::Empty | SlotState::Cell(_) => None,
        }
    }

    fn list(&self, at: usize) -> Option<ArenaHandle> {
        match self.value(at)? {
            Value::List(handle) if self.arena.facts(handle).ok()?.cold => None,
            Value::List(handle) => Some(handle),
            _ => None,
        }
    }

    fn int(&self, at: usize) -> Option<i64> {
        match self.value(at)? {
            Value::Int(value) => Some(value),
            _ => None,
        }
    }

    fn primitive(&self, lib: LibId, args: &[Value]) -> Option<Value> {
        let (crate::compile::Entry::Native(entry), _) =
            self.exe.lib(lib).specialization.prepared.as_ref()?
        else {
            return None;
        };
        (entry.entry)(args)
    }

    fn boolean(&self, lib: LibId, args: &[Value]) -> Option<bool> {
        match self.primitive(lib, args)? {
            Value::Bool(value) => Some(value),
            _ => None,
        }
    }

    fn integer(&self, lib: LibId, args: &[Value]) -> Option<i64> {
        match self.primitive(lib, args)? {
            Value::Int(value) => Some(value),
            _ => None,
        }
    }

    fn item(&self, list: ArenaHandle, at: u64) -> Option<Value> {
        let value = self.observe(self.arena.item(list, at).ok()??);
        self.fits(value).then_some(value)
    }

    fn fits(&self, value: Value) -> bool {
        !matches!(value, Value::Tuple(_) | Value::Error(_))
            || self.arena.summary_of(&value).depth as usize <= self.value_depth
    }

    fn hole(&self, value: Value) -> Option<bool> {
        match value {
            Value::Tuple(handle) => Some(self.arena.len(handle).ok()? == 0),
            _ => Some(false),
        }
    }

    fn site(&self, block: usize, next: usize) -> Option<lash_kernel_value::sites::Site> {
        let block = self.exe.block(self.compiled.blocks.get(block)?.id);
        Some(self.exe.stmt(*block.stmts.get(next)?).site.clone())
    }

    /// The caller supplies occurrence/recovery from its authenticated source
    /// activation. Check the actual completed statement and earliest unpaid
    /// event; never manufacture an occurrence from a physical block number.
    fn finish(
        &self,
        writes: Vec<(usize, Value)>,
        member: Option<(ArenaHandle, u64, Value)>,
        completed: (room::Point, room::Point),
        transfer: CollectionTransfer,
        exit: room::publication::Position,
    ) -> Option<CollectionFacts> {
        let source = &self.compiled.source_room;
        if !std::sync::Arc::ptr_eq(source, &self.exits.source)
            || exit.at != completed.0
            || exit.unpaid != Some(completed.1)
        {
            return None;
        }
        Some(CollectionFacts {
            source: std::sync::Arc::clone(source),
            writes,
            member,
            dependencies: Dependencies::capture(self.ledger, self.arena, &self.handles.borrow())?,
            exit,
            kind: room::ExitKind::Normal,
            branches: self.branches.borrow().clone(),
            transfer,
        })
    }

    fn committed(&self) -> Option<SourceExit> {
        self.exits.commit.clone()
    }

    fn tested(&self, taken: bool) -> Option<SourceExit> {
        self.exits.hole[usize::from(taken)].clone()
    }

    fn choose(&self, body: usize, statement: usize, taken: bool) -> Option<()> {
        self.branches
            .borrow_mut()
            .push((self.site(body, statement)?, taken));
        Some(())
    }

    fn after(&self, body: usize, statements: usize) -> Option<CollectionTransfer> {
        let block = self.compiled.blocks.get(body)?;
        let pc = block.starts.get(statements).copied().unwrap_or(block.end);
        Some(CollectionTransfer {
            continuation: CollectionContinuation::Commit,
            writes: Vec::new(),
            leave: None,
            clears: Vec::new(),
            resume: None,
            blocks: vec![(body, statements)],
            pc,
        })
    }
}

impl CollectionFacts {
    /// Bind survivors using a compiler-owned mapping of physical placements
    /// to source selectors. Interior temporary owners never reach this API.
    /// Inputs include the source owners replaced or released at this exit.
    #[expect(clippy::too_many_arguments, reason = "one source collection boundary")]
    pub(in crate::machine::islands) fn prepare_boundary<'a>(
        mut self,
        ledger: &Ledger,
        arena: &'a mut Arena,
        heap: &PagedHeap,
        source: &room::Source,
        invocation: &Invocation,
        inputs: &[Owner],
        placements: &[(usize, room::Owner)],
        entry: Dependencies,
    ) -> Result<Option<CollectionBoundary<'a>>, HeapFault> {
        if !self.dependencies.is_current(ledger, arena) || !entry.is_current(ledger, arena) {
            return Ok(None);
        }
        if self.exit.occurrence.invocation != invocation.id()
            || invocation.frame()
                != (Owner::Frame {
                    task: self.exit.occurrence.task,
                    invocation: invocation.id(),
                })
            || !std::ptr::eq(source, self.source.as_ref())
        {
            return Ok(None);
        }
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("collection source owners refused"))?;
        let mut outputs = Vec::with_capacity(self.writes.len());
        for (at, value) in &self.writes {
            let mut selected = placements.iter().filter(|(physical, _)| physical == at);
            let Some((_, selector @ room::Owner::Binding { .. })) = selected.next() else {
                return Ok(None);
            };
            if selected.next().is_some() {
                return Ok(None);
            }
            let Some(owner) = slots.root(selector, self.exit.occurrence.task, invocation.id())
            else {
                return Ok(None);
            };
            if outputs.iter().any(|(previous, _)| *previous == owner) {
                return Ok(None);
            }
            outputs.push((owner, RootValue::Value(value)));
        }
        let storage = match self.member {
            Some((handle, index, value)) => {
                let Some(write) = arena.prepare_list_write(handle, index, value)? else {
                    return Ok(None);
                };
                CollectionStorage(Some(write))
            }
            None => CollectionStorage(None),
        };
        let registration = arena
            .registration()
            .map_err(crate::machine::source_publication::native_fault)?;
        let mut boundary = ledger
            .prepare_source_boundary(registration, heap, inputs, &outputs)?
            .bind_dependencies(entry)?
            .bind_dependencies(self.dependencies)?;
        if let Some((handle, index, value)) = self.member {
            boundary = match boundary.then_member(
                ledger,
                heap.pages(),
                handle,
                Member::List(index),
                Some(value),
                &[],
                &[],
            ) {
                Ok(boundary) => boundary,
                Err(MemberFault::Heap(error)) => return Err(error),
                Err(MemberFault::Native(lash_kernel_doc::NativeError::Host(error))) => {
                    return Err(error);
                }
                Err(_) => return Ok(None),
            };
        }
        self.transfer.writes = self.writes;
        Ok(Some(CollectionBoundary {
            transaction: boundary,
            storage,
            transfer: self.transfer,
            exit: self.exit,
            kind: self.kind,
            branches: self.branches,
        }))
    }
}

impl CollectionStorage {
    /// Called only by common accepted source publication, under Registration.
    /// Slot and source-stack transfer follows the same successful publication.
    pub(in crate::machine::islands) fn publish(
        self,
        arena: &mut Arena,
        mut edits: Vec<MemberEdit>,
    ) {
        match self.0 {
            Some(write) => {
                assert_eq!(edits.len(), 1, "one collection member publication");
                let Some(edit) = edits.pop() else {
                    unreachable!("checked member count");
                };
                write.publish(arena, edit);
            }
            None => assert!(edits.is_empty(), "a read-only kernel has no member edits"),
        }
    }
}

impl CollectionTransfer {
    /// Reserve host storage and validate the semantic continuation before
    /// common source admission. An ineligible stack makes no frame edit.
    pub(in crate::machine::islands::execute) fn reserve_frame(
        &self,
        frame: &mut Values<'_, '_>,
    ) -> Result<bool, HeapFault> {
        let compiled = frame.state.callee.compiled;
        let leaving = self.leave.as_ref().map_or(0, |_| 2);
        if frame.state.control.len() < leaving
            || self.pc >= compiled.code.packets.len()
            || self.clears.iter().any(|at| *at >= frame.slots.len())
            || self.writes.iter().any(|(at, _)| {
                *at >= frame.slots.len() || matches!(frame.slots[*at], SlotState::Cell(_))
            })
            || self
                .blocks
                .iter()
                .chain(self.resume.iter())
                .any(|(block, next)| {
                    compiled
                        .blocks
                        .get(*block)
                        .is_none_or(|block| *next > block.starts.len())
                })
        {
            return Ok(false);
        }
        if let Some(blocks) = self.leave {
            for (actual, expected) in frame.state.control.iter().rev().zip(blocks) {
                if !matches!(actual, Position::Block(block, _) if *block == expected) {
                    return Ok(false);
                }
            }
        }
        if let Some((expected, _)) = self.resume
            && !matches!(frame.state.control.len().checked_sub(leaving + 1).and_then(|at| frame.state.control.get(at)), Some(Position::Block(block, _)) if *block == expected)
        {
            return Ok(false);
        }
        frame
            .state
            .control
            .try_reserve(self.blocks.len())
            .map_err(|_| HeapFault::Exhausted {
                resource: lash_kernel_doc::heap::HostResource::StagingBytes,
            })?;
        Ok(true)
    }
}

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    /// Common accepted publication invokes this after reserve_frame and the
    /// matching CollectionStorage publication. No source journal runs here.
    pub(in crate::machine::islands::execute) fn install_collection_transfer(
        &mut self,
        frame: &mut Values<'_, 'e>,
        transfer: CollectionTransfer,
    ) {
        if transfer.continuation == CollectionContinuation::Commit {
            self.end_statement(frame);
        }
        if transfer.leave.is_some() {
            frame.state.control.truncate(frame.state.control.len() - 2);
        }
        if let Some((block, next)) = transfer.resume {
            let Some(last) = frame.state.control.last_mut() else {
                unreachable!("reserved collection continuation");
            };
            *last = Position::Block(block, next);
        }
        for at in transfer.clears {
            frame.slots[at] = SlotState::Empty;
        }
        for (at, value) in transfer.writes {
            frame.slots[at] = SlotState::Value(value);
        }
        frame.state.control.extend(
            transfer
                .blocks
                .into_iter()
                .map(|(block, next)| Position::Block(block, next)),
        );
        frame.state.statement = None;
        frame.state.pc = transfer.pc;
    }
}

impl CollectionAppend {
    pub(in crate::machine::islands) fn prepare_region(
        &self,
        cx: CollectionRegion<'_>,
        exit: room::publication::Position,
    ) -> Option<CollectionFacts> {
        let target = cx.list(self.target)?;
        let item = cx.value(self.item)?;
        cx.fits(item).then_some(())?;
        let length = cx.arena.len(target).ok()?;
        // The source append computes its index through a Number value.
        i64::try_from(length).ok()?;
        cx.finish(
            Vec::new(),
            Some((target, length, item)),
            cx.committed()?,
            cx.after(self.body, 1)?,
            exit,
        )
    }
}

impl Reduction {
    pub(in crate::machine::islands) fn prepare_region(
        &self,
        cx: CollectionRegion<'_>,
        exit: room::publication::Position,
    ) -> Option<CollectionFacts> {
        let depth = match self.depth {
            Depth::None => 0,
            Depth::Operation(extra) => {
                if extra <= 1 {
                    0
                } else {
                    extra
                }
            }
            Depth::Template(extra) => extra,
        };
        (depth <= cx.call_depth).then_some(())?;
        let accumulator = cx.value(self.accumulator)?;
        let item = cx.value(self.item)?;
        let answer = self.answer(cx.exe, accumulator, item)?;
        cx.finish(
            vec![(self.accumulator, answer)],
            None,
            cx.committed()?,
            cx.after(self.body, 1)?,
            exit,
        )
    }
}

impl Placement {
    pub(in crate::machine::islands) fn prepare_region(
        &self,
        cx: CollectionRegion<'_>,
        exit: room::publication::Position,
    ) -> Option<CollectionFacts> {
        if let Some(at) = self.list {
            matches!(cx.value(at)?, Value::Bool(true)).then_some(())?;
            cx.choose(self.body, 1, true)?;
            cx.choose(self.body, 2, true)?;
        }
        let target = cx.list(self.target)?;
        let index = cx.value(self.index)?;
        let at = crate::compile::position(&index)?;
        let length = cx.arena.len(target).ok()?;
        (at < length).then_some(())?;
        if let Some(bound) = self.bound {
            let length = i64::try_from(length).ok()?;
            cx.boolean(bound, &[index, Value::Int(length)])?
                .then_some(())?;
        }
        let item = cx.value(self.value)?;
        cx.fits(item).then_some(())?;
        let (advance, _) = cx.exe.lib(self.advance).specialization.prepared.as_ref()?;
        let next = advance.call(&[index, self.increment])?;
        let mut writes = vec![(self.index, next)];
        if let Some(at) = self.in_place {
            // This source binding survives the advance Commit until the
            // loop body's Leave, even though no later expression reads it.
            writes.push((at, Value::Bool(true)));
        }
        cx.finish(
            writes,
            Some((target, at, item)),
            cx.committed()?,
            cx.after(self.body, self.statements)?,
            exit,
        )
    }
}

impl Visit {
    pub(in crate::machine::islands) fn prepare_region(
        &self,
        cx: CollectionRegion<'_>,
        exit: room::publication::Position,
    ) -> Option<CollectionFacts> {
        let index = cx.value(self.index)?;
        let length = cx.value(self.length)?;
        if !matches!(index, Value::Int(_) | Value::Float(_))
            || !matches!(length, Value::Int(_) | Value::Float(_))
            || cx.boolean(self.bound, &[index, length])?
        {
            return None;
        }
        cx.choose(self.body, 0, false)?;
        let item = cx.value(self.value)?;
        let arm = usize::from(!cx.hole(item)?);
        cx.choose(self.body, 1, arm == 0)?;
        let mut transfer = cx.after(self.body, 2)?;
        if let Some(block) = self.arms[arm] {
            transfer.blocks.push((block, 0));
        }
        transfer.pc = self.after[arm];
        // The condition's empty tuple is an interior source allocation. Core
        // includes its original ordinal and peak, although it does not survive.
        cx.finish(Vec::new(), None, cx.tested(arm == 0)?, transfer, exit)
    }
}

impl ReadSlot {
    pub(in crate::machine::islands) fn prepare_region(
        &self,
        cx: CollectionRegion<'_>,
        exit: room::publication::Position,
    ) -> Option<CollectionFacts> {
        matches!(cx.value(self.list)?, Value::Bool(true)).then_some(())?;
        cx.choose(self.body, 2, true)?;
        let target = cx.list(self.target)?;
        let index = cx.value(self.index)?;
        let length = i64::try_from(cx.arena.len(target).ok()?).ok()?;
        cx.boolean(self.bound, &[index, Value::Int(length)])?
            .then_some(())?;
        cx.branches
            .borrow_mut()
            .push((cx.exe.stmt(self.bound_statement).site.clone(), true));
        let item = cx.item(target, crate::compile::position(&index)?)?;
        let present = cx.boolean(self.mark, &[Value::Bool(cx.hole(item)?)])?;
        let mut transfer = cx.after(self.body, 3)?;
        transfer
            .blocks
            .extend([(self.arms[0], 1), (self.arms[1], 2)]);
        // The first unpaid event is the inner branch's Leave. Resume that
        // exact suffix, rather than skipping both nested source scopes.
        transfer.pc = cx.compiled.blocks[self.arms[1]].end;
        cx.finish(
            vec![(self.value, item), (self.present, Value::Bool(present))],
            None,
            cx.committed()?,
            transfer,
            exit,
        )
    }
}

impl Merge {
    pub(in crate::machine::islands) fn prepare_head(
        &self,
        cx: CollectionRegion<'_>,
        exit: room::publication::Position,
    ) -> Option<CollectionFacts> {
        let [dest, mut end, mut mid, mut left, mut right, n] = [
            cx.int(self.counters[0])?,
            cx.int(self.counters[1])?,
            cx.int(self.counters[2])?,
            cx.int(self.counters[3])?,
            cx.int(self.counters[4])?,
            cx.int(self.counters[6])?,
        ];
        (0..(1 << 48))
            .contains(&(dest | end | mid | left | right | n))
            .then_some(())?;
        (dest < n).then_some(())?;
        let sorted = cx.list(self.lists[0])?;
        let starts = cx.boolean(self.start, &[Value::Int(dest), Value::Int(end)])?;
        cx.choose(self.body, 0, starts)?;
        if starts {
            let width = cx.int(self.counters[5])?;
            (0..(1 << 48)).contains(&width).then_some(())?;
            let sum = cx.integer(self.sums[0], &[Value::Int(dest), Value::Int(width)])?;
            mid = cx.integer(self.minimum[0], &[Value::Int(sum), Value::Int(n)])?;
            let sum = cx.integer(self.sums[1], &[Value::Int(mid), Value::Int(width)])?;
            end = cx.integer(self.minimum[1], &[Value::Int(sum), Value::Int(n)])?;
            left = dest;
            right = mid;
        }
        (left <= mid && mid <= end && right <= end && end <= n).then_some(())?;
        let has_left = cx.boolean(self.tests[0], &[Value::Int(left), Value::Int(mid)])?;
        cx.choose(self.body, 2, has_left)?;
        let compares = if has_left {
            let has_right = cx.boolean(self.tests[1], &[Value::Int(right), Value::Int(end)])?;
            cx.choose(self.compared[0], 0, has_right)?;
            has_right
        } else {
            false
        };
        if compares {
            let first = cx.item(sorted, left as u64)?;
            let second = cx.item(sorted, right as u64)?;
            let mut writes = Vec::new();
            if starts {
                writes.extend(
                    self.counters[1..5]
                        .iter()
                        .copied()
                        .zip([end, mid, left, right].map(Value::Int)),
                );
            }
            writes.extend([
                (self.from_right, Value::Bool(false)),
                (self.items[0], first),
                (self.items[1], second),
            ]);
            return cx.finish(
                writes,
                None,
                cx.exits.callback.clone()?,
                CollectionTransfer {
                    continuation: CollectionContinuation::Callback,
                    writes: Vec::new(),
                    leave: None,
                    clears: Vec::new(),
                    resume: None,
                    blocks: vec![(self.body, 3), (self.compared[0], 1), (self.compared[1], 2)],
                    pc: self.callback,
                },
                exit,
            );
        }
        let from_right = !has_left;
        cx.choose(self.body, 3, from_right)?;
        let index = if from_right { right } else { left };
        (index >= 0 && index < n).then_some(())?;
        let item = cx.item(sorted, index as u64)?;
        let out = cx.list(self.lists[1])?;
        (sorted != out && dest as u64 <= cx.arena.len(out).ok()?).then_some(())?;
        let advanced = cx.integer(
            self.advances[usize::from(!from_right)],
            &[Value::Int(index), Value::Int(1)],
        )?;
        let destination = cx.integer(self.advances[2], &[Value::Int(dest), Value::Int(1)])?;
        if from_right {
            right = advanced;
        } else {
            left = advanced;
        }
        let mut writes: Vec<_> = self.counters[..5]
            .iter()
            .copied()
            .zip([destination, end, mid, left, right].map(Value::Int))
            .collect();
        writes.push((self.from_right, Value::Bool(from_right)));
        cx.finish(
            writes,
            Some((out, dest as u64, item)),
            cx.committed()?,
            cx.after(self.body, 5)?,
            exit,
        )
    }

    pub(in crate::machine::islands) fn prepare_tail(
        &self,
        cx: CollectionRegion<'_>,
        exit: room::publication::Position,
    ) -> Option<CollectionFacts> {
        let from_right = cx.boolean(self.tests[2], &[Value::Int(0), cx.value(self.order)?])?;
        cx.choose(self.compared[1], 4, from_right)?;
        cx.choose(self.body, 3, from_right)?;
        let selected = self.counters[if from_right { 4 } else { 3 }];
        let (dest, index) = (cx.int(self.counters[0])?, cx.int(selected)?);
        (0..(1 << 48)).contains(&(dest | index)).then_some(())?;
        let sorted = cx.list(self.lists[0])?;
        let out = cx.list(self.lists[1])?;
        (sorted != out && dest as u64 <= cx.arena.len(out).ok()?).then_some(())?;
        // Read again after the callback. It may have changed either list.
        // The callback's saved item operands remain in their own source roots.
        let item = cx.item(sorted, index as u64)?;
        let advanced = cx.integer(
            self.advances[usize::from(!from_right)],
            &[Value::Int(index), Value::Int(1)],
        )?;
        let destination = cx.integer(self.advances[2], &[Value::Int(dest), Value::Int(1)])?;
        let mut transfer = cx.after(self.body, 5)?;
        transfer.leave = Some([self.compared[1], self.compared[0]]);
        // The surrounding body already has its continuation on the stack.
        transfer.blocks.clear();
        transfer.resume = Some((self.body, 5));
        transfer.clears = self
            .compared
            .iter()
            .flat_map(|block| {
                cx.compiled.blocks[*block]
                    .declares
                    .iter()
                    .map(|at| *at as usize)
            })
            .collect();
        cx.finish(
            vec![
                (self.counters[0], Value::Int(destination)),
                (selected, Value::Int(advanced)),
                (self.from_right, Value::Bool(from_right)),
            ],
            Some((out, dest as u64, item)),
            cx.committed()?,
            transfer,
            exit,
        )
    }
}
