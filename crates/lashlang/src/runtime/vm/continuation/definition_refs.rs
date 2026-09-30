use super::{VmContinuation, visit_vm_roots};

impl VmContinuation {
    /// Candidate definition IDs held by every parked root and heap container.
    pub fn referenced_definition_ids(
        &self,
    ) -> std::collections::BTreeSet<lash_sansio::ProcessDefinitionId> {
        let mut roots = Vec::new();
        visit_vm_roots(self, &mut roots);
        crate::value_refs::definition_ids_reachable(roots.iter(), Some(&self.heap.heap))
    }
}
