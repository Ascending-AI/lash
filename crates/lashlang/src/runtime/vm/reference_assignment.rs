use super::*;

impl<'a, H: ExecutionHost> Vm<'a, H> {
    pub(super) fn materialize_model_view_slot(&mut self, slot: usize) -> Result<(), RuntimeError> {
        let Some(Value::Projected(projected)) = self.slots.get(slot) else {
            return Ok(());
        };
        if !projected.has_model_view() {
            return Ok(());
        }
        let projected = projected.clone();
        let value = projected.materialize()?;
        let imported = self.heap.import_values(vec![value], 1)?.remove(0);
        for root in self.slots.values.iter_mut().filter_map(Option::as_mut) {
            if matches!(root, Value::Projected(other) if projected.shares_model_view_value(other)) {
                *root = imported.clone();
            }
        }
        Ok(())
    }

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
        self.slots
            .ensure_assignable(slot, slot_names, self.active_projected_bindings())?;
        self.materialize_model_view_slot(slot)?;
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
        self.last_value = Some(value);
        Ok(())
    }
}
