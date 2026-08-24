//! Pure request serializers used by cross-provider regression tests.

use lash_core::LlmRequest;
use lash_core::provider::{CacheRetention, ProviderOptions};
use serde_json::Value;

use crate::{CodexProvider, OPENAI_BASE_URL, OpenAiCompatibleProvider};

#[derive(Clone, Debug, Default)]
pub struct ResponsesStreamParser {
    state: crate::responses_shared::ResponsesStreamState,
}

impl ResponsesStreamParser {
    pub fn parse_payload(
        &mut self,
        provider: &str,
        payload: &str,
    ) -> Result<(), lash_core::facade_support::LlmTransportError> {
        crate::responses_shared::parse_sse_payload(provider, payload, &mut self.state)
    }

    pub fn full_text(&self) -> String {
        self.state.full_text()
    }

    pub fn response_parts_len(&self) -> usize {
        self.state.response_parts().len()
    }

    pub fn usage(&self) -> &lash_core::llm::types::LlmUsage {
        &self.state.usage
    }
}

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
