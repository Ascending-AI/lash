//! Typed collection sites bind source placements before running a finite
//! kernel. Their output is detached preparation, never source acceptance.
use super::*;
use crate::compile::room::placement::{BindingPlacement, DenseActivation};
use lash_kernel_state::kernel_internals::logical::Slot;
use std::sync::Arc;

/// The type parameter selects a concrete kernel at compile time. There is no
/// runtime family tag or interpreter behind the direct packet entrypoints.
pub(in crate::machine::islands) struct CollectionSite<S> {
    executable: u64,
    shape: S,
    exits: CollectionExits,
    inputs: Inputs,
}
pub(in crate::machine::islands) type AppendSite = CollectionSite<CollectionAppend>;
pub(in crate::machine::islands) type ReduceSite = CollectionSite<Reduction>;
pub(in crate::machine::islands) type PlacementSite = CollectionSite<Placement>;
pub(in crate::machine::islands) type VisitSite = CollectionSite<Visit>;
pub(in crate::machine::islands) type ReadSlotSite = CollectionSite<ReadSlot>;
pub(in crate::machine::islands) type MergeSite = CollectionSite<Merge>;
pub(in crate::machine::islands) type MergeTailSite = CollectionSite<MergeTail>;

pub(in crate::machine::islands) struct MergeTail(Merge);

struct Inputs {
    placement: Arc<BindingPlacement>,
    read: Box<[(usize, Slot)]>,
    write: Box<[(usize, Slot)]>,
    clear: Box<[(usize, Slot)]>,
}

/// Entry and source owners are retained after the activation borrow ends.
/// A caller cannot replace a same-priced source input or choose output owners.
pub(super) struct BoundInputs {
    invocation: Invocation,
    writes: Box<[(usize, Slot)]>,
    clears: Box<[(usize, Slot)]>,
    reads: Box<[(Slot, Value)]>,
    dependencies: Dependencies,
}

/// The runtime's actual activation and source coordinate. This borrows no
/// arena views and is never persisted across a guest callback or cold reopen.
pub(in crate::machine::islands) struct CollectionEntry<'a> {
    pub(in crate::machine::islands) activation: &'a DenseActivation<'a>,
    pub(in crate::machine::islands) invocation: &'a Invocation,
    pub(in crate::machine::islands) exit: room::publication::Position,
}

impl Inputs {
    fn compile(compiled: &Compiled, read: &[usize], write: &[usize]) -> Option<Self> {
        let placement = Arc::clone(compiled.placement.as_ref()?);
        if !Arc::ptr_eq(placement.source(), &compiled.source_room) {
            return None;
        }
        let resolve = |physical: &[usize]| {
            let slots = compiled
                .source_room
                .collection_bindings(&placement, physical)
                .ok()?;
            Some(physical.iter().copied().zip(slots).collect())
        };
        let read = resolve(read)?;
        let write = resolve(write)?;
        Some(Self {
            placement,
            read,
            write,
            clear: Box::default(),
        })
    }

    fn bind(&self, cx: &CollectionRegion<'_>, entry: &CollectionEntry<'_>) -> Option<BoundInputs> {
        let source = self.placement.source();
        if !Arc::ptr_eq(source, &cx.compiled.source_room)
            || !entry
                .activation
                .current(source, cx.ledger, entry.invocation)
            || entry.exit.occurrence.invocation != entry.invocation.id()
            || entry.invocation.frame()
                != (Owner::Frame {
                    task: entry.exit.occurrence.task,
                    invocation: entry.invocation.id(),
                })
        {
            return None;
        }
        let mut handles = Vec::new();
        let mut reads = Vec::new();
        for &(physical, slot) in &self.read {
            let actual = cx.value(physical)?;
            let expected = entry.activation.value(slot)?;
            if !actual.same(&expected) {
                return None;
            }
            handles.extend(actual.handle());
            reads.push((slot, actual));
        }
        // Writes may introduce a binding in an entered source scope. Existing
        // values must still have their exact source placement, including Empty.
        for &(physical, slot) in self.write.iter().chain(&self.clear) {
            let actual = cx.slots.get(physical)?;
            let expected = entry.activation.state(slot)?;
            if !match (actual, expected) {
                (SlotState::Empty, SlotState::Empty) => true,
                (SlotState::Value(actual), SlotState::Value(expected)) => {
                    handles.extend(actual.handle());
                    actual.same(&expected)
                }
                _ => false,
            } {
                return None;
            }
        }
        Some(BoundInputs {
            invocation: entry.invocation.clone(),
            writes: self.write.clone(),
            clears: self.clear.clone(),
            reads: reads.into(),
            dependencies: Dependencies::capture(cx.ledger, cx.arena, &handles)?,
        })
    }
}

