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
    request
        .model
        .metadata_mut()
        .request_defaults
        .cache_retention = retention;
    // This serializer fixture requests a cap when neither caller nor profile does.
    if request.generation.output_token_cap.is_none()
        && request
            .model
            .metadata()
            .limits
            .output_tokens
            .default_cap()
            .is_none()
    {
        request.generation.output_token_cap = std::num::NonZeroUsize::new(4096);
    }
    AnthropicProvider::new("test").build_request_body(&request)
}
