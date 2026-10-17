//! Provider components for pluggable LLM backends.
//!
//! A provider is split into narrow capabilities: configured state,
//! request transport, and failure classification. [`ProviderHandle`] owns
//! those components and is the executable handle installed by the host for a
//! running session. Model capability metadata is host-supplied data that
//! travels with each request; the provider does not produce it.
//!
//! A host registers each model it serves, with the handle that executes it,
//! in a [`LlmProfileRegistry`]; sessions record the registry-minted binding and
//! bind it to its handle only to execute.

pub mod attachment_wire;
#[cfg(test)]
mod charge_safety_tests;
mod credential;
pub(crate) mod handle;
mod models;
mod options;
mod rate_limit;
mod slot_delivery;
mod support;
#[cfg(test)]
mod tests;
mod traits;

pub use credential::{
    ProviderToken, TokenError, TokenErrorKind, TokenRequest, TokenRequestReason, TokenSource,
};
pub use handle::{
    ProviderCancellationGuard, ProviderCompletion, ProviderCompletionError, ProviderComponents,
    ProviderHandle, UnconfiguredProvider,
};
pub use lash_protocol_types::{
    llm::capability::AnthropicThinkingRetention, llm::capability::AttachmentAcceptanceRule,
    llm::capability::AttachmentAcceptor, llm::capability::AttachmentCapabilitySnapshot,
    llm::capability::CacheControlDialect, llm::capability::CacheRetention,
    llm::capability::GoogleDialect, llm::capability::InstructionRole,
    llm::capability::LlmProfileCapability, llm::capability::LlmProfileEffortValidationCategory,
    llm::capability::LlmProfileEffortValidationError, llm::capability::LlmProfileRequestDefaults,
    llm::capability::OpenAiReasoningContext, llm::capability::ReasoningCapability,
    llm::capability::ReasoningEncoding, llm::capability::ReasoningIntent,
    llm::capability::ReasoningRetentionCapability, llm::capability::ReasoningRetentionPolicy,
    llm::capability::ReasoningRetentionSelection,
    llm::capability::ReasoningRetentionValidationCategory,
    llm::capability::ReasoningRetentionValidationError, llm::capability::ReasoningSelection,
    llm::capability::SamplingCapability, llm::capability::StreamTermination,
};
pub use models::{
    EmptyLlmProfiles, LlmProfileRegistry, LlmProfileUnavailable, LlmProfileUnavailableReason,
    LlmProfiles, RegisteredLlmProfile, RegistrationError,
};
pub use options::{
    DEFAULT_THROTTLE_WAIT_BUDGET_MS, GenerationEmission, GenerationWire, LlmTimeouts,
    OutputCapWire, ProviderOptions, ProviderRateLimitPolicy, ProviderRateWindow,
    ProviderReliability, ProviderRetryPolicy, ResolvedGenerationPolicy, RouteBound,
    RouteBoundAboveBudget, ThinkingSummaryWire, resolve_generation_policy,
};
pub use rate_limit::{ProviderRateLimitPermit, ProviderRateLimiter};
pub use slot_delivery::{
    AttachmentDeliveryError, AttachmentReadPolicy, CanonicalDeliveries, InlineDeliveries,
    NoSlotDeliveries, SlotDeliveries,
};
pub use traits::{
    AttemptEndReason, AttemptTerminal, DefaultProviderFailureClassifier, GenerationRetryGuarantee,
    LiveCallHorizon, Provider, ProviderAttempt, ProviderAttemptControl, ProviderFailureClassifier,
    ProviderTerminalPolicy, canonical_call, canonical_request, is_context_overflow_text,
};
