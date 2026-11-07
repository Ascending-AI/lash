//! Source binding identity and physical placement are verified separately.
//! A dense activation borrows no arena storage and never becomes durable code.
use super::*;
use crate::compile::{CodeId, Executable};
use crate::machine::SlotState;
use lash_kernel_doc::{TaskId, Value};
use lash_kernel_state::kernel_internals::{
    PagedHeap,
    logical::{self, Invocation, Ledger, Slot},
};

/// Each source binding has one physical home in this exact code layout.
pub(crate) struct BindingPlacement {
    source: Arc<Source>,
    code: CodeId,
    layout: Arc<PlacementLayout>,
}

struct PlacementLayout {
    slots: Box<[Slot]>,
    shared: Box<[bool]>,
    /// Each binding or capture slot's physical position.
    homes: std::collections::BTreeMap<Slot, u32>,
}
impl PlacementLayout {
    fn physical(&self, slot: Slot) -> Option<u32> {
        self.homes.get(&slot).copied()
    }
}

/// A view of the activation's actual initialized binding range. The values
/// remain in their persistent slab homes; entry allocates no mirror and scans
/// no bindings. The borrow retains the values until the prefix is prepared.
/// Source identity, invocation and ledger revision authenticate the view.
pub(crate) struct DenseActivation<'v> {
    source: Arc<Source>,
    invocation: Invocation,
    stamp: logical::Stamp,
    layout: Arc<PlacementLayout>,
    values: &'v [SlotState],
}

impl BindingPlacement {
    pub(crate) fn code(&self) -> CodeId {
        self.code
    }
    pub(crate) fn source(&self) -> &Arc<Source> {
        &self.source
    }
    pub(crate) fn compile(exe: &Executable, code: CodeId) -> Result<Self, Refused> {
        let body = exe.code(code);
        let source = Arc::clone(&body.source_room);
        source.verify()?;
        if body.slots_by_at.len() != body.slots.len() {
            return Err(Refused::Source);
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut slots = Vec::with_capacity(body.slots.len());
        let mut shared = Vec::with_capacity(body.slots.len());
        let mut homes = std::collections::BTreeMap::new();
        for &index in body.slots_by_at.iter() {
            if !seen.insert(index) {
                return Err(Refused::Source);
            }
            let physical = body.slots.get(index as usize).ok_or(Refused::Source)?;
            let binding = source
                .bindings
                .iter()
                .find(|binding| binding.index == index)
                .ok_or(Refused::Source)?;
            if binding.name != physical.name
                || binding.declared != physical.declared
                || binding.shared != physical.shared
            {
                return Err(Refused::Source);
            }
            let Some(logical::Owner::Value { slot, .. }) =
                source.binding_root(index, TaskId::MAIN, 0)
            else {
                return Err(Refused::Source);
            };
            if !matches!(slot, Slot::Binding(_) | Slot::Capture(_))
                || homes.insert(slot, slots.len() as u32).is_some()
            {
                return Err(Refused::Source);
            }
            slots.push(slot);
            shared.push(binding.shared);
        }
        Ok(Self {
            source,
            code,
            layout: Arc::new(PlacementLayout {
                slots: slots.into(),
                shared: shared.into(),
                homes,
            }),
        })
    }

    pub(crate) fn unshared(&self, physical: u32) -> bool {
        self.layout.shared.get(physical as usize) == Some(&false)
    }

    pub(crate) fn physical(&self, slot: Slot) -> Option<u32> {
        self.layout.physical(slot)
    }

    /// Validate a lowered source read against lexical resolution, rather than
    /// trusting a physical register which happens to hold a same-sized value.
    pub(crate) fn verify_read(&self, at: &Point, physical: u32) -> Result<(), Refused> {
        let super::region::ScalarInput::Binding(slot) = self
            .source
            .scalar_input(self.source.owner_slots()?.as_ref(), at)?
        else {
            return Err(Refused::Source);
        };
        if self.physical(slot) != Some(physical) {
            return Err(Refused::Source);
        }
        Ok(())
    }
    pub(crate) fn verify_literal(&self, at: &Point, actual: Value) -> Result<(), Refused> {
        match self
            .source
            .scalar_input(self.source.owner_slots()?.as_ref(), at)?
        {
            super::region::ScalarInput::Literal(expected) if expected.same(&actual) => Ok(()),
            _ => Err(Refused::Facts),
        }
    }

    pub(crate) fn capture<'v>(
        &self,
        code: CodeId,
        invocation: &Invocation,
        ledger: &Ledger,
        heap: &PagedHeap,
        physical: &'v [SlotState],
    ) -> Result<Option<DenseActivation<'v>>, doc::heap::HeapFault> {
        if code != self.code || physical.len() != self.layout.slots.len() {
            return Ok(None);
        }
        let logical::Owner::Frame { task, .. } = invocation.frame() else {
            return Ok(None);
        };
        if ledger
            .invocation(task, invocation.id(), heap.pages())
            .as_ref()
            .ok()
            != Some(invocation)
        {
            return Ok(None);
        }
        Ok(Some(DenseActivation {
            source: Arc::clone(&self.source),
            invocation: invocation.clone(),
            stamp: ledger.stamp(),
            layout: Arc::clone(&self.layout),
            values: physical,
        }))
    }
}

impl DenseActivation<'_> {
    pub(crate) fn current(
        &self,
        source: &Arc<Source>,
        ledger: &Ledger,
        invocation: &Invocation,
    ) -> bool {
        Arc::ptr_eq(source, &self.source)
            && ledger.is_current(&self.stamp)
            && invocation == &self.invocation
    }
    pub(crate) fn state(&self, slot: Slot) -> Option<SlotState> {
        let at = self.layout.physical(slot)?;
        self.values.get(at as usize).copied()
    }
    pub(crate) fn value(&self, slot: Slot) -> Option<Value> {
        match self.state(slot)? {
            SlotState::Value(value) => Some(value),
            SlotState::Empty | SlotState::Cell(_) => None,
        }
    }
}
