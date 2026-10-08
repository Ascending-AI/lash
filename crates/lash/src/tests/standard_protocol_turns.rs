//! Standard-protocol turn laws on the durable engine: a facade core over
//! SQLite memory stores runs the turn its session was sent, and the law reads
//! the committed state and what its scripted model was shown.

use super::*;
use lash_core::{MessageRole, PartKind, ProcessEventLogTestSupport as _};

/// A scripted model: each request is recorded, and the `n`th one answers
/// `answers(n)`.
fn scripted_provider(
    requests: Arc<StdMutex<Vec<LlmRequest>>>,
    answers: fn(usize) -> LlmResponse,
) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("standard-protocol-turns")
        .complete(move |request: LlmRequest| {
            let mut seen = requests.lock_recover();
            seen.push(request);
            let response = answers(seen.len());
            async move { Ok(response) }
        })
        .build()
        .into_handle()
}

/// A text answer whose prose is split by a whitespace-only text part and a
/// reasoning part.
fn whitespace_interleaved(_: usize) -> LlmResponse {
    LlmResponse {
        parts: vec![
            LlmOutputPart::Text {
                text: "a".to_string(),
                response_meta: None,
            },
            LlmOutputPart::Text {
                text: "   ".to_string(),
                response_meta: None,
            },
            LlmOutputPart::Reasoning {
                text: "r".to_string(),
                replay: None,
            },
            LlmOutputPart::Text {
                text: "b".to_string(),
                response_meta: None,
            },
        ],
        ..LlmResponse::default()
    }
}

/// A whitespace-only text part between two prose parts does not split the
/// turn's terminal answer into two assistant messages: the committed history
/// holds one assistant message whose prose and reasoning parts keep their
/// arrival order, and the turn's answer is that message's text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whitespace_only_text_does_not_split_terminal_history() {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(
        scripted_provider(Arc::clone(&requests), whitespace_interleaved),
        mock_llm_profile_spec(),
    )
    .build(crate::testing::runtime_lease_owner())
    .expect("standard core");
    let session = core
        .session(crate::SessionId::from("whitespace-response-session"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let output = session
        .send(crate::TurnInput::text("respond with mixed parts"))
        .output()
        .await
        .expect("the turn answers");
    assert!(output.is_success(), "{output:?}");

    let read_view = output.result.state.read_view();
    let assistant_messages = read_view
        .messages()
        .iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .collect::<Vec<_>>();
    assert_eq!(
        assistant_messages.len(),
        1,
        "the terminal output must not materialize a duplicate assistant message"
    );
    let stored = assistant_messages[0];
    assert_eq!(
        stored
            .parts
            .iter()
            .map(|part| part.kind())
            .collect::<Vec<_>>(),
        [PartKind::Prose, PartKind::Reasoning, PartKind::Prose]
    );
    let rendered_text = stored
        .parts
        .iter()
        .filter(|part| {
            matches!(
                part.kind(),
                PartKind::Prose | PartKind::Text | PartKind::Attachment | PartKind::ToolResult
            )
        })
        .map(|part| part.content())
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(output.assistant_message(), Some(rendered_text.as_str()));
    assert_eq!(requests.lock_recover().len(), 1);
    core.shutdown().await.expect("shutdown");
}

/// The model's raw, invalid argument text for its `status` call.
const MALFORMED_ARGS: &str = r#"{"path": "a.txt", "content": "he said "hi"}"#;

/// The first request calls `status` with arguments that are not JSON; every
/// later one answers `done`.
fn malformed_then_done(n: usize) -> LlmResponse {
    if n == 1 {
        return LlmResponse {
            parts: vec![LlmOutputPart::ToolCall {
                call_id: "malformed-call".to_string(),
                tool_name: "status".to_string(),
                input_json: MALFORMED_ARGS.to_string(),
                replay: None,
            }],
            ..LlmResponse::default()
        };
    }
    text_response("done")
}

/// A `status` tool that counts its executions.
struct CountingTools {
    executed: Arc<AtomicUsize>,
}

fn status_tool() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:status",
        "status",
        "",
        serde_json::json!({
            "type": "object",
            "properties": {
                "value": { "type": "string" }
            },
            "additionalProperties": true
        }),
        serde_json::json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
}

#[async_trait]
impl ToolProvider for CountingTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![status_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "status").then(|| Arc::new(status_tool().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.executed.fetch_add(1, Ordering::SeqCst);
        lash_core::ToolOutcome::ok(serde_json::json!("ran")).into()
    }
}

