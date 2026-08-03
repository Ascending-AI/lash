//! Pure request serializers used by cross-provider regression tests.

use lash_core::LlmRequest;
use lash_core::provider::{CacheRetention, ProviderOptions};
use serde_json::Value;

use crate::{CodexProvider, OPENAI_BASE_URL, OpenAiCompatibleProvider};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheBreakpointReport {
    pub requested: usize,
    pub emitted: usize,
    pub dropped: usize,
}

pub fn serialize_chat_request(
    request: &LlmRequest,
    retention: CacheRetention,
) -> Result<(Value, CacheBreakpointReport), lash_core::facade_support::LlmTransportError> {
    let provider = OpenAiCompatibleProvider::new("test", "https://provider.test").with_options(
        ProviderOptions {
            cache_retention: retention,
            ..ProviderOptions::default()
        },
    );
    let (body, diagnostics) = provider.build_chat_request_body_with_diagnostics(request, false)?;
    Ok((
        body,
        CacheBreakpointReport {
            requested: diagnostics.requested,
            emitted: diagnostics.emitted,
            dropped: diagnostics.dropped,
        },
    ))
}

pub fn serialize_responses_request(
    request: &LlmRequest,
    retention: CacheRetention,
) -> Result<Value, lash_core::facade_support::LlmTransportError> {
    OpenAiCompatibleProvider::new("test", OPENAI_BASE_URL)
        .with_compat(crate::OpenAiCompat {
            prompt_cache_key: Some(true),
            prompt_cache_retention: Some(true),
            ..crate::OpenAiCompat::default()
        })
        .with_options(ProviderOptions {
            cache_retention: retention,
            ..ProviderOptions::default()
        })
        .build_responses_request_body(request, false)
}

pub fn serialize_codex_request(
    request: &LlmRequest,
    retention: CacheRetention,
) -> Result<Value, lash_core::facade_support::LlmTransportError> {
    CodexProvider::new("access", "refresh", 0)
        .with_options(ProviderOptions {
            cache_retention: retention,
            ..ProviderOptions::default()
        })
        .build_request_body(request, false)
}
