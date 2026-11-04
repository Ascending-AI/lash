//! Typed collection sites bind source placements before running a finite
//! kernel. Their output is detached preparation, never source acceptance.
use super::*;
use crate::compile::room::placement::{BindingPlacement, DenseActivation};
use lash_kernel_state::kernel_internals::logical::Slot;
use std::sync::Arc;

/// The type parameter selects a concrete kernel at compile time. There is no
/// runtime family tag or interpreter behind the direct packet entrypoints.
pub(in crate::machine::islands) struct CollectionSite<S> {
    shape: S,
    exits: CollectionExits,
    inputs: Inputs,
    programs: Box<[Arc<room::CollectionProgram>]>,
}
pub(in crate::machine::islands) type AppendSite = CollectionSite<CollectionAppend>;
pub(in crate::machine::islands) type ReduceSite = CollectionSite<Reduction>;
pub(in crate::machine::islands) type PlacementSite = CollectionSite<Placement>;
pub(in crate::machine::islands) type VisitSite = CollectionSite<Visit>;
pub(in crate::machine::islands) type ReadSlotSite = CollectionSite<ReadSlot>;
pub(in crate::machine::islands) type MergeSite = CollectionSite<Merge>;
pub(in crate::machine::islands) type MergeTailSite = CollectionSite<MergeTail>;

pub(in crate::machine::islands) struct MergeTail(Merge);
impl MergeTail {
    pub(in crate::machine::islands) fn new(shape: Merge) -> Self {
        Self(shape)
    }
    pub(in crate::machine::islands) fn shape(&self) -> &Merge {
        &self.0
    }
}

pub(in crate::machine::islands) struct CandidateContext<'a> {
    pub(in crate::machine::islands) exe: &'a Executable,
    pub(in crate::machine::islands) compiled: &'a Compiled,
    pub(in crate::machine::islands) ledger: &'a Ledger,
    pub(in crate::machine::islands) arena: &'a Arena,
    pub(in crate::machine::islands) slots: &'a [SlotState],
    pub(in crate::machine::islands) limits: (usize, u32),
}
pub(in crate::machine::islands) trait CollectionCandidate {
    fn candidate(
        &self,
        context: CandidateContext<'_>,
        entry: CollectionEntry<'_>,
    ) -> Option<CollectionFacts>;
}

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
        for &(physical, slot) in &self.read {
            let actual = cx.value(physical)?;
            let expected = entry.activation.value(slot)?;
            if !actual.same(&expected) {
                return None;
            }
            handles.extend(actual.handle());
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
            dependencies: Dependencies::capture(cx.ledger, cx.arena, &handles)?,
        })
    }
}

fn source_programs(
    exe: &Executable,
    compiled: &Compiled,
    exits: &CollectionExits,
    body: usize,
    statement: Option<usize>,
) -> Option<Box<[Arc<room::CollectionProgram>]>> {
    let placement = compiled.placement.as_ref()?;
    let block = exe.block(compiled.blocks.get(body)?.id);
    let start = match statement {
        Some(index) => &exe.stmt(*block.stmts.get(index)?).site,
        None => &block.site,
    };
    let first = compiled.source_room.first_event(start).ok()?;
    let mut completed = Vec::new();
    let mut programs = Vec::new();
    for exit in exits
        .commit
        .iter()
        .chain(exits.callback.iter())
        .chain(exits.hole.iter().flatten())
    {
        if completed.contains(&exit.0) {
            continue;
        }
        completed.push(exit.0.clone());
        let Ok(paths) = room::CollectionProgram::compile_paths(
            exe,
            placement.code(),
            first.clone(),
            exit.0.clone(),
        ) else {
            continue;
        };
        for program in paths {
            program.verify(exe).ok()?;
            programs.push(Arc::new(program));
        }
    }
    (!programs.is_empty()).then(|| programs.into())
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
        if !Arc::ptr_eq(&compiled.source_room, &self.exits.source)
            || !Arc::ptr_eq(
                &exe.code(self.inputs.placement.code()).source_room,
                &self.exits.source,
            )
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
        let program = self
            .programs
            .iter()
            .find(|program| program.matches(&facts.exit, &facts.branches))?;
        facts.program = Some(Arc::clone(program));
        facts.entry = Some(entry);
        Some(facts)
    }
}

