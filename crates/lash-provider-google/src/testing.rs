//! Pure request serializer used by cross-provider regression tests.

use lash_core::LlmRequest;
use lash_core::provider::CacheRetention;
use serde_json::Value;

use crate::GoogleOAuthProvider;

pub fn serialize_request(
    request: &LlmRequest,
    retention: CacheRetention,
) -> Result<Value, lash_core::facade_support::LlmTransportError> {
    let provider = GoogleOAuthProvider::new(
        "access",
        "refresh",
        0,
        crate::GoogleOAuthClient {
            id: "oauth-client-id".into(),
            secret: "oauth-client-secret".into(),
        },
    );
    let mut request = request.clone();
    request.request_defaults.cache_retention = retention;
    GoogleOAuthProvider::validate_attachments(&request)?;
    let contents = provider.build_contents_with_attachment_parts(&request, &[])?;
    GoogleOAuthProvider::build_request(&provider, &request, contents, None)
}
