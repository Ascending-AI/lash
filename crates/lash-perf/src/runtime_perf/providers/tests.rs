use super::profiles::{empty_request, high_traffic_stream_profile, lashlang_block};
use super::tools::{
    GMAIL_LIKE_TOOL_NAMES, benchmark_oblique_search_tool_definition,
    benchmark_oblique_tool_definitions, oblique_search_output_schema,
};
use super::*;
use lash_core::{
    facade_support::build_tool_catalog,
    llm::types::{LlmMessage, LlmRole},
    test_support::ToolCatalogBuildInput,
};
use lash_lashlang_runtime::ToolManifestBindingExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn large_tool_catalog_fixture_matches_gmail_sized_callable_catalog() {
    let defs = BenchmarkLargeToolCatalog::build_tool_definitions();
    assert_eq!(defs.len(), 63);
    assert!(defs.iter().all(|def| {
        let binding = def
            .manifest
            .tool_binding()
            .expect("valid lashlang binding")
            .expect("benchmark tool has lashlang binding");
        binding.module_path == vec!["gmail".to_string()]
            && !def.contract.input_schema.canonical["properties"]
                .as_object()
                .expect("object schema")
                .is_empty()
    }));
    assert!(
        defs.iter()
            .any(|def| !def.contract.output_contract.is_static()),
        "fixture should cover dynamic output contracts"
    );
    let first = defs.first().expect("fixture tool");
    assert!(
        first.contract.input_schema.canonical["$defs"]["message_part"]["properties"]["parts"]
            ["items"]
            ["$ref"]
            .as_str()
            == Some("#/$defs/message_part"),
        "fixture should include recursive nested schema refs"
    );
    assert!(
        first.contract.input_schema.canonical["properties"]["payload"]["oneOf"]
            .as_array()
            .is_some_and(|variants| variants.len() >= 4),
        "fixture should include provider-style payload unions"
    );
    assert!(
        first.contract.input_schema.canonical["properties"]["projection"]["anyOf"]
            .as_array()
            .is_some_and(|variants| variants.len() >= 2),
        "fixture should include output projection unions"
    );
}

#[test]
fn rlm_large_tool_catalog_does_not_resolve_nested_schema_contracts_without_tool_calls() {
    let definitions = BenchmarkLargeToolCatalog::build_tool_definitions();
    let manifests = definitions
        .iter()
        .map(|definition| definition.manifest())
        .collect::<Vec<_>>();
    let contract_resolutions = Arc::new(AtomicUsize::new(0));
    let resolver_count = Arc::clone(&contract_resolutions);

    let surface = build_tool_catalog(ToolCatalogBuildInput {
        tools: manifests,
        resolve_contract: Some(Arc::new(move |name| {
            resolver_count.fetch_add(1, Ordering::SeqCst);
            definitions
                .iter()
                .find(|definition| definition.name() == name)
                .map(|definition| Arc::new(definition.contract()))
        })),
        contributions: Vec::new(),
    });

    // Every member is callable under the flat catalog; building the
    // catalog resolves no contracts (rendering is lazy and protocol-owned).
    assert_eq!(surface.callable_tools().len(), GMAIL_LIKE_TOOL_NAMES.len());
    assert_eq!(contract_resolutions.load(Ordering::SeqCst), 0);
}

#[test]
fn oblique_fixture_exposes_retrieval_judge_and_handle_tools_to_lashlang() {
    let definitions = benchmark_oblique_tool_definitions();
    assert_eq!(definitions.len(), 3);
    let bindings = definitions
        .iter()
        .map(|definition| {
            let binding = definition
                .manifest
                .tool_binding()
                .expect("valid lashlang binding")
                .expect("oblique fixture tool has lashlang binding");
            (
                definition.name().to_string(),
                binding.module_path,
                binding.operation,
            )
        })
        .collect::<Vec<_>>();
    assert!(bindings.contains(&(
        "oblique_search".to_string(),
        vec!["obliq".to_string()],
        Some("search".to_string())
    )));
    assert!(bindings.contains(&(
        "oblique_judge_candidates".to_string(),
        vec!["obliq".to_string()],
        Some("judge_candidates".to_string())
    )));
    assert!(bindings.contains(&(
        "oblique_list_async_handles".to_string(),
        vec!["obliq".to_string()],
        Some("list_async_handles".to_string())
    )));
    let search_contract = benchmark_oblique_search_tool_definition().contract;
    assert!(search_contract.output_contract.is_static());
    assert_eq!(
        search_contract.output_schema.canonical,
        oblique_search_output_schema()
    );
}

