//! Executable generation-setting dispositions at the provider transport seam.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use lash_core::NonNegativeFiniteF64;
use lash_core::facade_support::LlmTransportError;
use lash_core::provider::{
    AnthropicThinkingRetention, GoogleDialect, ModelCapability, OpenAiReasoningContext,
    ProviderHandle, ProviderOptions, ReasoningCapability, ReasoningEncoding,
    ReasoningRetentionCapability, ReasoningRetentionPolicy, ReasoningRetentionSelection,
    ReasoningSelection, SamplingCapability,
};
use lash_llm_transport::{LlmHttpRequest, LlmHttpResponse, LlmHttpTransport};
use lash_provider_anthropic::AnthropicProvider;
use lash_provider_google::{GoogleOAuthClient, GoogleOAuthProvider};
use lash_provider_openai::codex::ws_testing::{ScriptedWsAction, spawn_scripted_websocket};
use lash_provider_openai::{CodexProvider, OpenAiCompatibleProvider, OpenAiProvider};
use lash_sansio::llm::types::{
    GenerationOptionOutcome as Outcome, GenerationReceipt, LlmEventSender, LlmMessage, LlmRequest,
    LlmRequestScope, LlmRole, LlmToolChoice, LlmToolSpec,
};
use lash_sansio::sync::MutexExt;
use lash_sansio::{FailureCode, TurnFailureCode};
use serde_json::{Value, json};

use crate::provider::{ProviderWireEvent, ProviderWireScript, ScriptedLlmHttpTransport};

#[derive(Debug)]
struct CapturingTransport {
    inner: Arc<dyn LlmHttpTransport>,
    bodies: Mutex<Vec<Value>>,
}

#[async_trait]
impl LlmHttpTransport for CapturingTransport {
    async fn send(
        &self,
        request: LlmHttpRequest,
        timeout: Option<std::time::Duration>,
    ) -> Result<LlmHttpResponse, LlmTransportError> {
        self.bodies
            .lock_recover()
            .push(serde_json::from_slice(&request.body).expect("request JSON"));
        self.inner.send(request, timeout).await
    }
}

#[derive(Clone, Copy, Debug)]
enum Dialect {
    Anthropic,
    GoogleLegacy,
    GoogleGemini3,
    GoogleClaudeOnVertex,
    OpenAiChat,
    OpenRouter,
    CompatibleDefault,
    OpenAiResponses,
    CodexSse,
}

const HTTP_DIALECTS: [Dialect; 9] = [
    Dialect::Anthropic,
    Dialect::GoogleLegacy,
    Dialect::GoogleGemini3,
    Dialect::GoogleClaudeOnVertex,
    Dialect::OpenAiChat,
    Dialect::OpenRouter,
    Dialect::CompatibleDefault,
    Dialect::OpenAiResponses,
    Dialect::CodexSse,
];

impl Dialect {
    fn is_google(self) -> bool {
        matches!(
            self,
            Self::GoogleLegacy | Self::GoogleGemini3 | Self::GoogleClaudeOnVertex
        )
    }

    fn is_chat(self) -> bool {
        matches!(
            self,
            Self::OpenAiChat | Self::OpenRouter | Self::CompatibleDefault
        )
    }

