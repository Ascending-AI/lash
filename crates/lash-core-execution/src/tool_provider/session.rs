#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolSessionLlmProfile {
    /// The owner's recorded binding and reasoning selection.
    pub model: crate::LlmProfileConfig,
    pub attachment_acceptance: std::sync::Arc<crate::provider::AttachmentCapabilitySnapshot>,
    /// Original session intent; provider resolution applies the recorded limits.
    pub generation: crate::GenerationOptions,
}
