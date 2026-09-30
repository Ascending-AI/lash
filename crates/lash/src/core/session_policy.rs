use super::*;

/// The creation defaults of the sessions this core creates.
///
/// These setters only seed [`SessionSpec`]; a session's
/// [`SessionCreation::spec`](crate::SessionCreation::spec) is resolved against
/// them when [`create`](crate::SessionBuilder::create) records its config,
/// and never at open. The execution controls — turn budget, autonomy,
/// no-progress budget, charge safety — are session config like the model
/// (FIG-4376): state them per session in its creation spec. The core's
/// default turn budget is the one [`LashCore::builder`] takes.
impl LashCoreBuilder {
    pub fn model(mut self, model: lash_core::ModelSpec) -> Self {
        self.session_spec = self.session_spec.model(model);
        self
    }

    /// Generation options — output token cap, temperature, seed — carried by
    /// every LLM call in every session this core creates.
    pub fn generation(mut self, generation: lash_core::GenerationOptions) -> Self {
        self.session_spec = self.session_spec.generation(generation);
        self
    }

    pub fn session_spec(mut self, spec: SessionSpec) -> Self {
        self.session_spec = spec;
        self
    }
}
