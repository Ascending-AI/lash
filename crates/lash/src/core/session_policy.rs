use super::*;

/// The creation defaults of the sessions this core creates.
///
/// These setters only seed [`SessionSpec`]; a session's
/// [`SessionCreation::spec`](crate::SessionCreation::spec) is resolved against
/// them when [`create`](crate::SessionBuilder::create) records its config,
/// and never at open. The execution controls — turn budget, autonomy,
/// no-progress budget, charge safety — are session config like the model
/// (FIG-4376): state them per session in its creation spec. The core's
/// default turn budget is the one [`LashCore::builder`] takes. So is the
/// core's prompt layer (the [`PromptLayerSink`](crate::PromptLayerSink)
/// setters on this builder) (FIG-4397): a session records the core's at
/// creation and keeps it on every worker, whatever that worker's core states.
impl LashCoreBuilder {
    /// The model a session is created with when its creation names none, by
    /// the host's key. Required: [`build`](Self::build) refuses a core with
    /// no default key, and one its [`models`](Self::models) do not register.
    pub fn model(mut self, key: impl Into<lash_core::ModelKey>) -> Self {
        self.session_spec = self.session_spec.model(key);
        self
    }

    /// The reasoning a session runs its model with when its creation names
    /// none.
    pub fn reasoning(mut self, reasoning: lash_core::ReasoningSelection) -> Self {
        self.session_spec = self.session_spec.reasoning(reasoning);
        self
    }

    /// The attachment-acceptance rules a session renders attachments against
    /// when its creation names none (ADR 0026). They are recorded apart from
    /// the model, so a model change keeps them.
    pub fn attachment_acceptance(
        mut self,
        acceptance: Arc<lash_core::AttachmentCapabilitySnapshot>,
    ) -> Self {
        self.session_spec = self.session_spec.attachment_acceptance(acceptance);
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
