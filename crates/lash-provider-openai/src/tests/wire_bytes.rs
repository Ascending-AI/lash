//! The byte-identity law for OpenAI Responses, Chat Completions and Codex
//! (FIG-6443): every corpus history, under each request shape, lowers to
//! the body recorded before lowering became incremental.

use super::*;
use lash_protocol_types::{
    llm::wire_corpus::corpus_tool, llm::wire_corpus::digest_line, llm::wire_corpus::reuse_cases,
};

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
    let mut anthropic_cache = base.clone();
    anthropic_cache
        .model
        .metadata_mut()
        .capability
        .cache_control = Some(CacheControlDialect::Anthropic);
    vec![
        ("base", base),
        ("instructed", instructed),
        ("uncached", uncached),
        ("cache-control", anthropic_cache),
    ]
}

/// The invalidation law: through one cache shared by Responses, Chat
/// Completions and Codex, an edited, renamed or compacted history and
/// another cache configuration each lower to what a cold lowering sends,
/// a refusal included.
#[tokio::test]
async fn a_reused_prefix_sends_the_bytes_a_cold_lowering_sends() {
    let model = "openai/gpt-5.4";
    let cache = SegmentCache::new();
    let retained_cache = SegmentCache::new();

    let mut providers: Vec<(&str, Box<dyn Provider>)> = vec![
        ("responses", Box::new(OpenAiProvider::new("key"))),
        ("chat", Box::new(openrouter_provider())),
        (
            "codex",
            Box::new(crate::CodexProvider::new(Arc::new(
                lash_core::provider::ProviderToken::new("access"),
            ))),
        ),
    ];
    let route = providers[0].1.route_identity(model);
    for (case, histories) in reuse_cases(&route) {
        for (step, messages) in histories.into_iter().enumerate() {
            for (variant, request) in variants(messages) {
                for (label, provider) in &mut providers {
                    let name = format!("{label} {case} step {step} {variant}");
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
