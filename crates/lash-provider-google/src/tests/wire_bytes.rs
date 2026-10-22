//! The byte-identity law for Google Gemini (FIG-6443): every corpus
//! history, under each dialect, lowers to the body recorded before lowering
//! became incremental.

use super::*;
use lash_core::llm::types::SegmentCache;
use lash_protocol_types::{
    llm::wire_corpus::corpus_tool, llm::wire_corpus::digest_line, llm::wire_corpus::reuse_cases,
    llm::wire_corpus::wire_corpus,
};

/// Digests recorded from the coalescing lowering at de7f61e3dc.
const RECORDED: &str = "
base/prefix-01 f6fa9394602aa8ebb0355538
instructed/prefix-01 d94f15d6595de8b9db3904db
gemini3/prefix-01 f6fa9394602aa8ebb0355538
vertex-claude/prefix-01 f6fa9394602aa8ebb0355538
base/prefix-02 0ee001e9c04bdb94e81cb8f0
instructed/prefix-02 600235f5e8ff7556d2497f8b
gemini3/prefix-02 212a2cd686a2c16fd89b3c78
vertex-claude/prefix-02 0ee001e9c04bdb94e81cb8f0
base/prefix-03 085cdd283e9c181c4c6f61cd
instructed/prefix-03 2ecfea39ebe9723715836de9
gemini3/prefix-03 438590854878092a535edd13
vertex-claude/prefix-03 085cdd283e9c181c4c6f61cd
base/prefix-04 4446919a2e87c870ca1504de
instructed/prefix-04 407151390a1127ea917eaef9
gemini3/prefix-04 0161a40e38ede24ee9abfefc
vertex-claude/prefix-04 4446919a2e87c870ca1504de
base/prefix-05 be5e7ff629768c40e3c2876b
instructed/prefix-05 086628c5c52c88b8456f2502
gemini3/prefix-05 005a28cd23a3066d96864faa
vertex-claude/prefix-05 be5e7ff629768c40e3c2876b
base/prefix-06 d41b4ec458e6379d8aa5de0f
instructed/prefix-06 5275bffb95d349e364b374fd
gemini3/prefix-06 19039e2c6b2490a2b6b27cc7
vertex-claude/prefix-06 d41b4ec458e6379d8aa5de0f
base/prefix-07 fa9e4704fa006b3ea7f08483
instructed/prefix-07 a20c467ad81c49fd8bf7056a
gemini3/prefix-07 d64a1014cd8d1099929bde34
vertex-claude/prefix-07 fa9e4704fa006b3ea7f08483
base/prefix-08 bbf118f1be7bcc96bd367ec8
instructed/prefix-08 b3f39aa0b2ab8c2db47f5b99
gemini3/prefix-08 bb7ec2d79465153db1bbbe1c
vertex-claude/prefix-08 bbf118f1be7bcc96bd367ec8
base/prefix-09 586df3414cc1d77b35e1b195
instructed/prefix-09 d4b0889ce858dc4f83b8e86a
gemini3/prefix-09 bf007183d5bd9860b71b674f
vertex-claude/prefix-09 586df3414cc1d77b35e1b195
base/prefix-10 253a96e60ae424ef8eb2decd
instructed/prefix-10 dfe87a136f4256fb8a2d72ce
gemini3/prefix-10 d71a35a55e5d6ab446a66821
vertex-claude/prefix-10 253a96e60ae424ef8eb2decd
base/prefix-11 544d2371b0c5bbbf18007fa0
instructed/prefix-11 f77eb33a560a74fc97392484
gemini3/prefix-11 fb2f2c8b956a83b0dfe29c31
vertex-claude/prefix-11 544d2371b0c5bbbf18007fa0
base/prefix-12 5ef5e2c8da1400d3161d92bd
instructed/prefix-12 518f53627cd194d2cab999a4
gemini3/prefix-12 829d98b14527a23a784de5df
vertex-claude/prefix-12 5ef5e2c8da1400d3161d92bd
base/prefix-13 59494d7ec4ef6b23a690b0e3
instructed/prefix-13 a5f9233b63dde6772928f815
gemini3/prefix-13 e3ccecd53fcbe62c7f2f8737
vertex-claude/prefix-13 59494d7ec4ef6b23a690b0e3
base/prefix-14 e281641c097c84eb4e47214e
instructed/prefix-14 fbf1124164df7707f39645b1
gemini3/prefix-14 ad286c08dcefa70eeb05b289
vertex-claude/prefix-14 e281641c097c84eb4e47214e
base/empty-history 622141ab56d0d51b0062446c
instructed/empty-history dcb0ddb09277df0aff24c5bb
gemini3/empty-history 622141ab56d0d51b0062446c
vertex-claude/empty-history 622141ab56d0d51b0062446c
base/only-feedback d5361551e373d366d43ee9d4
instructed/only-feedback ee7517a9d8ebb5a24d6d0123
gemini3/only-feedback d5361551e373d366d43ee9d4
vertex-claude/only-feedback d5361551e373d366d43ee9d4
base/unicode 3c021a3ba3bc90d95f43dc11
instructed/unicode 26edabbbcaf1f0d0fd4a7c2e
gemini3/unicode 3c021a3ba3bc90d95f43dc11
vertex-claude/unicode 3c021a3ba3bc90d95f43dc11
";

fn variants(messages: Vec<LlmMessage>) -> Vec<(&'static str, LlmRequest)> {
    let base = LlmRequest {
        messages,
        ..request(None)
    };
    let mut instructed = base.clone();
    instructed.instructions = Some("You are a careful coding agent.".into());
    instructed.tools = Arc::new((vec![corpus_tool()]).into());
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

#[tokio::test]
async fn lowering_sends_the_recorded_bytes() {
    let mut provider =
        GoogleOAuthProvider::new(Arc::new(lash_core::provider::ProviderToken::new("access")))
            .with_project_id(Some("wire-corpus-project".into()));
    let route = provider.route_identity("gemini-3.1-pro-preview");
    // One cache across the corpus: each prefix reuses the contents the
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
/// compacted history, another dialect and another file scope each lower to
/// the bytes a cold lowering sends.
#[tokio::test]
async fn a_reused_prefix_sends_the_bytes_a_cold_lowering_sends() {
    let token = || Arc::new(lash_core::provider::ProviderToken::new("access"));
    let cache = SegmentCache::new();
    let retained_cache = SegmentCache::new();
    let budget = lash_protocol_types::allocation::AllocationBudget::unowned();
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
                    let persistent = provider
                        .lower_retained(&retained, &retained_cache, &budget)
                        .await;
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