    fn model(self) -> &'static str {
        match self {
            Self::Anthropic => "claude-matrix",
            Self::GoogleLegacy | Self::GoogleGemini3 => "gemini-matrix",
            Self::GoogleClaudeOnVertex => "claude-matrix",
            Self::OpenAiChat | Self::OpenRouter | Self::CompatibleDefault => "openai/matrix",
            Self::OpenAiResponses => "gpt-matrix",
            Self::CodexSse => "gpt-matrix-codex",
        }
    }

    fn cap_path(self) -> &'static str {
        match self {
            Self::Anthropic => "/max_tokens",
            Self::GoogleLegacy | Self::GoogleGemini3 | Self::GoogleClaudeOnVertex => {
                "/request/generationConfig/maxOutputTokens"
            }
            Self::OpenAiChat => "/max_completion_tokens",
            Self::OpenRouter | Self::CompatibleDefault => "/max_tokens",
            Self::OpenAiResponses => "/max_output_tokens",
            Self::CodexSse => "/max_output_tokens",
        }
    }

    fn field_path(self, setting: Setting) -> &'static str {
        match setting {
            Setting::Cap => self.cap_path(),
            Setting::Temperature if self.is_google() => "/request/generationConfig/temperature",
            Setting::Temperature => "/temperature",
            Setting::Seed if self.is_google() => "/request/generationConfig/seed",
            Setting::Seed => "/seed",
            Setting::Stop if self.is_google() => "/request/generationConfig/stopSequences",
            Setting::Stop if self.is_chat() => "/stop",
            Setting::Stop => "/stop_sequences",
            Setting::Parallel if matches!(self, Self::Anthropic) => {
                "/tool_choice/disable_parallel_tool_use"
            }
            Setting::Parallel => "/parallel_tool_calls",
        }
    }

    fn script(self) -> ProviderWireScript {
        let source = match self {
            Self::Anthropic => {
                include_str!("../provider-scripts/canonical/anthropic.messages-text-stream.json")
            }
            Self::GoogleLegacy | Self::GoogleGemini3 | Self::GoogleClaudeOnVertex => {
                include_str!(
                    "../provider-scripts/canonical/google.stream-generate-content-text-stream.json"
                )
            }
            Self::OpenAiChat | Self::OpenRouter | Self::CompatibleDefault => {
                include_str!(
                    "../provider-scripts/canonical/openai-compatible.chat-tool-call-split-stream.json"
                )
            }
            Self::OpenAiResponses => {
                include_str!("../provider-scripts/canonical/openai.responses-text-stream.json")
            }
            Self::CodexSse => {
                include_str!("../provider-scripts/canonical/codex.responses-text-stream.json")
            }
        };
        let mut script: Value = serde_json::from_str(source).expect("canonical script JSON");
        script["request_match"] = json!({"any": true});
        ProviderWireScript::from_json_str(&script.to_string()).expect("matrix script")
    }

    fn provider(
        self,
        transport: Arc<ScriptedLlmHttpTransport>,
        cap: Option<u64>,
        headers: Vec<(String, String)>,
    ) -> ProviderHandle {
        let transport: Arc<dyn LlmHttpTransport> = transport;
        let options = ProviderOptions {
            max_output_tokens: cap,
            reliability: lash_core::provider::ProviderReliability::disabled(),
            ..ProviderOptions::default()
        };
        match self {
            Self::Anthropic => ProviderHandle::new(
                AnthropicProvider::new("matrix-key")
                    .with_options(options)
                    .with_extra_headers(headers)
                    .with_transport(transport)
                    .into_components(),
            ),
            Self::GoogleLegacy | Self::GoogleGemini3 | Self::GoogleClaudeOnVertex => {
                ProviderHandle::new(
                    GoogleOAuthProvider::new(
                        "access",
                        "refresh",
                        0,
                        GoogleOAuthClient {
                            id: "matrix-id".into(),
                            secret: "matrix-secret".into(),
                        },
                    )
                    .with_project_id(Some("matrix-project".into()))
                    .with_options(options)
                    .with_extra_headers(headers)
                    .with_transport(transport)
                    .into_components(),
                )
            }
            Self::OpenAiChat => ProviderHandle::new(
                OpenAiCompatibleProvider::new("matrix-key", "https://openai-chat.matrix")
                    .with_compat(lash_provider_openai::OpenAiCompat::openai_chat())
                    .with_options(options)
                    .with_extra_headers(headers)
                    .with_transport(transport)
                    .into_components(),
            ),
            Self::OpenRouter => ProviderHandle::new(
                OpenAiCompatibleProvider::new("matrix-key", "https://openrouter.matrix")
                    .with_compat(lash_provider_openai::OpenAiCompat::openrouter())
                    .with_options(options)
                    .with_extra_headers(headers)
                    .with_transport(transport)
                    .into_components(),
            ),
            Self::CompatibleDefault => ProviderHandle::new(
                OpenAiCompatibleProvider::new("matrix-key", "https://compatible.matrix")
                    .with_options(options)
                    .with_extra_headers(headers)
                    .with_transport(transport)
                    .into_components(),
            ),
            Self::OpenAiResponses => ProviderHandle::new(
                OpenAiProvider::new("matrix-key")
                    .with_options(options)
                    .with_extra_headers(headers)
                    .with_transport(transport)
                    .into_components(),
            ),
            Self::CodexSse => ProviderHandle::new(
                CodexProvider::new("access", "refresh", 0)
                    .force_sse_transport()
                    .with_options(options)
                    .with_extra_headers(headers)
                    .with_http_transport(transport)
                    .into_components(),
            ),
        }
    }

    fn request(self) -> LlmRequest {
        let google_dialect = match self {
            Self::GoogleGemini3 => GoogleDialect::Gemini3,
            Self::GoogleClaudeOnVertex => GoogleDialect::ClaudeOnVertex,
            _ => GoogleDialect::Legacy,
        };
        LlmRequest {
            instructions: None,
            model: self.model().into(),
            messages: vec![LlmMessage::text(LlmRole::User, "answer directly")],
            resolved_stored: Default::default(),
            tools: Arc::new(Vec::new()),
            tool_choice: LlmToolChoice::Auto,
            model_variant: ReasoningSelection::ProviderDefault,
            model_capability: ModelCapability {
                google_dialect,
                ..ModelCapability::default()
            },
            extra_body: Default::default(),
            generation: Default::default(),
            scope: LlmRequestScope::new("matrix-session", "matrix-frame", "matrix-request"),
            output_spec: None,
            stream_events: Some(LlmEventSender::new(|_| {})),
            provider_trace: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Setting {
    Cap,
    Temperature,
    Seed,
    Stop,
    Parallel,
}

impl Setting {
    fn apply(self, request: &mut LlmRequest) -> Value {
        match self {
            Self::Cap => {
                request.generation.output_token_cap = NonZeroUsize::new(2048);
                json!(2048)
            }
            Self::Temperature => {
                request.generation.temperature =
                    Some(NonNegativeFiniteF64::new(0.25).expect("finite"));
                json!(0.25)
            }
            Self::Seed => {
                request.generation.seed = Some(-7);
                json!(-7)
            }
            Self::Stop => {
                request.generation.stop_sequences = vec!["END".into()];
                json!(["END"])
            }
            Self::Parallel => {
                request.tools = Arc::new(vec![LlmToolSpec {
                    name: "lookup".into(),
                    description: "Lookup".into(),
                    input_schema: json!({"type":"object"}).into(),
                    output_schema: json!({}).into(),
                }]);
                request.generation.parallel_tool_calls = Some(false);
                json!(false)
            }
        }
    }

    fn receipt(self, receipt: &GenerationReceipt) -> Outcome {
        match self {
            Self::Cap => receipt.output_token_cap,
            Self::Temperature => receipt.temperature,
            Self::Seed => receipt.seed,
            Self::Stop => receipt.stop_sequences,
            Self::Parallel => receipt.parallel_tool_calls,
        }
    }

    fn supported(self, dialect: Dialect) -> bool {
        match self {
            Self::Cap => !matches!(dialect, Dialect::CodexSse),
            Self::Temperature => !matches!(dialect, Dialect::CodexSse),
            Self::Seed => dialect.is_google() || dialect.is_chat(),
            Self::Stop => {
                dialect.is_google() || dialect.is_chat() || matches!(dialect, Dialect::Anthropic)
            }
            Self::Parallel => !dialect.is_google(),
        }
    }
}

async fn run(
    dialect: Dialect,
    request: LlmRequest,
    cap: Option<u64>,
) -> (Result<(Value, GenerationReceipt), FailureCode>, usize) {
    let transport = Arc::new(ScriptedLlmHttpTransport::new(dialect.script()).expect("script"));
    let mut provider = dialect.provider(transport.clone(), cap, Vec::new());
    let result = provider
        .complete(request)
        .await
        .map(|completion| {
            let body = completion
                .request_body
                .as_deref()
                .expect("captured request body");
            let receipt = completion
                .generation_disposition
                .expect("generation receipt");
            (serde_json::from_str(body).expect("request JSON"), receipt)
        })
        .map_err(|error| error.error.code.expect("typed failure code"));
    let calls = transport.exchanges().expect("exchanges").len();
    (result, calls)
}

async fn every_simple_generation_control_is_sent_or_refused_before_io() {
    let start = Instant::now();
    let mut count = 0;
    for dialect in HTTP_DIALECTS {
        for setting in [
            Setting::Cap,
            Setting::Temperature,
            Setting::Seed,
            Setting::Stop,
            Setting::Parallel,
        ] {
            let mut request = dialect.request();
            let expected = setting.apply(&mut request);
            let cap = matches!(dialect, Dialect::Anthropic).then_some(4096);
            let (result, calls) = run(dialect, request, cap).await;
            let label = format!("{dialect:?} {setting:?}");
            if setting.supported(dialect) {
                let (body, receipt) = result.unwrap_or_else(|error| panic!("{label}: {error}"));
                assert_eq!(calls, 1, "{label}");
                let actual = body.pointer(dialect.field_path(setting)).expect(&label);
                let expected = if matches!(setting, Setting::Parallel | Setting::Cap)
                    && matches!(dialect, Dialect::Anthropic)
                {
                    if matches!(setting, Setting::Parallel) {
                        json!(true)
                    } else {
                        expected
                    }
                } else {
                    expected
                };
                assert_eq!(actual, &expected, "{label}");
                assert_eq!(setting.receipt(&receipt), Outcome::Applied, "{label}");
            } else {
                assert_eq!(
                    result.unwrap_err(),
                    TurnFailureCode::UnsupportedGenerationOption.into(),
                    "{label}"
                );
                assert_eq!(calls, 0, "{label} reached transport");
            }
            count += 1;
        }
    }
    eprintln!(
        "generation disposition matrix: {count} cases in {:?}",
        start.elapsed()
    );
}

async fn runtime_clamps_a_requested_cap_and_reports_the_reduced_wire_value() {
    let start = Instant::now();
    let script = crate::runtime_providers::runtime_script_for_text(
        crate::runtime_providers::OPENAI_COMPATIBLE,
        "done",
    )
    .expect("runtime script");
    let transport = Arc::new(CapturingTransport {
        inner: Arc::new(ScriptedLlmHttpTransport::new(script).expect("scripted transport")),
        bodies: Mutex::new(Vec::new()),
    });
    let (provider, _, _) = crate::runtime_providers::runtime_provider_components(
        crate::runtime_providers::OPENAI_COMPATIBLE,
        &transport,
    )
    .expect("runtime provider");
    let model = lash::ModelSpec::builder("openai/gpt-5.4")
        .context_window_tokens(200_000)
        .output_token_capacity(2_048)
        .build()
        .expect("model");
    let engine = crate::backend::SimEngine::new(0x4122)
        .await
        .expect("sim engine");
    let core = lash::LashCore::standard_builder(engine.backend(), lash::TurnBudget::Unbounded)
        .generation(lash_core::GenerationOptions {
            output_token_cap: NonZeroUsize::new(32_000),
            ..Default::default()
        })
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .provider(provider)
        .model(model)
        .build(crate::sim_process_owner())
        .expect("runtime core");
    let session = crate::open_created_session(&core, "matrix-cap")
        .await
        .expect("session");
    let turn = engine
        .run_text_turn(&session, "matrix-cap-turn", "answer")
        .await
        .expect("handler")
        .expect("turn");
    let bodies = transport.bodies.lock_recover();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0].pointer("/max_tokens"), Some(&json!(2_048)));
    let receipt = turn
        .result
        .llm_calls
        .iter()
        .flat_map(|call| &call.attempts)
        .find_map(|attempt| attempt.generation_disposition)
        .unwrap_or_else(|| panic!("attempt receipt: {:?}", turn.result.llm_calls));
    assert_eq!(receipt.output_token_cap, Outcome::ClampedToCapacity);
    eprintln!(
        "runtime cap disposition matrix: 1 case in {:?}",
        start.elapsed()
    );
}

