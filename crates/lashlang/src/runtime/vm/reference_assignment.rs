use super::*;

impl<'a, H: ExecutionHost> Vm<'a, H> {
    pub(super) fn execute_reference_path_assignment(
        &mut self,
        slot: usize,
        path: usize,
    ) -> Result<(), RuntimeError> {
        let value = self.pop_stack()?;
        let path = &self.chunk.assign_paths[path];
        let index_start = self.stack_drain_start(path.dynamic_index_count)?;
        let indexes = self.stack[index_start..].to_vec();
        let slot_names = slot_names_for(self.chunk, self.active_function);
        let root_name = &slot_names[slot];
        self.slots.ensure_assignable(slot, slot_names)?;
        let root =
            self.slots
                .get(slot)
                .cloned()
                .ok_or_else(|| RuntimeError::UndefinedVariable {
                    name: root_name.text.to_string(),
                })?;
        self.heap
            .assign_path_reference(&root, path, &indexes, value.clone(), &self.chunk.names)?;
        self.stack.truncate(index_start);
        self.record_assignment(slot);
        self.last_value = Some(value);
        Ok(())
    }
}
