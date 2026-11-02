//! Runtime adapters record semantic values and source owners. The ledger owns
//! projection, membership, transfer and publication at the source frontier.
use super::*;
use crate::arena::Arena;
use crate::paged::heap_impl::PagedHeap;
use lash_kernel_doc::{Value, arena::ArenaHandle};
use std::cell::RefCell;

#[derive(Clone)]
pub enum SourceValue {
    Value(Value),
    Page(lash_kernel_doc::heap::PageValue),
    Frozen(crate::paged::FrozenRef),
    Capture(ArenaHandle),
    /// A break or continue departs with no value.
    Departure,
}
impl SourceValue {
    pub(super) fn root(&self) -> RootValue<'_> {
        match self {
            Self::Value(value) => RootValue::Value(value),
            Self::Page(value) => RootValue::Page(value),
            Self::Frozen(value) => RootValue::Frozen(value),
            Self::Capture(handle) => RootValue::Capture(*handle),
            Self::Departure => RootValue::Departure,
        }
    }
}

/// Root writes a state has made since its ledger last committed one.
#[derive(Clone, Default)]
pub(super) enum Written {
    #[default]
    None,
    Owners(Vec<Owner>),
    /// Holdings were replaced wholesale, as by normalization.
    All,
}
impl Written {
    pub(super) fn owners(&mut self, owners: impl Iterator<Item = Owner>) {
        match self {
            Self::None => *self = Self::Owners(owners.collect()),
            Self::Owners(written) => written.extend(owners),
            Self::All => {}
        }
    }
}

/// The value a runtime event last published to an owner, when republishing
/// it would project the same holding: a value or a borrowed cell is projected
/// from its identity and immutable facts alone, under unchanged ledger keys.
#[derive(Clone, Copy)]
enum Published {
    Absent,
    Value(Value),
    Capture(ArenaHandle),
    Departure,
}
impl Published {
    fn of(next: &Option<SourceValue>) -> Option<Self> {
        Some(match next {
            None => Self::Absent,
            Some(SourceValue::Value(value)) => Self::Value(*value),
            Some(SourceValue::Capture(handle)) => Self::Capture(*handle),
            Some(SourceValue::Departure) => Self::Departure,
            Some(SourceValue::Page(_) | SourceValue::Frozen(_)) => return None,
        })
    }
    fn same(&self, next: &Option<SourceValue>) -> bool {
        match (self, next) {
            (Self::Absent, None) | (Self::Departure, Some(SourceValue::Departure)) => true,
            (Self::Value(value), Some(SourceValue::Value(next))) => value.same(next),
            (Self::Capture(handle), Some(SourceValue::Capture(next))) => handle == next,
            _ => false,
        }
    }
}

/// Each owner's last runtime publication, valid until a root write to that
/// owner or a wholesale replacement expires it. The ledger owns it, so no
/// edit path can change a holding without expiring the fact.
#[derive(Default)]
pub(super) struct Publications(
    std::collections::HashMap<Owner, Published, foldhash::fast::RandomState>,
);
impl Publications {
    pub(super) fn expire(&mut self, written: Written) {
        match written {
            Written::None => {}
            Written::Owners(owners) => {
                for owner in owners {
                    self.0.remove(&owner);
                }
            }
            Written::All => self.0.clear(),
        }
    }
    /// Whether publishing `next` to `owner` would leave its holding as is.
    fn holds(&self, owner: &Owner, next: &Option<SourceValue>) -> bool {
        self.0
            .get(owner)
            .is_some_and(|published| published.same(next))
    }
    fn record(&mut self, published: &[Replacement]) {
        for (owner, next) in published {
            if let Some(next) = Published::of(next) {
                self.0.insert(*owner, next);
            }
        }
    }
}