async fn protocol_owned_stop_is_absent_from_the_wire_and_reported_suppressed() {
    let start = Instant::now();
    let paired = Arc::new(
        crate::provider_variations::PairedProviderStopTransport::new(
            crate::provider_variations::ProviderStopDialect::OpenAiCompatibleChat,
        ),
    );
    let transport = Arc::new(CapturingTransport {
        inner: paired,
        bodies: Mutex::new(Vec::new()),
    });
    let (provider, model, _) = crate::runtime_providers::runtime_provider_components(
        crate::runtime_providers::OPENAI_COMPATIBLE,
        &transport,
    )
    .expect("runtime provider");
    let engine = crate::backend::SimEngine::new(0x4123)
        .await
        .expect("sim engine");
    let backend = engine.backend();
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        &backend,
    );
    let core = lash::LashCore::rlm_builder(backend, lash::TurnBudget::Unbounded, factory)
        .generation(lash_core::GenerationOptions {
            stop_sequences: vec![
                crate::provider_variations::TYPESCRIPT_CLOSE_DELIMITER.to_string(),
            ],
            ..Default::default()
        })
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .provider(provider)
        .model(model)
        .build(crate::sim_process_owner())
        .expect("RLM core");
    let session = crate::open_created_session(&core, "matrix-stop")
        .await
        .expect("session");
    let turn = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        engine.run_text_turn(
            &session,
            "matrix-stop-turn",
            "finish with the scripted value",
        ),
    )
    .await
    .expect("turn timeout")
    .expect("handler")
    .expect("turn");
    let bodies = transport.bodies.lock_recover();
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].pointer("/stop").is_none(), "{:?}", bodies[0]);
    let receipt = turn.result.llm_calls[0].attempts[0]
        .generation_disposition
        .expect("attempt receipt");
    assert_eq!(receipt.stop_sequences, Outcome::SuppressedProtocolOwned);
    eprintln!(
        "protocol stop disposition matrix: 1 case in {:?}",
        start.elapsed()
    );
}

