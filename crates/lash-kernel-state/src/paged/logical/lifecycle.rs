//! Runtime adapters record semantic values and source owners. The ledger owns
//! projection, membership, transfer and publication at the source frontier.
use super::*;
use crate::arena::Arena;
use crate::paged::heap_impl::PagedHeap;
use std::cell::RefCell;

#[derive(Clone)]
pub struct Lifecycle {
    ledger: Rc<RefCell<Ledger>>,
    /// The open region's journal: each owner's last replacement recorded
    /// since the ledger last published, in owner order, committed together
    /// as one runtime event.
    pending: Rc<RefCell<Journal>>,
}
/// One recorded root replacement; `None` releases the owner.
pub(super) type Replacement = (Owner, Option<PublicationValue>);
#[derive(Default)]
struct Journal {
    /// Each owner's last replacement: the one the runtime event publishes.
    last: std::collections::BTreeMap<Owner, Option<PublicationValue>>,
}
impl Journal {
    fn record(&mut self, owner: Owner, next: Option<PublicationValue>) {
        self.last.insert(owner, next);
    }
}
impl Lifecycle {
    pub fn new(ledger: Rc<RefCell<Ledger>>) -> Self {
        Self {
            ledger,
            pending: Rc::default(),
        }
    }
    pub fn ledger(&self) -> Rc<RefCell<Ledger>> {
        Rc::clone(&self.ledger)
    }
    pub fn replace(&self, owner: Owner, next: Option<PublicationValue>) {
        self.pending.borrow_mut().record(owner, next);
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
            arena.return_member_events(events, true);
            return Ok(());
        }
        let result = (|| {
            let mut ledger = self.ledger.borrow_mut();
            let pending: Vec<Replacement> = journal
                .last
                .iter()
                .map(|(owner, next)| (*owner, next.clone()))
                .collect();
            let mut registration = arena
                .registration()
                .map_err(|error| invalid(&error.to_string()))?;
            let prepared =
                ledger.prepare_runtime_event(registration.arena(), heap, &pending, &events)?;
            let accepted = prepared
                .admit(u64::MAX)
                .map_err(|_| invalid("source holding overflow"))?;
            ledger.publish(accepted, || registration.commit())?;
            journal.last.clear();
            Ok(())
        })();
        arena.return_member_events(events, result.is_ok());
        result
    }
}
