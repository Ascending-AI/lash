//! What a [`crate::CellSave`] works with at a session cell's end
//! (`K-SES-004`): the dialect's saving rules run over the cell's heap, and
//! each saved function becomes its session record, without the embedder
//! touching the heap.

use std::collections::BTreeSet;

use lash_kernel_dialect::{Left, Saved, SavedFunction};
use lash_kernel_doc::heap::{HeapFault, PageValue};
use lash_kernel_state::kernel_internals::PagedHeap;
use lash_kernel_state::paged::{FunctionRecord, NameRef};
use lash_vocabulary::names::Name;

/// A session cell's end, as its [`crate::CellSave`] saves functions on it.
pub struct CellSaveContext<'a> {
    heap: &'a mut PagedHeap,
    held: lash_kernel_state::paged::KeyedRoot,
}

impl<'a> CellSaveContext<'a> {
    pub(crate) fn new(heap: &'a mut PagedHeap, held: lash_kernel_state::paged::KeyedRoot) -> Self {
        Self { heap, held }
    }

    /// Every closure the bindings `left` drops reach, saved once with its
    /// captures frozen, the value each of those bindings is bound to
    /// instead, and why each other one is not carried (the dialect's
    /// `save`). `held` names the functions the session already holds.
    ///
    /// # Errors
    ///
    /// The heap's faults.
    pub fn save(&mut self, left: Left<'_>, held: &BTreeSet<Name>) -> Result<Saved, HeapFault> {
        lash_kernel_dialect::kernel_internals::save(self.heap, left, held)
    }

    /// Whether this exact environment and callable already have a session
    /// record. Check before making its description or freezing metadata: the
    /// machine retains that record unchanged, so new pages would be discarded.
    ///
    /// # Errors
    /// The heap's faults reading the current function table.
    pub fn already_recorded(&self, function: &SavedFunction) -> Result<bool, HeapFault> {
        use lash_kernel_state::kernel_internals::{metadata::metadata_key, tree::Tree};
        Ok(Tree::<FunctionRecord>::of_keyed(&self.held)
            .get(&metadata_key(&function.name), self.heap.pages())?
            .is_some_and(|held| {
                held.environment == function.environment && held.callable == function.callable
            }))
    }

    /// The session record of the saved `function`, described by
    /// `description`.
    ///
    /// # Errors
    ///
    /// The heap's faults.
    pub fn record(
        &mut self,
        function: &SavedFunction,
        description: &[u8],
    ) -> Result<FunctionRecord, HeapFault> {
        let (before, after) = function
            .token
            .as_ref()
            .map(|token| (token.before.clone(), token.after.clone()))
            .unwrap_or_default();
        Ok(FunctionRecord {
            environment: function.environment,
            callable: function.callable,
            name: NameRef::new(&function.name, self.heap.pages())?,
            meta: PageValue::Bytes(self.heap.bytes(description)?),
            closure: function.closure,
            captures: function.captures.clone(),
            before,
            after,
        })
    }
}