async fn unset_controls_invent_no_generation_fields() {
    for dialect in HTTP_DIALECTS {
        let (result, calls) = run(dialect, dialect.request(), None).await;
        if matches!(dialect, Dialect::Anthropic) {
            assert_eq!(
                result.unwrap_err(),
                TurnFailureCode::OutputTokenCapRequired.into()
            );
            assert_eq!(calls, 0, "{dialect:?}");
            continue;
        }
        let (body, receipt) = result.unwrap_or_else(|error| panic!("{dialect:?}: {error}"));
        assert_eq!(calls, 1, "{dialect:?}");
        for setting in [
            Setting::Cap,
            Setting::Temperature,
            Setting::Seed,
            Setting::Stop,
            Setting::Parallel,
        ] {
            assert!(
                body.pointer(dialect.field_path(setting)).is_none(),
                "{dialect:?} invented {setting:?}: {body}"
            );
            assert_eq!(
                setting.receipt(&receipt),
                Outcome::NotRequested,
                "{dialect:?} {setting:?}"
            );
        }
        assert_eq!(receipt.reasoning, Outcome::NotRequested, "{dialect:?}");
        assert_eq!(
            receipt.reasoning_retention,
            Outcome::NotRequested,
            "{dialect:?}"
        );
        assert!(body.pointer("/reasoning/effort").is_none(), "{dialect:?}");
        assert!(body.pointer("/reasoning_effort").is_none(), "{dialect:?}");
    }
}

