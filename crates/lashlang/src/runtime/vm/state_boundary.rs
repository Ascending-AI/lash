impl<'a, H: ExecutionHost> Vm<'a, H> {
    /// Builds a VM from authored globals for an externally driven execution.
    pub fn from_state(
        program: &'a CompiledProgram,
        state: &mut State,
        host: &'a H,
    ) -> Result<Self, RuntimeError> {
        state.validate_program(program)?;
        let projected = host.projected_bindings();
        let (globals, heap) = state.take_runtime();
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
