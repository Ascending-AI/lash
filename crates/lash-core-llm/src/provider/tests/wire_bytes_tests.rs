//! The byte-identity law for the canonical body every in-process model
//! reads (FIG-6443): every corpus history lowers to the body recorded
//! before lowering became incremental.

use super::*;
use lash_sansio::llm::attachment_delivery::{AttachmentPosition, DeliveryForms};
use lash_sansio::llm::wire_corpus::{corpus_tool, digest_line, reuse_cases, wire_corpus};

/// Digests recorded from the coalescing lowering at de7f61e3dc.
const RECORDED: &str = "
base/prefix-01 4133e8067438e2dc3b415fe4
instructed/prefix-01 d2feb7eea32bceb31f205d14
base/prefix-02 925bc853bab63ca57a477235
instructed/prefix-02 3714360bcde2c07f03bd4f0a
base/prefix-03 afac72799f7316f83050e74e
instructed/prefix-03 8574c27a883a6ed19b763f3f
base/prefix-04 9e5c3a2f91c0cf5d68979dd9
instructed/prefix-04 c6032da82bdae6f205ecd374
base/prefix-05 ef5ae301d7c3e1654e95ce4d
instructed/prefix-05 824641ff05e27be2b63807d2
base/prefix-06 5bc7a5d68069a4bc76caa382
instructed/prefix-06 c37f7483febcea80c428c09f
base/prefix-07 3baaa413d8f65000e5be5a55
instructed/prefix-07 5d796238f8d0839ead57ac99
base/prefix-08 494c1478ecbf564377cc2ffe
instructed/prefix-08 1bf939949769a9f3f823cd47
base/prefix-09 10759c578839777eeb1b26a5
instructed/prefix-09 b6c53f87419255efde2dd916
base/prefix-10 1b4d1b532a5b4c77dc57a7ba
instructed/prefix-10 0587b6a4e98400d906593a02
base/prefix-11 257742b95e4b5c5120fcc059
instructed/prefix-11 982991ac5b1725602fcd221c
base/prefix-12 3222f6b29e9be1394115cbac
instructed/prefix-12 a69a66e02bd0adadd0db9236
base/prefix-13 46605cd8ddfd16bff8ce06ac
instructed/prefix-13 7dcf2f5f2d0eee1940ec1832
base/prefix-14 5f40860a4583c046f79bcca8
instructed/prefix-14 fcba5490a8bca0cd14a2f60f
base/empty-history 86d1ec655f1b6b3983bdfa06
instructed/empty-history 220c07ef000111e0705574ec
base/only-feedback 7b1485bc41a8ff38059e47fc
instructed/only-feedback 66591b5f0d595e1fd6b6696a
base/unicode 273876af7434c5f80fabfdf8
instructed/unicode cd13926e7637fb3edc4c5e36
";

fn canonical_route() -> ProviderRouteIdentity {
    ProviderRouteIdentity::for_endpoint("canonical", "https://canonical.example/v1", "model")
}

fn variants(messages: Vec<LlmMessage>) -> Vec<(&'static str, LlmRequest)> {
    let mut base = empty_request();
    base.messages = messages;
    base.attachment_acceptance = Arc::new(crate::provider::AttachmentCapabilitySnapshot {
        revision: "corpus".into(),
        acceptors: vec![crate::provider::AttachmentAcceptor {
            provider: "canonical".into(),
            rules: vec![crate::provider::AttachmentAcceptanceRule {
                positions: vec![AttachmentPosition::Message, AttachmentPosition::ToolResult],
                media_types: vec!["image/png".into()],
                media_families: vec![],
                forms: DeliveryForms {
                    bytes: true,
                    url: true,
                    provider_file: true,
                },
            }],
        }],
    });
    let mut instructed = base.clone();
    instructed.instructions = Some("You are a careful coding agent.".into());
    instructed.tools = Arc::new(vec![corpus_tool()]);
    instructed.tool_choice = LlmToolChoice::Auto;
    vec![("base", base), ("instructed", instructed)]
}

