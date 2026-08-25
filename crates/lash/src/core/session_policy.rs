use super::*;

/// The session policy every session this core opens starts from.
///
/// These setters only seed [`SessionSpec`]; the values they carry are resolved
/// against it at build time and again per session, so they live together and
/// away from the builder's dependency wiring.
impl LashCoreBuilder {
    /// Configures the model and returns the updated builder.
    pub fn model(mut self, model: lash_core::ModelSpec) -> Self {
        self.session_spec = self.session_spec.model(model);
        self
    }

    /// Configures the turn budget and returns the updated builder.
    pub fn turn_budget(mut self, turn_budget: lash_core::TurnBudget) -> Self {
        self.session_spec = self.session_spec.turn_budget(turn_budget);
        self
    }

    /// Bound the consecutive provider attempts a turn may spend without
    /// committing a successful execution.
    ///
    /// Unset, sessions carry [`lash_core::NoProgressBudget::default`], which is
    /// bounded. Pass [`lash_core::NoProgressBudget::Unbounded`] to opt a
    /// deployment out of the bound deliberately.
    pub fn no_progress_budget(mut self, no_progress_budget: lash_core::NoProgressBudget) -> Self {
        self.session_spec = self.session_spec.no_progress_budget(no_progress_budget);
        self
    }

    /// Generation options — output token cap, temperature, seed — carried by
    /// every LLM call in every session this core opens.
    pub fn generation(mut self, generation: lash_core::GenerationOptions) -> Self {
        self.session_spec = self.session_spec.generation(generation);
        self
    }

    /// Configures the session spec and returns the updated builder.
    pub fn session_spec(mut self, spec: SessionSpec) -> Self {
        self.session_spec = spec;
        self
    }
}
