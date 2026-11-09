//! The first slot heap: the kernel-state [`Arena`]'s slot vector of
//! [`lash_kernel_doc::arena::Body`] values, with its canonical keys and
//! tables, admitted by a logical schedule and collected by mark-sweep from
//! the machine's roots.
//!
//! Each live slot's schedule is kept beside it, read from its body
//! ([`super::meter::body`]), so a write that grows an object settles to
//! exactly what the object then holds (contract §2, R7).

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::arena::Objects;
use lash_kernel_doc::arena::native::{Args, NativeError, NewObject, Nursery};
use lash_kernel_state::arena::{Arena, ArenaSemantics, Visit};

use super::open;
use super::{Heap, LoadError, MemoryRefused};

pub(crate) use super::meter::{CELL, closure as closure_units};

/// The schedule units `object` costs once placed.
pub(crate) fn schedule(object: &NewObject) -> u64 {
    super::meter::new_object(object)
}

/// Builds the error value of `kind`, `message` and `data` into `nursery`
/// (`K-ERR-001`): its texts, its data object and itself.
fn build_error<'call>(
    nursery: &mut Nursery<'call>,
    args: Args<'call>,
    kind: &str,
    message: &str,
    data: Value,
) -> Result<Value, NativeError> {
    let kind = nursery.text(kind);
    let message = nursery.text(message);
    let data = lash_kernel_doc::error_object::native(args, nursery, kind, message, data)?;
    Ok(nursery.allocate(NewObject::Error {
        kind,
        message,
        data,
    }))
}

/// A slot heap bounded by `bound` schedule units.
pub(crate) struct SlotHeap {
    arena: Arena,
    /// The run's directory, once the heap has one to load from.
    durable: Option<super::open::Durable>,
    /// Each slot's schedule units, by slot index, while its object lives.
    units: Vec<u64>,
    held: u64,
    bound: u64,
}

impl SlotHeap {
    pub(crate) fn new(bound: u64) -> Self {
        Self {
            arena: Arena::new(),
            durable: None,
            units: Vec::new(),
            held: 0,
            bound,
        }
    }

    /// Records what slot `handle` costs; its admission came first.
    fn place(&mut self, handle: ArenaHandle, units: u64) {
        let index = handle.index() as usize;
        if self.units.len() <= index {
            self.units.resize(index + 1, 0);
        }
        self.units[index] = units;
    }

    /// A heap bounded by `bound` that writes its objects to `directory`.
    pub(crate) fn durable(bound: u64, durable: open::Durable) -> Self {
        let mut heap = Self::new(bound);
        heap.durable = Some(durable);
        heap
    }

    /// The heap of a sealed `root`, read from `directory` as it needs:
    /// the root values, each object by proxy, and nothing loaded.
    ///
    /// # Errors
    ///
    /// [`LoadError::Memory`] when the bound does not admit the root
    /// proxies; [`LoadError::Store`] for a root the arena cannot hold.
    pub(crate) fn open(
        bound: u64,
        root: &lash_kernel_state::paged::run::root::HeapRoot,
        directory: lash_kernel_state::paged::run::directory::Directory,
    ) -> Result<(Self, Vec<Value>), LoadError> {
        let mut heap = Self::durable(
            bound,
            open::Durable {
                directory,
                high_water: root.high_water,
            },
        );
        let mut values = Vec::with_capacity(root.roots.len());
        for value in &root.roots {
            let value = open::value(&mut heap.arena, value).map_err(|reason| {
                LoadError::Store(lash_kernel_doc::heap::HeapFault::Malformed(reason))
            })?;
            if let Some(handle) = value.handle()
                && heap
                    .units
                    .get(handle.index() as usize)
                    .is_none_or(|units| *units == 0)
            {
                let units = heap.arena.body(handle).map_or(0, super::meter::body);
                heap.admit(units).map_err(LoadError::Memory)?;
                heap.place(handle, units);
            }
            values.push(value);
        }
        Ok((heap, values))
    }

    /// Seals every object `roots` reach into the heap's directory.
    ///
    /// # Errors
    ///
    /// As [`super::seal::seal`]; a heap with no directory cannot seal.
    pub(crate) fn seal(
        &mut self,
        roots: &[Value],
    ) -> Result<lash_kernel_state::paged::run::root::HeapRoot, crate::paged::SealError> {
        let durable = self
            .durable
            .as_mut()
            .ok_or(crate::paged::SealError::Fault {
                problem: "the heap has no directory".into(),
            })?;
        super::seal::seal(
            &mut self.arena,
            roots,
            &mut durable.directory,
            &mut durable.high_water,
        )
    }