async fn pinned_sampling_and_mandatory_reasoning_refuse_without_io() {
    for dialect in HTTP_DIALECTS {
        let mut pinned = dialect.request();
        pinned.model_capability.sampling = SamplingCapability::Pinned;
        pinned.generation.temperature = Some(NonNegativeFiniteF64::new(0.25).expect("finite"));
        let (result, calls) = run(dialect, pinned, Some(4096)).await;
        assert_eq!(
            result.unwrap_err(),
            TurnFailureCode::UnsupportedGenerationOption.into(),
            "{dialect:?}"
        );
        assert_eq!(calls, 0, "{dialect:?}");

        let mut mandatory = dialect.request();
        mandatory.model_capability.reasoning = Some(ReasoningCapability {
            efforts: vec!["high".into()],
            mandatory: true,
            ..ReasoningCapability::default()
        });
        let (result, calls) = run(dialect, mandatory, Some(4096)).await;
        assert_eq!(
            result.unwrap_err(),
            TurnFailureCode::EffortRequired.into(),
            "{dialect:?}"
        );
        assert_eq!(calls, 0, "{dialect:?}");
    }
}

#[derive(Clone, Copy, Debug)]
enum ReasoningCase {
    Effort,
    Budget,
    Off,
}

fn reasoning_expectation(
    dialect: Dialect,
    case: ReasoningCase,
) -> Result<(&'static str, Value), TurnFailureCode> {
    match (dialect, case) {
        (Dialect::Anthropic, ReasoningCase::Effort) => Ok(("/output_config/effort", json!("high"))),
        (Dialect::Anthropic, ReasoningCase::Budget) => Ok(("/thinking/budget_tokens", json!(1024))),
        (Dialect::Anthropic, ReasoningCase::Off) => Ok(("/thinking/type", json!("disabled"))),
        (Dialect::GoogleLegacy | Dialect::GoogleGemini3, ReasoningCase::Effort) => Ok((
            "/request/generationConfig/thinkingConfig/thinkingLevel",
            json!("high"),
        )),
        (
            Dialect::GoogleLegacy | Dialect::GoogleGemini3 | Dialect::GoogleClaudeOnVertex,
            ReasoningCase::Budget,
        ) => Ok((
            "/request/generationConfig/thinkingConfig/thinkingBudget",
            json!(1024),
        )),
        (Dialect::GoogleLegacy, ReasoningCase::Off) => Ok((
            "/request/generationConfig/thinkingConfig/thinkingBudget",
            json!(0),
        )),
        (Dialect::GoogleGemini3 | Dialect::GoogleClaudeOnVertex, ReasoningCase::Off)
        | (Dialect::GoogleClaudeOnVertex, ReasoningCase::Effort) => {
            Err(TurnFailureCode::ReasoningEncodingUnrepresentable)
        }
        (Dialect::OpenAiChat, ReasoningCase::Effort) => Ok(("/reasoning_effort", json!("high"))),
        (Dialect::OpenAiChat, ReasoningCase::Off) => Ok(("/reasoning_effort", json!("none"))),
        (Dialect::OpenRouter, ReasoningCase::Effort) => Ok(("/reasoning/effort", json!("high"))),
        (Dialect::OpenRouter, ReasoningCase::Budget) => Ok(("/reasoning/max_tokens", json!(1024))),
        (Dialect::OpenRouter, ReasoningCase::Off) => Ok(("/reasoning/enabled", json!(false))),
        (Dialect::CompatibleDefault, _) => Err(TurnFailureCode::ReasoningEncodingUnrepresentable),
        (Dialect::OpenAiResponses | Dialect::CodexSse, ReasoningCase::Effort) => {
            Ok(("/reasoning/effort", json!("high")))
        }
        (Dialect::OpenAiResponses | Dialect::CodexSse, ReasoningCase::Off) => {
            Ok(("/reasoning/effort", json!("none")))
        }
        (
            Dialect::OpenAiChat | Dialect::OpenAiResponses | Dialect::CodexSse,
            ReasoningCase::Budget,
        ) => Err(TurnFailureCode::ReasoningEncodingUnrepresentable),
    }
}

async fn reasoning_selection_uses_only_the_declared_dialect() {
    for dialect in HTTP_DIALECTS {
        for case in [
            ReasoningCase::Effort,
            ReasoningCase::Budget,
            ReasoningCase::Off,
        ] {
            let mut request = dialect.request();
            request.model_capability.reasoning = Some(ReasoningCapability {
                efforts: vec!["high".into()],
                encoding: if matches!(case, ReasoningCase::Budget) {
                    ReasoningEncoding::Budget([("high".into(), 1024)].into())
                } else {
                    ReasoningEncoding::Effort
                },
                disable: true,
                mandatory: false,
            });
            request.model_variant = match case {
                ReasoningCase::Off => ReasoningSelection::Disabled,
                ReasoningCase::Effort | ReasoningCase::Budget => {
                    ReasoningSelection::Effort("high".into())
                }
            };
            let cap = matches!(dialect, Dialect::Anthropic).then_some(4096);
            let (result, calls) = run(dialect, request, cap).await;
            let label = format!("{dialect:?} {case:?}");
            match reasoning_expectation(dialect, case) {
                Ok((pointer, expected)) => {
                    let (body, receipt) = result.unwrap_or_else(|error| panic!("{label}: {error}"));
                    assert_eq!(calls, 1, "{label}");
                    assert_eq!(body.pointer(pointer), Some(&expected), "{label}: {body}");
                    assert_eq!(receipt.reasoning, Outcome::Applied, "{label}");
                }
                Err(code) => {
                    assert_eq!(result.unwrap_err(), code.into(), "{label}");
                    assert_eq!(calls, 0, "{label} reached transport");
                }
            }
        }
    }
}