impl<S> CollectionSite<S> {
    fn context<'a>(
        &'a self,
        exe: &'a Executable,
        compiled: &'a Compiled,
        ledger: &'a Ledger,
        arena: &'a Arena,
        slots: &'a [SlotState],
        limits: (usize, u32),
    ) -> Option<CollectionRegion<'a>> {
        if exe.local() != self.executable
            || !Arc::ptr_eq(&compiled.source_room, &self.exits.source)
            || !compiled
                .placement
                .as_ref()
                .is_some_and(|placement| Arc::ptr_eq(placement, &self.inputs.placement))
        {
            return None;
        }
        Some(CollectionRegion::new(
            exe,
            compiled,
            &self.exits,
            ledger,
            arena,
            slots,
            limits,
        ))
    }

    fn finish(&self, mut facts: CollectionFacts, entry: BoundInputs) -> Option<CollectionFacts> {
        if facts.writes.iter().any(|(at, _)| {
            entry
                .writes
                .iter()
                .filter(|(physical, _)| physical == at)
                .count()
                != 1
        }) || facts.writes.iter().enumerate().any(|(index, (at, _))| {
            facts.writes[..index]
                .iter()
                .any(|(previous, _)| previous == at)
        }) {
            return None;
        }
        facts.entry = Some(entry);
        Some(facts)
    }
}

// Only immutable descriptors from the actual recognized packet metadata may
// compile. The old tag is read here once during construction, never at entry.
macro_rules! site {
    ($shape:ty, $variant:ident, $reads:expr, $writes:expr, $prepare:ident) => {
        impl CollectionSite<$shape> {
            pub(in crate::machine::islands) fn compile(
                exe: &Executable,
                compiled: &Compiled,
                shape: &$shape,
            ) -> Option<Self> {
                let (step, _) = compiled.code.sites.collection.iter().find(|(step, tail)| {
                    !tail && matches!(&**step, CollectionStep::$variant(actual) if std::ptr::eq(actual, shape))
                })?;
                Some(Self {
                    executable: exe.local(),
                    shape: shape.clone(),
                    exits: CollectionExits::compile(step, exe, compiled)?,
                    inputs: Inputs::compile(compiled, &($reads)(shape), &($writes)(shape))?,
                })
            }

            #[expect(clippy::too_many_arguments, reason = "one authenticated collection entry")]
            pub(in crate::machine::islands) fn prepare(
                &self,
                exe: &Executable,
                compiled: &Compiled,
                ledger: &Ledger,
                arena: &Arena,
                slots: &[SlotState],
                limits: (usize, u32),
                entry: CollectionEntry<'_>,
            ) -> Option<CollectionFacts> {
                let cx = self.context(exe, compiled, ledger, arena, slots, limits)?;
                let bound = self.inputs.bind(&cx, &entry)?;
                let facts = self.shape.$prepare(cx, entry.exit)?;
                self.finish(facts, bound)
            }
        }
    };
}

