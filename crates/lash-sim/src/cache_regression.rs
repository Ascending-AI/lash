use std::sync::Arc;

use lash_core::llm::types::{LlmContentBlock, LlmMessage, LlmRequest, LlmRole};
use lash_core::provider::{CacheControlDialect, CacheRetention};
use lash_llm_transport::cache_regression::{
    SerializedPromptRequest, assert_prefix_stability, strip_cache_directives,
};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProtocolKind {
    Standard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProviderSerializer {
    ChatAnthropicDialect,
    ChatGeminiDialect,
    AnthropicDirect,
    GoogleDirect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CacheWireForm {
    Nothing,
    CacheControl,
    CacheControlOneHour,
    PromptCacheKey,
    PromptCacheKeyOneDay,
}

#[derive(Clone, Copy, Debug)]
enum CoveragePath {
    ChatAnthropicDialect,
    ChatGeminiDialect,
    ChatNoDialect,
    AnthropicDirect,
    GoogleDirect,
    OpenAiResponses,
    CodexResponses,
}

#[derive(Clone, Copy, Debug)]
struct CoverageCase {
    path: CoveragePath,
    retention: CacheRetention,
    expected: CacheWireForm,
}

fn text_block(text: &str, cache_breakpoint: bool) -> LlmContentBlock {
    LlmContentBlock::Text {
        text: text.into(),
        response_meta: None,
        cache_breakpoint,
    }
}

fn request(model: &str, messages: Vec<LlmMessage>) -> LlmRequest {
    LlmRequest {
        instructions: Some(Arc::from("stable system")),
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_sansio::llm_profile::LlmProfileMetadata::builder(model.to_string())
                    .context_window_tokens(128_000)
                    .capability(Default::default())
                    .extra_body(Default::default())
                    .request_defaults(Default::default())
                    .build()
                    .expect("valid profile"),
            ),
        )
        .with_reasoning(Default::default()),
        messages,
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: Default::default(),
        attachment_acceptance: Default::default(),
        scope: lash_core::LlmRequestScope::new(
            "cache-regression-session",
            "cache-regression-frame",
            "cache-regression-request",
        ),
        output_spec: None,
        stream_events: None,
        generation: Default::default(),
        provider_trace: None,
    }
}

fn with_cache_control(mut request: LlmRequest, dialect: CacheControlDialect) -> LlmRequest {
    request.model.metadata_mut().capability.cache_control = Some(dialect);
    request
}

fn standard_iterations(model: &str) -> Vec<LlmRequest> {
    let first = vec![
        LlmMessage::text(LlmRole::User, "solve the task"),
        LlmMessage::text(LlmRole::System, "stable runtime feedback"),
    ];
    let mut second = first.clone();
    second.extend([
        LlmMessage::text(LlmRole::Assistant, "called lookup"),
        LlmMessage::text(LlmRole::User, "lookup result: one"),
    ]);
    let mut third = second.clone();
    third.extend([
        LlmMessage::text(LlmRole::Assistant, "called lookup again"),
        LlmMessage::text(LlmRole::User, "lookup result: two"),
    ]);
    vec![
        request(model, first),
        request(model, second),
        request(model, third),
    ]
}

async fn captured_rlm_iterations() -> Vec<LlmRequest> {
    captured_rlm_requests(
        lash_protocol_rlm::RlmExecutionPolicy::Chronological,
        &[
            "let value = 1;\nprint(value);",
            "value = value + 1;\nprint(value);",
            "finish(value);",
        ],
        Arc::new(|session: &lash::LashSession| {
            session
                .send(lash::TurnInput::text("increment a bound value twice"))
                .require_finish()
        }),
    )
    .await
}

/// Four relay steps of one turn: an empty start, a first commit, an append
/// and a rewrite of the first entry.
async fn captured_relay_steps() -> Vec<LlmRequest> {
    captured_rlm_requests(
        lash_protocol_rlm::RlmExecutionPolicy::Relay,
        &[
            r#"await control.next({ context: ["a", "b"] });"#,
            r#"await control.next({ context: [...context, "c"] });"#,
            r#"await control.next({ context: ["x", ...context.slice(1)] });"#,
            r#"await control.send_user_output({ text: "done" });
await control.next({ context, final: true });"#,
        ],
        Arc::new(|session: &lash::LashSession| {
            Ok(session.send(lash::TurnInput::text("edit the context")))
        }),
    )
    .await
}