/// A tool call whose arguments are not valid JSON is refused, never
/// dispatched: the tool body never runs, the model's next request states the
/// typed refusal, and the committed history keeps the model's raw argument
/// text verbatim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_tool_arguments_are_refused_not_dispatched() {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let executed = Arc::new(AtomicUsize::new(0));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(
        scripted_provider(Arc::clone(&requests), malformed_then_done),
        mock_llm_profile_spec(),
    )
    .tools(Arc::new(CountingTools {
        executed: Arc::clone(&executed),
    }))
    .build(crate::testing::runtime_lease_owner())
    .expect("standard core");
    let session = core
        .session(crate::SessionId::from("malformed-args-session"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let output = session
        .send(crate::TurnInput::text("check status"))
        .output()
        .await
        .expect("the turn answers");
    assert!(output.is_success(), "{output:?}");
    assert_eq!(output.assistant_message(), Some("done"));
    assert_eq!(
        executed.load(Ordering::SeqCst),
        0,
        "a call whose arguments never parsed must never reach the tool body"
    );

    let requests = requests.lock_recover().clone();
    assert_eq!(
        requests.len(),
        2,
        "the refusal loops back to the model once"
    );
    let projected = format!("{:?}", requests[1].messages);
    for expected in [
        "invalid_tool_call_json",
        "not valid JSON",
        "line 1 column",
        "not executed",
    ] {
        assert!(
            projected.contains(expected),
            "the model-visible refusal must state `{expected}`: {projected}"
        );
    }

    let read_view = output.result.state.read_view();
    let tool_call_contents = read_view
        .messages()
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter(|part| part.kind() == PartKind::ToolCall)
        .map(|part| part.content().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        tool_call_contents,
        vec![MALFORMED_ARGS.to_string()],
        "history must keep the model's raw argument text unchanged"
    );
    core.shutdown().await.expect("shutdown");
}

/// The refusal the Standard protocol's discovery answers for a call to a
/// tool its request did not list.
const DISCOVERY_REFUSAL: &str = "Tool `catalog_only` was not listed in this request; use a listed discovery operation or batch.";

/// The first request calls the unlisted `catalog_only` and, when `mixed`,
/// the listed `tools.search`; every later one answers `done`.
fn discovery_calls(mixed: bool) -> fn(usize) -> LlmResponse {
    fn all(n: usize) -> LlmResponse {
        discovery_response(n, false)
    }
    fn mixed_calls(n: usize) -> LlmResponse {
        discovery_response(n, true)
    }
    if mixed { mixed_calls } else { all }
}

fn discovery_response(n: usize, mixed: bool) -> LlmResponse {
    if n > 1 {
        return text_response("done");
    }
    let mut parts = vec![LlmOutputPart::ToolCall {
        call_id: "refused-call".to_string(),
        tool_name: "catalog_only".to_string(),
        input_json: r#"{"probe":"refused"}"#.to_string(),
        replay: None,
    }];
    if mixed {
        parts.push(LlmOutputPart::ToolCall {
            call_id: "admitted-call".to_string(),
            tool_name: "tools.search".to_string(),
            input_json: r#"{"probe":"admitted"}"#.to_string(),
            replay: None,
        });
    }
    LlmResponse {
        parts,
        ..LlmResponse::default()
    }
}

/// `tools.search`, the listed discovery operation, and `catalog_only`, a
/// catalog tool no request lists; each counts its executions.
struct DiscoveryTools {
    admitted: Arc<AtomicUsize>,
    refused: Arc<AtomicUsize>,
}

fn discovery_tool(name: &str, inline: bool) -> lash_core::ToolDefinition {
    let mut tool = lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "discovery refusal regression tool",
        serde_json::json!({
            "type": "object",
            "properties": { "probe": { "type": "string" } },
            "required": ["probe"],
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas");
    tool.manifest.inline = inline;
    tool
}

#[async_trait]
impl ToolProvider for DiscoveryTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![
            discovery_tool("tools.search", true).manifest(),
            discovery_tool("catalog_only", false).manifest(),
        ]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        match name {
            "tools.search" => Some(Arc::new(discovery_tool(name, true).contract())),
            "catalog_only" => Some(Arc::new(discovery_tool(name, false).contract())),
            _ => None,
        }
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        match call.name() {
            "tools.search" => {
                self.admitted.fetch_add(1, Ordering::SeqCst);
                lash_core::ToolOutcome::ok(serde_json::json!("admitted")).into()
            }
            "catalog_only" => {
                self.refused.fetch_add(1, Ordering::SeqCst);
                lash_core::ToolOutcome::ok(serde_json::json!("must not run")).into()
            }
            name => panic!("unexpected discovery test tool: {name}"),
        }
    }
}

/// The trace records of `kind` for the call the provider named
/// `provider_call_id`, by position.
fn trace_positions(
    entries: &[serde_json::Value],
    provider_call_id: &str,
    kind: &str,
) -> Vec<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            entry.get("type").and_then(serde_json::Value::as_str) == Some(kind)
                && entry
                    .get("provider_call_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(provider_call_id)
        })
        .map(|(position, _)| position)
        .collect()
}