#[derive(Clone)]
pub struct Lifecycle {
    ledger: Rc<RefCell<Ledger>>,
    /// The open region's journal: each owner's last replacement recorded
    /// since the ledger last published, in owner order, committed together
    /// as one runtime event.
    pending: Rc<RefCell<Journal>>,
    /// Bytes the open region's admitted acquisitions may add above the
    /// committed balance: an upper bound on every interior peak.
    growth: Rc<std::cell::Cell<u64>>,
}
/// A region publishes once it has recorded this many replacements. Its
/// journal keeps one record per owner, so lookups stay logarithmic; the cap
/// bounds the pure work between publications, which a crash discards and
/// resume re-executes from the last checkpoint either way.
const REGION_RECORDS: usize = 4096;
/// One recorded root replacement; `None` releases the owner.
pub(super) type Replacement = (Owner, Option<SourceValue>);
#[derive(Default)]
struct Journal {
    /// Each owner's last replacement: the one the runtime event publishes.
    last: std::collections::BTreeMap<Owner, Option<SourceValue>>,
    /// Replacements recorded since the last publication, superseded or not.
    records: usize,
}
impl Journal {
    fn record(&mut self, owner: Owner, next: Option<SourceValue>) {
        self.last.insert(owner, next);
        self.records += 1;
    }
}
impl Lifecycle {
    pub fn new(ledger: Rc<RefCell<Ledger>>) -> Self {
        Self {
            ledger,
            pending: Rc::default(),
            growth: Rc::default(),
        }
    }
    /// Whether `owner` holds a value once the open region publishes: its
    /// last record decides, else the committed ledger.
    pub fn holds(&self, owner: Owner, access: &Access<'_>) -> Result<bool, HeapFault> {
        if let Some(next) = self.pending.borrow().last.get(&owner) {
            return Ok(next.is_some());
        }
        Ok(self.ledger.borrow().holding(owner, access)?.is_some())
    }
    /// The plain value the open region last recorded for `owner`.
    pub fn recorded(&self, owner: Owner) -> Option<Value> {
        match self.pending.borrow().last.get(&owner) {
            Some(Some(SourceValue::Value(value))) => Some(*value),
            _ => None,
        }
    }
    /// Admit `bytes` of acquisition into the open region under `bound`: the
    /// committed balance plus every byte the region already admitted bounds
    /// each interior peak, so no event inside could have been refused. A
    /// region past its record cap, or one whose upper bound no longer fits,
    /// first publishes; the check then runs on the exact committed balance.
    pub fn admit(
        &self,
        arena: &mut Arena,
        heap: &PagedHeap,
        bytes: u64,
        bound: u64,
    ) -> Result<bool, HeapFault> {
        self.bounded(arena, heap)?;
        let fits = |growth: u64| -> Result<bool, HeapFault> {
            let held = self.ledger.borrow().balance()?.total()?;
            Ok(held
                .checked_add(growth)
                .and_then(|held| held.checked_add(bytes))
                .is_some_and(|peak| peak <= bound))
        };
        if !fits(self.growth.get())? {
            if self.growth.get() == 0 {
                return Ok(false);
            }
            self.flush(arena, heap)?;
            if !fits(0)? {
                return Ok(false);
            }
        }
        self.growth.set(self.growth.get().saturating_add(bytes));
        Ok(true)
    }
    pub fn ledger(&self) -> Rc<RefCell<Ledger>> {
        Rc::clone(&self.ledger)
    }
    pub fn replace(&self, owner: Owner, next: Option<SourceValue>) {
        self.pending.borrow_mut().record(owner, next);
    }
    /// Re-projects a binding or control slot into the open region: `next`
    /// supersedes the owner's earlier record, and an owner without one that
    /// still holds what the ledger last published for it records nothing.
    pub fn rebind(&self, owner: Owner, next: Option<SourceValue>) {
        let mut pending = self.pending.borrow_mut();
        if let Some(recorded) = pending.last.get_mut(&owner) {
            *recorded = next;
            return;
        }
        if self
            .ledger
            .try_borrow()
            .is_ok_and(|ledger| ledger.published.holds(&owner, &next))
        {
            return;
        }
        pending.record(owner, next);
    }
    /// Publishes the open region once its journal reaches the record cap.
    pub fn bounded(&self, arena: &mut Arena, heap: &PagedHeap) -> Result<(), HeapFault> {
        if self.pending.borrow().records >= REGION_RECORDS {
            self.flush(arena, heap)?;
        }
        Ok(())
    }
    /// Retire only this activation's actual operands, including unpublished
    /// replacements. The caller can reacquire live roles in the same journal.
    pub fn clear_operands(
        &self,
        invocation: &Invocation,
        access: &Access<'_>,
    ) -> Result<(), HeapFault> {
        let owners = self
            .ledger
            .borrow()
            .invocation_operands(invocation, access)?;
        let mut pending = self.pending.borrow_mut();
        let first = invocation.owner(Slot::Operand(0));
        let last = invocation.owner(Slot::Operand(u32::MAX));
        for (_, value) in pending.last.range_mut(first..=last) {
            *value = None;
        }
        for owner in owners {
            pending.record(owner, None);
        }
        Ok(())
    }
    /// Rekeys the ledger to the durable ids the projections since the last
    /// call gave objects it priced under provisional keys.
    pub fn rekey(&self, arena: &mut Arena, heap: &PagedHeap) -> Result<(), HeapFault> {
        let rekeyed = arena.take_rekeyed();
        self.ledger.borrow_mut().rekey(&rekeyed, heap.pages())
    }
    pub fn flush(&self, arena: &mut Arena, heap: &PagedHeap) -> Result<(), HeapFault> {
        self.rekey(arena, heap)?;
        let mut journal = self.pending.borrow_mut();
        let events = arena.take_member_events();
        if journal.last.is_empty() && events.is_empty() {
            journal.records = 0;
            arena.return_member_events(events, true);
            self.growth.set(0);
            return Ok(());
        }
        let result = (|| {
            let mut ledger = self.ledger.borrow_mut();
            // An owner still holding what it last published needs no event.
            let pending: Vec<Replacement> = journal
                .last
                .iter()
                .filter(|(owner, next)| !ledger.published.holds(owner, next))
                .map(|(owner, next)| (*owner, next.clone()))
                .collect();
            if pending.is_empty() && events.is_empty() {
                journal.last.clear();
                journal.records = 0;
                return Ok(());
            }
            let mut registration = arena
                .registration()
                .map_err(|error| invalid(&error.to_string()))?;
            let prepared =
                ledger.prepare_runtime_event(registration.arena(), heap, &pending, &events)?;
            let accepted = prepared
                .admit(u64::MAX)
                .map_err(|_| invalid("source holding overflow"))?;
            ledger.publish(accepted, || registration.commit())?;
            ledger.published.record(&pending);
            journal.last.clear();
            journal.records = 0;
            Ok(())
        })();
        arena.return_member_events(events, result.is_ok());
        if result.is_ok() {
            self.growth.set(0);
        }
        result
    }
    /// New cells consume only the execution owners of the previous cell.
    /// Session bindings and saved source environments already live in the
    /// heap-owned ledger and remain cold, with their complete prices.
    pub fn begin_cell(&self, arena: &mut Arena, heap: &PagedHeap) -> Result<(), HeapFault> {
        self.flush(arena, heap)?;
        let mut ledger = self.ledger.borrow_mut();
        let mut inputs = Vec::new();
        // Canonical key ranges keep a new activation from walking cold
        // session bindings or saved environments just to discard run owners.
        for prefix in [0_u8, 2] {
            let mut cursor = vec![prefix];
            while let Some((key, root)) =
                ledger.state.execution.after(Some(&cursor), heap.pages())?
            {
                if key.first() != Some(&prefix) {
                    break;
                }
                inputs.push(root.owner);
                cursor = key;
            }
        }
        visit_tree(&ledger.state.ended, heap.pages(), &mut |root| {
            inputs.push(root.owner);
            Ok(())
        })?;
        let prepared = ledger.prepare_source_ownership(arena, heap, &inputs, &[])?;
        let accepted = prepared
            .admit(u64::MAX)
            .map_err(|_| invalid("cell root cost overflow"))?;
        ledger.commit(accepted)
    }
    /// A carry is a terminal source frontier, so selecting its complete root
    /// set may visit the session. It never warms guest payloads or arena bodies.
    pub fn carry(
        &self,
        arena: &mut Arena,
        heap: &PagedHeap,
        bindings: KeyedRoot,
        functions: KeyedRoot,
    ) -> Result<(), HeapFault> {
        use crate::paged::{BindingRecord, BoundRecord, FunctionRecord};
        use lash_kernel_doc::heap::PageValue;
        self.flush(arena, heap)?;
        let access = heap.pages();
        let mut roots = Vec::new();
        visit_tree(
            &Tree::<BindingRecord>::of_keyed(&bindings),
            access,
            &mut |record| {
                let value = match &record.value {
                    BoundRecord::Value(value) => value.clone(),
                    BoundRecord::Shared(entity) => PageValue::Ref(Identity::Object(*entity)),
                    BoundRecord::Captured(_) => {
                        return Err(invalid("session binding borrows a frame capture"));
                    }
                };
                roots.push((Owner::session(&record.name.resolve(access)?), value));
                Ok(())
            },
        )?;
        // Snapshot pages and retained code mirror the saved source environment.
        // The environment owns its occurrences and identity graph exactly once.
        let mut environments = Vec::new();
        visit_tree(
            &Tree::<FunctionRecord>::of_keyed(&functions),
            access,
            &mut |record| {
                environments.push(record.environment);
                roots.push((
                    Owner::saved(&record.name.resolve(access)?),
                    PageValue::Closure(record.environment),
                ));
                Ok(())
            },
        )?;
        let mut ledger = self.ledger.borrow_mut();
        let mut inputs = Vec::new();
        visit_tree(&ledger.state.owners, access, &mut |root| {
            inputs.push(root.owner);
            Ok(())
        })?;
        visit_tree(&ledger.state.execution, access, &mut |root| {
            inputs.push(root.owner);
            Ok(())
        })?;
        visit_tree(&ledger.state.ended, access, &mut |root| {
            inputs.push(root.owner);
            Ok(())
        })?;
        let mut outputs: Vec<_> = roots
            .iter()
            .map(|(owner, value)| (*owner, RootValue::Page(value)))
            .collect();
        let result = Owner::Incoming(TaskId::MAIN);
        if ledger.holding(result, access)?.is_some() {
            outputs.push((result, RootValue::Existing(result)));
        }
        let prepared = ledger
            .prepare_saved_ownership(heap, &inputs, &outputs, &environments)?
            .normalize(access, crate::paged::census_impl::TraceLimits::default())?;
        let accepted = prepared
            .admit(u64::MAX)
            .map_err(|_| invalid("carry root cost overflow"))?;
        ledger.publish(accepted, || ())
    }
    pub fn handoff(&self, arena: &mut Arena, heap: &PagedHeap) -> Result<(), HeapFault> {
        self.flush(arena, heap)?;
        self.ledger.borrow_mut().handoff()
    }
    pub fn enter(
        &self,
        task: TaskId,
        arena: &mut Arena,
        heap: &PagedHeap,
        inputs: &[Owner],
        bindings: &[(Slot, SourceValue)],
    ) -> Result<Invocation, HeapFault> {
        self.flush(arena, heap)?;
        let mut registration = arena
            .registration()
            .map_err(|error| invalid(&error.to_string()))?;
        let bindings: Vec<_> = bindings
            .iter()
            .map(|(slot, value)| (*slot, value.root()))
            .collect();
        let mut ledger = self.ledger.borrow_mut();
        let (invocation, prepared) =
            ledger.prepare_source_enter(task, registration.arena(), heap, inputs, &bindings)?;
        let accepted = prepared
            .admit(u64::MAX)
            .map_err(|_| invalid("source holding overflow"))?;
        ledger.publish(accepted, || registration.commit())?;
        Ok(invocation)
    }
    pub fn leave(
        &self,
        invocation: &Invocation,
        arena: &mut Arena,
        heap: &PagedHeap,
        result: (Owner, Value),
    ) -> Result<(), HeapFault> {
        self.flush(arena, heap)?;
        let mut ledger = self.ledger.borrow_mut();
        let inputs = ledger.invocation_owners(invocation, heap.pages())?;
        let mut registration = arena
            .registration()
            .map_err(|error| invalid(&error.to_string()))?;
        let mut consumed = inputs;
        if ledger.holding(result.0, heap.pages())?.is_some() {
            consumed.push(result.0);
        }
        let prepared = ledger.prepare_source_ownership(
            registration.arena(),
            heap,
            &consumed,
            &[(result.0, RootValue::Value(&result.1))],
        )?;
        let accepted = prepared
            .admit(u64::MAX)
            .map_err(|_| invalid("source holding overflow"))?;
        ledger.publish(accepted, || registration.commit())
    }
}
