//! Provider components for pluggable LLM backends.
//!
//! A provider is split into narrow capabilities: configured state,
//! request transport, and failure classification. [`ProviderHandle`] owns
//! those components and is the executable handle installed by the host for a
//! running session. Model capability metadata is host-supplied data that
//! travels with each request; the provider does not produce it.
//!
//! [`ProviderSpec`] is only a host/config-file data shape. Runtime
//! persistence records provider identity separately and never rebuilds a
//! [`ProviderHandle`] from disk.

mod factory;
mod handle;
mod options;
mod rate_limit;
mod resolver;
mod spec;
mod support;
#[cfg(test)]
mod tests;
mod traits;

pub use factory::ProviderFactory;
pub(crate) use handle::synthetic_terminal_call_record;
pub use handle::{
    ProviderCompletion, ProviderCompletionError, ProviderComponents, ProviderHandle,
    UnconfiguredProvider,
};
pub use lash_sansio::llm::capability::{
    CacheControlDialect, ModelCapability, ModelEffortValidationCategory,
    ModelEffortValidationError, ReasoningCapability, ReasoningDisableEncoding, ReasoningEncoding,
    ReasoningSelection, SamplingCapability, StreamTermination,
};
pub use options::{
    CacheRetention, DEFAULT_CHUNK_TIMEOUT_MS, DEFAULT_REQUEST_TIMEOUT_MS,
    DEFAULT_THROTTLE_WAIT_BUDGET_MS, LlmTimeouts, ProviderOptions, ProviderRateLimitPolicy,
    ProviderReliability, ProviderRetryPolicy, RequestTimeout, ResolvedGenerationPolicy,
    resolve_generation_policy,
};
pub use rate_limit::{ProviderRateLimitPermit, ProviderRateLimiter};
pub use resolver::{
    EmptyProviderResolver, MapProviderResolver, ProviderBinding, ProviderResolutionError,
    RuntimeProviderResolver, SingleProviderResolver,
};
pub use spec::ProviderSpec;
pub use traits::{
    DefaultProviderFailureClassifier, GenerationRetryGuarantee, Provider,
    ProviderFailureClassifier, is_context_overflow_text,
};
