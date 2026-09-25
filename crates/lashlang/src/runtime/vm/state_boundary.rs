use super::*;

impl<'a, H: ExecutionHost> Vm<'a, H> {
    /// Restored closure metadata is checked against `program` before the state
    /// is removed from its owner.
    pub fn from_state(
        program: &'a CompiledProgram,
        state: &mut State,
        host: &'a H,
    ) -> Result<Self, RuntimeError> {
        state.validate_program(program)?;
        let projected = host.projected_bindings();
        let (mut globals, mut heap) = state.take_runtime();
        // A snapshot restore leaves placeholders wherever a projection was
        // nested inside a container or a heap object; the slot-name rebinding
        // in `from_globals` never revisits those (FIG-2865).
        crate::runtime::projected_refresh::refresh_record(&mut globals, &projected);
        crate::runtime::projected_refresh::refresh_heap(&mut heap, &projected);
        let slots = SlotState::from_globals(
            globals,
            &program.chunk.slot_names,
            &program.chunk.private_slots,
            &projected,
            Vec::new(),
        );
        let mut vm = Self::new(program, slots, host, None, host.execution_mode());
        vm.install_heap(heap);
        if host.profile_execution() {
            vm.enable_profile();
        }
        Ok(vm)
    }

    pub fn flush_profile(&mut self, program: &CompiledProgram, host: &H) {
        if host.profile_execution() {
            let mut profile = self.take_profile();
            profile.compile_stats = program.compile_stats;
            host.observe_profile(profile);
        }
    }
}
pub(super) fn validation_plan_cache_entry(schema: &Value) -> Option<(usize, Arc<Record>)> {
    match schema {
        Value::Record(record) => Some((Arc::as_ptr(record) as usize, record.clone())),
        _ => None,
    }
}

impl<'a, H: ExecutionHost> Vm<'a, H> {
    /// Materializes host-visible globals, omitting any entire binding that
    /// contains a function value at any depth.
    pub fn into_globals(mut self) -> Result<Record, RuntimeError> {
        let runtime_globals = self.slots.into_globals(
            &self.chunk.slot_names,
            &self.chunk.private_slots,
            &self.projected_bindings,
            None,
        )?;
        super::super::state::host_view(&runtime_globals, &mut self.heap)
    }

    pub(crate) fn into_state_parts(self) -> Result<(Record, Heap), RuntimeError> {
        let globals = self.slots.into_globals(
            &self.chunk.slot_names,
            &self.chunk.private_slots,
            &self.projected_bindings,
            None,
        )?;
        Ok((globals, self.heap))
    }

    pub(crate) fn recycle_into_state_parts(
        mut self,
        scratch: &mut ExecutionScratch,
    ) -> Result<(Record, Heap), RuntimeError> {
        self.stack.clear();
        self.iter_stack.clear();
        scratch.stack = std::mem::take(&mut self.stack);
        scratch.iter_stack = std::mem::take(&mut self.iter_stack);
        let globals = self.slots.into_globals(
            &self.chunk.slot_names,
            &self.chunk.private_slots,
            &self.projected_bindings,
            Some(&mut scratch.slot_values),
        )?;
        Ok((globals, self.heap))
    }
}
