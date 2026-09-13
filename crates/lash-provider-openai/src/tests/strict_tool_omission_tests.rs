use super::*;

use async_trait::async_trait;
use lash::tools::{StaticToolExecute, StaticToolProvider, ToolCall, ToolDefinition, ToolOutcome};
use lash::{LashCore, TurnInput};
use lash_core::llm::types::{LlmContentBlock, LlmRole};
use lash_core::{
    EffectAddress, ExecutionScope, LlmRequestSpec, RuntimeAttribution, RuntimeEffectCommand,
    RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectInvocation, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
};
use lash_sqlite_store::SqliteRuntimeEffectController;
use std::sync::Mutex;

const TOOL_NAME: &str = "strict_omission_probe";
const DEFAULT_LIMIT: usize = 37;

#[derive(Clone, Copy, Debug)]
enum Endpoint {
    Chat,
    Responses,
}

impl Endpoint {
    fn label(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Responses => "responses",
        }
    }
}

#[derive(Debug)]
struct CapturingScriptedTransport {
    responses: Mutex<VecDeque<String>>,
    requests: Mutex<Vec<LlmHttpRequest>>,
}

impl CapturingScriptedTransport {
    fn new(responses: impl IntoIterator<Item = String>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn request_bodies(&self) -> Vec<Value> {
        self.requests
            .lock_recover()
            .iter()
            .map(|request| serde_json::from_slice(&request.body).expect("JSON request body"))
            .collect()
    }
}

#[async_trait]
impl LlmHttpTransport for CapturingScriptedTransport {
    async fn send(
        &self,
        request: LlmHttpRequest,
        _timeout: Option<std::time::Duration>,
    ) -> Result<lash_llm_transport::LlmHttpResponse, LlmTransportError> {
        self.requests.lock_recover().push(request);
        let body = self
            .responses
            .lock_recover()
            .pop_front()
            .expect("scripted response");
        Ok(lash_llm_transport::LlmHttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: LlmHttpBody::buffered(body),
        })
    }
}

#[derive(Debug, PartialEq)]
struct CapturedCall {
    args: Value,
    limit: Option<usize>,
}

struct OmissionProbe {
    seen: Arc<Mutex<Vec<CapturedCall>>>,
}

#[async_trait]
impl StaticToolExecute for OmissionProbe {
    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        let limit = match lash_tool_support::parse_optional_usize_arg(
            call.args,
            "limit",
            Some(DEFAULT_LIMIT),
            false,
            1,
        ) {
            Ok(limit) => limit,
            Err(outcome) => return outcome,
        };
        self.seen.lock_recover().push(CapturedCall {
            args: call.args.clone(),
            limit,
        });
        ToolOutcome::ok(json!({ "limit": limit }))
    }
}

fn tool_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "required_name": { "type": "string" },
            "limit": { "type": "integer", "minimum": 1 },
            "nullable_note": { "type": ["string", "null"] },
            "nullable_referenced": { "$ref": "#/$defs/Nullable" },
            "nested": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "optional_count": { "type": "integer" }
                },
                "required": ["id"],
                "additionalProperties": false
            },
            "rows": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "optional_count": { "type": "integer" }
                    },
                    "required": ["id"],
                    "additionalProperties": false
                }
            },
            "referenced": { "$ref": "#/$defs/Referenced" }
        },
        "required": ["required_name", "nested", "rows", "referenced"],
        "additionalProperties": false,
        "$defs": {
            "Nullable": { "type": ["string", "null"] },
            "Referenced": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "optional_count": { "type": "integer" }
                },
                "required": ["id"],
                "additionalProperties": false
            }
        }
    })
}

fn tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:strict_omission_probe",
        TOOL_NAME,
        "Capture strict omission behavior.",
        tool_input_schema(),
        json!({
            "type": "object",
            "properties": { "limit": { "type": ["integer", "null"] } },
            "required": ["limit"],
            "additionalProperties": false
        }),
    )
}

fn provider(
    endpoint: Endpoint,
    strict_tools: bool,
    transport: Arc<CapturingScriptedTransport>,
) -> ProviderHandle {
    let compat = OpenAiCompat {
        strict_tools: Some(strict_tools),
        ..OpenAiCompat::default()
    };
    match endpoint {
        Endpoint::Chat => ProviderHandle::new(
            OpenAiCompatibleProvider::new("key", "https://openai.test/v1")
                .with_compat(compat)
                .with_transport(transport)
                .into_components(),
        ),
        Endpoint::Responses => {
            let mut provider = OpenAiProvider::new("key").with_transport(transport);
            provider.inner.compat = compat;
            ProviderHandle::new(provider.into_components())
        }
    }
}

