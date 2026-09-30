//! Pure request serializer used by cross-provider regression tests.

use lash_core::LlmRequest;
use lash_core::provider::CacheRetention;
use serde_json::Value;

use crate::AnthropicProvider;

pub fn serialize_request(
    request: &LlmRequest,
    retention: CacheRetention,
) -> Result<Value, lash_core::facade_support::LlmTransportError> {
    let mut request = request.clone();
    request.request_defaults.cache_retention = retention;
    // Messages requires a cap, and lash invents none.
    request
        .request_defaults
        .max_output_tokens
        .get_or_insert(4_096);
    AnthropicProvider::new("test").build_request_body(&request)
}
