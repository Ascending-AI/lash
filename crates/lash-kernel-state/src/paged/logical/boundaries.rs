//! Terminal source boundaries publish directly through the ledger owner.
//! Callers settle their pending precise-source work before entering these
//! transactions. There is no runtime mirror or boundary adapter underneath.
use super::*;
use crate::arena::Arena;
use crate::paged::heap_impl::PagedHeap;
use lash_kernel_doc::Value;

impl Ledger {
    /// New cells consume only the execution owners of the previous cell.
    /// Session bindings and saved source environments already live in the
    /// heap-owned ledger and remain cold, with their complete prices.
    pub fn begin_cell(&mut self, arena: &mut Arena, heap: &PagedHeap) -> Result<(), HeapFault> {
        let mut inputs = Vec::new();
        // Canonical key ranges keep a new activation from walking cold
        // session bindings or saved environments just to discard run owners.
        for prefix in [0_u8, 2] {
            let mut cursor = vec![prefix];
            while let Some((key, root)) = self.state.execution.after(Some(&cursor), heap.pages())? {
                if key.first() != Some(&prefix) {
                    break;
                }
                inputs.push(root.owner);
                cursor = key;
            }
        }
        visit_tree(&self.state.ended, heap.pages(), &mut |root| {
            inputs.push(root.owner);
            Ok(())
        })?;
        let prepared = self.prepare_source_ownership(arena, heap, &inputs, &[])?;
        let accepted = prepared
            .admit(u64::MAX)
            .map_err(|_| invalid("cell root cost overflow"))?;
        self.commit(accepted)
    }
    /// A carry is a terminal source frontier, so selecting its complete root
    /// set may visit the session. It never warms guest payloads or arena bodies.
    pub fn carry(
        &mut self,
        heap: &PagedHeap,
        bindings: KeyedRoot,
        functions: KeyedRoot,
    ) -> Result<(), HeapFault> {
        use crate::paged::{BindingRecord, BoundRecord, FunctionRecord};
        use lash_kernel_doc::heap::PageValue;
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
        let mut inputs = Vec::new();
        visit_tree(&self.state.owners, access, &mut |root| {
            inputs.push(root.owner);
            Ok(())
        })?;
        visit_tree(&self.state.execution, access, &mut |root| {
            inputs.push(root.owner);
            Ok(())
        })?;
        visit_tree(&self.state.ended, access, &mut |root| {
            inputs.push(root.owner);
            Ok(())
        })?;
        let mut outputs: Vec<_> = roots
            .iter()
            .map(|(owner, value)| (*owner, RootValue::Page(value)))
            .collect();
        let result = Owner::Incoming(TaskId::MAIN);
        if self.holding(result, access)?.is_some() {
            outputs.push((result, RootValue::Existing(result)));
        }
        let prepared = self
            .prepare_saved_ownership(heap, &inputs, &outputs, &environments)?
            .normalize(access, crate::paged::census_impl::TraceLimits::default())?;
        let accepted = prepared
            .admit(u64::MAX)
            .map_err(|_| invalid("carry root cost overflow"))?;
        self.publish(accepted, || ())
    }
    /// Transfer explicit live source owners in one canonical graph publication.
    /// Every consumed owner must exist; outputs may borrow an existing holding
    /// without projecting its value again. No replacements are deferred.
    pub fn transfer_source(
        &mut self,
        arena: &mut Arena,
        heap: &PagedHeap,
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
    ) -> Result<(), HeapFault> {
        self.rekey(&arena.take_rekeyed(), heap.pages())?;
        let events = arena.take_member_events();
        let result = (|| {
            if inputs.is_empty() && outputs.is_empty() && events.is_empty() {
                return Ok(());
            }
            let mut registration = arena
                .registration()
                .map_err(|error| invalid(&error.to_string()))?;
            let prepared =
                self.prepare_source_event(registration.arena(), heap, inputs, outputs, &events)?;
            let accepted = prepared
                .admit(u64::MAX)
                .map_err(|_| invalid("source transfer holding overflow"))?;
            self.publish(accepted, || registration.commit())
        })();
        arena.return_member_events(events, result.is_ok());
        result
    }
    pub fn enter_source(
        &mut self,
        task: TaskId,
        arena: &mut Arena,
        heap: &PagedHeap,
        inputs: &[Owner],
        bindings: &[(Slot, PublicationValue)],
    ) -> Result<Invocation, HeapFault> {
        let mut registration = arena
            .registration()
            .map_err(|error| invalid(&error.to_string()))?;
        let bindings: Vec<_> = bindings
            .iter()
            .map(|(slot, value)| (*slot, value.root()))
            .collect();
        let (invocation, prepared) =
            self.prepare_source_enter(task, registration.arena(), heap, inputs, &bindings)?;
        let accepted = prepared
            .admit(u64::MAX)
            .map_err(|_| invalid("source holding overflow"))?;
        self.publish(accepted, || registration.commit())?;
        Ok(invocation)
    }
    pub fn leave_source(
        &mut self,
        invocation: &Invocation,
        arena: &mut Arena,
        heap: &PagedHeap,
        result: (Owner, Value),
    ) -> Result<(), HeapFault> {
        let inputs = self.invocation_owners(invocation, heap.pages())?;
        let mut registration = arena
            .registration()
            .map_err(|error| invalid(&error.to_string()))?;
        let mut consumed = inputs;
        if self.holding(result.0, heap.pages())?.is_some() {
            consumed.push(result.0);
        }
        let prepared = self.prepare_source_ownership(
            registration.arena(),
            heap,
            &consumed,
            &[(result.0, RootValue::Value(&result.1))],
        )?;
        let accepted = prepared
            .admit(u64::MAX)
            .map_err(|_| invalid("source holding overflow"))?;
        self.publish(accepted, || registration.commit())
    }
}
