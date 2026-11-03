//! Runtime adapters record semantic values and source owners. The ledger owns
//! projection, membership, transfer and publication at the source frontier.
use super::*;
use crate::arena::Arena;
use crate::paged::heap_impl::PagedHeap;
use lash_kernel_doc::{Value, arena::ArenaHandle};
use std::cell::RefCell;

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
    fn of(next: &Option<PublicationValue>) -> Option<Self> {
        Some(match next {
            None => Self::Absent,
            Some(PublicationValue::Native(value)) => Self::Value(*value),
            Some(PublicationValue::Capture(handle)) => Self::Capture(*handle),
            Some(PublicationValue::Departure) => Self::Departure,
            Some(
                PublicationValue::Page(_)
                | PublicationValue::Frozen(_)
                | PublicationValue::Existing(_)
                | PublicationValue::Frame,
            ) => return None,
        })
    }
    fn same(&self, next: &Option<PublicationValue>) -> bool {
        match (self, next) {
            (Self::Absent, None) | (Self::Departure, Some(PublicationValue::Departure)) => true,
            (Self::Value(value), Some(PublicationValue::Native(next))) => value.same(next),
            (Self::Capture(handle), Some(PublicationValue::Capture(next))) => handle == next,
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
    fn holds(&self, owner: &Owner, next: &Option<PublicationValue>) -> bool {
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
    /// Re-projects a binding or control slot into the open region: `next`
    /// supersedes the owner's earlier record, and an owner without one that
    /// still holds what the ledger last published for it records nothing.
    pub fn rebind(&self, owner: Owner, next: Option<PublicationValue>) {
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
            // An owner still holding what it last published needs no event.
            let pending: Vec<Replacement> = journal
                .last
                .iter()
                .filter(|(owner, next)| !ledger.published.holds(owner, next))
                .map(|(owner, next)| (*owner, next.clone()))
                .collect();
            if pending.is_empty() && events.is_empty() {
                journal.last.clear();
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
            Ok(())
        })();
        arena.return_member_events(events, result.is_ok());
        result
    }
}
