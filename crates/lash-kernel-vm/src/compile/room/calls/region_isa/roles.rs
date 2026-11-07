//! Logical call roles hold lease values, including fresh and overlaid objects.
use super::*;
use std::marker::PhantomData;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CallRole {
    slot: Slot,
    kind: PhantomData<fn() -> LeaseValue>,
}
impl CallRole {
    pub(super) fn new(slot: Slot) -> Self {
        Self {
            slot,
            kind: PhantomData,
        }
    }
    pub(crate) fn slot(self) -> Slot {
        self.slot
    }
    /// Copying a non-owning lease value preserves its object identity. The
    /// executor couples source holds/releases to the verified semantic phase.
    pub(crate) fn read(self, read: impl FnOnce(Slot) -> Option<LeaseValue>) -> Option<LeaseValue> {
        read(self.slot)
    }
}

/// A shared parameter receives a fresh local cell containing its input value;
/// direct parameters receive that same input value without graph copying.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ParameterBinding {
    pub(crate) role: CallRole,
    pub(crate) shared: bool,
}

/// Actual callable bindings borrow D's view. They grant no invocation or
/// semantic authority and snapshot no mutable capture contents.
pub(crate) struct CallBinding<'lease> {
    pub(super) plan: Arc<CalleePlan>,
    pub(super) callable: LeaseValue,
    pub(super) view: LeaseCallable<'lease>,
}
impl CallBinding<'_> {
    pub(crate) fn plan(&self) -> &Arc<CalleePlan> {
        &self.plan
    }
    pub(crate) fn identity(&self) -> lash_kernel_value::CallableId {
        *self.view.identity()
    }
    pub(crate) fn callable(&self) -> LeaseValue {
        self.callable
    }
    /// The canonical entity the callable was imported as.
    pub(crate) fn entity(&self) -> Option<lash_kernel_doc::heap::EntityId> {
        self.view.entity()
    }
    pub(crate) fn capture_roles(
        &self,
    ) -> impl ExactSizeIterator<Item = (CallRole, LeaseValue)> + '_ {
        self.plan
            .captures()
            .iter()
            .zip(self.view.captures())
            .map(|(binding, cell)| (binding.role, *cell))
    }
}
