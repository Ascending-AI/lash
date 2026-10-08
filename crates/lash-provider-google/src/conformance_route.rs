use lash_core::llm::types::LlmOutputPart;
use lash_core::provider::Provider;

use crate::GoogleOAuthProvider;

#[expect(
    clippy::expect_used,
    reason = "test support: the conformance output was minted by the same route literal above, so it always accepts it back"
)]
pub(super) fn stamp_google_replay_origin(parts: &mut [LlmOutputPart]) {
    let route = GoogleOAuthProvider::new(std::sync::Arc::new(
        lash_core::provider::ProviderToken::new("access"),
    ))
    .route_identity("gemini-3.1-pro-preview");
    for part in parts {
        part.stamp_replay_origin(&route)
            .expect("conformance output accepts its minting route");
    }
}
