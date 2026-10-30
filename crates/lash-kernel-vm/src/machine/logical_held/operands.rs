use super::*;
use crate::compile::room::{Owner as Selector, OwnerSlots};
use lash_kernel_doc::TaskId;
use lash_kernel_state::kernel_internals::logical::{Lifecycle, Owner, SourceValue};
use lash_kernel_value::sites::Site;
use std::sync::Arc;

#[derive(Clone)]
pub(in crate::machine) struct BankRoots {
    pub(super) lifecycle: Lifecycle,
    pub(super) task: TaskId,
    pub(super) invocation: u64,
    pub(super) slots: Arc<OwnerSlots>,
}
impl BankRoots {
    pub(in crate::machine) fn operand(&self, site: &Site) -> Result<Owner, HeapFault> {
        self.slots
            .root(&Selector::Operand(site.clone()), self.task, self.invocation)
            .ok_or_else(|| HeapFault::Malformed("source operand has no canonical owner".into()))
    }
    pub(in crate::machine) fn atom(&self, site: &Site, index: u32) -> Result<Owner, HeapFault> {
        self.slots
            .root(
                &Selector::Atom {
                    at: site.clone(),
                    index,
                },
                self.task,
                self.invocation,
            )
            .ok_or_else(|| HeapFault::Malformed("source atom has no canonical owner".into()))
    }
    pub(in crate::machine) fn continuation(&self, site: &Site) -> Result<Owner, HeapFault> {
        self.slots
            .continuation(site, self.task, self.invocation)
            .ok_or_else(|| HeapFault::Malformed("continuation has another source role".into()))
    }
    pub(in crate::machine) fn acquire(&self, owner: Owner, value: Value) {
        self.lifecycle
            .replace(owner, Some(SourceValue::Value(value)));
    }
    pub(in crate::machine) fn release(&self, owner: Owner) {
        self.lifecycle.replace(owner, None);
    }
}
