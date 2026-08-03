//! Pure request serializer used by cross-provider regression tests.

use lash_core::LlmRequest;
use lash_core::provider::{CacheRetention, ProviderOptions};
use serde_json::Value;

use crate::AnthropicProvider;

pub fn serialize_request(
    request: &LlmRequest,
    retention: CacheRetention,
) -> Result<Value, lash_core::facade_support::LlmTransportError> {
    AnthropicProvider::new("test")
        .with_options(ProviderOptions {
            cache_retention: retention,
            ..ProviderOptions::default()
        })
        .build_request_body(request)
}