// Only immutable descriptors from the actual recognized packet metadata may
// compile. The old tag is read here once during construction, never at entry.
macro_rules! site {
    ($shape:ty, $variant:ident, $table:ident, $reads:expr, $writes:expr, $prepare:ident) => {
        impl CollectionSite<$shape> {
            pub(in crate::machine::islands) fn compile(
                exe: &Executable,
                compiled: &Compiled,
                shape: &$shape,
            ) -> Option<Self> {
                compiled
                    .code
                    .sites
                    .$table
                    .iter()
                    .find(|site| std::ptr::eq(&site.shape, shape))?;
                let step = CollectionStep::$variant(shape.clone());
                let exits = CollectionExits::compile(&step, exe, compiled)?;
                let programs = source_programs(exe, compiled, &exits, shape.body, None)?;
                Some(Self {
                    shape: shape.clone(),
                    exits,
                    programs,
                    inputs: Inputs::compile(compiled, &($reads)(shape), &($writes)(shape))?,
                })
            }

            #[expect(
                clippy::too_many_arguments,
                reason = "one authenticated collection entry"
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
                let facts = self.shape.$prepare(cx, entry.exit)?;
                self.finish(facts, bound)
            }
        }
        impl CollectionCandidate for CollectionSite<$shape> {
            fn candidate(
                &self,
                context: CandidateContext<'_>,
                entry: CollectionEntry<'_>,
            ) -> Option<CollectionFacts> {
                self.prepare(
                    context.exe,
                    context.compiled,
                    context.ledger,
                    context.arena,
                    context.slots,
                    context.limits,
                    entry,
                )
            }
        }
    };
}

site!(
    CollectionAppend,
    Append,
    collection_append,
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
        compiled
            .code
            .sites
            .collection_reduce
            .iter()
            .find(|site| std::ptr::eq(&site.shape, shape))?;
        let step = CollectionStep::Reduce(shape.clone());
        let function = exe.lib(shape.callback_site.library()?);
        // Body reductions require core's composed source/call acceptance. A
        // prepared scalar entry is not authority to skip their logical frame.
        if !matches!(
            function.run,
            crate::compile::LibRun::Native { body: None, .. }
        ) {
            return None;
        }
        let exits = CollectionExits::compile(&step, exe, compiled)?;
        let programs = source_programs(exe, compiled, &exits, shape.body, None)?;
        Some(Self {
            shape: shape.clone(),
            exits,
            programs,
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
    collection_placement,
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
    collection_visit,
    |s: &Visit| vec![s.index, s.length, s.value],
    |_: &Visit| Vec::<usize>::new(),
    prepare_region
);
site!(
    ReadSlot,
    ReadSlot,
    collection_read,
    |s: &ReadSlot| vec![s.target, s.index, s.list],
    |s: &ReadSlot| vec![s.value, s.present],
    prepare_region
);
site!(
    Merge,
    Merge,
    collection_merge,
    |s: &Merge| s.counters.into_iter().chain(s.lists).collect::<Vec<_>>(),
    |s: &Merge| s.counters[..5]
        .iter()
        .copied()
        .chain(s.right_flag())
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
        compiled
            .code
            .sites
            .collection_tail
            .iter()
            .find(|site| std::ptr::eq(&site.shape.0, shape))?;
        let step = CollectionStep::Merge(shape.clone());
        if shape.loop_.is_some() {
            let reads: Vec<_> = [
                shape.counters[0],
                shape.counters[3],
                shape.counters[4],
                shape.order,
                shape.lists[1],
            ]
            .into_iter()
            .chain(shape.items)
            .collect();
            let inputs = Inputs::compile(
                compiled,
                &reads,
                &[shape.counters[0], shape.counters[3], shape.counters[4]],
            )?;
            let mut exits = CollectionExits::compile(&step, exe, compiled)?;
            exits.callback = None;
            let programs = source_programs(exe, compiled, &exits, shape.body, Some(4))?;
            return Some(Self {
                shape: MergeTail(shape.clone()),
                exits,
                inputs,
                programs,
            });
        }
        let reads: Vec<_> = [
            shape.counters[0],
            shape.counters[3],
            shape.counters[4],
            shape.order,
            shape.right_flag()?,
        ]
        .into_iter()
        .chain(shape.lists)
        .chain(shape.items)
        .collect();
        let writes = [
            shape.counters[0],
            shape.counters[3],
            shape.counters[4],
            shape.right_flag()?,
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
        let mut exits = CollectionExits::compile(&step, exe, compiled)?;
        // Tail cannot re-enter the head's callback preparation.
        exits.callback = None;
        let programs = source_programs(exe, compiled, &exits, shape.compared[1], Some(4))?;
        Some(Self {
            shape: MergeTail(shape.clone()),
            exits,
            inputs,
            programs,
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

impl CollectionCandidate for ReduceSite {
    fn candidate(
        &self,
        context: CandidateContext<'_>,
        entry: CollectionEntry<'_>,
    ) -> Option<CollectionFacts> {
        self.prepare(
            context.exe,
            context.compiled,
            context.ledger,
            context.arena,
            context.slots,
            context.limits,
            entry,
        )
    }
}
impl CollectionCandidate for MergeTailSite {
    fn candidate(
        &self,
        context: CandidateContext<'_>,
        entry: CollectionEntry<'_>,
    ) -> Option<CollectionFacts> {
        self.prepare(
            context.exe,
            context.compiled,
            context.ledger,
            context.arena,
            context.slots,
            context.limits,
            entry,
        )
    }
}
impl<S> CollectionSite<S> {
    pub(in crate::machine::islands) fn positions(
        &self,
        occurrence: room::publication::Occurrence,
    ) -> impl Iterator<Item = room::publication::Position> + '_ {
        self.programs
            .iter()
            .flat_map(move |program| program.publication_positions(occurrence.clone()))
    }
}

impl CollectionFacts {
    /// Final owners come only from the compiled source placement. Core must
    /// still authenticate the selected multi-event path, debt and interior peak
    /// before accepting this detached boundary.
    #[cfg(test)]
    pub(in crate::machine::islands) fn prepare_direct_boundary<'a>(
        mut self,
        ledger: &Ledger,
        arena: &'a mut Arena,
        heap: &PagedHeap,
    ) -> Result<Option<CollectionBoundary<'a>>, HeapFault> {
        let Some(program) = self.program.as_ref() else {
            return Ok(None);
        };
        if !Arc::ptr_eq(program.source(), &self.source)
            || !program.matches(&self.exit, &self.branches)
        {
            return Ok(None);
        }
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
        }))
    }
}

