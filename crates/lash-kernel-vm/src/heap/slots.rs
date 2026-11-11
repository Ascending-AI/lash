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

use lash_kernel_state::arena::{LOOKUP, ProxyKind};
use lash_kernel_state::paged::run::chunked::{Chunked, Members, Placement, placement};
use lash_kernel_state::paged::run::ids::ObjectId;
use lash_kernel_state::paged::run::objects::{ObjectKind, Paged, PagedNode, Record};

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
    /// The large lists and tables the heap opened or wrote by chunk.
    large: super::large::Large,
    /// The edge each cold chunk of a paged text or bytes stands for, by
    /// object and in chunk order (none for a resident chunk): the counts
    /// and identity its parent's read certified, which its own read is
    /// checked against.
    payloads: std::collections::HashMap<ObjectId, Vec<Option<Paged>>>,
    /// The edge summary of each cold object the collector traced past,
    /// from its row in the current directory, until the next seal.
    summaries: std::collections::HashMap<ObjectId, Vec<lash_kernel_doc::heap::EntityId>>,
    /// Each slot's schedule units, by slot index, while its object lives.
    units: Vec<u64>,
    /// The slots of the program's placed constants: code, which the run
    /// never holds, resident or cold (contract §2).
    code_slots: std::collections::HashSet<u32>,
    held: u64,
    bound: u64,
    /// Every function a closure or function value the heap sealed names,
    /// since the run started: code a stored value may still call
    /// (contract §7).
    pub(crate) code: std::collections::BTreeSet<lash_kernel_value::FunctionId>,
}

