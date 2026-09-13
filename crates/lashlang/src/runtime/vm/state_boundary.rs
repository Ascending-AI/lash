use super::*;

impl<'a, H: ExecutionHost> Vm<'a, H> {
    /// Builds a VM from authored globals for an externally driven execution.
    ///
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
        let slots = SlotState::from_globals(globals, &program.chunk.slot_names, &projected);
        let mut vm = Self::new_with_mode(&program.chunk, slots, host, host.execution_mode());
        vm.install_heap(heap);
        if host.profile_execution() {
            vm.enable_profile();
        }
        Ok(vm)
    }

    /// Emits the accumulated profile after an externally driven VM finishes.
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
