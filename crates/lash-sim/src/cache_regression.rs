use lash_sansio::sync::MutexExt;
use std::sync::Arc;

use lash::rlm::RlmTurnBuilderExt;
use lash_core::llm::types::{LlmContentBlock, LlmMessage, LlmRequest, LlmRole};
use lash_core::provider::{CacheControlDialect, CacheRetention};
use lash_llm_transport::cache_regression::{
    SerializedPromptRequest, assert_prefix_stability, strip_cache_directives,
};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProtocolKind {
    Standard,
    Rlm,
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
        model: model.to_string(),
        messages,
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: Default::default(),
        model_variant: Default::default(),
        model_capability: Default::default(),
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
    request.model_capability.cache_control = Some(dialect);
    request
}

fn standard_iterations(model: &str) -> Vec<LlmRequest> {
    let first = vec![
        LlmMessage::text(LlmRole::System, "stable system"),
        LlmMessage::text(LlmRole::User, "solve the task"),
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
    use std::collections::VecDeque;

    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let responses = Arc::new(tokio::sync::Mutex::new(VecDeque::from([
        "<lashlang>\nvalue = 1\nprint(value)\n</lashlang>".to_string(),
        "<lashlang>\nvalue = value + 1\nprint(value)\n</lashlang>".to_string(),
        "<lashlang>\nfinish value\n</lashlang>".to_string(),
    ])));
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
                        full_text: text.clone(),
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
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::new(
            lash_protocol_rlm::ExecutionBound::instructions(1_000_000),
            lash_protocol_rlm::ExecutionBound::secs(30),
        ),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    let core = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .effect_host(Arc::new(
            lash::durability::InlineEffectHost::default().allow_process_lifetime_completion_keys(),
        ))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .store_factory(Arc::new(
            lash::persistence::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(lash_core::TestLocalProcessRegistry::default())
            as Arc<dyn lash_core::ProcessRegistry>)
        .provider(provider)
        .model(
            lash_core::ModelSpec::builder("cache-regression-model")
                .context_window_tokens(200_000)
                .build()
                .expect("cache regression model"),
        )
        .build()
        .expect("RLM cache regression core");
    let session = core
        .session("cache-regression-session")
        .open()
        .await
        .expect("RLM cache regression session");
    session
        .turn(lash::TurnInput::text("increment a bound value twice"))
        .require_finish()
        .expect("finish-required RLM turn")
        .run()
        .await
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

fn non_system_message_count(request: &LlmRequest, stable_messages: usize) -> usize {
    request.messages[..stable_messages]
        .iter()
        .filter(|message| message.role != LlmRole::System)
        .count()
}

fn prefix_for_anthropic(
    request: &LlmRequest,
    body: Value,
    prefix_body: &Value,
    stable_messages: usize,
) -> SerializedPromptRequest {
    let message_count = non_system_message_count(request, stable_messages);
    let mut stable_prefix = json!({
        "system": prefix_body.get("system").cloned(),
        "messages": prefix_body["messages"]
            .as_array()
            .expect("Anthropic messages")
            .iter()
            .take(message_count)
            .cloned()
            .collect::<Vec<_>>(),
    });
    strip_cache_directives(&mut stable_prefix);
    SerializedPromptRequest {
        body,
        stable_prefix,
    }
}

fn prefix_for_google(
    request: &LlmRequest,
    body: Value,
    prefix_body: &Value,
    stable_messages: usize,
) -> SerializedPromptRequest {
    let message_count = non_system_message_count(request, stable_messages);
    let wire_request = &prefix_body["request"];
    let stable_prefix = json!({
        "systemInstruction": wire_request.get("systemInstruction").cloned(),
        "contents": wire_request["contents"]
            .as_array()
            .expect("Google contents")
            .iter()
            .take(message_count)
            .cloned()
            .collect::<Vec<_>>(),
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
            prefix_for_openai_chat(body, stable_messages)
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
            prefix_for_anthropic(request, body, &prefix_body, stable_messages)
        }
        ProviderSerializer::GoogleDirect => {
            let body =
                lash_provider_google::testing::serialize_request(request, CacheRetention::Short);
            let mut prefix_request = request.clone();
            prefix_request.messages.truncate(stable_messages);
            let prefix_body = lash_provider_google::testing::serialize_request(
                &prefix_request,
                CacheRetention::Short,
            );
            prefix_for_google(request, body, &prefix_body, stable_messages)
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

#[tokio::test]
async fn rlm_live_bound_state_is_prefix_stable_for_every_serializer() {
    let captured = captured_rlm_iterations().await;
    assert_eq!(captured.len(), 3, "RLM protocol call count");
    for serializer in [
        ProviderSerializer::ChatAnthropicDialect,
        ProviderSerializer::ChatGeminiDialect,
        ProviderSerializer::AnthropicDirect,
        ProviderSerializer::GoogleDirect,
    ] {
        let model = match serializer {
            ProviderSerializer::ChatAnthropicDialect | ProviderSerializer::AnthropicDirect => {
                "anthropic/claude-sonnet-4.6"
            }
            ProviderSerializer::ChatGeminiDialect | ProviderSerializer::GoogleDirect => {
                "google/gemini-3.1-pro-preview"
            }
        };
        let iterations = captured
            .iter()
            .cloned()
            .map(|mut request| {
                request.model = model.to_string();
                request
            })
            .collect::<Vec<_>>();
        assert_prefix_stability(
            &format!("{:?} x {serializer:?}", ProtocolKind::Rlm),
            &iterations,
            |request, stable_messages| serialize_prefix(serializer, request, stable_messages),
        );
    }
}

fn cache_request(model: &str) -> LlmRequest {
    request(
        model,
        vec![
            LlmMessage::text(LlmRole::System, "stable system"),
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
        ),
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
