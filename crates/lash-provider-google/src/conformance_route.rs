use lash_core::llm::types::LlmOutputPart;
use lash_core::provider::Provider;

use crate::GoogleOAuthProvider;

pub(super) fn stamp_google_replay_origin(parts: &mut [LlmOutputPart]) {
    let route = GoogleOAuthProvider::new(
        "access",
        "refresh",
        0,
        crate::GoogleOAuthClient {
            id: "oauth-client-id".into(),
            secret: "oauth-client-secret".into(),
        },
    )
    .route_identity("gemini-3.1-pro-preview");
    for part in parts {
        part.stamp_replay_origin(&route)
            .expect("conformance output accepts its minting route");
    }
}