impl SlotHeap {
    pub(crate) fn new(bound: u64) -> Self {
        Self {
            arena: Arena::new(),
            durable: None,
            large: super::large::Large::default(),
            payloads: std::collections::HashMap::new(),
            summaries: std::collections::HashMap::new(),
            units: Vec::new(),
            code_slots: std::collections::HashSet::new(),
            held: 0,
            bound,
            code: std::collections::BTreeSet::new(),
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
            let value = open::value(&mut heap.arena, value, &root.heads).map_err(|reason| {
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

    /// The directory the heap writes its objects to, if it has one.
    pub(crate) fn directory(&self) -> Option<&lash_kernel_state::paged::run::directory::Directory> {
        self.durable.as_ref().map(|durable| &durable.directory)
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
        let (root, moved) = super::seal::seal(
            &mut self.arena,
            roots,
            &mut durable.directory,
            &mut durable.high_water,
            &mut self.large,
            &mut self.code,
        )?;
        self.summaries.clear();
        // A large object whose chunks moved goes back to its row: the
        // arena's chunks are always the row's.
        for handle in moved {
            if self
                .arena
                .drop_large(handle)
                .map_err(|error| crate::paged::SealError::Fault {
                    problem: error.to_string(),
                })?
            {
                self.resettle(handle);
            }
        }
        Ok(root)
    }

    /// Slot `handle`'s body changed in place: it holds what its body does.
    fn resettle(&mut self, handle: ArenaHandle) {
        let index = handle.index() as usize;
        let before = self.units.get(index).copied().unwrap_or(0);
        let now = self.holds(handle);
        self.held = self.held.saturating_sub(before).saturating_add(now);
        self.place(handle, now);
    }

    /// What slot `handle` holds under the schedule: its body's units, or
    /// nothing for a placed constant.
    fn holds(&self, handle: ArenaHandle) -> u64 {
        if self.code_slots.contains(&handle.index()) {
            return 0;
        }
        self.arena.body(handle).map_or(0, super::meter::body)
    }

    /// Seals what `roots` reach, then puts every clean list, map, set and
    /// record, and every immutable with a row, back on its pages as a cold
    /// proxy, so the next read of it loads its row: the force-cold laws'
    /// statement boundary. Closures, cells, functions and host handles
    /// stay resident.
    #[cfg(test)]
    pub(crate) fn page_out(
        &mut self,
        roots: &[Value],
    ) -> Result<lash_kernel_state::paged::run::root::HeapRoot, crate::paged::SealError> {
        use lash_kernel_doc::arena::Body;
        let root = self.seal(roots)?;
        for index in 0..self.units.len() {
            let Some(handle) = self.arena.objects().handle_at(index as u32) else {
                continue;
            };
            let paged = matches!(
                self.arena.body(handle),
                Ok(Body::List(_)
                    | Body::Map(_)
                    | Body::Set(_)
                    | Body::Record(_)
                    | Body::Tuple(_)
                    | Body::Error(_)
                    | Body::BigInt(_)
                    | Body::Timestamp(_)
                    | Body::LongText(_)
                    | Body::Bytes(_))
            );
            let fault = |error: NativeError| crate::paged::SealError::Fault {
                problem: error.to_string(),
            };
            let evicted = paged
                && (self.arena.drop_large(handle).map_err(fault)?
                    || self.arena.evict(handle).map_err(fault)?);
            if evicted {
                self.resettle(handle);
            }
        }
        Ok(root)
    }

    /// The page port the heap seals through, when it has a directory.
    pub(crate) fn access(
        &self,
    ) -> Option<&lash_kernel_state::kernel_internals::tree::Access<'static>> {
        self.durable
            .as_ref()
            .map(|durable| durable.directory.access())
    }

    /// Bounds the heap by `bound` from now on.
    #[cfg(test)]
    pub(crate) fn rebound(&mut self, bound: u64) {
        self.bound = bound;
    }

    /// Forgets every page the heap's directory holds decoded, so the next
    /// read of each comes from the store: a cold access.
    #[cfg(test)]
    pub(crate) fn forget_pages(&self) {
        if let Some(durable) = &self.durable {
            durable.directory.access().clear();
        }
    }

    /// What loading `row` adds: its body and an opened referent for each
    /// the arena does not hold.
    fn load_units(&self, row: &lash_kernel_state::paged::run::directory::Row) -> u64 {
        let mut units = super::meter::record(&row.record);
        for (id, opening) in open::referents(&row.record, &row.heads).unwrap_or_default() {
            if self.arena.entity_handle(open::entity(id)).is_none() {
                units = units.saturating_add(super::meter::body(&opening.placeholder()));
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

impl SlotHeap {
    /// Opens the cold proxy `object` (the large object `id`) from its row
    /// alone: every chunk cold, nothing of its members read.
    fn open_large(
        &mut self,
        object: ArenaHandle,
        id: ObjectId,
        chunked: &Chunked,
        roots: &mut dyn FnMut(&mut Visit<'_>),
    ) -> Result<(), LoadError> {
        use lash_kernel_doc::heap::HeapFault;
        let malformed = |reason: String| LoadError::Store(HeapFault::Malformed(reason));
        let placeholder = self
            .units
            .get(object.index() as usize)
            .copied()
            .unwrap_or(0);
        let wanted = super::meter::record(&Record::Chunked(Box::new(chunked.clone())))
            .saturating_sub(placeholder);
        if self.admit(wanted).is_err() {
            self.collect(roots);
            self.admit(wanted).map_err(LoadError::Memory)?;
        }
        let chunks = usize::try_from(chunked.chunks())
            .map_err(|_| malformed("a large object's chunk count is past memory".into()))?;
        let opened = match chunked.kind {
            ObjectKind::List => self.arena.open_list(object, chunked.len),
            kind => {
                let proxy = match kind {
                    ObjectKind::Map => ProxyKind::Map,
                    ObjectKind::Set => ProxyKind::Set,
                    _ => ProxyKind::Record,
                };
                self.arena
                    .open_keyed_table(object, proxy, chunked.len, chunked.next, chunks)
            }
        };
        opened.map_err(|error| malformed(error.to_string()))?;
        if self
            .arena
            .chunk_count(object)
            .map_err(|error| malformed(error.to_string()))?
            != chunks
        {
            return Err(malformed(format!(
                "large list {} has {chunks} chunks for {} values",
                id.0, chunked.len
            )));
        }
        self.large.opened(id, chunked.clone());
        self.held = self.held.saturating_sub(wanted.saturating_add(placeholder));
        let now = self.holds(object);
        self.held = self.held.saturating_add(now);
        self.place(object, now);
        Ok(())
    }

    /// Loads what the resident `object` waits for: chunk `chunk`, the
    /// chunk the key of its last lookup is placed in ([`LOOKUP`]), or, once
    /// every chunk is resident, the join of its chunks.
    fn load_chunk(
        &mut self,
        object: ArenaHandle,
        chunk: u32,
        roots: &mut dyn FnMut(&mut Visit<'_>),
    ) -> Result<(), LoadError> {
        use lash_kernel_doc::heap::HeapFault;
        let malformed = |reason: String| LoadError::Store(HeapFault::Malformed(reason));
        let id = open::id_of(&self.arena, object).ok();
        if let Some(id) = id.filter(|id| self.payloads.contains_key(id)) {
            return self.read_payload(object, id, chunk, roots);
        }
        let Some(id) = id.filter(|id| self.large.row(*id).is_some()) else {
            return Ok(());
        };
        let cold = |heap: &Self, chunk: u32| -> Result<bool, LoadError> {
            Ok(heap
                .arena
                .cold_chunks(object, chunk, 1)
                .map_err(|error| malformed(error.to_string()))?
                .first()
                .is_some_and(|(at, _)| *at == chunk))
        };
        if chunk == LOOKUP {
            let (key, normal) = self
                .arena
                .take_lookup(object)
                .map_err(|error| malformed(error.to_string()))?
                .ok_or_else(|| malformed("a lookup with no key".into()))?;
            let at = placement(&normal);
            let durable = self
                .durable
                .as_ref()
                .ok_or_else(|| malformed("a large object in a heap with no directory".into()))?;
            let target = self
                .large
                .chunk_of_key(durable.directory.access(), id, &at)
                .map_err(LoadError::Store)?;
            let found =
                cold(self, target)? && self.read_chunk(object, id, target, roots)?.contains(&at);
            if !found {
                self.arena
                    .resolved(object, key, None)
                    .map_err(|error| malformed(error.to_string()))?;
            }
            return Ok(());
        }
        if cold(self, chunk)? {
            self.read_chunk(object, id, chunk, roots)?;
            return Ok(());
        }
        let first = self
            .arena
            .cold_chunks(object, 0, 1)
            .map_err(|error| malformed(error.to_string()))?
            .first()
            .map(|(at, _)| *at);
        match first {
            Some(first) => {
                self.read_chunk(object, id, first, roots)?;
            }
            None => {
                if !self
                    .arena
                    .join_chunks(object)
                    .map_err(|error| malformed(error.to_string()))?
                {
                    return Err(malformed(format!(
                        "large object {} asks for resident chunk {chunk}",
                        id.0
                    )));
                }
                self.resettle(object);
            }
        }
        Ok(())
    }

    /// Reads cold chunk `chunk` of the paged text or bytes `object` (the
    /// object `id`): the one node its edge names, checked against the
    /// counts and identity that edge commits to. A leaf becomes resident;
    /// an index node becomes one cold chunk per child, each standing for
    /// the edge the read certified.
    fn read_payload(
        &mut self,
        object: ArenaHandle,
        id: ObjectId,
        chunk: u32,
        roots: &mut dyn FnMut(&mut Visit<'_>),
    ) -> Result<(), LoadError> {
        use lash_kernel_doc::heap::HeapFault;
        use lash_kernel_state::arena::PayloadPart;
        let malformed = |reason: String| LoadError::Store(HeapFault::Malformed(reason));
        let edge = self
            .payloads
            .get(&id)
            .and_then(|edges| edges.get(chunk as usize).copied().flatten())
            .ok_or_else(|| malformed(format!("payload {} has no cold chunk {chunk}", id.0)))?;
        let span = self
            .arena
            .payload_chunk(object, chunk)
            .map_err(|error| malformed(error.to_string()))?;
        // A chunk's length saturates at `u32::MAX`; its node's read then
        // checks its counts.
        if span.is_none_or(|span| {
            let len = span.end - span.start;
            len != edge.summary.utf8_bytes && len != u64::from(u32::MAX)
        }) {
            return Err(malformed(format!(
                "payload {} chunk {chunk} is not the node its edge names",
                id.0
            )));
        }
        let durable = self
            .durable
            .as_ref()
            .ok_or_else(|| malformed("a paged payload in a heap with no directory".into()))?;
        let whole = lash_kernel_state::arena::take_whole_read(object);
        let access = durable.directory.access();
        let node = edge.node(access).map_err(LoadError::Store)?;
        // A whole read consumes every node: an index's children are
        // fetched as one batch of siblings, then each is checked against
        // its edge as it is read.
        if let PagedNode::Index(children) = &node
            && whole
        {
            let pages: Vec<_> = children.iter().map(|child| child.root.page).collect();
            access.prefetch(&pages);
        }
        let (part, edges) = match node {
            PagedNode::Leaf(bytes) => {
                let wanted = bytes.len() as u64;
                if self.admit(wanted).is_err() {
                    self.collect(roots);
                    self.admit(wanted).map_err(LoadError::Memory)?;
                }
                self.held = self.held.saturating_sub(wanted);
                (PayloadPart::Leaf(bytes), vec![None])
            }
            PagedNode::Index(children) => (
                PayloadPart::Index(
                    children
                        .iter()
                        .map(|child| (child.root.page, child.summary.utf8_bytes))
                        .collect(),
                ),
                children.into_iter().map(Some).collect(),
            ),
        };
        self.arena
            .decoded_payload(object, chunk, part)
            .map_err(|error| malformed(error.to_string()))?;
        if let Some(held) = self.payloads.get_mut(&id) {
            held.splice(chunk as usize..=chunk as usize, edges);
            if held.iter().all(Option::is_none) {
                self.payloads.remove(&id);
                // Every node is resident: join them, as a large object's
                // chunks are, so a whole read borrows one slice. No edge
                // is left to answer a request for the join.
                self.arena
                    .join_chunks(object)
                    .map_err(|error| malformed(error.to_string()))?;
            }
        }
        // It holds what its body does.
        let before = self
            .units
            .get(object.index() as usize)
            .copied()
            .unwrap_or(0);
        let now = self.holds(object);
        self.held = self.held.saturating_sub(before).saturating_add(now);
        self.place(object, now);
        Ok(())
    }

    /// Reads chunk `chunk` of the large object `id` into `object`: its
    /// members decoded, each referent opened. Gives the placements of a
    /// table chunk's keys.
    fn read_chunk(
        &mut self,
        object: ArenaHandle,
        id: ObjectId,
        chunk: u32,
        roots: &mut dyn FnMut(&mut Visit<'_>),
    ) -> Result<Vec<Placement>, LoadError> {
        use lash_kernel_doc::heap::HeapFault;
        let malformed = |reason: String| LoadError::Store(HeapFault::Malformed(reason));
        let durable = self
            .durable
            .as_ref()
            .ok_or_else(|| malformed("a large object in a heap with no directory".into()))?;
        let page = self
            .large
            .read(durable.directory.access(), id, chunk)
            .map_err(LoadError::Store)?;
        let corrupt = |reason: String| {
            LoadError::Store(self.durable.as_ref().map_or_else(
                || HeapFault::Malformed(reason.clone()),
                |durable| durable.corrupt(reason.clone()),
            ))
        };
        let referents =
            open::values_referents(page.members.values(), &page.heads).map_err(corrupt)?;
        let wanted = |heap: &Self| {
            let mut seen = std::collections::HashSet::new();
            let fresh: u64 = referents
                .iter()
                .filter(|(id, _)| {
                    seen.insert(*id) && heap.arena.entity_handle(open::entity(*id)).is_none()
                })
                .map(|(_, opening)| super::meter::body(&opening.placeholder()))
                .sum();
            let per = match &page.members {
                Members::List(values) => values.len() as u64,
                Members::Table(entries) => entries.len() as u64,
            };
            super::meter::values(per)
                .saturating_sub(super::meter::COLD_CHUNK)
                .saturating_add(fresh)
        };
        let units = wanted(self);
        if self.admit(units).is_err() {
            self.collect(roots);
            self.admit(wanted(self)).map_err(LoadError::Memory)?;
        }
        let fresh: Vec<_> = referents
            .iter()
            .filter(|(id, _)| self.arena.entity_handle(open::entity(*id)).is_none())
            .map(|(id, opening)| (*id, super::meter::body(&opening.placeholder())))
            .collect();
        let mut placements = Vec::new();
        let decoded = match &page.members {
            Members::List(members) => {
                let values = members
                    .iter()
                    .map(|member| open::value(&mut self.arena, member, &page.heads))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(malformed)?;
                self.arena.decoded_list(object, chunk, values)
            }
            Members::Table(entries) => {
                let mut decoded = Vec::with_capacity(entries.len());
                for entry in entries {
                    let key =
                        open::value(&mut self.arena, &entry.key, &page.heads).map_err(malformed)?;
                    let value = open::value(&mut self.arena, &entry.value, &page.heads)
                        .map_err(malformed)?;
                    let normal = self
                        .arena
                        .semantics()
                        .normal_key(&key)
                        .map_err(|error| malformed(error.to_string()))?
                        .ok_or_else(|| malformed("a table key has no normal form".into()))?;
                    if placement(&normal) != entry.placement {
                        return Err(malformed(format!(
                            "large table {} holds a key out of its placement",
                            id.0
                        )));
                    }
                    placements.push(entry.placement);
                    decoded.push((entry.seq, key, value, normal));
                }
                self.arena.decoded_table(object, chunk, decoded)
            }
        };
        decoded.map_err(|error| malformed(error.to_string()))?;
        let mut placed = std::collections::HashSet::new();
        for (id, units) in fresh {
            if placed.insert(id)
                && let Some(handle) = self.arena.entity_handle(open::entity(id))
            {
                self.place(handle, units);
            }
        }
        // Admitted as its schedule; it holds what its body does.
        let index = object.index() as usize;
        let before = self.units.get(index).copied().unwrap_or(0);
        let now = self.holds(object);
        let fresh_units = units.saturating_sub(
            super::meter::values(page.members.len() as u64)
                .saturating_sub(super::meter::COLD_CHUNK),
        );
        self.held = self
            .held
            .saturating_sub(units.saturating_sub(fresh_units))
            .saturating_sub(before)
            .saturating_add(now);
        self.place(object, now);
        Ok(placements)
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

    fn grow(&mut self, units: u64) {
        self.bound = self.bound.saturating_add(units);
    }

    fn create(&mut self, object: NewObject) -> Result<Value, NativeError> {
        let admitted = schedule(&object);
        let value = self.arena.create(object)?;
        if let Some(handle) = value.handle() {
            self.settle_placed(handle, admitted);
        }
        Ok(value)
    }

    fn constant(&mut self, object: NewObject) -> Result<Value, NativeError> {
        let value = self.arena.create(object)?;
        if let Some(handle) = value.handle() {
            self.place(handle, 0);
            self.code_slots.insert(handle.index());
        }
        Ok(value)
    }

    fn adopt_constant(&mut self, value: Value) {
        if let Some(handle) = value.handle() {
            let units = self
                .units
                .get(handle.index() as usize)
                .copied()
                .unwrap_or(0);
            self.held = self.held.saturating_sub(units);
            self.place(handle, 0);
            self.code_slots.insert(handle.index());
        }
    }

    fn install(&mut self, built: &mut Vec<(ArenaHandle, NewObject)>) -> Result<(), NativeError> {
        // A native's batch is nearly always a few objects: their schedules
        // wait on the stack while the arena takes the objects.
        let mut few = [(ArenaHandle::new(0, 0), 0); 4];
        let many: Vec<(ArenaHandle, u64)>;
        let placed = if built.len() <= few.len() {
            for (slot, (handle, object)) in few.iter_mut().zip(built.iter()) {
                *slot = (*handle, schedule(object));
            }
            &few[..built.len()]
        } else {
            many = built
                .iter()
                .map(|(handle, object)| (*handle, schedule(object)))
                .collect();
            &many[..]
        };
        self.arena.install(built)?;
        for &(handle, admitted) in placed {
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
        let mut built = built;
        self.install(&mut built)?;
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
        let collected = match &self.durable {
            Some(durable) => {
                let summaries = &mut self.summaries;
                self.arena.collect_reachable(roots, &mut |entity| {
                    let id = ObjectId(entity.ordinal);
                    if let Some(edges) = summaries.get(&id) {
                        return Ok(edges.clone());
                    }
                    let edges: Vec<_> = match durable.directory.get(id)? {
                        Some(row) => durable
                            .directory
                            .edges(&row)?
                            .into_iter()
                            .map(open::entity)
                            .collect(),
                        None => Vec::new(),
                    };
                    summaries.insert(id, edges.clone());
                    Ok(edges)
                })
            }
            None => Err(lash_kernel_doc::heap::HeapFault::Malformed(
                "no directory".into(),
            )),
        };
        if collected.is_err() {
            // No edge summary to trace a cold object by: what it may name
            // stays, as the conservative collection keeps it.
            self.arena.collect_cycles(roots);
        }
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
            return self.load_chunk(object, chunk, roots);
        }
        let (id, row) = {
            let Some(durable) = self.durable.as_ref() else {
                return Err(malformed(
                    "a cold object in a heap with no directory".into(),
                ));
            };
            let corrupt = |reason: String| LoadError::Store(durable.corrupt(reason));
            let id = open::id_of(&self.arena, object).map_err(malformed)?;
            let row = durable
                .directory
                .get(id)
                .map_err(LoadError::Store)?
                .ok_or_else(|| corrupt(format!("object {} has no row", id.0)))?;
            open::referents(&row.record, &row.heads).map_err(corrupt)?;
            (id, row)
        };
        if let Record::Chunked(chunked) = &row.record {
            return self.open_large(object, id, chunked, roots);
        }

        // Admission of the growth: the body replaces the proxy's
        // placeholder. Then one collection from the roots, and once more.
        let placeholder = self
            .units
            .get(object.index() as usize)
            .copied()
            .unwrap_or(0);
        let wanted = self.load_units(&row).saturating_sub(placeholder);
        if self.admit(wanted).is_err() {
            self.collect(roots);
            let wanted = self.load_units(&row).saturating_sub(placeholder);
            self.admit(wanted).map_err(LoadError::Memory)?;
        }

        let fresh: Vec<_> = open::referents(&row.record, &row.heads)
            .unwrap_or_default()
            .into_iter()
            .filter(|(id, _)| self.arena.entity_handle(open::entity(*id)).is_none())
            .map(|(id, opening)| (id, super::meter::body(&opening.placeholder())))
            .collect();
        // An immutable's row must be the object every referrer's head
        // certifies, before the run sees a word of it. A payload's row
        // commits its counts and root id, so it is compared before any
        // page is read, and its read checks every edge against that id.
        let head = self
            .arena
            .facts(object)
            .map_err(|error| malformed(error.to_string()))?
            .head()
            .cloned();
        macro_rules! refuse {
            ($held:expr) => {{
                let reason = format!(
                    "a row disagrees with the head its referrers name it by: {:?}",
                    $held
                );
                LoadError::Store(self.durable.as_ref().map_or_else(
                    || HeapFault::Malformed(reason.clone()),
                    |durable| durable.corrupt(reason.clone()),
                ))
            }};
        }
        if let (Some(head), Record::Paged(paged)) = (&head, &row.record)
            && *head != paged.head()
        {
            return Err(refuse!(paged.head()));
        }
        // A text or bytes past one payload leaf opens cold: one chunk
        // standing for its root, read node by node as reads reach it.
        let body = match &row.record {
            Record::Paged(paged) => {
                self.payloads.insert(id, vec![Some(*paged)]);
                payload_body(paged)
            }
            record => open::body(&mut self.arena, record, &row.heads).map_err(malformed)?,
        };
        for (id, units) in fresh {
            if let Some(handle) = self.arena.entity_handle(open::entity(id)) {
                self.place(handle, units);
            }
        }
        if let Some(head) = head
            && !matches!(row.record, Record::Paged(_))
        {
            let held = self.arena.detached_head(&body);
            if held.as_ref().ok() != Some(&head) {
                return Err(refuse!(held));
            }
        }
        self.arena
            .materialized(object, body)
            .map_err(|error| malformed(error.to_string()))?;
        // The body was admitted as the record's schedule less the
        // placeholder; it holds what its body does.
        let now = self.holds(object);
        self.held = self
            .held
            .saturating_sub(super::meter::record(&row.record))
            .saturating_add(now);
        self.place(object, now);
        Ok(())
    }
}

/// The body a paged text or bytes opens to: its counts, and one cold chunk
/// standing for its root.
fn payload_body(paged: &Paged) -> lash_kernel_doc::arena::Body {
    use lash_kernel_doc::arena::{Body, Bytes, Chunk};
    fn root<T>(paged: &Paged) -> [Chunk<T>; 1] {
        [Chunk::Cold {
            page: paged.root.page,
            len: u32::try_from(paged.summary.utf8_bytes).unwrap_or(u32::MAX),
        }]
    }
    if paged.text {
        Body::LongText(lash_kernel_doc::arena::LongText {
            summary: paged.summary,
            chunks: Box::new(root(paged)),
        })
    } else {
        Body::Bytes(Bytes {
            len: paged.summary.utf8_bytes,
            chunks: Box::new(root(paged)),
        })
    }
}