site!(
    CollectionAppend,
    Append,
    |s: &CollectionAppend| vec![s.target, s.item],
    |_: &CollectionAppend| Vec::<usize>::new(),
    prepare_region
);
impl ReduceSite {
    pub(in crate::machine::islands) fn compile(
        exe: &Executable,
        compiled: &Compiled,
        shape: &Reduction,
    ) -> Option<Self> {
        let (step, _) = compiled.code.sites.collection.iter().find(|(step, tail)| {
            !tail
                && matches!(&**step, CollectionStep::Reduce(actual) if std::ptr::eq(actual, shape))
        })?;
        let function = exe.lib(shape.lib);
        // Body reductions require core's composed source/call acceptance. A
        // prepared scalar entry is not authority to skip their logical frame.
        if !matches!(
            function.run,
            crate::compile::LibRun::Native { body: None, .. }
        ) {
            return None;
        }
        Some(Self {
            executable: exe.local(),
            shape: shape.clone(),
            exits: CollectionExits::compile(step, exe, compiled)?,
            inputs: Inputs::compile(
                compiled,
                &[shape.accumulator, shape.item],
                &[shape.accumulator],
            )?,
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one authenticated native reduction entry"
    )]
    pub(in crate::machine::islands) fn prepare(
        &self,
        exe: &Executable,
        compiled: &Compiled,
        ledger: &Ledger,
        arena: &Arena,
        slots: &[SlotState],
        limits: (usize, u32),
        entry: CollectionEntry<'_>,
    ) -> Option<CollectionFacts> {
        let cx = self.context(exe, compiled, ledger, arena, slots, limits)?;
        let bound = self.inputs.bind(&cx, &entry)?;
        let facts = self.shape.prepare_region(cx, entry.exit)?;
        self.finish(facts, bound)
    }
}
site!(
    Placement,
    Place,
    |s: &Placement| [s.target, s.index, s.value]
        .into_iter()
        .chain(s.list)
        .collect::<Vec<_>>(),
    |s: &Placement| [s.index].into_iter().chain(s.in_place).collect::<Vec<_>>(),
    prepare_region
);
site!(
    Visit,
    Visit,
    |s: &Visit| vec![s.index, s.length, s.value],
    |_: &Visit| Vec::<usize>::new(),
    prepare_region
);
site!(
    ReadSlot,
    ReadSlot,
    |s: &ReadSlot| vec![s.target, s.index, s.list],
    |s: &ReadSlot| vec![s.value, s.present],
    prepare_region
);
site!(
    Merge,
    Merge,
    |s: &Merge| s
        .counters
        .into_iter()
        .chain(s.lists)
        .chain(s.callback_binding)
        .collect::<Vec<_>>(),
    |s: &Merge| s.counters[..5]
        .iter()
        .copied()
        .chain([s.from_right])
        .chain(s.items)
        .collect::<Vec<_>>(),
    prepare_head
);

impl MergeTailSite {
    pub(in crate::machine::islands) fn compile(
        exe: &Executable,
        compiled: &Compiled,
        shape: &Merge,
    ) -> Option<Self> {
        let (step, _) = compiled.code.sites.collection.iter().find(|(step, tail)| {
            *tail && matches!(&**step, CollectionStep::Merge(actual) if std::ptr::eq(actual, shape))
        })?;
        let reads: Vec<_> = [
            shape.counters[0],
            shape.counters[3],
            shape.counters[4],
            shape.order,
        ]
        .into_iter()
        .chain(shape.lists)
        .chain(shape.items)
        .chain(shape.callback_binding)
        .collect();
        let writes = [
            shape.counters[0],
            shape.counters[3],
            shape.counters[4],
            shape.from_right,
        ];
        let mut inputs = Inputs::compile(compiled, &reads, &writes)?;
        let clears: Vec<_> = shape
            .compared
            .iter()
            .flat_map(|block| {
                compiled.blocks[*block]
                    .declares
                    .iter()
                    .map(|at| *at as usize)
            })
            .collect();
        let logical = compiled
            .source_room
            .collection_bindings(&inputs.placement, &clears)
            .ok()?;
        inputs.clear = clears.into_iter().zip(logical).collect();
        Some(Self {
            executable: exe.local(),
            shape: MergeTail(shape.clone()),
            exits: CollectionExits::compile(step, exe, compiled)?,
            inputs,
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one post-callback collection entry"
    )]
    pub(in crate::machine::islands) fn prepare(
        &self,
        exe: &Executable,
        compiled: &Compiled,
        ledger: &Ledger,
        arena: &Arena,
        slots: &[SlotState],
        limits: (usize, u32),
        entry: CollectionEntry<'_>,
    ) -> Option<CollectionFacts> {
        let cx = self.context(exe, compiled, ledger, arena, slots, limits)?;
        let bound = self.inputs.bind(&cx, &entry)?;
        let facts = self.shape.0.prepare_tail(cx, entry.exit)?;
        self.finish(facts, bound)
    }
}

impl CollectionFacts {
    /// Final owners come only from the compiled source placement. Core must
    /// still authenticate the selected multi-event path, debt and interior peak
    /// before accepting this detached boundary.
    pub(in crate::machine::islands) fn prepare_direct_boundary<'a>(
        mut self,
        ledger: &Ledger,
        arena: &'a mut Arena,
        heap: &PagedHeap,
    ) -> Result<Option<CollectionBoundary<'a>>, HeapFault> {
        let Some(entry) = self.entry.take() else {
            return Ok(None);
        };
        if !entry.dependencies.is_current(ledger, arena)
            || !self.dependencies.is_current(ledger, arena)
        {
            return Ok(None);
        }
        let mut inputs = Vec::new();
        for physical in &self.transfer.clears {
            let mut selected = entry.clears.iter().filter(|(at, _)| at == physical);
            let Some((_, slot)) = selected.next() else {
                return Ok(None);
            };
            if selected.next().is_some() {
                return Ok(None);
            }
            let owner = entry.invocation.owner(*slot);
            if ledger.holding(owner, heap.pages())?.is_some() && !inputs.contains(&owner) {
                inputs.push(owner);
            }
        }
        let mut outputs = Vec::new();
        for (physical, value) in &self.writes {
            let mut selected = entry.writes.iter().filter(|(at, _)| at == physical);
            let Some((_, slot)) = selected.next() else {
                return Ok(None);
            };
            if selected.next().is_some() {
                return Ok(None);
            }
            let owner = entry.invocation.owner(*slot);
            if ledger.holding(owner, heap.pages())?.is_some() {
                inputs.push(owner);
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
        let mut transaction = ledger
            .prepare_source_boundary(registration, heap, &inputs, &outputs)?
            .bind_dependencies(entry.dependencies)?
            .bind_dependencies(self.dependencies)?;
        if let Some((handle, index, value)) = self.member {
            transaction = match transaction.then_member(
                ledger,
                heap.pages(),
                handle,
                Member::List(index),
                Some(value),
                &[],
                &[],
            ) {
                Ok(transaction) => transaction,
                Err(MemberFault::Heap(error)) => return Err(error),
                Err(MemberFault::Native(lash_kernel_doc::NativeError::Host(error))) => {
                    return Err(error);
                }
                Err(_) => return Ok(None),
            };
        }
        self.transfer.writes = self.writes;
        Ok(Some(CollectionBoundary {
            transaction,
            storage,
            transfer: self.transfer,
            exit: self.exit,
            kind: self.kind,
            branches: self.branches,
        }))
    }
}

/// Owned callback transfer for K's phase certificate. These are actual source
/// values and semantic counters, not a native buffer's unwritten scratch tail.
/// No arena view or physical PC survives guest callback entry.
pub(in crate::machine::islands) struct CollectionCallback {
    source: Arc<room::Source>,
    invocation: Invocation,
    position: room::publication::Position,
    dependencies: Dependencies,
    pub(in crate::machine::islands) statement: crate::compile::StmtId,
    pub(in crate::machine::islands) target: CallbackTarget,
    pub(in crate::machine::islands) buffers: [ArenaHandle; 2],
    pub(in crate::machine::islands) lengths: [u64; 2],
    pub(in crate::machine::islands) arguments: [Value; 2],
    pub(in crate::machine::islands) before: Box<[(Slot, Value)]>,
    pub(in crate::machine::islands) after: Box<[(Slot, Value)]>,
    pub(in crate::machine::islands) pending: PendingComparison,
}

pub(in crate::machine::islands) enum CallbackTarget {
    Value(Value),
    Declared(crate::compile::CodeId),
}

pub(in crate::machine::islands) struct PendingComparison {
    pub(in crate::machine::islands) destination: u64,
    pub(in crate::machine::islands) left: u64,
    pub(in crate::machine::islands) right: u64,
    pub(in crate::machine::islands) middle: u64,
    pub(in crate::machine::islands) end: u64,
    pub(in crate::machine::islands) width: u64,
    pub(in crate::machine::islands) length: u64,
}

impl MergeSite {
    /// This shell does not execute or certify the callback. K binds its phase
    /// proof and the common call compiler authenticates the actual callee.
    /// The input facts must be the candidate returned by this site's prepare.
    pub(in crate::machine::islands) fn callback(
        &self,
        facts: &CollectionFacts,
        ledger: &Ledger,
        arena: &Arena,
    ) -> Option<CollectionCallback> {
        let entry = facts.entry.as_ref()?;
        let selected = self.exits.callback.as_ref()?;
        if facts.exit.at != selected.0
            || facts.exit.unpaid.as_ref() != Some(&selected.1)
            || facts.transfer.continuation != CollectionContinuation::Callback
            || !Arc::ptr_eq(&facts.source, &self.exits.source)
            || !entry.dependencies.is_current(ledger, arena)
            || !facts.dependencies.is_current(ledger, arena)
        {
            return None;
        }
        let read = |physical| {
            let slot = self.inputs.read.iter().find(|(at, _)| *at == physical)?.1;
            entry
                .reads
                .iter()
                .find_map(|(at, value)| (*at == slot).then_some(*value))
        };
        let written = |physical| {
            facts
                .writes
                .iter()
                .find_map(|(at, value)| (*at == physical).then_some(*value))
                .or_else(|| read(physical))
        };
        let number = |physical| match written(physical)? {
            Value::Int(value) => u64::try_from(value).ok(),
            _ => None,
        };
        let [Value::List(sorted), Value::List(out)] =
            [read(self.shape.lists[0])?, read(self.shape.lists[1])?]
        else {
            return None;
        };
        if sorted == out {
            return None;
        }
        let arguments = self.shape.items.map(written);
        let [Some(first), Some(second)] = arguments else {
            return None;
        };
        let pending = PendingComparison {
            destination: number(self.shape.counters[0])?,
            left: number(self.shape.counters[3])?,
            right: number(self.shape.counters[4])?,
            middle: number(self.shape.counters[2])?,
            end: number(self.shape.counters[1])?,
            width: number(self.shape.counters[5])?,
            length: number(self.shape.counters[6])?,
        };
        if !arena.item(sorted, pending.left).ok()??.same(&first)
            || !arena.item(sorted, pending.right).ok()??.same(&second)
            || pending.left >= pending.middle
            || pending.right >= pending.end
        {
            return None;
        }
        let target = match (self.shape.callback_binding, self.shape.callback_declared) {
            (Some(at), None) => {
                let value = read(at)?;
                if !matches!(value, Value::Function(_) | Value::Closure(_)) {
                    return None;
                }
                CallbackTarget::Value(value)
            }
            (None, Some(code)) => CallbackTarget::Declared(code),
            _ => return None,
        };
        let after = facts
            .writes
            .iter()
            .map(|(physical, value)| {
                let slot = entry.writes.iter().find(|(at, _)| at == physical)?.1;
                Some((slot, *value))
            })
            .collect::<Option<Box<_>>>()?;
        let handles: Vec<_> = entry
            .reads
            .iter()
            .map(|(_, value)| *value)
            .chain(facts.writes.iter().map(|(_, value)| *value))
            .filter_map(|value| value.handle())
            .collect();
        Some(CollectionCallback {
            source: Arc::clone(&facts.source),
            invocation: entry.invocation.clone(),
            position: facts.exit.clone(),
            dependencies: Dependencies::capture(ledger, arena, &handles)?,
            statement: self.shape.callback_statement,
            target,
            buffers: [sorted, out],
            lengths: [arena.len(sorted).ok()?, arena.len(out).ok()?],
            arguments: [first, second],
            before: entry.reads.clone(),
            after,
            pending,
        })
    }
}

impl CollectionCallback {
    pub(in crate::machine::islands) fn source(&self) -> &Arc<room::Source> {
        &self.source
    }
    pub(in crate::machine::islands) fn invocation(&self) -> &Invocation {
        &self.invocation
    }
    pub(in crate::machine::islands) fn position(&self) -> &room::publication::Position {
        &self.position
    }
    pub(in crate::machine::islands) fn current(
        &self,
        source: &Arc<room::Source>,
        invocation: &Invocation,
        position: &room::publication::Position,
        ledger: &Ledger,
        arena: &Arena,
    ) -> bool {
        Arc::ptr_eq(source, &self.source)
            && invocation == &self.invocation
            && position == &self.position
            && self.dependencies.is_current(ledger, arena)
    }
}
