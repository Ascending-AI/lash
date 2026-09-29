#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolSessionModel {
    pub model: String,
    pub model_variant: crate::ReasoningSelection,
    pub model_capability: crate::provider::ModelCapability,
    /// The session's generation options, so a tool making its own LLM call on
    /// the session's behalf runs under the same sampling intent as the turn
    /// that invoked it rather than at provider defaults.
    ///
    /// Already bounded by `model`'s output-token capacity, because the direct
    /// path a tool calls has no limits of its own to check a cap against. A
    /// tool that substitutes its own model owns that pairing: the cap it gets
    /// here is the one the *session's* model can produce.
    pub generation: crate::GenerationOptions,
}