    /// What loading `record` adds: its body and a proxy for each referent
    /// the arena does not hold.
    fn load_units(&self, record: &lash_kernel_state::paged::run::objects::Record) -> u64 {
        let mut units = super::meter::record(record);
        for (id, kind) in open::referents(record).unwrap_or_default() {
            if self.arena.entity_handle(open::entity(id)).is_none() {
                units = units.saturating_add(super::meter::body(&kind.placeholder()));
            }
        }
        units
    }

    /// Records the new object in slot `handle`, which was admitted for
    /// `admitted`: the holding settles to what its body holds (a map
    /// built from pairs that repeat a key holds fewer than it was
    /// admitted for).
    fn settle_placed(&mut self, handle: ArenaHandle, admitted: u64) {
        if self
            .units
            .get(handle.index() as usize)
            .is_some_and(|units| *units != 0)
        {
            // A shared object the arena gave again (the empty tuple):
            // counted once, by its slot.
            self.held = self.held.saturating_sub(admitted);
            return;
        }
        let now = self.arena.body(handle).map_or(0, super::meter::body);
        self.held = self.held.saturating_sub(admitted).saturating_add(now);
        self.place(handle, now);
    }
}

impl Heap for SlotHeap {
    #[inline]
    fn objects(&self) -> &Objects {
        self.arena.objects()
    }