async fn passthrough_is_sent_or_refused_without_io() {
    for dialect in HTTP_DIALECTS {
        let mut request = dialect.request();
        request.extra_body = json!({"host_matrix":{"enabled":true}})
            .as_object()
            .cloned()
            .expect("object");
        let cap = matches!(dialect, Dialect::Anthropic).then_some(4096);
        let (result, calls) = run(dialect, request, cap).await;
        let (body, receipt) = result.unwrap_or_else(|error| panic!("{dialect:?}: {error}"));
        let pointer = if dialect.is_google() {
            "/request/host_matrix/enabled"
        } else {
            "/host_matrix/enabled"
        };
        assert_eq!(
            body.pointer(pointer),
            Some(&json!(true)),
            "{dialect:?}: {body}"
        );
        assert_eq!(receipt.passthrough, Outcome::Applied, "{dialect:?}");
        assert_eq!(calls, 1, "{dialect:?}");

        let mut conflict = dialect.request();
        conflict.extra_body = if dialect.is_google() {
            json!({"contents":[]})
        } else {
            json!({"model":"other"})
        }
        .as_object()
        .cloned()
        .expect("object");
        let (result, calls) = run(dialect, conflict, cap).await;
        assert_eq!(
            result.unwrap_err(),
            TurnFailureCode::PassthroughConflict.into(),
            "{dialect:?}"
        );
        assert_eq!(calls, 0, "{dialect:?} reached transport");
    }
}

async fn replay_of_generation_intent_has_the_same_body_and_receipt() {
    for dialect in HTTP_DIALECTS {
        let mut request = dialect.request();
        request.generation.temperature = Some(NonNegativeFiniteF64::new(0.25).expect("finite"));
        if matches!(dialect, Dialect::CodexSse) {
            request.generation.temperature = None;
            request.model_capability.reasoning = Some(ReasoningCapability {
                efforts: vec!["high".into()],
                ..ReasoningCapability::default()
            });
            request.model_variant = ReasoningSelection::Effort("high".into());
        }
        let cap = matches!(dialect, Dialect::Anthropic).then_some(4096);
        let (first, first_calls) = run(dialect, request.clone(), cap).await;
        let (second, second_calls) = run(dialect, request, cap).await;
        assert_eq!((first_calls, second_calls), (1, 1), "{dialect:?}");
        let (first_body, first_receipt) =
            first.unwrap_or_else(|error| panic!("{dialect:?}: {error}"));
        let (second_body, second_receipt) =
            second.unwrap_or_else(|error| panic!("{dialect:?}: {error}"));
        assert_eq!(first_body, second_body, "{dialect:?} replay body");
        assert_eq!(first_receipt, second_receipt, "{dialect:?} replay receipt");
    }
}

async fn route_headers_are_sent_or_refused_before_io() {
    for dialect in HTTP_DIALECTS {
        let cap = matches!(dialect, Dialect::Anthropic).then_some(4096);
        let transport = Arc::new(ScriptedLlmHttpTransport::new(dialect.script()).expect("script"));
        let mut provider = dialect.provider(
            transport.clone(),
            cap,
            vec![("x-matrix-route".into(), "selected".into())],
        );
        let completion = provider
            .complete(dialect.request())
            .await
            .unwrap_or_else(|error| panic!("{dialect:?}: {error}"));
        assert_eq!(
            completion
                .generation_disposition
                .expect("header receipt")
                .passthrough,
            Outcome::Applied,
            "{dialect:?}"
        );
        let exchanges = transport.exchanges().expect("exchanges");
        assert_eq!(exchanges.len(), 1, "{dialect:?}");
        assert!(
            exchanges[0].request.headers.iter().any(|header| {
                header.name.eq_ignore_ascii_case("x-matrix-route") && header.value == "selected"
            }),
            "{dialect:?}"
        );

        let transport = Arc::new(ScriptedLlmHttpTransport::new(dialect.script()).expect("script"));
        let mut provider = dialect.provider(
            transport.clone(),
            cap,
            vec![("CoNtEnT-TyPe".into(), "other".into())],
        );
        let error = provider
            .complete(dialect.request())
            .await
            .expect_err("header conflict");
        assert_eq!(
            error
                .error
                .code
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("lash:passthrough_conflict"),
            "{dialect:?}"
        );
        assert!(
            transport.exchanges().expect("exchanges").is_empty(),
            "{dialect:?} reached transport"
        );
    }
}