#[test]
fn streamed_paired_lashlang_profile_splits_tags_and_trailing_suffix() {
    let profile = benchmark_stream_profile(RuntimePerfScenario::RlmStreamedPairedLashlang);
    assert_eq!(profile.full_text.matches("<lashlang>").count(), 1);
    assert_eq!(profile.full_text.matches("</lashlang>").count(), 1);
    assert!(
        profile
            .full_text
            .ends_with("This suffix must be ignored after the close tag.")
    );
    assert_eq!(profile.deltas.len(), 4);
    assert!(profile.deltas[0].ends_with("<lash"));
    assert!(profile.deltas[1].starts_with("lang>"));
    assert!(profile.deltas[2].ends_with("</lash"));
    assert!(profile.deltas[3].starts_with("lang>"));
    assert!(profile.deltas[3].contains("This suffix must be ignored"));
    assert!(profile.parts.is_empty());
}

#[test]
fn ingress_claim_projection_profile_uses_latest_request_item_marker() {
    let mut unmarked_request = empty_request();
    unmarked_request
        .messages
        .push(request_input("continue the ingress projection"));
    unmarked_request.messages.push(LlmMessage::text(
        LlmRole::User,
        "synthetic current iteration suffix",
    ));
    let unmarked_profile = benchmark_stream_profile_for_request(
        RuntimePerfScenario::IngressClaimProjection,
        &unmarked_request,
    );
    assert_eq!(
        unmarked_profile.full_text,
        lashlang_block(r#"print("checkpoint before projection")"#)
    );

    let mut marked_request = empty_request();
    marked_request
        .messages
        .push(request_input("continue with the ingress projection marker"));
    marked_request.messages.push(LlmMessage::text(
        LlmRole::User,
        "synthetic current iteration suffix",
    ));
    let marked_profile = benchmark_stream_profile_for_request(
        RuntimePerfScenario::IngressClaimProjection,
        &marked_request,
    );
    assert_eq!(
        marked_profile.full_text,
        lashlang_block(r#"finish "runtime perf benchmark ok""#)
    );

    let mut historical_marker_request = marked_request;
    historical_marker_request
        .messages
        .push(request_input("start the next ingress projection turn"));
    historical_marker_request.messages.push(LlmMessage::text(
        LlmRole::User,
        "synthetic current iteration suffix",
    ));
    let historical_marker_profile = benchmark_stream_profile_for_request(
        RuntimePerfScenario::IngressClaimProjection,
        &historical_marker_request,
    );
    assert_eq!(
        historical_marker_profile.full_text,
        lashlang_block(r#"print("checkpoint before projection")"#)
    );
}

#[test]
fn high_traffic_trigger_registration_is_session_scoped() {
    let mut request = empty_request();
    request.messages.push(request_input(
        "load-kind:trigger operation:7 session:runtime-perf-session-a",
    ));
    let profile = high_traffic_stream_profile(&request);

    assert!(
        profile
            .full_text
            .contains("runtime-perf-load-trigger-runtime-perf-session-a")
    );
    assert!(
        !profile
            .full_text
            .contains("name: \"runtime-perf-load-trigger\"")
    );
}

fn request_input(text: &str) -> LlmMessage {
    LlmMessage::new(
        LlmRole::User,
        vec![LlmContentBlock::Text {
            text: text.into(),
            response_meta: None,
            cache_breakpoint: true,
        }],
    )
}
