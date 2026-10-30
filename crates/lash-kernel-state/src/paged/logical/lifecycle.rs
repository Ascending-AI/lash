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
}
impl SourceValue {
    fn root(&self) -> RootValue<'_> {
        match self {
            Self::Value(value) => RootValue::Value(value),
            Self::Page(value) => RootValue::Page(value),
            Self::Frozen(value) => RootValue::Frozen(value),
            Self::Capture(handle) => RootValue::Capture(*handle),
        }
    }
}

#[derive(Clone)]
pub struct Lifecycle {
    ledger: Rc<RefCell<Ledger>>,
    pending: Rc<RefCell<BTreeMap<Owner, Option<SourceValue>>>>,
}
impl Lifecycle {
    pub fn new(ledger: Rc<RefCell<Ledger>>) -> Self {
        Self {
            ledger,
            pending: Rc::new(RefCell::new(BTreeMap::new())),
        }
    }
    pub fn ledger(&self) -> Rc<RefCell<Ledger>> {
        Rc::clone(&self.ledger)
    }
    pub fn replace(&self, owner: Owner, next: Option<SourceValue>) {
        self.pending.borrow_mut().insert(owner, next);
    }
    pub fn flush(&self, arena: &mut Arena, heap: &PagedHeap) -> Result<(), HeapFault> {
        let mut pending = self.pending.borrow_mut();
        let events = arena.take_member_events();
        if pending.is_empty() && events.is_empty() {
            arena.return_member_events(events, true);
            return Ok(());
        }
        let result = (|| {
            let mut ledger = self.ledger.borrow_mut();
            let mut inputs = Vec::new();
            let mut outputs = Vec::new();
            for (owner, next) in pending.iter() {
                if ledger.holding(*owner, heap.pages())?.is_some() {
                    inputs.push(*owner);
                }
                if let Some(value) = next {
                    outputs.push((*owner, value.root()));
                }
            }
            let mut registration = arena
                .registration()
                .map_err(|error| invalid(&error.to_string()))?;
            let prepared = ledger.prepare_source_event(
                registration.arena(),
                heap,
                &inputs,
                &outputs,
                &events,
            )?;
            let accepted = prepared
                .admit(u64::MAX)
                .map_err(|_| invalid("source holding overflow"))?;
            ledger.publish(accepted, || registration.commit())?;
            pending.clear();
            Ok(())
        })();
        arena.return_member_events(events, result.is_ok());
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
            while let Some((key, root)) = ledger.state.owners.after(Some(&cursor), heap.pages())? {
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
