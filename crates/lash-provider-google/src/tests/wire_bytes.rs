//! The byte-identity law for Google Gemini (FIG-6443): every corpus
//! history, under each dialect, lowers to the body recorded before lowering
//! became incremental.

use super::*;
use lash_core::llm::types::SegmentCache;
use lash_protocol_types::{
    llm::wire_corpus::corpus_tool, llm::wire_corpus::digest_line, llm::wire_corpus::reuse_cases,
};

fn variants(messages: Vec<LlmMessage>) -> Vec<(&'static str, LlmRequest)> {
    let base = LlmRequest {
        messages,
        ..request(None)
    };
    let mut instructed = base.clone();
    instructed.instructions = Some("You are a careful coding agent.".into());
    instructed.tools = Arc::new(vec![corpus_tool()]);
    let mut gemini3 = base.clone();
    gemini3.model.metadata_mut().capability.google_dialect = lash_core::GoogleDialect::Gemini3;
    let mut vertex_claude = base.clone();
    vertex_claude.model.metadata_mut().capability.google_dialect =
        lash_core::GoogleDialect::ClaudeOnVertex;
    vec![
        ("base", base),
        ("instructed", instructed),
        ("gemini3", gemini3),
        ("vertex-claude", vertex_claude),
    ]
}

/// The invalidation law: through one cache, an edited, renamed or
/// compacted history, another dialect and another file scope each lower to
/// the bytes a cold lowering sends.
#[tokio::test]
async fn a_reused_prefix_sends_the_bytes_a_cold_lowering_sends() {
    let token = || Arc::new(lash_core::provider::ProviderToken::new("access"));
    let cache = SegmentCache::new();
    let retained_cache = SegmentCache::new();

    let mut providers = [
        GoogleOAuthProvider::new(token()).with_project_id(Some("wire-corpus-project".into())),
        GoogleOAuthProvider::new(token())
            .with_project_id(Some("wire-corpus-project".into()))
            .with_attachment_credential_scope("workspace-b"),
    ];
    let route = providers[0].route_identity("gemini-3.1-pro-preview");
    for (case, histories) in reuse_cases(&route) {
        for (step, messages) in histories.into_iter().enumerate() {
            for (variant, request) in variants(messages) {
                for provider in &mut providers {
                    let name = format!("{case} step {step} {variant}");
                    let warm = provider.lower(&request, &cache).await;
                    let cold = provider.lower(&request, &SegmentCache::disabled()).await;
                    let retained =
                        lash_protocol_types::llm::types::RetainedLlmRequest::from_request(
                            request.clone(),
                        );
                    let persistent = provider.lower_retained(&retained, &retained_cache).await;
                    assert_eq!(
                        digest_line(
                            &name,
                            persistent.as_ref().map_err(|error| error.message.clone())
                        ),
                        digest_line(&name, cold.as_ref().map_err(|error| error.message.clone())),
                        "retained {name}"
                    );
                    assert_eq!(
                        digest_line(&name, warm.as_ref().map_err(|error| error.message.clone())),
                        digest_line(&name, cold.as_ref().map_err(|error| error.message.clone())),
                    );
                }
            }
        }
    }
    let stats = cache.stats();
    assert!(stats.hits > stats.misses, "{stats:?}");
}
