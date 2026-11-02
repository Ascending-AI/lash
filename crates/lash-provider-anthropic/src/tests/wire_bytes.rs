//! The byte-identity law for Anthropic Messages (FIG-6443): every corpus
//! history, under each request shape, lowers to the body recorded before
//! lowering became incremental.

use super::*;
use lash_core::llm::types::SegmentCache;
use lash_protocol_types::{llm::wire_corpus::corpus_tool, llm::wire_corpus::reuse_cases};

fn variants(messages: Vec<LlmMessage>) -> Vec<(&'static str, LlmRequest)> {
    let base = request(messages);
    let mut instructed = base.clone();
    instructed.instructions = Some("You are a careful coding agent.".into());
    instructed.tools = Arc::new(vec![corpus_tool()]);
    let mut uncached = base.clone();
    uncached
        .model
        .metadata_mut()
        .request_defaults
        .cache_retention = CacheRetention::None;
    let mut long = base.clone();
    long.model.metadata_mut().request_defaults.cache_retention = CacheRetention::Long;
    vec![
        ("base", base),
        ("instructed", instructed),
        ("uncached", uncached),
        ("long", long),
    ]
}

/// The invalidation law: through one cache, an edited, renamed or
/// compacted history, another cache retention, another endpoint and
/// another file scope each lower to the bytes a cold lowering sends.
#[tokio::test]
async fn a_reused_prefix_sends_the_bytes_a_cold_lowering_sends() {
    let cache = SegmentCache::new();
    let retained_cache = SegmentCache::new();

    let mut providers = [
        AnthropicProvider::new("key"),
        AnthropicProvider::new("key").with_base_url(Some("https://anthropic-b.example".into())),
        AnthropicProvider::new("key").with_attachment_credential_scope("workspace-b"),
    ];
    let route = providers[0].route_identity("claude-sonnet-4-6");
    for (case, histories) in reuse_cases(&route) {
        for (step, messages) in histories.into_iter().enumerate() {
            for (variant, request) in variants(messages) {
                for provider in &mut providers {
                    let warm = provider.lower(&request, &cache).await.unwrap();
                    let cold = provider
                        .lower(&request, &SegmentCache::disabled())
                        .await
                        .unwrap();
                    let retained =
                        lash_protocol_types::llm::types::RetainedLlmRequest::from_request(
                            request.clone(),
                        );
                    let persistent = provider
                        .lower_retained(&retained, &retained_cache)
                        .await
                        .unwrap();
                    assert_eq!(
                        serde_json::to_vec(&persistent).unwrap(),
                        serde_json::to_vec(&cold).unwrap(),
                        "retained {case} step {step} {variant}"
                    );
                    assert_eq!(
                        serde_json::to_vec(&warm).unwrap(),
                        serde_json::to_vec(&cold).unwrap(),
                        "{case} step {step} {variant}"
                    );
                }
            }
        }
    }
    let stats = cache.stats();
    assert!(stats.hits > stats.misses, "{stats:?}");
}