#[test]
fn canonical_lowering_sends_the_recorded_bytes() {
    let mut lines = Vec::new();
    for (name, messages) in wire_corpus(&canonical_route()) {
        for (variant, request) in variants(messages) {
            let lowered = RecordedRequestTemplate::of_request(
                canonical_route(),
                &request,
                &SegmentCache::disabled(),
            );
            lines.push(digest_line(
                &format!("{variant}/{name}"),
                lowered.as_ref().map_err(ToString::to_string),
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

fn digest(template: &RecordedRequestTemplate) -> String {
    digest_line("", Ok(template))
}

/// Lowering through one cache sends what a cold lowering sends, across
/// growth, an edited message, compaction, and a switch of route or
/// attachment acceptance: a reused segment never carries stale content.
#[test]
fn a_reused_prefix_sends_the_bytes_a_cold_lowering_sends() {
    let cache = SegmentCache::new();
    let other_route = ProviderRouteIdentity::for_endpoint(
        "canonical",
        "https://canonical-b.example/v1",
        "model-b",
    );
    for (case, histories) in reuse_cases(&canonical_route()) {
        for (step, messages) in histories.into_iter().enumerate() {
            for (variant, mut request) in variants(messages) {
                for route in [canonical_route(), other_route.clone()] {
                    let warm = RecordedRequestTemplate::of_request(route.clone(), &request, &cache)
                        .unwrap();
                    let cold = RecordedRequestTemplate::of_request(
                        route,
                        &request,
                        &SegmentCache::disabled(),
                    )
                    .unwrap();
                    assert_eq!(digest(&warm), digest(&cold), "{case} step {step} {variant}");
                }
                // A narrower acceptance is another context: the slots it
                // pins accept less.
                Arc::make_mut(&mut request.attachment_acceptance).acceptors[0].rules[0]
                    .forms
                    .url = false;
                let warm = RecordedRequestTemplate::of_request(canonical_route(), &request, &cache)
                    .unwrap();
                let cold = RecordedRequestTemplate::of_request(
                    canonical_route(),
                    &request,
                    &SegmentCache::disabled(),
                )
                .unwrap();
                assert_eq!(digest(&warm), digest(&cold), "{case} step {step} narrowed");
            }
        }
    }
    let stats = cache.stats();
    assert!(stats.hits > stats.misses, "{stats:?}");
}

/// The custom-plugin law: a plugin that rewrites the whole body as one
/// value, and one that maps, replaces, inserts and drops segments, each
/// sends the request it means, byte for byte what lowering that request
/// sends, and the body reads back as that request.
#[test]
fn a_custom_plugin_sends_the_request_it_means() {
    use lash_sansio::llm::types::{RequestSegment, TemplateError};
    use lash_sansio::llm::wire_corpus::{growing_history, text_only};

    let route = canonical_route();
    let cache = SegmentCache::new();
    let (_, request) = variants(text_only(growing_history(&route))).remove(1);
    let lowered = RecordedRequestTemplate::of_request(route.clone(), &request, &cache).unwrap();
    let sends = |meant: &LlmRequest, template: &RecordedRequestTemplate| {
        let cold =
            RecordedRequestTemplate::of_request(route.clone(), meant, &SegmentCache::disabled())
                .unwrap();
        assert_eq!(digest(template), digest(&cold));
        assert_eq!(
            serde_json::to_value(template.canonical_request().unwrap()).unwrap(),
            serde_json::to_value(meant).unwrap(),
        );
    };

    // A whole-body transformer reads the body as one tree and rewrites it.
    let mut tree = lowered.to_json().unwrap();
    tree.set("instructions", serde_json::json!("Rewritten by a plugin."));
    let whole = RecordedRequestTemplate::from_json::<TemplateError>(
        route.clone(),
        lowered.stream(),
        lowered.generation,
        &tree,
        &mut |_, _| Err(TemplateError::InvalidJson),
    )
    .unwrap();
    let mut meant = request.clone();
    meant.instructions = Some("Rewritten by a plugin.".into());
    sends(&meant, &whole);
    assert_eq!(
        whole.segments().len(),
        1,
        "a whole-body rewrite opts out of reuse"
    );

    // A segment mapper replaces one message, drops the first and appends one.
    let item = |message: &LlmMessage| {
        RequestSegment::item_json(&serde_json::to_value(message).unwrap()).unwrap()
    };
    let items: Vec<usize> = (0..lowered.segments().len())
        .filter(|&index| lowered.segments()[index].is_item())
        .collect();
    let replaced = LlmMessage::text(LlmRole::User, "A plugin rewrote this message.");
    let appended = LlmMessage::text(LlmRole::User, "A plugin appended this message.");
    let mapped = lowered
        .clone()
        .edit_segments(|segments| {
            segments[items[3]] = item(&replaced);
            segments.insert(items[items.len() - 1] + 1, item(&appended));
            segments.remove(items[0]);
        })
        .unwrap();
    let mut meant = request.clone();
    meant.messages[3] = replaced;
    meant.messages.push(appended);
    meant.messages.remove(0);
    sends(&meant, &mapped);
    assert!(
        RequestSegment::same(
            &mapped.segments()[items[1] - 1],
            &lowered.segments()[items[1]]
        ),
        "a segment the mapper kept stays shared"
    );
}
