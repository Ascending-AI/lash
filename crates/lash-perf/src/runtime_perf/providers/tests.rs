use super::profiles::{empty_request, typescript_block};
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
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn large_tool_catalog_fixture_matches_gmail_sized_callable_catalog() {
    let defs = BenchmarkLargeToolCatalog::build_tool_definitions();
    assert_eq!(defs.len(), 63);
    assert!(defs.iter().all(|def| {
        def.manifest.tool().starts_with("gmail.")
            && !def.contract.input_schema.canonical.as_value()["properties"]
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
        first.contract.input_schema.canonical.as_value()["$defs"]["message_part"]["properties"]["parts"]
            ["items"]
            ["$ref"]
            .as_str()
            == Some("#/$defs/message_part"),
        "fixture should include recursive nested schema refs"
    );
    assert!(
        first.contract.input_schema.canonical.as_value()["properties"]["payload"]["oneOf"]
            .as_array()
            .is_some_and(|variants| variants.len() >= 4),
        "fixture should include provider-style payload unions"
    );
    assert!(
        first.contract.input_schema.canonical.as_value()["properties"]["projection"]["anyOf"]
            .as_array()
            .is_some_and(|variants| variants.len() >= 2),
        "fixture should include output projection unions"
    );
}

#[test]
fn rlm_large_tool_catalog_pins_each_contract_once() {
    let definitions = BenchmarkLargeToolCatalog::build_tool_definitions();
    let manifests = definitions
        .iter()
        .map(|definition| definition.manifest())
        .collect::<Vec<_>>();
    let contract_resolutions = Arc::new(AtomicUsize::new(0));
    let resolver_count = Arc::clone(&contract_resolutions);

    let surface = build_tool_catalog(ToolCatalogBuildInput {
        tools: manifests,
        resolve_contract: Some(Arc::new(move |manifest| {
            resolver_count.fetch_add(1, Ordering::SeqCst);
            definitions
                .iter()
                .find(|definition| definition.manifest.id == manifest.id)
                .map(|definition| Arc::new(definition.contract()))
        })),
        contributions: Vec::new(),
        isolation_engines: std::collections::BTreeSet::new(),
    })
    .expect("complete resident definitions");

    // Provider-side resolution remains lazy until the effective resident set
    // is known, then every surviving member is pinned exactly once.
    assert_eq!(surface.callable_tools().len(), GMAIL_LIKE_TOOL_NAMES.len());
    assert_eq!(
        contract_resolutions.load(Ordering::SeqCst),
        GMAIL_LIKE_TOOL_NAMES.len()
    );
}

#[test]
fn oblique_fixture_exposes_retrieval_judge_and_handle_tools_to_lash_vm() {
    let definitions = benchmark_oblique_tool_definitions();
    assert_eq!(definitions.len(), 3);
    let operations = definitions
        .iter()
        .map(|definition| definition.manifest.tool())
        .collect::<Vec<_>>();
    for operation in [
        "obliq.search",
        "obliq.judge_candidates",
        "obliq.list_async_handles",
    ] {
        assert!(
            operations.iter().any(|name| name == operation),
            "{operations:?}"
        );
    }
    let search_contract = benchmark_oblique_search_tool_definition().contract;
    assert!(search_contract.output_contract.is_static());
    assert_eq!(
        search_contract.output_schema.canonical().clone(),
        oblique_search_output_schema()
    );
}

#[test]
fn ingress_admission_projection_profile_uses_latest_request_item_marker() {
    let mut unmarked_request = empty_request();
    unmarked_request
        .messages
        .push(request_input("continue the ingress projection"));
    unmarked_request.messages.push(LlmMessage::text(
        LlmRole::User,
        "synthetic current iteration suffix",
    ));
    let unmarked_profile = benchmark_stream_profile_for_request(
        RuntimePerfScenario::IngressAdmissionProjection,
        &unmarked_request,
    );
    assert_eq!(
        unmarked_profile.parts,
        typescript_block(r#"console.log("checkpoint before projection");"#).parts
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
        RuntimePerfScenario::IngressAdmissionProjection,
        &marked_request,
    );
    assert_eq!(
        marked_profile.parts,
        typescript_block(r#"await control.finish("runtime perf benchmark ok");"#).parts
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
        RuntimePerfScenario::IngressAdmissionProjection,
        &historical_marker_request,
    );
    assert_eq!(
        historical_marker_profile.parts,
        typescript_block(r#"console.log("checkpoint before projection");"#).parts
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

#[test]
fn delegated_child_task_routing_ignores_parent_spawn_history() {
    const TASK: &str = "Submit `{ len: chunk.length }` using the seeded `chunk` variable.";
    for scenario in [
        RuntimePerfScenario::DurableAgentChildTurnSqlite,
        RuntimePerfScenario::HighTrafficLoadSqlite,
        RuntimePerfScenario::HighTrafficKneeSqlite,
    ] {
        let mut child = empty_request();
        // A neutral child prompt has no delegation instructions or cache fence.
        child.messages.push(LlmMessage::text(LlmRole::User, TASK));
        let child_profile = benchmark_stream_profile_for_request(scenario, &child);
        assert_eq!(
            child_profile.parts,
            typescript_block("await control.finish({ len: chunk.length });").parts
        );

        let mut parent = empty_request();
        parent
            .messages
            .push(request_input("load-kind:child operation:0"));
        parent.messages.push(LlmMessage::text(
            LlmRole::Assistant,
            format!("const result = await agents.spawn({{ task: {TASK:?} }});"),
        ));
        parent
            .messages
            .push(request_input("load-kind:child operation:1"));
        let parent_profile = benchmark_stream_profile_for_request(scenario, &parent);
        assert!(
            parent_profile.parts.iter().any(|part| matches!(part, LlmOutputPart::ToolCall { input_json, .. } if input_json.contains("agents.spawn"))),
            "{} routed the parent's next turn as its child",
            scenario.name()
        );
    }
}