async fn thinking_visibility_and_summary_have_distinct_receipts() {
    for dialect in HTTP_DIALECTS {
        let transport = Arc::new(ScriptedLlmHttpTransport::new(dialect.script()).expect("script"));
        let cap = matches!(dialect, Dialect::Anthropic).then_some(4096);
        let mut provider = dialect.provider(transport.clone(), cap, Vec::new());
        let mut options = provider.options();
        options.expose_thinking = true;
        provider.set_options(options);
        let completion = provider
            .complete(dialect.request())
            .await
            .unwrap_or_else(|error| panic!("{dialect:?}: {error}"));
        let receipt = completion.generation_disposition.expect("receipt");
        let body: Value =
            serde_json::from_str(completion.request_body.as_deref().expect("body")).expect("JSON");
        assert_eq!(receipt.thinking_visibility, Outcome::Applied, "{dialect:?}");
        let summary_path = if dialect.is_google() {
            Some("/request/generationConfig/thinkingConfig/includeThoughts")
        } else if matches!(dialect, Dialect::OpenAiResponses | Dialect::CodexSse) {
            Some("/reasoning/summary")
        } else {
            None
        };
        match summary_path {
            Some(path) => {
                assert!(body.pointer(path).is_some(), "{dialect:?}: {body}");
                assert_eq!(receipt.thinking_summary, Outcome::Applied, "{dialect:?}");
            }
            None => assert_eq!(
                receipt.thinking_summary,
                Outcome::NotRequested,
                "{dialect:?}"
            ),
        }
        assert_eq!(
            transport.exchanges().expect("exchanges").len(),
            1,
            "{dialect:?}"
        );
    }
}

async fn receipt_survives_a_failure_after_send() {
    let dialect = Dialect::OpenAiResponses;
    let mut script = dialect.script();
    script.timeline_mut().truncate(3);
    script.timeline_mut().push(ProviderWireEvent::Disconnect {
        at: 30,
        message: Some("matrix disconnect".into()),
        retryable: Some(false),
    });
    let transport = Arc::new(ScriptedLlmHttpTransport::new(script).expect("disconnect script"));
    let mut provider = dialect.provider(transport.clone(), None, Vec::new());
    let mut request = dialect.request();
    request.generation.temperature = Some(NonNegativeFiniteF64::new(0.25).expect("finite"));
    let error = provider
        .complete(request)
        .await
        .expect_err("provider failure");
    assert_eq!(transport.exchanges().expect("exchanges").len(), 1);
    let partial = error
        .error
        .partial_response
        .expect("partial response after send");
    assert_eq!(
        partial.generation_disposition.expect("receipt").temperature,
        Outcome::Applied
    );
    assert_eq!(error.call_record.attempts.len(), 1);
    assert_eq!(
        error.call_record.attempts[0]
            .generation_disposition
            .as_ref()
            .expect("attempt receipt")
            .temperature,
        Outcome::Applied
    );
}

async fn websocket_generation_settings_have_the_same_dispositions_as_sse() {
    let start = Instant::now();
    let dialect = Dialect::CodexSse;
    for setting in [
        Setting::Cap,
        Setting::Temperature,
        Setting::Seed,
        Setting::Stop,
        Setting::Parallel,
    ] {
        let server = spawn_scripted_websocket(vec![ScriptedWsAction::Complete {
            response_id: "ws-matrix",
            message_id: "ws-message",
            text: "done",
        }])
        .await;
        let mut provider = ProviderHandle::new(
            CodexProvider::new("access", "refresh", 0)
                .force_websocket_transport()
                .with_endpoint_urls("http://127.0.0.1:9/unused-sse", server.url.clone())
                .with_options(ProviderOptions {
                    reliability: lash_core::provider::ProviderReliability::disabled(),
                    ..ProviderOptions::default()
                })
                .into_components(),
        );
        let mut request = dialect.request();
        let expected = setting.apply(&mut request);
        let result = provider.complete(request).await;
        let captured = server.captured();
        if setting.supported(dialect) {
            let completion = result.unwrap_or_else(|error| panic!("{setting:?}: {error}"));
            assert_eq!(captured.len(), 1, "{setting:?}");
            assert_eq!(
                captured[0].pointer(dialect.field_path(setting)),
                Some(&expected),
                "{setting:?}"
            );
            assert_eq!(
                setting.receipt(&completion.generation_disposition.expect("receipt")),
                Outcome::Applied,
                "{setting:?}"
            );
        } else {
            let error = result.expect_err("unsupported setting");
            assert_eq!(
                error
                    .error
                    .code
                    .as_ref()
                    .map(ToString::to_string)
                    .as_deref(),
                Some("lash:unsupported_generation_option"),
                "{setting:?}"
            );
            assert!(captured.is_empty(), "{setting:?} reached transport");
        }
    }
    eprintln!(
        "websocket generation disposition matrix: 5 cases in {:?}",
        start.elapsed()
    );
}

