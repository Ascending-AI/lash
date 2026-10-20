//! What a [`crate::CellSave`] works with at a session cell's end
//! (`K-SES-004`): the dialect's saving rules run over the cell's heap, and
//! each kept function becomes its session record, without the embedder
//! touching the heap.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_dialect::{Kept, Left, NotSaved, SavedFunction};
use lash_kernel_doc::Name;
use lash_kernel_doc::heap::{HeapFault, PageValue};
use lash_kernel_state::kernel_internals::PagedHeap;
use lash_kernel_state::paged::{FunctionRecord, HeapWriteError, NameRef};

/// The functions a cell's end saves, by name, and why each other one is
/// not saved.
type Sorted = (BTreeMap<Name, Kept>, BTreeMap<Name, NotSaved>);

/// A session cell's end, as its [`crate::CellSave`] saves functions on it.
pub struct CellSaveContext<'a> {
    heap: &'a mut PagedHeap,
}

impl<'a> CellSaveContext<'a> {
    pub(crate) fn new(heap: &'a mut PagedHeap) -> Self {
        Self { heap }
    }

    /// The functions `left` holds that its dialect's rules save, each with
    /// its captures frozen once, and why each other one is not saved
    /// (the dialect's `save`). `held` names the functions the
    /// session already holds.
    ///
    /// # Errors
    ///
    /// The heap's faults.
    pub fn save(&mut self, left: Left<'_>, held: &BTreeSet<Name>) -> Result<Sorted, HeapFault> {
        lash_kernel_dialect::kernel_internals::save(self.heap, left, held)
    }

    /// The session record of the saved `function`, described by
    /// `description`, and the value every binding that held it is bound to:
    /// data, which carries.
    ///
    /// # Errors
    ///
    /// The heap's faults.
    pub fn record(
        &mut self,
        function: &SavedFunction,
        description: &[u8],
    ) -> Result<(FunctionRecord, PageValue), HeapFault> {
        let (before, after) = function
            .token
            .as_ref()
            .map(|token| (token.before.clone(), token.after.clone()))
            .unwrap_or_default();
        let record = FunctionRecord {
            name: NameRef::new(&function.name, self.heap.pages())?,
            meta: PageValue::Bytes(self.heap.bytes(description)?),
            closure: function.closure,
            captures: function.captures.clone(),
            before,
            after,
        };
        let value = lash_kernel_dialect::kernel_internals::saved_value(function, self.heap)
            .map_err(write)?;
        Ok((record, value))
    }
}

fn write(error: HeapWriteError) -> HeapFault {
    match error {
        HeapWriteError::Fault(fault) => fault,
        other => HeapFault::Malformed(format!("a saved function's value: {other}")),
    }
}
