//! Pure request serializer used by cross-provider regression tests.

use lash_core::LlmRequest;
use lash_core::provider::{CacheRetention, ProviderOptions};
use serde_json::Value;

use crate::GoogleOAuthProvider;

pub fn serialize_request(request: &LlmRequest, retention: CacheRetention) -> Value {
    let provider = GoogleOAuthProvider::new(
        "access",
        "refresh",
        0,
        crate::GoogleOAuthClient {
            id: "oauth-client-id".into(),
            secret: "oauth-client-secret".into(),
        },
    )
    .with_options(ProviderOptions {
        cache_retention: retention,
        ..ProviderOptions::default()
    });
    let contents = provider.build_contents_with_attachment_parts(request, &[]);
    GoogleOAuthProvider::build_request(&provider, request, contents, None)
}