async fn retention_is_projected_or_refused_before_io() {
    for dialect in HTTP_DIALECTS {
        let cap = matches!(dialect, Dialect::Anthropic).then_some(4096);
        let (result, calls) = run(dialect, dialect.request(), cap).await;
        let (_, receipt) = result.unwrap_or_else(|error| panic!("{dialect:?}: {error}"));
        assert_eq!(calls, 1, "{dialect:?}");
        assert_eq!(
            receipt.reasoning_retention,
            Outcome::NotRequested,
            "{dialect:?} default retention"
        );
        let mut unsupported = dialect.request();
        *unsupported.model_capability.reasoning_retention = if matches!(dialect, Dialect::Anthropic)
        {
            ReasoningRetentionPolicy {
                capability: Some(ReasoningRetentionCapability::OpenAiContext {
                    supported: vec![OpenAiReasoningContext::CurrentTurn],
                }),
                selection: ReasoningRetentionSelection::OpenAiContext {
                    context: OpenAiReasoningContext::CurrentTurn,
                },
            }
        } else {
            ReasoningRetentionPolicy {
                capability: Some(ReasoningRetentionCapability::AnthropicClearThinking),
                selection: ReasoningRetentionSelection::AnthropicClearThinking {
                    keep: AnthropicThinkingRetention::All,
                },
            }
        };
        let (result, calls) = run(dialect, unsupported, cap).await;
        assert_eq!(
            result.unwrap_err(),
            TurnFailureCode::UnsupportedReasoningRetention.into(),
            "{dialect:?}"
        );
        assert_eq!(calls, 0, "{dialect:?}");

        let mut supported = dialect.request();
        let expected = match dialect {
            Dialect::Anthropic => {
                *supported.model_capability.reasoning_retention = ReasoningRetentionPolicy {
                    capability: Some(ReasoningRetentionCapability::AnthropicClearThinking),
                    selection: ReasoningRetentionSelection::AnthropicClearThinking {
                        keep: AnthropicThinkingRetention::All,
                    },
                };
                Some(("/context_management/edits/0/keep", json!("all")))
            }
            Dialect::OpenAiResponses | Dialect::CodexSse => {
                *supported.model_capability.reasoning_retention = ReasoningRetentionPolicy {
                    capability: Some(ReasoningRetentionCapability::OpenAiContext {
                        supported: vec![OpenAiReasoningContext::CurrentTurn],
                    }),
                    selection: ReasoningRetentionSelection::OpenAiContext {
                        context: OpenAiReasoningContext::CurrentTurn,
                    },
                };
                Some(("/reasoning/context", json!("current_turn")))
            }
            _ => {
                *supported.model_capability.reasoning_retention = ReasoningRetentionPolicy {
                    capability: Some(ReasoningRetentionCapability::ClientSideUserSegments),
                    selection: ReasoningRetentionSelection::ClientSideUserSegments {
                        max_segments: NonZeroUsize::new(1).expect("positive"),
                    },
                };
                supported.messages = vec![
                    LlmMessage::text(LlmRole::User, "old matrix segment").with_user_segment_start(),
                    LlmMessage::text(LlmRole::Assistant, "old answer"),
                    LlmMessage::text(LlmRole::User, "new matrix segment").with_user_segment_start(),
                ];
                None
            }
        };
        let (result, calls) = run(dialect, supported, cap).await;
        let (body, receipt) = result.unwrap_or_else(|error| panic!("{dialect:?}: {error}"));
        assert_eq!(calls, 1, "{dialect:?}");
        assert_eq!(receipt.reasoning_retention, Outcome::Applied, "{dialect:?}");
        if let Some((path, value)) = expected {
            assert_eq!(body.pointer(path), Some(&value), "{dialect:?}: {body}");
        } else {
            assert!(
                !body.to_string().contains("old matrix segment"),
                "{dialect:?}: {body}"
            );
            assert!(
                body.to_string().contains("new matrix segment"),
                "{dialect:?}: {body}"
            );
        }
    }
}

#[tokio::test]
async fn generation_disposition_matrix() {
    let start = Instant::now();
    every_simple_generation_control_is_sent_or_refused_before_io().await;
    runtime_clamps_a_requested_cap_and_reports_the_reduced_wire_value().await;
    protocol_owned_stop_is_absent_from_the_wire_and_reported_suppressed().await;
    unset_controls_invent_no_generation_fields().await;
    pinned_sampling_and_mandatory_reasoning_refuse_without_io().await;
    reasoning_selection_uses_only_the_declared_dialect().await;
    passthrough_is_sent_or_refused_without_io().await;
    replay_of_generation_intent_has_the_same_body_and_receipt().await;
    route_headers_are_sent_or_refused_before_io().await;
    thinking_visibility_and_summary_have_distinct_receipts().await;
    receipt_survives_a_failure_after_send().await;
    websocket_generation_settings_have_the_same_dispositions_as_sse().await;
    retention_is_projected_or_refused_before_io().await;
    eprintln!(
        "generation disposition matrix: 188 cases in {:?}",
        start.elapsed()
    );
}