impl Compiled {
    /// Final relocation is complete. Source exits and placements are compiled
    /// once into each typed site, never rebuilt by packet dispatch.
    pub(in crate::machine::islands) fn compile_collection_sites(
        &self,
        exe: &Executable,
    ) -> Option<()> {
        macro_rules! compile {
            ($table:ident, $site:ty, $shape:expr) => {
                for packet in &self.code.sites.$table {
                    packet
                        .proof
                        .set(<$site>::compile(exe, self, ($shape)(&packet.shape)))
                        .ok()?;
                }
            };
        }
        compile!(collection_append, AppendSite, |shape| shape);
        compile!(collection_reduce, ReduceSite, |shape| shape);
        compile!(collection_placement, PlacementSite, |shape| shape);
        compile!(collection_visit, VisitSite, |shape| shape);
        compile!(collection_read, ReadSlotSite, |shape| shape);
        compile!(collection_merge, MergeSite, |shape| shape);
        compile!(collection_tail, MergeTailSite, MergeTail::shape);
        Some(())
    }
}

impl CollectionFacts {
    #[expect(
        clippy::too_many_arguments,
        reason = "one source candidate and affine resource window"
    )]
    pub(in crate::machine::islands) fn accept<'a, 'r>(
        mut self,
        exe: &Executable,
        frontier: &room::publication::Frontier,
        activation: &DenseActivation<'_>,
        ledger: &Ledger,
        heap: &PagedHeap,
        arena: &'a mut Arena,
        resources: crate::admission::SourceResources<'r>,
        memory_bound: u64,
    ) -> Result<
        Option<(
            crate::admission::SourceAccepted<
                'r,
                room::publication::boundary::PreparedPublication<'a>,
            >,
            room::CollectionPublicationStorage,
            CollectionTransfer,
        )>,
        HeapFault,
    > {
        let Some(entry) = self.entry.as_ref() else {
            return Ok(None);
        };
        let Some(program) = &self.program else {
            return Ok(None);
        };
        if !entry.dependencies.is_current(ledger, arena)
            || !self.dependencies.is_current(ledger, arena)
        {
            return Ok(None);
        }
        let Some(writes) = self
            .writes
            .iter()
            .map(|(physical, value)| {
                entry
                    .writes
                    .iter()
                    .find(|(at, _)| at == physical)
                    .map(|(_, slot)| (*slot, *value))
            })
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(None);
        };
        let Some(clears) = self
            .transfer
            .clears
            .iter()
            .map(|physical| {
                entry
                    .clears
                    .iter()
                    .find(|(at, _)| at == physical)
                    .map(|(_, slot)| *slot)
            })
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(None);
        };
        let member = self.member.map(|(handle, index, value)| MemberEdit {
            handle,
            member: Member::List(index),
            next: Some(value),
        });
        let Some((accepted, storage)) = program.prepare_publication(
            exe,
            frontier,
            activation,
            &entry.invocation,
            ledger,
            heap,
            arena,
            room::CollectionResult {
                writes: &writes,
                clears: &clears,
                member: member.as_ref(),
                branches: &self.branches,
                exit: &self.exit,
            },
            resources,
            memory_bound,
        )?
        else {
            return Ok(None);
        };
        self.transfer.writes = self.writes;
        Ok(Some((accepted, storage, self.transfer)))
    }
    pub(in crate::machine::islands::execute) fn reserve(
        &mut self,
        frame: &mut Values<'_, '_>,
    ) -> Result<bool, HeapFault> {
        self.transfer.writes = self.writes.clone();
        self.transfer.reserve_frame(frame)
    }
    #[cfg(test)]
    pub(in crate::machine::islands::execute) fn publication_start(
        &self,
    ) -> Option<room::publication::Position> {
        self.program.as_ref()?.publication_start(&self.exit)
    }
}
