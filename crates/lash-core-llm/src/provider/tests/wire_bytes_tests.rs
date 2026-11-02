//! Byte-identity laws for the canonical body every in-process model reads
//! (FIG-6443): incremental lowering sends what a cold lowering sends.

use super::*;
use lash_protocol_types::{
    llm::attachment_delivery::AttachmentPosition, llm::attachment_delivery::DeliveryForms,
};

use lash_protocol_types::{
    llm::wire_corpus::corpus_tool, llm::wire_corpus::digest_line, llm::wire_corpus::reuse_cases,
};

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
    use lash_protocol_types::{llm::types::RequestSegment, llm::types::TemplateError};

    use lash_protocol_types::{llm::wire_corpus::growing_history, llm::wire_corpus::text_only};

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
/// Lowering work follows the changed suffix, while every retained segment
/// and every admitted body's bytes survive growth, branching and cache loss.
#[test]
fn incremental_canonical_lowering_keeps_only_the_changed_suffix_work() {
    use lash_protocol_types::llm::types::RequestSegment;
    use lash_protocol_types::llm::wire_corpus::growing_history;

    let route = canonical_route();
    let history = growing_history(&route);
    let (_, mut request) = variants(history.clone()).remove(1);
    let cache = SegmentCache::new();
    let mut histories: Vec<_> = (1..=history.len())
        .map(|len| history[..len].to_vec())
        .collect();
    let mut branch = history[..5].to_vec();
    branch.push(LlmMessage::text(LlmRole::User, "A different branch."));
    histories.push(branch.clone());
    branch.push(LlmMessage::text(LlmRole::Assistant, "The branch's answer."));
    histories.push(branch);
    histories.push(history.clone());
    // These edits keep the blocks' allocation and change only another
    // part of the message's identity.
    let mut role = history.clone();
    role[2].role = LlmRole::Assistant;
    histories.push(role);
    let mut marker = history.clone();
    marker[2].starts_user_segment = !marker[2].starts_user_segment;
    histories.push(marker);
    histories.push(history[..3].to_vec());
    histories.push(history);

    let mut admitted = Vec::new();
    let mut previous: Option<(Vec<LlmMessage>, RecordedRequestTemplate)> = None;
    for (step, messages) in histories.into_iter().enumerate() {
        request.messages = messages;
        let before = cache.stats();
        let warm = RecordedRequestTemplate::of_request(route.clone(), &request, &cache).unwrap();
        let after = cache.stats();
        let cold =
            RecordedRequestTemplate::of_request(route.clone(), &request, &SegmentCache::disabled())
                .unwrap();
        let bytes = serde_json::to_vec(&warm).unwrap();
        assert!(
            bytes == serde_json::to_vec(&cold).unwrap(),
            "incremental lowering differs from full lowering at step {step}"
        );
        if let Some((messages, template)) = &previous {
            let shared = messages
                .iter()
                .zip(&request.messages)
                .take_while(|(a, b)| {
                    a.role == b.role
                        && a.starts_user_segment == b.starts_user_segment
                        && Arc::ptr_eq(&a.blocks, &b.blocks)
                })
                .count();
            for (a, b) in template
                .segments()
                .iter()
                .filter(|s| s.is_item())
                .zip(warm.segments().iter().filter(|s| s.is_item()))
                .take(shared)
            {
                assert!(
                    RequestSegment::same(a, b),
                    "the prefix retains its identity"
                );
            }
        }
        assert!(
            after.hits + after.misses - before.hits - before.misses <= 1,
            "one carried history lookup, independent of depth: {before:?} -> {after:?}"
        );
        // Admission owns an immutable body. Later lowering cannot rewrite
        // it, and reopening it needs neither this cache nor its source Arcs.
        admitted.push((warm.clone(), bytes));
        for (template, bytes) in &admitted {
            assert_eq!(&serde_json::to_vec(template).unwrap(), bytes);
            let reopened: RecordedRequestTemplate = serde_json::from_slice(bytes).unwrap();
            assert_eq!(&serde_json::to_vec(&reopened).unwrap(), bytes);
            assert_eq!(
                serde_json::to_value(reopened.canonical_request().unwrap()).unwrap(),
                serde_json::to_value(template.canonical_request().unwrap()).unwrap(),
            );
        }
        previous = Some((request.messages.clone(), warm));
    }
}
/// Growing a carried history cannot retain its oversized suffix outside
/// the byte budget. An admitted template still owns its literal segments.
#[test]
fn a_growing_canonical_history_respects_the_cache_byte_budget() {
    let route = canonical_route();
    let mut request = empty_request();
    request.messages = vec![
        LlmMessage::text(LlmRole::User, "first"),
        LlmMessage::text(LlmRole::Assistant, "second"),
    ];
    let cold =
        RecordedRequestTemplate::of_request(route.clone(), &request, &SegmentCache::disabled())
            .unwrap();
    let budget = cold
        .segments()
        .iter()
        .filter(|s| s.is_item())
        .map(|s| s.literal_len())
        .sum();
    let cache = SegmentCache::with_budget(budget);
    let retained = Arc::downgrade(&request.messages[1].blocks);
    RecordedRequestTemplate::of_request(route.clone(), &request, &cache).unwrap();
    request
        .messages
        .push(LlmMessage::text(LlmRole::User, "x".repeat(budget + 1)));
    let oversized = Arc::downgrade(&request.messages[2].blocks);
    let admitted = RecordedRequestTemplate::of_request(route, &request, &cache).unwrap();
    request.messages.clear();
    assert!(
        retained.upgrade().is_some(),
        "the bounded earlier prefix stays reusable"
    );
    assert!(
        oversized.upgrade().is_none(),
        "the oversized suffix is not retained"
    );
    assert_eq!(admitted.canonical_request().unwrap().messages.len(), 3);
    drop(cache);
    assert!(
        retained.upgrade().is_none(),
        "the cache owns and releases the source identities"
    );
}