fn core(provider: ProviderHandle, seen: Arc<Mutex<Vec<CapturedCall>>>, label: &str) -> LashCore {
    LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .without_queued_work()
        .provider(provider)
        .model(
            lash::ModelSpec::builder("gpt-5.4")
                .context_window_tokens(16_000)
                .build()
                .expect("valid model spec"),
        )
        .tools(Arc::new(StaticToolProvider::new(
            vec![tool_definition()],
            OmissionProbe { seen },
        )))
        .effect_host(Arc::new(lash::durability::NativeEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .store_factory(Arc::new(
            lash::persistence::InMemorySessionStoreFactory::new(),
        ))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            format!("strict-omission-{label}"),
            format!("strict-omission-{label}-boot"),
        ))
        .expect("core")
}

fn tool_response(endpoint: Endpoint, arguments: &Value) -> String {
    let arguments = serde_json::to_string(arguments).expect("arguments JSON");
    match endpoint {
        Endpoint::Chat => json!({
            "id": "chat-tool",
            "model": "gpt-5.4",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": { "name": TOOL_NAME, "arguments": arguments }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })
        .to_string(),
        Endpoint::Responses => json!({
            "id": "response-tool",
            "model": "gpt-5.4",
            "status": "completed",
            "output": [{
                "type": "function_call",
                "id": "fc-1",
                "call_id": "call-1",
                "name": TOOL_NAME,
                "arguments": arguments,
                "status": "completed"
            }]
        })
        .to_string(),
    }
}

fn final_response(endpoint: Endpoint) -> String {
    match endpoint {
        Endpoint::Chat => json!({
            "id": "chat-final",
            "model": "gpt-5.4",
            "choices": [{
                "message": { "role": "assistant", "content": "done" },
                "finish_reason": "stop"
            }]
        })
        .to_string(),
        Endpoint::Responses => json!({
            "id": "response-final",
            "model": "gpt-5.4",
            "status": "completed",
            "output": [{
                "type": "message",
                "id": "message-1",
                "role": "assistant",
                "status": "completed",
                "content": [{ "type": "output_text", "text": "done" }]
            }]
        })
        .to_string(),
    }
}

struct CaseResult {
    seen: Arc<Mutex<Vec<CapturedCall>>>,
    requests: Vec<Value>,
}

async fn run_case(
    endpoint: Endpoint,
    strict_tools: bool,
    arguments: Value,
    label: &str,
) -> CaseResult {
    let transport = Arc::new(CapturingScriptedTransport::new([
        tool_response(endpoint, &arguments),
        final_response(endpoint),
    ]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let runtime = core(
        provider(endpoint, strict_tools, Arc::clone(&transport)),
        Arc::clone(&seen),
        label,
    );
    let session = runtime
        .session(format!("strict-omission-{label}"))
        .open()
        .await
        .expect("session");
    let result = session
        .turn(TurnInput::text("Call the probe."))
        .run()
        .await
        .expect("turn");
    assert_eq!(result.assistant_message(), Some("done"));
    CaseResult {
        seen,
        requests: transport.request_bodies(),
    }
}

fn strict_arguments() -> Value {
    json!({
        "required_name": "ok",
        "limit": null,
        "nullable_note": null,
        "nullable_referenced": null,
        "nested": { "id": "nested", "optional_count": null },
        "rows": [
            { "id": "first", "optional_count": null },
            { "id": "second", "optional_count": 9 }
        ],
        "referenced": { "id": "ref", "optional_count": null }
    })
}

fn canonical_arguments() -> Value {
    json!({
        "required_name": "ok",
        "nullable_note": null,
        "nullable_referenced": null,
        "nested": { "id": "nested" },
        "rows": [{ "id": "first" }, { "id": "second", "optional_count": 9 }],
        "referenced": { "id": "ref" }
    })
}

fn advertised_tool(endpoint: Endpoint, request: &Value) -> &Value {
    request["tools"]
        .as_array()
        .expect("advertised tools")
        .iter()
        .map(|tool| match endpoint {
            Endpoint::Chat => &tool["function"],
            Endpoint::Responses => tool,
        })
        .find(|tool| tool["name"] == TOOL_NAME)
        .expect("advertised omission probe")
}

fn recorded_arguments(endpoint: Endpoint, request: &Value) -> Value {
    let item = match endpoint {
        Endpoint::Chat => request["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .find_map(|message| message["tool_calls"].as_array()?.first())
            .expect("recorded Chat tool call"),
        Endpoint::Responses => request["input"]
            .as_array()
            .expect("input")
            .iter()
            .find(|item| item["type"] == "function_call")
            .expect("recorded Responses tool call"),
    };
    let encoded = match endpoint {
        Endpoint::Chat => item["function"]["arguments"].as_str(),
        Endpoint::Responses => item["arguments"].as_str(),
    }
    .expect("encoded arguments");
    serde_json::from_str(encoded).expect("recorded arguments JSON")
}

async fn strict_omission_round_trip(endpoint: Endpoint) {
    let result = run_case(
        endpoint,
        true,
        strict_arguments(),
        &format!("{}-round-trip", endpoint.label()),
    )
    .await;
    assert_eq!(result.requests.len(), 2);

    let advertised = advertised_tool(endpoint, &result.requests[0]);
    assert_eq!(advertised["strict"], true);
    assert_eq!(
        advertised["parameters"]["required"],
        json!([
            "limit",
            "nested",
            "nullable_note",
            "nullable_referenced",
            "referenced",
            "required_name",
            "rows"
        ])
    );
    assert_eq!(
        advertised["parameters"]["properties"]["limit"]["type"],
        json!(["integer", "null"])
    );

    assert_eq!(
        result.seen.lock_recover().as_slice(),
        [CapturedCall {
            args: canonical_arguments(),
            limit: Some(DEFAULT_LIMIT),
        }]
    );
    assert_eq!(
        recorded_arguments(endpoint, &result.requests[1]),
        canonical_arguments(),
        "the follow-up built from the journaled effect must contain canonical arguments"
    );
}

#[tokio::test]
async fn chat_strict_default_nullable_nested_array_ref_and_journal_round_trip() {
    strict_omission_round_trip(Endpoint::Chat).await;
}

#[tokio::test]
async fn responses_strict_default_nullable_nested_array_ref_and_journal_round_trip() {
    strict_omission_round_trip(Endpoint::Responses).await;
}

async fn rejected_before_dispatch(endpoint: Endpoint, arguments: Value, field: &str, label: &str) {
    let result = run_case(endpoint, true, arguments, label).await;
    assert!(result.seen.lock_recover().is_empty());
    let follow_up = result.requests[1].to_string();
    assert!(
        follow_up.contains(field) && follow_up.contains("Tool execution failed"),
        "expected a validation error for {field}, got {follow_up}"
    );
}

#[tokio::test]
async fn chat_strict_required_nonnullable_null_is_rejected_before_dispatch() {
    let mut arguments = strict_arguments();
    arguments["required_name"] = Value::Null;
    rejected_before_dispatch(
        Endpoint::Chat,
        arguments,
        "required_name",
        "chat-required-null",
    )
    .await;
}

#[tokio::test]
async fn responses_strict_required_nonnullable_null_is_rejected_before_dispatch() {
    let mut arguments = strict_arguments();
    arguments["required_name"] = Value::Null;
    rejected_before_dispatch(
        Endpoint::Responses,
        arguments,
        "required_name",
        "responses-required-null",
    )
    .await;
}

#[tokio::test]
async fn chat_strict_wrong_type_is_rejected_before_dispatch() {
    let mut arguments = strict_arguments();
    arguments["required_name"] = json!(42);
    rejected_before_dispatch(
        Endpoint::Chat,
        arguments,
        "required_name",
        "chat-wrong-type",
    )
    .await;
}

#[tokio::test]
async fn responses_strict_wrong_type_is_rejected_before_dispatch() {
    let mut arguments = strict_arguments();
    arguments["required_name"] = json!(42);
    rejected_before_dispatch(
        Endpoint::Responses,
        arguments,
        "required_name",
        "responses-wrong-type",
    )
    .await;
}

async fn non_strict_optional_null_is_rejected(endpoint: Endpoint) {
    let arguments = json!({
        "required_name": "ok",
        "limit": null,
        "nested": { "id": "nested" },
        "rows": [],
        "referenced": { "id": "ref" }
    });
    let result = run_case(
        endpoint,
        false,
        arguments,
        &format!("{}-non-strict-null", endpoint.label()),
    )
    .await;
    assert!(result.seen.lock_recover().is_empty());
    let advertised = advertised_tool(endpoint, &result.requests[0]);
    assert_eq!(advertised["strict"], false);
    assert_eq!(
        advertised["parameters"]["properties"]["limit"]["type"],
        "integer"
    );
    assert!(
        !advertised["parameters"]["required"]
            .as_array()
            .expect("required")
            .contains(&json!("limit"))
    );
    assert_eq!(
        recorded_arguments(endpoint, &result.requests[1])["limit"],
        Value::Null
    );
    assert!(
        result.requests[1]
            .to_string()
            .contains("Tool execution failed")
    );
}

#[tokio::test]
async fn chat_non_strict_optional_null_is_not_normalized() {
    non_strict_optional_null_is_rejected(Endpoint::Chat).await;
}

#[tokio::test]
async fn responses_non_strict_optional_null_is_not_normalized() {
    non_strict_optional_null_is_rejected(Endpoint::Responses).await;
}

fn replay_request_with_canonical_call() -> LlmRequest {
    let mut req = request(vec![LlmMessage::new(
        LlmRole::Assistant,
        vec![LlmContentBlock::ToolCall {
            call_id: "call-1".to_string(),
            tool_name: TOOL_NAME.to_string(),
            input_json: serde_json::to_string(&canonical_arguments()).unwrap(),
            replay: None,
        }],
    )]);
    req.tools = Arc::new(vec![LlmToolSpec {
        name: TOOL_NAME.to_string(),
        description: "Capture strict omission behavior.".to_string(),
        input_schema: tool_input_schema().into(),
        output_schema: json!({ "type": "object" }).into(),
    }]);
    req
}

fn assert_journaled_call_replay_ignores_strict_toggle(endpoint: Endpoint) {
    let req = replay_request_with_canonical_call();
    let chat_body = |strict_tools| {
        OpenAiCompatibleProvider::new("key", "https://openai.test/v1")
            .with_compat(OpenAiCompat {
                strict_tools: Some(strict_tools),
                ..OpenAiCompat::default()
            })
            .build_chat_request_body(&req, false)
            .unwrap()
    };
    let responses_body = |strict_tools| {
        let mut provider = OpenAiProvider::new("key");
        provider.inner.compat.strict_tools = Some(strict_tools);
        provider.build_responses_request_body(&req, false).unwrap()
    };

    let (strict, non_strict) = match endpoint {
        Endpoint::Chat => (chat_body(true), chat_body(false)),
        Endpoint::Responses => (responses_body(true), responses_body(false)),
    };
    assert_eq!(
        recorded_arguments(endpoint, &strict),
        recorded_arguments(endpoint, &non_strict)
    );
}

#[test]
fn chat_journaled_canonical_call_replays_identically_when_strict_tools_toggle() {
    assert_journaled_call_replay_ignores_strict_toggle(Endpoint::Chat);
}

#[test]
fn responses_journaled_canonical_call_replays_identically_when_strict_tools_toggle() {
    assert_journaled_call_replay_ignores_strict_toggle(Endpoint::Responses);
}

fn effect_request_spec(request: &LlmRequest) -> LlmRequestSpec {
    LlmRequestSpec {
        instructions: request.instructions.clone(),
        model: request.model.clone(),
        messages: request.messages.clone(),
        tools: Arc::clone(&request.tools),
        tool_choice: request.tool_choice.clone(),
        model_variant: request.model_variant.clone(),
        model_capability: request.model_capability.clone(),
        generation: request.generation.clone(),
        scope: request.scope.clone(),
        output_spec: request.output_spec.clone(),
    }
}

fn tool_arguments_from_effect(outcome: &RuntimeEffectOutcome) -> Value {
    let RuntimeEffectOutcome::LlmCall { result, .. } = outcome else {
        panic!("expected journaled LLM-call outcome");
    };
    let response = result
        .as_ref()
        .as_ref()
        .expect("journaled successful provider response");
    let input_json = response
        .parts
        .iter()
        .find_map(|part| match part {
            lash_core::LlmOutputPart::ToolCall { input_json, .. } => Some(input_json),
            _ => None,
        })
        .expect("journaled tool call");
    serde_json::from_str(input_json).expect("journaled tool arguments JSON")
}

async fn persisted_effect_replay_ignores_strict_toggle(endpoint: Endpoint) {
    let dir = tempfile::tempdir().expect("effect replay tempdir");
    let journal_path = dir
        .path()
        .join(format!("{}-effects.sqlite", endpoint.label()));
    let scope = ExecutionScope::turn(
        format!("{}-replay-session", endpoint.label()),
        format!("{}-replay-turn", endpoint.label()),
    );
    let provider_request = replay_request_with_canonical_call();
    let envelope = RuntimeEffectEnvelope::new(
        RuntimeEffectInvocation::new(
            EffectAddress::new(scope.clone(), "strict-tool-call").expect("valid effect address"),
            RuntimeAttribution::for_turn(
                format!("{}-replay-session", endpoint.label()),
                format!("{}-replay-turn", endpoint.label()),
                0,
                0,
            ),
            "strict-tool-call",
        ),
        RuntimeEffectCommand::LlmCall {
            request: Box::new(effect_request_spec(&provider_request)),
        },
    );

    let strict_transport = Arc::new(CapturingScriptedTransport::new([tool_response(
        endpoint,
        &strict_arguments(),
    )]));
    let mut strict_provider = provider(endpoint, true, Arc::clone(&strict_transport));
    let controller = SqliteRuntimeEffectController::open(&journal_path, scope.clone())
        .await
        .expect("open durable effect controller");
    let first = controller
        .execute_effect(
            envelope.clone(),
            RuntimeEffectLocalExecutor::testing(move |_| async move {
                let completion =
                    strict_provider
                        .complete(provider_request)
                        .await
                        .map_err(|error| {
                            RuntimeEffectControllerError::foreign(
                                "strict_provider_call_failed",
                                error.to_string(),
                            )
                        })?;
                Ok(RuntimeEffectOutcome::LlmCall {
                    result: Box::new(Ok(completion.response)),
                    text_streamed: false,
                    call_record: Some(completion.call_record),
                })
            }),
        )
        .await
        .expect("record normalized LLM outcome");
    assert_eq!(tool_arguments_from_effect(&first), canonical_arguments());
    assert_eq!(strict_transport.request_bodies().len(), 1);
    drop(controller);

    let toggled_transport = Arc::new(CapturingScriptedTransport::new([tool_response(
        endpoint,
        &strict_arguments(),
    )]));
    let mut toggled_provider = provider(endpoint, false, Arc::clone(&toggled_transport));
    let replay_request = replay_request_with_canonical_call();
    let replay_controller = SqliteRuntimeEffectController::open(&journal_path, scope)
        .await
        .expect("restore durable effect controller");
    replay_controller.start_replay();
    let replayed = replay_controller
        .execute_effect(
            envelope,
            RuntimeEffectLocalExecutor::testing(move |_| async move {
                let completion =
                    toggled_provider
                        .complete(replay_request)
                        .await
                        .map_err(|error| {
                            RuntimeEffectControllerError::foreign(
                                "toggled_provider_call_failed",
                                error.to_string(),
                            )
                        })?;
                Ok(RuntimeEffectOutcome::LlmCall {
                    result: Box::new(Ok(completion.response)),
                    text_streamed: false,
                    call_record: Some(completion.call_record),
                })
            }),
        )
        .await
        .expect("replay persisted normalized LLM outcome");

    assert_eq!(tool_arguments_from_effect(&replayed), canonical_arguments());
    assert_eq!(
        serde_json::to_value(&replayed).expect("encode replayed outcome"),
        serde_json::to_value(&first).expect("encode recorded outcome")
    );
    assert!(
        toggled_transport.request_bodies().is_empty(),
        "persisted replay must not invoke the provider or rerun normalization"
    );
}

#[tokio::test]
async fn chat_persisted_effect_replay_ignores_strict_tools_toggle() {
    persisted_effect_replay_ignores_strict_toggle(Endpoint::Chat).await;
}

#[tokio::test]
async fn responses_persisted_effect_replay_ignores_strict_tools_toggle() {
    persisted_effect_replay_ignores_strict_toggle(Endpoint::Responses).await;
}

#[test]
fn strict_decoder_leaves_override_and_ref_backed_ambiguous_union_nulls_untouched() {
    let override_schema = json!({
        "type": "object",
        "properties": { "value": { "type": ["integer", "null"] } },
        "required": ["value"],
        "additionalProperties": false
    });
    let mut req = request(Vec::new());
    req.tools = Arc::new(vec![LlmToolSpec {
        name: "override_probe".to_string(),
        description: "override".to_string(),
        input_schema: lash_sansio::SchemaContract::new(json!({
            "type": "object",
            "properties": { "value": { "type": "integer" } }
        }))
        .with_override(
            lash_sansio::SchemaDialect::OPENAI_STRICT_TOOL_PARAMETERS,
            override_schema,
        ),
        output_schema: json!({}).into(),
    }]);
    let capabilities = lash_sansio::ProviderSchemaCapabilities::openai(true);
    let override_decoder = crate::responses_shared::ToolArgumentDecoder::for_request(
        "test",
        &req,
        true,
        &capabilities,
    )
    .unwrap();
    assert_eq!(
        override_decoder.decode("override_probe", r#"{"value":null}"#.to_string()),
        r#"{"value":null}"#
    );

    req.tools = Arc::new(vec![LlmToolSpec {
        name: "union_probe".to_string(),
        description: "union".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "choice": {
                    "anyOf": [
                        { "$ref": "#/$defs/Left" },
                        { "$ref": "#/$defs/Right" }
                    ]
                }
            },
            "required": ["choice"],
            "$defs": {
                "Left": {
                    "type": "object",
                    "properties": {
                        "kind": { "const": "left" },
                        "value": { "type": "integer" }
                    },
                    "required": ["kind"],
                    "additionalProperties": false
                },
                "Right": {
                    "type": "object",
                    "properties": {
                        "kind": { "const": "right" },
                        "value": { "type": ["integer", "null"] }
                    },
                    "required": ["kind", "value"],
                    "additionalProperties": false
                }
            }
        })
        .into(),
        output_schema: json!({}).into(),
    }]);
    let union_decoder = crate::responses_shared::ToolArgumentDecoder::for_request(
        "test",
        &req,
        true,
        &capabilities,
    )
    .unwrap();
    let arguments = r#"{"choice":{"kind":"left","value":null}}"#;
    assert_eq!(
        union_decoder.decode("union_probe", arguments.to_string()),
        arguments
    );
}

#[test]
fn strict_decoder_preserves_nested_ref_union_omission_null() {
    let mut req = request(Vec::new());
    req.tools = Arc::new(vec![LlmToolSpec {
        name: "nested_union_probe".to_string(),
        description: "nested union".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "choice": {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": { "v": { "$ref": "#/$defs/A" } },
                            "required": ["v"],
                            "additionalProperties": false
                        },
                        {
                            "type": "object",
                            "properties": { "v": { "$ref": "#/$defs/B" } },
                            "required": ["v"],
                            "additionalProperties": false
                        }
                    ]
                }
            },
            "required": ["choice"],
            "$defs": {
                "A": {
                    "type": "object",
                    "properties": { "n": { "type": "integer" } },
                    "additionalProperties": false
                },
                "B": {
                    "type": "object",
                    "properties": { "n": { "type": ["integer", "null"] } },
                    "required": ["n"],
                    "additionalProperties": false
                }
            }
        })
        .into(),
        output_schema: json!({}).into(),
    }]);
    let decoder = crate::responses_shared::ToolArgumentDecoder::for_request(
        "test",
        &req,
        true,
        &lash_sansio::ProviderSchemaCapabilities::openai(true),
    )
    .unwrap();
    let arguments = r#"{"choice":{"v":{"n":null}}}"#;

    assert_eq!(
        decoder.decode("nested_union_probe", arguments.to_string()),
        arguments
    );
}

#[test]
fn strict_decoder_strips_single_branch_all_of_omission_null() {
    let mut req = request(Vec::new());
    req.tools = Arc::new(vec![LlmToolSpec {
        name: "all_of_probe".to_string(),
        description: "single-branch allOf".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "limit": {
                    "allOf": [{ "type": "integer" }],
                    "default": 37
                }
            }
        })
        .into(),
        output_schema: json!({}).into(),
    }]);
    let decoder = crate::responses_shared::ToolArgumentDecoder::for_request(
        "test",
        &req,
        true,
        &lash_sansio::ProviderSchemaCapabilities::openai(true),
    )
    .unwrap();

    assert_eq!(
        decoder.decode("all_of_probe", r#"{"limit":null}"#.to_string()),
        "{}"
    );
}