    #[inline]
    fn semantics(&self) -> ArenaSemantics<'_> {
        self.arena.semantics()
    }

    #[inline]
    fn admit(&mut self, units: u64) -> Result<(), MemoryRefused> {
        match self.held.checked_add(units) {
            Some(held) if held <= self.bound => {
                self.held = held;
                Ok(())
            }
            _ => Err(MemoryRefused {
                held: self.held,
                wanted: units,
                bound: self.bound,
            }),
        }
    }

    #[inline]
    fn held(&self) -> u64 {
        self.held
    }

    #[inline]
    fn bound(&self) -> u64 {
        self.bound
    }

    fn create(&mut self, object: NewObject) -> Result<Value, NativeError> {
        let admitted = schedule(&object);
        let value = self.arena.create(object)?;
        if let Some(handle) = value.handle() {
            self.settle_placed(handle, admitted);
        }
        Ok(value)
    }

    fn install(&mut self, built: Vec<(ArenaHandle, NewObject)>) -> Result<(), NativeError> {
        let placed: Vec<(ArenaHandle, u64)> = built
            .iter()
            .map(|(handle, object)| (*handle, schedule(object)))
            .collect();
        self.arena.install(built)?;
        for (handle, admitted) in placed {
            self.settle_placed(handle, admitted);
        }
        Ok(())
    }

    fn error_units(&self, kind: &str, message: &str, data: Value) -> Result<u64, NativeError> {
        let objects = self.arena.objects();
        let semantics = self.arena.semantics();
        let mut nursery = Nursery::unaccounted(objects);
        build_error(
            &mut nursery,
            Args::new(&[], objects, &semantics),
            kind,
            message,
            data,
        )?;
        let units = nursery
            .built()
            .iter()
            .map(|(_, object)| schedule(object))
            .fold(0, u64::saturating_add);
        nursery.discard();
        Ok(units)
    }

    fn error(&mut self, kind: &str, message: &str, data: Value) -> Result<Value, NativeError> {
        let (value, built) = {
            let objects = self.arena.objects();
            let semantics = self.arena.semantics();
            let mut nursery = Nursery::unaccounted(objects);
            let value = build_error(
                &mut nursery,
                Args::new(&[], objects, &semantics),
                kind,
                message,
                data,
            )?;
            (value, nursery.into_new())
        };
        self.install(built)?;
        Ok(value)
    }

    #[inline]
    fn release(&mut self, units: u64) {
        self.held = self.held.saturating_sub(units);
    }

    fn settle(&mut self, handle: ArenaHandle, admitted: u64) {
        let index = handle.index() as usize;
        let before = self.units.get(index).copied().unwrap_or(0);
        let now = self.arena.body(handle).map_or(0, super::meter::body);
        self.held = self
            .held
            .saturating_sub(admitted)
            .saturating_sub(before)
            .saturating_add(now);
        self.place(handle, now);
    }

    fn cell(&mut self, value: Value) -> ArenaHandle {
        let handle = self.arena.new_cell(value);
        self.place(handle, CELL);
        handle
    }

    fn cell_get(&self, cell: ArenaHandle) -> Result<Value, NativeError> {
        self.arena.cell(cell)
    }

    fn cell_set(&mut self, cell: ArenaHandle, value: Value) -> Result<(), NativeError> {
        self.arena.cell_set(cell, value)
    }

    fn closure(
        &mut self,
        identity: std::sync::Arc<lash_kernel_doc::arena::ClosureIdentity>,
        captures: Vec<ArenaHandle>,
    ) -> ArenaHandle {
        let units = closure_units(captures.len());
        let handle = self.arena.new_closure(identity, captures);
        self.place(handle, units);
        handle
    }

    #[inline]
    fn arena(&self) -> &Arena {
        &self.arena
    }

    #[inline]
    fn arena_mut(&mut self) -> &mut Arena {
        &mut self.arena
    }

    fn collect(&mut self, roots: &mut dyn FnMut(&mut Visit<'_>)) {
        self.arena.collect_cycles(roots);
        let objects = self.arena.objects();
        let mut freed = 0u64;
        for (index, units) in self.units.iter_mut().enumerate() {
            if *units != 0 && objects.handle_at(index as u32).is_none() {
                freed = freed.saturating_add(*units);
                *units = 0;
            }
        }
        // What the objects did not hold (frames, the program, transients)
        // is not the collector's to free.
        self.held = self.held.saturating_sub(freed);
    }

    fn load(
        &mut self,
        object: ArenaHandle,
        chunk: u32,
        roots: &mut dyn FnMut(&mut Visit<'_>),
    ) -> Result<(), LoadError> {
        use lash_kernel_doc::heap::HeapFault;
        let malformed = |reason: String| LoadError::Store(HeapFault::Malformed(reason));
        if !self
            .arena
            .facts(object)
            .map_err(|error| malformed(error.to_string()))?
            .cold
        {
            return Ok(());
        }
        let record = {
            let Some(durable) = self.durable.as_ref() else {
                return Err(malformed(
                    "a cold object in a heap with no directory".into(),
                ));
            };
            let corrupt = |reason: String| LoadError::Store(durable.corrupt(reason));
            if chunk != 0 {
                return Err(corrupt(format!("a whole row has no chunk {chunk}")));
            }
            let id = open::id_of(&self.arena, object).map_err(malformed)?;
            let record = durable
                .directory
                .get(id)
                .map_err(LoadError::Store)?
                .ok_or_else(|| corrupt(format!("object {} has no row", id.0)))?
                .record;
            open::referents(&record).map_err(corrupt)?;
            record
        };

        // Admission of the growth: the body replaces the proxy's
        // placeholder. Then one collection from the roots, and once more.
        let placeholder = self
            .units
            .get(object.index() as usize)
            .copied()
            .unwrap_or(0);
        let wanted = self.load_units(&record).saturating_sub(placeholder);
        if self.admit(wanted).is_err() {
            self.collect(roots);
            let wanted = self.load_units(&record).saturating_sub(placeholder);
            self.admit(wanted).map_err(LoadError::Memory)?;
        }

        let fresh: Vec<_> = open::referents(&record)
            .unwrap_or_default()
            .into_iter()
            .filter(|(id, _)| self.arena.entity_handle(open::entity(*id)).is_none())
            .collect();
        let body = open::body(&mut self.arena, &record).map_err(malformed)?;
        for (id, kind) in fresh {
            if let Some(handle) = self.arena.entity_handle(open::entity(id)) {
                self.place(handle, super::meter::body(&kind.placeholder()));
            }
        }
        self.arena
            .materialized(object, body)
            .map_err(|error| malformed(error.to_string()))?;
        // The body was admitted as the record's schedule less the
        // placeholder; it holds what its body does.
        let now = self.arena.body(object).map_or(0, super::meter::body);
        self.held = self
            .held
            .saturating_sub(super::meter::record(&record))
            .saturating_add(now);
        self.place(object, now);
        Ok(())
    }
}