/// A discovery refusal is reported and accounted before the turn goes on:
/// the refused call never runs, it is recorded as one `unknown_tool` failure
/// whose refusal text reaches the next model request, and it and every
/// admitted call beside it have exactly one ordered start and completion in
/// the trace and in the turn's activity.
#[allow(
    clippy::disallowed_methods,
    reason = "the test host reads back the trace file it configured"
)]
async fn assert_discovery_refusal_is_reported_and_accounted(mixed: bool) {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let admitted = Arc::new(AtomicUsize::new(0));
    let refused = Arc::new(AtomicUsize::new(0));
    let trace_dir = tempfile::tempdir().expect("trace directory");
    let trace_path = trace_dir.path().join("discovery-refusal.jsonl");
    let core = explicit_ephemeral_facets(
        LashCore::builder(sqlite_memory_store_backend().await).protocol_plugin(Arc::new(
            lash_protocol_standard::StandardProtocolPluginFactory::with_config(
                lash_protocol_standard::StandardProtocolConfig {
                    discovery: Some(lash_core::ToolDiscovery {
                        operation: "tools.search".to_string(),
                    }),
                    ..lash_protocol_standard::StandardProtocolConfig::default()
                },
            ),
        )),
    )
    .serve_test_llm_profile(
        scripted_provider(Arc::clone(&requests), discovery_calls(mixed)),
        mock_llm_profile_spec(),
    )
    .tools(Arc::new(DiscoveryTools {
        admitted: Arc::clone(&admitted),
        refused: Arc::clone(&refused),
    }))
    .trace_sink(Arc::new(lash_trace::JsonlTraceSink::new(
        trace_path.clone(),
    )))
    .build(crate::testing::runtime_lease_owner())
    .expect("standard core with discovery");
    let session_id = if mixed {
        "discovery-refusal-mixed"
    } else {
        "discovery-refusal-all"
    };
    let session = core
        .session(crate::SessionId::from(session_id))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let output = session
        .send(crate::TurnInput::text("exercise discovery refusal"))
        .output()
        .await
        .expect("the turn answers");
    assert!(output.is_success(), "{output:?}");

    let requests = requests.lock_recover().clone();
    assert_eq!(requests.len(), 2, "the refusal continues the turn");
    assert!(
        format!("{:?}", requests[1].messages).contains(DISCOVERY_REFUSAL),
        "the refusal text reaches the next model request"
    );
    assert_eq!(refused.load(Ordering::SeqCst), 0);
    assert_eq!(admitted.load(Ordering::SeqCst), usize::from(mixed));
    assert_eq!(output.result.tool_calls.len(), if mixed { 2 } else { 1 });
    let refusal = output
        .result
        .tool_calls
        .iter()
        .find(|record| record.provider_call_id.as_deref() == Some("refused-call"))
        .expect("the refused call is accounted");
    let lash_core::ToolCallOutcome::Failure(failure) = &refusal.output.outcome else {
        panic!("the refused call stays a failure: {refusal:?}");
    };
    assert_eq!(failure.code, "unknown_tool");
    assert_eq!(failure.message, DISCOVERY_REFUSAL);

    core.shutdown().await.expect("shutdown");
    let entries = lash_core::facade_support::parse_jsonl_records::<serde_json::Value>(
        &std::fs::read_to_string(&trace_path).expect("read the discovery trace"),
    )
    .expect("trace entries");
    let calls: &[&str] = if mixed {
        &["refused-call", "admitted-call"]
    } else {
        &["refused-call"]
    };
    for call_id in calls {
        let started = trace_positions(&entries, call_id, "tool_call_started");
        let completed = trace_positions(&entries, call_id, "tool_call_completed");
        assert_eq!(started.len(), 1, "one start for {call_id}: {entries:?}");
        assert_eq!(
            completed.len(),
            1,
            "one completion for {call_id}: {entries:?}"
        );
        assert!(started[0] < completed[0], "ordered lifecycle for {call_id}");

        let lash_call_id = output
            .activities
            .iter()
            .find_map(|activity| match &activity.event {
                lash_core::TurnEvent::ToolCallStarted {
                    call_id: lash_call_id,
                    provider_call_id: Some(observed),
                    ..
                } if observed == call_id => Some(lash_call_id.clone()),
                _ => None,
            })
            .expect("the call started");
        let correlation = lash_core::TurnActivityId::new(format!("tool:{lash_call_id}"));
        let lifecycle = output
            .activities
            .iter()
            .filter_map(|activity| match &activity.event {
                lash_core::TurnEvent::ToolCallStarted {
                    call_id: observed, ..
                } if *observed == lash_call_id => Some(("started", &activity.correlation_id)),
                lash_core::TurnEvent::ToolCallCompleted {
                    call_id: observed, ..
                } if *observed == lash_call_id => Some(("completed", &activity.correlation_id)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            lifecycle,
            [("started", &correlation), ("completed", &correlation)],
            "exactly one ordered activity pair keyed by {call_id}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "FIG-5314: a durable turn's report omits a protocol-refused call and its trace has no tool lifecycle records"]
async fn all_discovery_refusals_are_reported_and_accounted_before_continuing() {
    assert_discovery_refusal_is_reported_and_accounted(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "FIG-5314: a durable turn's report omits a protocol-refused call and its trace has no tool lifecycle records"]
async fn mixed_discovery_refusals_and_admitted_calls_are_each_reported_once() {
    assert_discovery_refusal_is_reported_and_accounted(true).await;
}

/// A response's reasoning and final-answer text, each with its provider
/// metadata.
fn provider_parts() -> Vec<LlmOutputPart> {
    vec![
        LlmOutputPart::Reasoning {
            text: "reasoning summary".to_string(),
            replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                item_id: Some("reasoning-1".to_string()),
                encrypted_content: Some("opaque-reasoning".to_string()),
                ..Default::default()
            }),
        },
        LlmOutputPart::Text {
            text: "answer".to_string(),
            response_meta: Some(lash_core::llm::types::ResponseTextMeta {
                id: Some("message-1".to_string()),
                status: Some("completed".to_string()),
                phase: Some(lash_core::llm::types::ResponsePhase::FinalAnswer),
                provider_payload: Some("opaque-text".to_string()),
                ..Default::default()
            }),
        },
    ]
}

/// The provider parts alone: a final answer.
fn final_provider_response(_: usize) -> LlmResponse {
    LlmResponse {
        parts: provider_parts(),
        ..LlmResponse::default()
    }
}

/// The provider parts and a `lookup` call, then `done`.
fn tool_calling_provider_response(n: usize) -> LlmResponse {
    if n > 1 {
        return text_response("done");
    }
    let mut parts = provider_parts();
    parts.push(LlmOutputPart::ToolCall {
        call_id: "call-1".to_string(),
        tool_name: "lookup".to_string(),
        input_json: "{}".to_string(),
        replay: None,
    });
    LlmResponse {
        parts,
        ..LlmResponse::default()
    }
}

struct LookupTool;

fn lookup_tool() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:lookup",
        "lookup",
        "",
        serde_json::json!({ "type": "object", "additionalProperties": true }),
        serde_json::json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
}

#[async_trait]
impl ToolProvider for LookupTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![lookup_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "lookup").then(|| Arc::new(lookup_tool().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!("found")).into()
    }
}

/// The committed parts of the turn's assistant message that carries the
/// provider's text metadata, and how many model calls the turn made.
async fn persisted_provider_parts(
    answers: fn(usize) -> LlmResponse,
    session_id: &'static str,
) -> (Vec<lash_core::Part>, usize) {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let provider = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let requests = Arc::clone(&requests);
            move |request: LlmRequest| {
                let mut seen = requests.lock_recover();
                seen.push(request);
                let response = answers(seen.len());
                async move { Ok(response) }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .tools(Arc::new(LookupTool))
    .build(crate::testing::runtime_lease_owner())
    .expect("standard core");
    let session = core
        .session(crate::SessionId::from(session_id))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let output = session
        .send(crate::TurnInput::text("respond"))
        .output()
        .await
        .expect("the turn answers");
    assert!(output.is_success(), "{output:?}");
    let read_view = output.result.state.read_view();
    let parts = read_view
        .messages()
        .iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .find(|message| {
            message
                .parts
                .iter()
                .any(|part| part.response_meta().is_some())
        })
        .expect("the provider-bearing assistant message is committed")
        .parts
        .to_vec();
    let calls = requests.lock_recover().len();
    core.shutdown().await.expect("shutdown");
    (parts, calls)
}

/// A final answer and a tool-calling response commit identical typed
/// provider parts: the same reasoning replay and text metadata, each stamped
/// with the route that produced it, and the tool call after them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_and_tool_calling_responses_persist_identical_typed_provider_parts() {
    let (final_parts, final_calls) =
        persisted_provider_parts(final_provider_response, "final-provider-parts").await;
    let (tool_parts, tool_calls) = persisted_provider_parts(
        tool_calling_provider_response,
        "tool-calling-provider-parts",
    )
    .await;

    assert_eq!(
        final_parts
            .iter()
            .map(|part| part.kind())
            .collect::<Vec<_>>(),
        [PartKind::Reasoning, PartKind::Prose]
    );
    assert_eq!(
        tool_parts
            .iter()
            .map(|part| part.kind())
            .collect::<Vec<_>>(),
        [PartKind::Reasoning, PartKind::Prose, PartKind::ToolCall]
    );
    assert_eq!(final_calls, 1);
    assert_eq!(tool_calls, 2);
    assert_eq!(tool_parts[2].provider_call_id(), Some("call-1"));
    assert_eq!(tool_parts[2].tool_name(), Some("lookup"));

    let origin = lash_core::ProviderRouteIdentity::new("stub", "stub", "mock-model");
    let expected_reasoning = lash_core::llm::types::ProviderReasoningReplay {
        item_id: Some("reasoning-1".to_string()),
        encrypted_content: Some("opaque-reasoning".to_string()),
        origin: Some(origin.clone()),
        ..Default::default()
    };
    assert_eq!(final_parts[0].reasoning_meta(), Some(&expected_reasoning));
    assert_eq!(tool_parts[0].reasoning_meta(), Some(&expected_reasoning));
    let expected_text = lash_core::llm::types::ResponseTextMeta {
        id: Some("message-1".to_string()),
        status: Some("completed".to_string()),
        phase: Some(lash_core::llm::types::ResponsePhase::FinalAnswer),
        provider_payload: Some("opaque-text".to_string()),
        origin: Some(origin),
        ..Default::default()
    };
    assert_eq!(final_parts[1].response_meta(), Some(&expected_text));
    assert_eq!(tool_parts[1].response_meta(), Some(&expected_text));
}

/// The fixture process every signal, emit and cancel intent targets.
const INTENT_TARGET_EVENT: &str = "standard.intent.note";

/// `intent_leaf` returns one of each v1 intent: a start, and a signal, an
/// emit and a cancel of `target`.
struct IntentLeaf {
    target: lash_sansio::ProcessId,
}

fn intent_leaf() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:intent_leaf",
        "intent_leaf",
        "Return all four v1 tool intents.",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
    .expect("valid declared tool schemas")
    .with_declaration(lash_core::ToolDeclaration::default().with_intents([
        lash_core::ToolIntentKind::StartProcess,
        lash_core::ToolIntentKind::SignalProcess,
        lash_core::ToolIntentKind::EmitProcessEvent,
        lash_core::ToolIntentKind::CancelProcess,
    ]))
}

#[async_trait]
impl ToolProvider for IntentLeaf {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![intent_leaf().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "intent_leaf").then(|| Arc::new(intent_leaf().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let owner = call.context.owner().runtime_owner();
        let (session_id, frame, env) = match (
            call.context.session_id(),
            call.context.agent_frame_id(),
            call.context.process_execution_env_ref(),
        ) {
            (Ok(session), Ok(frame), Ok(env)) => (session.clone(), frame.clone(), env),
            (session, frame, env) => {
                panic!("the call runs in a session's frame: {session:?} {frame:?} {env:?}")
            }
        };
        lash_core::ToolAttemptOutcome::done(
            lash_core::ToolOutcomeDone::ok(serde_json::json!({"provider": "done"})),
            lash_core::ToolIntents::v3(vec![
                lash_core::ToolIntent::StartProcess(Box::new(lash_core::StartProcessIntent {
                    owner: owner.clone(),
                    declaration: lash_core::ProcessStartDeclaration::new(
                        lash_core::testing::held_engine_input(serde_json::json!({"kind": "start"})),
                        lash_core::ProcessOriginator::Session {
                            session_id,
                            agent_frame_id: Some(frame),
                        },
                        lash_core::Lifetime::Detached,
                    )
                    .with_env_ref(env),
                })),
                lash_core::ToolIntent::SignalProcess(lash_core::SignalProcessIntent {
                    owner: owner.clone(),
                    process_id: self.target.clone(),
                    signal_name: "resume".to_string(),
                    payload: serde_json::json!({"kind": "signal"}),
                }),
                lash_core::ToolIntent::EmitProcessEvent(lash_core::EmitProcessEventIntent {
                    owner: owner.clone(),
                    process_id: self.target.clone(),
                    event_type: INTENT_TARGET_EVENT.to_string(),
                    payload: serde_json::json!({"kind": "emit"}),
                }),
                lash_core::ToolIntent::CancelProcess(lash_core::CancelProcessIntent {
                    owner,
                    process_id: self.target.clone(),
                }),
            ]),
        )
    }
}

/// The first request calls `intent_leaf`; the second answers.
fn intent_then_done(n: usize) -> LlmResponse {
    match n {
        1 => LlmResponse {
            parts: vec![LlmOutputPart::ToolCall {
                call_id: "tc-intents".to_string(),
                tool_name: "intent_leaf".to_string(),
                input_json: "{}".to_string(),
                replay: None,
            }],
            ..LlmResponse::default()
        },
        2 => text_response("intent feedback observed"),
        index => panic!("unexpected Standard model call {index}"),
    }
}

/// Every v1 intent a Standard tool returns is realized and its outcome is
/// projected into the model's next request exactly once, in declaration
/// order: start, signal, emit and cancel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standard_protocol_scenario_projects_every_v1_intent_outcome_into_model_feedback() {
    const SESSION: &str = "standard-protocol-scenario";
    let backend = sqlite_memory_store_backend().await;
    let registry = backend.process_registry();
    let target = registry
        .register_process_with_observers(
            lash_core::testing::held_engine_registration(
                serde_json::Value::Null,
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_extra_event_types([
                lash_core::ProcessEventType {
                    name: "signal.resume".to_string(),
                    payload_schema: lash_core::JsonSchema::any(),
                    semantics: lash_core::ProcessEventSemanticsSpec::default(),
                },
                lash_core::ProcessEventType {
                    name: INTENT_TARGET_EVENT.to_string(),
                    payload_schema: lash_core::JsonSchema::any(),
                    semantics: lash_core::ProcessEventSemanticsSpec::default(),
                },
            ]),
            &[lash_sansio::SessionId::from(SESSION)],
        )
        .await
        .expect("register the intents' target")
        .id;
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(
            scripted_provider(Arc::clone(&requests), intent_then_done),
            mock_llm_profile_spec(),
        )
        .plugin(lash_core::testing::process_engine_plugin_fixture())
        .tools(Arc::new(IntentLeaf { target }))
        .build(crate::testing::runtime_lease_owner())
        .expect("standard core");
    let session = core
        .session(crate::SessionId::from(SESSION))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let output = session
        .send(crate::TurnInput::text("run durable follow-on work"))
        .output()
        .await
        .expect("the turn answers");
    assert!(output.is_success(), "{output:?}");
    assert_eq!(output.assistant_message(), Some("intent feedback observed"));

    let requests = requests.lock_recover().clone();
    assert_eq!(requests.len(), 2);
    let feedback = serde_json::to_string(&requests[1].messages).expect("serialize messages");
    let mut at = Vec::new();
    for literal in [
        "[tool intent start_process #0 executed:",
        "[tool intent signal_process #1 executed:",
        "[tool intent emit_process_event #2 executed:",
        "[tool intent cancel_process #3 executed:",
    ] {
        assert_eq!(
            feedback.matches(literal).count(),
            1,
            "expected one `{literal}` in {feedback}"
        );
        at.push(feedback.find(literal).expect("found"));
    }
    assert!(
        at.is_sorted(),
        "the outcomes keep declaration order: {at:?}"
    );
    core.shutdown().await.expect("shutdown");
}

/// `over_budget` returns one signal intent more than a call may declare, all
/// to `target`.
struct OverBudget {
    target: lash_sansio::ProcessId,
}

fn over_budget_tool() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:over_budget",
        "over_budget",
        "Return more signal intents than a call may declare.",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
    .expect("valid declared tool schemas")
    .with_declaration(
        lash_core::ToolDeclaration::default()
            .with_intents([lash_core::ToolIntentKind::SignalProcess]),
    )
}

#[async_trait]
impl ToolProvider for OverBudget {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![over_budget_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "over_budget").then(|| Arc::new(over_budget_tool().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let owner = call.context.owner().runtime_owner();
        lash_core::ToolAttemptOutcome::done(
            lash_core::ToolOutcomeDone::ok(serde_json::json!({"provider": "done"})),
            lash_core::ToolIntents::v3(
                (0..=lash_core::TOOL_INTENT_MAX_COUNT)
                    .map(|index| {
                        lash_core::ToolIntent::SignalProcess(lash_core::SignalProcessIntent {
                            owner: owner.clone(),
                            process_id: self.target.clone(),
                            signal_name: "resume".to_string(),
                            payload: serde_json::json!({"index": index}),
                        })
                    })
                    .collect(),
            ),
        )
    }
}

/// The first request calls `over_budget`; the second answers.
fn over_budget_then_done(n: usize) -> LlmResponse {
    match n {
        1 => LlmResponse {
            parts: vec![LlmOutputPart::ToolCall {
                call_id: "tc-over-budget".to_string(),
                tool_name: "over_budget".to_string(),
                input_json: "{}".to_string(),
                replay: None,
            }],
            ..LlmResponse::default()
        },
        _ => text_response("budget refusal observed"),
    }
}

/// A call that declares more intents than the per-call count budget has its
/// whole batch refused at admission: every intent is answered refused with
/// `count_budget_exceeded`, none executes, and the process they address
/// receives no signal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn over_budget_intent_batch_refuses_every_intent_and_executes_zero_commands() {
    const SESSION: &str = "over-budget-intents";
    let backend = sqlite_memory_store_backend().await;
    let registry = backend.process_registry();
    let target = registry
        .register_process_with_observers(
            lash_core::testing::held_engine_registration(
                serde_json::Value::Null,
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_extra_event_types([lash_core::ProcessEventType {
                name: "signal.resume".to_string(),
                payload_schema: lash_core::JsonSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec::default(),
            }]),
            &[lash_sansio::SessionId::from(SESSION)],
        )
        .await
        .expect("register the intents' target")
        .id;
    let events_before = registry
        .full_event_window(&target, 0)
        .await
        .expect("the target's events")
        .len();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(
            scripted_provider(Arc::clone(&requests), over_budget_then_done),
            mock_llm_profile_spec(),
        )
        .plugin(lash_core::testing::process_engine_plugin_fixture())
        .tools(Arc::new(OverBudget {
            target: target.clone(),
        }))
        .build(crate::testing::runtime_lease_owner())
        .expect("standard core");
    let session = core
        .session(crate::SessionId::from(SESSION))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let output = session
        .send(crate::TurnInput::text("declare too many intents"))
        .output()
        .await
        .expect("the turn answers");
    assert!(output.is_success(), "{output:?}");

    let requests = requests.lock_recover().clone();
    assert_eq!(requests.len(), 2);
    let feedback = serde_json::to_string(&requests[1].messages).expect("serialize messages");
    for index in 0..=lash_core::TOOL_INTENT_MAX_COUNT {
        let refused =
            format!("[tool intent signal_process #{index} refused: count_budget_exceeded]");
        assert_eq!(
            feedback.matches(&refused).count(),
            1,
            "every declared intent is refused once: {refused}"
        );
    }
    assert!(
        !feedback.contains("executed:"),
        "no intent of an over-budget batch executes: {feedback}"
    );
    assert_eq!(
        registry
            .full_event_window(&target, 0)
            .await
            .expect("the target's events")
            .len(),
        events_before,
        "the over-budget batch issued no command"
    );
    core.shutdown().await.expect("shutdown");
}