/// Every request one scripted RLM turn under `policy` makes; the model
/// answers the `cells` in order.
async fn captured_rlm_requests(
    policy: lash_protocol_rlm::RlmExecutionPolicy,
    cells: &[&str],
    build: crate::backend::SimTurnBuild,
) -> Vec<LlmRequest> {
    use std::collections::VecDeque;

    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let responses = Arc::new(tokio::sync::Mutex::new(
        cells
            .iter()
            .map(|cell| format!("<typescript>\n{cell}\n</typescript>"))
            .collect::<VecDeque<_>>(),
    ));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("cache-regression-rlm")
        .complete({
            let captures = Arc::clone(&captures);
            move |request| {
                let captures = Arc::clone(&captures);
                let responses = Arc::clone(&responses);
                async move {
                    captures.lock_recover().push(request);
                    let text = responses
                        .lock()
                        .await
                        .pop_front()
                        .expect("RLM response script");
                    Ok(lash_core::LlmResponse {
                        parts: vec![lash_core::LlmOutputPart::Text {
                            text,
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..lash_core::LlmResponse::default()
                    })
                }
            }
        })
        .build()
        .into_handle();
    let engine = crate::backend::SimEngine::new(0x5eed_7004)
        .await
        .expect("sim engine");
    let backend = engine.backend();
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build()
            .with_execution_policy(policy),
        std::sync::Arc::new(lash_protocol_rlm::TypescriptDialect),
        &backend,
    );
    let core = lash::LashCore::rlm_builder(backend, factory)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .serve_test_llm_profile(
            provider,
            lash_core::LlmProfileMetadata::builder("cache-regression-model")
                .context_window_tokens(200_000)
                .build()
                .expect("cache regression model"),
        )
        .build(crate::sim_process_owner())
        .expect("RLM cache regression core");
    let session =
        crate::open_created_session("cache-regression-model", &core, "cache-regression-session")
            .await
            .expect("RLM cache regression session");
    engine
        .run_turn(
            &session,
            "cache-regression-turn",
            Arc::new(crate::backend::DiscardedTurnActivity),
            build,
        )
        .await
        .expect("RLM cache regression handler")
        .expect("RLM cache regression turn");

    captures.lock_recover().clone()
}

fn prefix_for_openai_chat(body: Value, stable_messages: usize) -> SerializedPromptRequest {
    let mut stable_prefix = Value::Array(
        body["messages"]
            .as_array()
            .expect("OpenAI-compatible messages")
            .iter()
            .take(stable_messages)
            .cloned()
            .collect(),
    );
    strip_cache_directives(&mut stable_prefix);
    SerializedPromptRequest {
        body,
        stable_prefix,
    }
}

fn prefix_for_anthropic(body: Value, prefix_body: &Value) -> SerializedPromptRequest {
    let mut stable_prefix = json!({
        "system": prefix_body.get("system").cloned(),
        "messages": prefix_body["messages"]
            .as_array()
            .expect("Anthropic messages")
            .to_vec(),
    });
    strip_cache_directives(&mut stable_prefix);
    SerializedPromptRequest {
        body,
        stable_prefix,
    }
}

fn prefix_for_google(body: Value, prefix_body: &Value) -> SerializedPromptRequest {
    let wire_request = &prefix_body["request"];
    let stable_prefix = json!({
        "systemInstruction": wire_request.get("systemInstruction").cloned(),
        "contents": wire_request["contents"]
            .as_array()
            .expect("Google contents")
            .to_vec(),
    });
    SerializedPromptRequest {
        body,
        stable_prefix,
    }
}

fn serialize_prefix(
    serializer: ProviderSerializer,
    request: &LlmRequest,
    stable_messages: usize,
) -> SerializedPromptRequest {
    match serializer {
        ProviderSerializer::ChatAnthropicDialect | ProviderSerializer::ChatGeminiDialect => {
            let dialect = match serializer {
                ProviderSerializer::ChatAnthropicDialect => CacheControlDialect::Anthropic,
                ProviderSerializer::ChatGeminiDialect => CacheControlDialect::Gemini,
                _ => unreachable!(),
            };
            let request = with_cache_control(request.clone(), dialect);
            let (body, _) = lash_provider_openai::testing::serialize_chat_request(
                &request,
                CacheRetention::Short,
            )
            .expect("OpenAI-compatible Chat request");
            prefix_for_openai_chat(
                body,
                stable_messages + usize::from(request.instructions.is_some()),
            )
        }
        ProviderSerializer::AnthropicDirect => {
            let body =
                lash_provider_anthropic::testing::serialize_request(request, CacheRetention::Short)
                    .expect("Anthropic request");
            let mut prefix_request = request.clone();
            prefix_request.messages.truncate(stable_messages);
            let prefix_body = lash_provider_anthropic::testing::serialize_request(
                &prefix_request,
                CacheRetention::Short,
            )
            .expect("Anthropic prefix request");
            prefix_for_anthropic(body, &prefix_body)
        }
        ProviderSerializer::GoogleDirect => {
            let body =
                lash_provider_google::testing::serialize_request(request, CacheRetention::Short)
                    .expect("Google schema projection");
            let mut prefix_request = request.clone();
            prefix_request.messages.truncate(stable_messages);
            let prefix_body = lash_provider_google::testing::serialize_request(
                &prefix_request,
                CacheRetention::Short,
            )
            .expect("Google schema projection");
            prefix_for_google(body, &prefix_body)
        }
    }
}

#[test]
fn prefix_stability_matrix_runs_consecutive_protocol_iterations() {
    let cases = [
        (ProtocolKind::Standard, ProviderSerializer::AnthropicDirect),
        (ProtocolKind::Standard, ProviderSerializer::GoogleDirect),
    ];

    for (protocol, serializer) in cases {
        let model = match serializer {
            ProviderSerializer::ChatAnthropicDialect | ProviderSerializer::AnthropicDirect => {
                "anthropic/claude-sonnet-4.6"
            }
            ProviderSerializer::ChatGeminiDialect | ProviderSerializer::GoogleDirect => {
                "google/gemini-3.1-pro-preview"
            }
        };
        let iterations = standard_iterations(model);
        let case_name = format!("{protocol:?} x {serializer:?}");
        assert_prefix_stability(&case_name, &iterations, |request, stable_messages| {
            serialize_prefix(serializer, request, stable_messages)
        });
    }
}

#[test]
fn chat_cache_dialect_prefix_shape_is_stable_as_breakpoints_roll() {
    for serializer in [
        ProviderSerializer::ChatAnthropicDialect,
        ProviderSerializer::ChatGeminiDialect,
    ] {
        let model = match serializer {
            ProviderSerializer::ChatAnthropicDialect => "custom/model-a",
            ProviderSerializer::ChatGeminiDialect => "custom/model-b",
            _ => unreachable!(),
        };
        let iterations = standard_iterations(model);
        assert_prefix_stability(
            &format!("{:?} x {serializer:?}", ProtocolKind::Standard),
            &iterations,
            |request, stable_messages| serialize_prefix(serializer, request, stable_messages),
        );
    }
}

fn cache_request(model: &str) -> LlmRequest {
    request(
        model,
        vec![
            LlmMessage::new(LlmRole::User, vec![text_block("stable history", true)]),
            LlmMessage::text(LlmRole::User, "volatile tail"),
        ],
    )
}

fn count_key(value: &Value, key: &str) -> usize {
    match value {
        Value::Object(object) => {
            usize::from(object.contains_key(key))
                + object
                    .values()
                    .map(|child| count_key(child, key))
                    .sum::<usize>()
        }
        Value::Array(array) => array.iter().map(|child| count_key(child, key)).sum(),
        _ => 0,
    }
}

fn observed_wire_form(body: &Value) -> CacheWireForm {
    if body.get("prompt_cache_key").is_some() {
        return if body.get("prompt_cache_retention") == Some(&json!("24h")) {
            CacheWireForm::PromptCacheKeyOneDay
        } else {
            CacheWireForm::PromptCacheKey
        };
    }
    if count_key(body, "cache_control") > 0 {
        return if count_key(body, "ttl") > 0 {
            CacheWireForm::CacheControlOneHour
        } else {
            CacheWireForm::CacheControl
        };
    }
    CacheWireForm::Nothing
}

fn serialize_coverage_case(case: CoverageCase) -> Value {
    match case.path {
        CoveragePath::ChatAnthropicDialect => {
            let request = with_cache_control(
                cache_request("custom/model-a"),
                CacheControlDialect::Anthropic,
            );
            lash_provider_openai::testing::serialize_chat_request(&request, case.retention)
                .expect("Anthropic cache-control dialect body")
                .0
        }
        CoveragePath::ChatGeminiDialect => {
            let request =
                with_cache_control(cache_request("custom/model-b"), CacheControlDialect::Gemini);
            lash_provider_openai::testing::serialize_chat_request(&request, case.retention)
                .expect("Gemini cache-control dialect body")
                .0
        }
        CoveragePath::ChatNoDialect => {
            lash_provider_openai::testing::serialize_chat_request(
                &cache_request("anthropic/claude-gemini-lookalike"),
                case.retention,
            )
            .expect("capability-free Chat body")
            .0
        }
        CoveragePath::AnthropicDirect => lash_provider_anthropic::testing::serialize_request(
            &cache_request("claude-sonnet-4-6"),
            case.retention,
        )
        .expect("Anthropic body"),
        CoveragePath::GoogleDirect => lash_provider_google::testing::serialize_request(
            &cache_request("gemini-3.1-pro-preview"),
            case.retention,
        )
        .expect("Google schema projection"),
        CoveragePath::OpenAiResponses => {
            lash_provider_openai::testing::serialize_responses_request(
                &cache_request("gpt-5.4"),
                case.retention,
            )
            .expect("OpenAI Responses body")
        }
        CoveragePath::CodexResponses => lash_provider_openai::testing::serialize_codex_request(
            &cache_request("gpt-5.4"),
            case.retention,
        )
        .expect("Codex body"),
    }
}

#[test]
fn cache_coverage_matrix_matches_capability_and_retention_dialects() {
    use CacheRetention::{Long, None, Short};
    use CacheWireForm::{
        CacheControl, CacheControlOneHour, Nothing, PromptCacheKey, PromptCacheKeyOneDay,
    };
    use CoveragePath::{
        AnthropicDirect, ChatAnthropicDialect, ChatGeminiDialect, ChatNoDialect, CodexResponses,
        GoogleDirect, OpenAiResponses,
    };

    let cases = [
        CoverageCase {
            path: ChatAnthropicDialect,
            retention: None,
            expected: Nothing,
        },
        CoverageCase {
            path: ChatAnthropicDialect,
            retention: Short,
            expected: CacheControl,
        },
        CoverageCase {
            path: ChatAnthropicDialect,
            retention: Long,
            expected: CacheControlOneHour,
        },
        CoverageCase {
            path: ChatGeminiDialect,
            retention: None,
            expected: Nothing,
        },
        CoverageCase {
            path: ChatGeminiDialect,
            retention: Short,
            expected: CacheControl,
        },
        CoverageCase {
            path: ChatGeminiDialect,
            retention: Long,
            expected: CacheControl,
        },
        CoverageCase {
            path: ChatNoDialect,
            retention: None,
            expected: Nothing,
        },
        CoverageCase {
            path: ChatNoDialect,
            retention: Short,
            expected: Nothing,
        },
        CoverageCase {
            path: ChatNoDialect,
            retention: Long,
            expected: Nothing,
        },
        CoverageCase {
            path: AnthropicDirect,
            retention: None,
            expected: Nothing,
        },
        CoverageCase {
            path: AnthropicDirect,
            retention: Short,
            expected: CacheControl,
        },
        CoverageCase {
            path: AnthropicDirect,
            retention: Long,
            expected: CacheControlOneHour,
        },
        CoverageCase {
            path: GoogleDirect,
            retention: None,
            expected: Nothing,
        },
        CoverageCase {
            path: GoogleDirect,
            retention: Short,
            expected: Nothing,
        },
        CoverageCase {
            path: GoogleDirect,
            retention: Long,
            expected: Nothing,
        },
        CoverageCase {
            path: OpenAiResponses,
            retention: None,
            expected: Nothing,
        },
        CoverageCase {
            path: OpenAiResponses,
            retention: Short,
            expected: PromptCacheKey,
        },
        CoverageCase {
            path: OpenAiResponses,
            retention: Long,
            expected: PromptCacheKeyOneDay,
        },
        CoverageCase {
            path: CodexResponses,
            retention: None,
            expected: Nothing,
        },
        CoverageCase {
            path: CodexResponses,
            retention: Short,
            expected: PromptCacheKey,
        },
        CoverageCase {
            path: CodexResponses,
            retention: Long,
            expected: PromptCacheKey,
        },
    ];

    let mismatches = cases
        .into_iter()
        .filter_map(|case| {
            let observed = observed_wire_form(&serialize_coverage_case(case));
            (observed != case.expected).then(|| {
                format!(
                    "{:?} x {:?}: expected {:?}, observed {:?}",
                    case.path, case.retention, case.expected, observed
                )
            })
        })
        .collect::<Vec<_>>();
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

#[test]
fn gemini_dialect_is_independent_of_model_name() {
    for (retention, expected) in [
        (CacheRetention::None, CacheWireForm::Nothing),
        (CacheRetention::Short, CacheWireForm::CacheControl),
        (CacheRetention::Long, CacheWireForm::CacheControl),
    ] {
        let request = with_cache_control(
            cache_request("unrecognized/model"),
            CacheControlDialect::Gemini,
        );
        let body = lash_provider_openai::testing::serialize_chat_request(&request, retention)
            .expect("Gemini cache-control dialect body")
            .0;
        assert_eq!(observed_wire_form(&body), expected, "{retention:?}");
    }
}

#[test]
fn requested_breakpoints_report_capability_driven_emission_and_drop() {
    let request = with_cache_control(
        cache_request("unrecognized/model"),
        CacheControlDialect::Gemini,
    );
    let (_, report) =
        lash_provider_openai::testing::serialize_chat_request(&request, CacheRetention::Short)
            .expect("Gemini cache-control dialect body");

    assert_eq!(report.requested, 1);
    assert_eq!(report.emitted, 1);
    assert_eq!(report.dropped, 0);

    let request = cache_request("anthropic/claude-gemini-lookalike");
    let (_, unsupported) =
        lash_provider_openai::testing::serialize_chat_request(&request, CacheRetention::Short)
            .expect("capability-free Chat body");
    assert_eq!(unsupported.dropped, 1);
}

#[test]
fn runtime_feedback_participates_in_serialized_cache_prefixes() {
    for serializer in [
        ProviderSerializer::ChatAnthropicDialect,
        ProviderSerializer::ChatGeminiDialect,
        ProviderSerializer::AnthropicDirect,
        ProviderSerializer::GoogleDirect,
    ] {
        let iterations = standard_iterations("model");
        for request in &iterations {
            let serialized = serialize_prefix(serializer, request, 2);
            let prefix = serialized.stable_prefix.to_string();
            let expected = match serializer {
                ProviderSerializer::ChatAnthropicDialect
                | ProviderSerializer::ChatGeminiDialect => "stable runtime feedback",
                ProviderSerializer::AnthropicDirect | ProviderSerializer::GoogleDirect => {
                    "<runtime_feedback>stable runtime feedback</runtime_feedback>"
                }
            };
            assert!(prefix.contains(expected), "{serializer:?}: {prefix}");
        }
        assert_prefix_stability(
            &format!("feedback {serializer:?}"),
            &iterations,
            |request, stable| serialize_prefix(serializer, request, stable),
        );
    }
}

/// The JSON paths of every `cache_control` marker in a request body, sorted.
fn cache_marker_paths(body: &Value) -> Vec<String> {
    fn walk(value: &Value, path: String, out: &mut Vec<String>) {
        match value {
            Value::Object(object) => {
                if object.contains_key("cache_control") {
                    out.push(path.clone());
                }
                for (key, child) in object {
                    if !matches!(
                        key.as_str(),
                        "cache_control" | "input_schema" | "parameters"
                    ) {
                        walk(child, format!("{path}.{key}"), out);
                    }
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    walk(child, format!("{path}[{index}]"), out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(body, String::new(), &mut out);
    out.sort();
    out
}

fn sorted_paths(paths: &[&str]) -> Vec<String> {
    let mut paths = paths
        .iter()
        .map(|path| path.to_string())
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

/// `request` with one tool on the wire, so the body carries the provider's
/// tool marker too.
fn with_wire_tool(request: &LlmRequest) -> LlmRequest {
    let mut request = request.clone();
    request.tools = Arc::new(vec![lash_core::llm::types::LlmToolSpec {
        name: "probe".to_string(),
        description: "Probe".to_string(),
        input_schema: lash_sansio::SchemaContract::admit(json!({"type": "object"}))
            .expect("valid declared schema"),
        output_schema: lash_sansio::SchemaContract::admit(json!({}))
            .expect("valid declared schema"),
    }]);
    request
}

fn anthropic_body(request: &LlmRequest) -> Value {
    lash_provider_anthropic::testing::serialize_request(request, CacheRetention::Short)
        .expect("Anthropic request")
}

fn chat_anthropic_dialect_body(
    request: &LlmRequest,
) -> (Value, lash_provider_openai::testing::CacheBreakpointReport) {
    lash_provider_openai::testing::serialize_chat_request(
        &with_cache_control(request.clone(), CacheControlDialect::Anthropic),
        CacheRetention::Short,
    )
    .expect("OpenAI-compatible Chat request")
}

fn relay_request_contexts(requests: &[LlmRequest]) -> Vec<Vec<String>> {
    requests
        .iter()
        .map(|request| match request.messages.as_slice() {
            [context, _harness] => context
                .blocks
                .iter()
                .filter_map(|block| match block {
                    LlmContentBlock::Text { text, .. } => Some(text.to_string()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

/// On the Anthropic wire a relay step marks the system prompt, the last
/// context block unchanged since the previous commit, the end of the context
/// and the end of the harness; its two user messages merge into one. With no
/// unchanged prefix the first context marker is absent, and over the
/// four-marker limit the earliest message marker goes.
#[tokio::test]
async fn relay_anthropic_body_marks_system_unchanged_prefix_context_end_and_harness() {
    let requests = captured_relay_steps().await;
    assert_eq!(
        relay_request_contexts(&requests),
        [
            vec![],
            vec!["a", "b"],
            vec!["a", "b", "c"],
            vec!["x", "b", "c"]
        ]
    );
    let markers = |request: &LlmRequest| cache_marker_paths(&anthropic_body(request));
    // An empty context: only the harness.
    assert_eq!(
        markers(&requests[0]),
        sorted_paths(&[".system[0]", ".messages[0].content[0]"])
    );
    // The first commit: nothing was unchanged.
    assert_eq!(
        markers(&requests[1]),
        sorted_paths(&[
            ".system[0]",
            ".messages[0].content[1]",
            ".messages[0].content[2]"
        ])
    );
    // An append: the old end (`b`), the new end (`c`), the harness.
    assert_eq!(
        markers(&requests[2]),
        sorted_paths(&[
            ".system[0]",
            ".messages[0].content[1]",
            ".messages[0].content[2]",
            ".messages[0].content[3]"
        ])
    );
    // A rewrite of entry 0: nothing unchanged.
    assert_eq!(
        markers(&requests[3]),
        sorted_paths(&[
            ".system[0]",
            ".messages[0].content[2]",
            ".messages[0].content[3]"
        ])
    );
    // System, tool and three message markers: the earliest message marker goes.
    assert_eq!(
        markers(&with_wire_tool(&requests[2])),
        sorted_paths(&[
            ".system[0]",
            ".tools[0]",
            ".messages[0].content[2]",
            ".messages[0].content[3]"
        ])
    );
}

/// On an OpenAI-compatible chat route with the Anthropic cache dialect
/// (OpenRouter's Claude models) a relay step carries the same markers on its
/// system, context and harness messages, and over the limit the earliest
/// message marker is dropped and reported.
#[tokio::test]
async fn relay_chat_anthropic_dialect_body_marks_system_unchanged_prefix_context_end_and_harness() {
    let requests = captured_relay_steps().await;
    let markers = |request: &LlmRequest| {
        let (body, report) = chat_anthropic_dialect_body(request);
        (
            cache_marker_paths(&body),
            report.requested,
            report.emitted,
            report.dropped,
        )
    };
    assert_eq!(
        markers(&requests[0]),
        (
            sorted_paths(&[".messages[0].content[0]", ".messages[1].content[0]"]),
            1,
            1,
            0
        )
    );
    assert_eq!(
        markers(&requests[1]),
        (
            sorted_paths(&[
                ".messages[0].content[0]",
                ".messages[1].content[1]",
                ".messages[2].content[0]"
            ]),
            2,
            2,
            0
        )
    );
    assert_eq!(
        markers(&requests[2]),
        (
            sorted_paths(&[
                ".messages[0].content[0]",
                ".messages[1].content[1]",
                ".messages[1].content[2]",
                ".messages[2].content[0]"
            ]),
            3,
            3,
            0
        )
    );
    assert_eq!(
        markers(&requests[3]),
        (
            sorted_paths(&[
                ".messages[0].content[0]",
                ".messages[1].content[2]",
                ".messages[2].content[0]"
            ]),
            2,
            2,
            0
        )
    );
    assert_eq!(
        markers(&with_wire_tool(&requests[2])),
        (
            sorted_paths(&[
                ".messages[0].content[0]",
                ".tools[0]",
                ".messages[1].content[2]",
                ".messages[2].content[0]"
            ]),
            3,
            2,
            1
        )
    );
}

/// Chronological RLM marks one message block, its rolling history fence,
/// and both Anthropic serializers put exactly the system marker and that one
/// message marker on the wire, at the fenced block (or, before any history,
/// at the last user block).
#[tokio::test]
async fn chronological_rlm_bodies_carry_the_system_marker_and_one_message_marker() {
    let requests = captured_rlm_iterations().await;
    assert_eq!(requests.len(), 3, "RLM protocol call count");
    for (index, request) in requests.iter().enumerate() {
        let fenced = request
            .messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .filter_map(|block| match block {
                LlmContentBlock::Text {
                    text,
                    cache_breakpoint: true,
                    ..
                } => Some(text.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(fenced.len() <= 1, "request {index} marks {fenced:?}");
        let marked_message_texts = |messages: &Value| {
            messages
                .as_array()
                .expect("messages")
                .iter()
                .flat_map(|message| message["content"].as_array().expect("content").iter())
                .filter(|block| block.get("cache_control").is_some())
                .map(|block| block["text"].as_str().expect("marked text").to_string())
                .collect::<Vec<_>>()
        };

        let anthropic = anthropic_body(request);
        let expected = fenced.first().cloned().unwrap_or_else(|| {
            let last = anthropic["messages"]
                .as_array()
                .and_then(|messages| messages.last())
                .expect("a last message");
            assert_eq!(last["role"], "user");
            last["content"]
                .as_array()
                .and_then(|content| content.last())
                .and_then(|block| block["text"].as_str())
                .expect("last user text")
                .to_string()
        });
        assert_eq!(count_key(&anthropic, "cache_control"), 2, "request {index}");
        assert!(anthropic["system"][0].get("cache_control").is_some());
        assert_eq!(
            marked_message_texts(&anthropic["messages"]),
            [expected.clone()],
            "request {index}"
        );

        let (chat, _) = chat_anthropic_dialect_body(request);
        assert_eq!(count_key(&chat, "cache_control"), 2, "request {index}");
        let chat_messages = chat["messages"].as_array().expect("chat messages");
        assert!(
            chat_messages[0]["content"][0]
                .get("cache_control")
                .is_some()
        );
        assert_eq!(
            marked_message_texts(&Value::Array(chat_messages[1..].to_vec())),
            [expected],
            "request {index}"
        );
    }
}
