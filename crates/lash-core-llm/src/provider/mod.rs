//! Provider components for pluggable LLM backends.
//!
//! A provider is split into narrow capabilities: configured state,
//! request transport, and failure classification. [`ProviderHandle`] owns
//! those components and is the executable handle installed by the host for a
//! running session. Model capability metadata is host-supplied data that
//! travels with each request; the provider does not produce it.
//!
//! A host registers each model it serves, with the handle that executes it,
//! in a [`ModelRegistry`]; sessions record the registry-minted binding and
//! bind it to its handle only to execute.

#[cfg(test)]
mod charge_safety_tests;
mod dispatch_admission;
pub(crate) mod handle;
mod models;
mod options;
mod rate_limit;
mod support;
#[cfg(test)]
mod tests;
mod traits;

pub use dispatch_admission::{DispatchAdmission, DispatchRefused, ProviderDispatch};
pub use handle::{
    ProviderCompletion, ProviderCompletionError, ProviderComponents, ProviderHandle,
    UnconfiguredProvider,
};
pub use lash_sansio::llm::capability::{
    AnthropicThinkingRetention, AttachmentAcceptanceRule, AttachmentAcceptor,
    AttachmentCapabilitySnapshot, AttachmentMimeSource, CacheControlDialect, CacheRetention,
    GoogleDialect, InstructionRole, ModelCapability, ModelEffortValidationCategory,
    ModelEffortValidationError, ModelRequestDefaults, OpenAiReasoningContext, ReasoningCapability,
    ReasoningEncoding, ReasoningIntent, ReasoningRetentionCapability, ReasoningRetentionPolicy,
    ReasoningRetentionSelection, ReasoningRetentionValidationCategory,
    ReasoningRetentionValidationError, ReasoningSelection, SamplingCapability, StreamTermination,
};
pub use models::{
    EmptyModels, ModelRegistry, ModelUnavailable, ModelUnavailableReason, RegisteredModel,
    RegistrationError, RuntimeModels,
};
pub use options::{
    DEFAULT_CHUNK_TIMEOUT_MS, DEFAULT_REQUEST_TIMEOUT_MS, DEFAULT_THROTTLE_WAIT_BUDGET_MS,
    GenerationEmission, GenerationWire, LlmTimeouts, OutputCapWire, ProviderOptions,
    ProviderRateLimitPolicy, ProviderReliability, ProviderRetryPolicy, RequestTimeout,
    ResolvedGenerationPolicy, ThinkingSummaryWire, resolve_generation_policy,
};
pub use rate_limit::{ProviderRateLimitPermit, ProviderRateLimiter};
pub use traits::{
    DefaultProviderFailureClassifier, GenerationRetryGuarantee, Provider,
    ProviderFailureClassifier, ReconciledUsage, is_context_overflow_text,
};
