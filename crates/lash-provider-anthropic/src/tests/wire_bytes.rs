//! The byte-identity law for Anthropic Messages (FIG-6443): every corpus
//! history, under each request shape, lowers to the body recorded before
//! lowering became incremental.

use super::*;
use lash_core::llm::types::SegmentCache;
use lash_protocol_types::{
    llm::wire_corpus::corpus_tool, llm::wire_corpus::digest_line, llm::wire_corpus::reuse_cases,
    llm::wire_corpus::wire_corpus,
};

/// Digests recorded from the coalescing lowering at de7f61e3dc.
const RECORDED: &str = "
base/prefix-01 f78bd1c6d25e00917a683bdf
instructed/prefix-01 1cf356edb0f8f6cee9a62497
uncached/prefix-01 fb1a6dac4abeed7481add233
long/prefix-01 f0f5f12abb29ce68438f89ab
base/prefix-02 8387e676a169b0e2b79c80cc
instructed/prefix-02 5c21ba95a1efdb24c1ee359d
uncached/prefix-02 8387e676a169b0e2b79c80cc
long/prefix-02 8387e676a169b0e2b79c80cc
base/prefix-03 ab38f83b4a41c1e41eddde2d
instructed/prefix-03 40638cd780062eb550fcc4e2
uncached/prefix-03 194def54c00149d1500d06e6
long/prefix-03 8bbb90b97508c21b44c08b8a
base/prefix-04 854ccd121af3660a343a53fc
instructed/prefix-04 6fe3a67fdee782f2eb1bf62d
uncached/prefix-04 60dfa6fa3cf54d016ece32b4
long/prefix-04 ec3e0632e4252ead924c8354
base/prefix-05 802c47d8c89a297ccae2c4b0
instructed/prefix-05 c9bdde843cb0c94e872beb9a
uncached/prefix-05 802c47d8c89a297ccae2c4b0
long/prefix-05 802c47d8c89a297ccae2c4b0
base/prefix-06 2cc8bc256c1663c0ae14cbad
instructed/prefix-06 fd0c45fb9f12643b69035679
uncached/prefix-06 2cc8bc256c1663c0ae14cbad
long/prefix-06 2cc8bc256c1663c0ae14cbad
base/prefix-07 f3592bf2c0112684253838c9
instructed/prefix-07 5ebce76b39c52bda946fc14e
uncached/prefix-07 1805df83a2555de8bbfe1b7d
long/prefix-07 8a25165b83362b84072c011b
base/prefix-08 7ebaf0313ef1586d4f92664d
instructed/prefix-08 5e96058309b6da5f3e633728
uncached/prefix-08 33d5686f8c02821727af2b3a
long/prefix-08 3b77406350ff74e0d771030f
base/prefix-09 6c69adfb0636aae61f27b2b4
instructed/prefix-09 f167f920c4757abe388c0d71
uncached/prefix-09 7b8ba443bf72cdc436bbdd46
long/prefix-09 3d6e0c7c7977f7d098f1f556
base/prefix-10 f26feb0082506b4afef3b36b
instructed/prefix-10 0736b4bd4249215000792677
uncached/prefix-10 7e6cddf702044e965e36b345
long/prefix-10 3fab4de161a2a54c15e4d542
base/prefix-11 2bd3d0f3a875259dc1e93c50
instructed/prefix-11 fed36b2e8a95e49fb07b59f1
uncached/prefix-11 2f98eaaf0e4db6a748d4e157
long/prefix-11 bfcba6d0be7cb89a5ac37190
base/prefix-12 65467c70c1ee416f4ab2c4bc
instructed/prefix-12 4bb3ec742baece5799570331
uncached/prefix-12 80a09f0b20605424b1392090
long/prefix-12 0e5be32aa0ece3377adbac9e
base/prefix-13 6722b9b08e181a7f1514657a
instructed/prefix-13 97fcf5788a2e8add69288e87
uncached/prefix-13 625fe5642bf3bea434af5ec3
long/prefix-13 50df471f04ddfb611f01f169
base/prefix-14 cd12c13ef09f402d4aa226bd
instructed/prefix-14 627b8413e4b9dad91be8cb10
uncached/prefix-14 d506e4cd05b057db40f85a44
long/prefix-14 99661303b820802fd398e816
base/empty-history 0aa37d8287d19f3b9f8f87c1
instructed/empty-history ecd7c20d28b1edb413fe7c5d
uncached/empty-history 0aa37d8287d19f3b9f8f87c1
long/empty-history 0aa37d8287d19f3b9f8f87c1
base/only-feedback 8456378f365f52080b7c4ca5
instructed/only-feedback 54fb0efe847db8e7aae00ca5
uncached/only-feedback 624c972f8cb66a54a748530e
long/only-feedback 040d8e3162282b6eee035bf6
base/unicode b97bdf69cbc786fed19c3d25
instructed/unicode d2d6d38c70f113eeb1c3cb0f
uncached/unicode 40c49544774c42894b5826fb
long/unicode 1802f13bba7f021f9dc9a1ec
";

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

#[tokio::test]
async fn lowering_sends_the_recorded_bytes() {
    let mut provider = AnthropicProvider::new("key");
    let route = provider.route_identity("claude-sonnet-4-6");
    // One cache across the corpus: each prefix reuses the messages the
    // previous one lowered, and still sends the recorded bytes.
    let cache = SegmentCache::new();
    let mut lines = Vec::new();
    for (name, messages) in wire_corpus(&route) {
        for (variant, request) in variants(messages) {
            let lowered = provider.lower(&request, &cache).await;
            lines.push(digest_line(
                &format!("{variant}/{name}"),
                lowered.as_ref().map_err(|error| error.message.clone()),
            ));
        }
    }
    assert_eq!(
        lines.join("\n"),
        RECORDED.trim(),
        "\n{}\n",
        lines.join("\n")
    );
}

/// The invalidation law: through one cache, an edited, renamed or
/// compacted history, another cache retention, another endpoint and
/// another file scope each lower to the bytes a cold lowering sends.
#[tokio::test]
async fn a_reused_prefix_sends_the_bytes_a_cold_lowering_sends() {
    let cache = SegmentCache::new();
    let retained_cache = SegmentCache::new();
    let budget = lash_protocol_types::allocation::AllocationBudget::unowned();
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
                        .lower_retained(&retained, &retained_cache, &budget)
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
