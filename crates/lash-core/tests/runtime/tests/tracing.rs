// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use ::tracing::Instrument;
use lash_core::facade_support::ToolStateFacadeOps;
use lash_sansio::sync::MutexExt;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Layer, Registry};

fn composition_change_entries(path: &std::path::Path) -> Vec<serde_json::Value> {
    lash_trace::parse_jsonl_records::<serde_json::Value>(
        &std::fs::read_to_string(path).expect("read composition trace"),
    )
    .expect("trace records")
    .into_iter()
    .filter(|entry| {
        entry.get("type").and_then(serde_json::Value::as_str) == Some("composition_changed")
    })
    .collect()
}

fn composition_tool_contract<'a>(
    entry: &'a serde_json::Value,
    name: &str,
) -> &'a serde_json::Value {
    entry["tool_schemas"]
        .as_array()
        .expect("ordered tool schemas")
        .iter()
        .find(|tool| tool["name"] == name)
        .expect("named tool contract remains present")
}

fn completed_text_call(text: &str) -> MockCall {
    MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: text.to_string(),
                response_meta: None,
            }],
            ..LlmResponse::default()
        }),
    }
}

async fn run_composition_probe_turn(runtime: &mut LashRuntime, turn_id: &TurnId) {
    runtime
        .run_turn_assembled(
            TurnInput::text(turn_id),
            CancellationToken::new(),
            named_turn_scope(&SessionId::from("root"), turn_id),
        )
        .await
        .expect("composition probe turn");
}

#[tokio::test]
async fn composition_trace_is_snapshot_on_change_and_ignores_route_capacity_noise() {
    let transport = mock_provider(vec![
        completed_text_call("first"),
        completed_text_call("unchanged"),
        completed_text_call("route noise"),
        completed_text_call("prompt changed"),
    ]);
    let trace_path = std::env::temp_dir().join(format!(
        "lash-composition-trace-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let mut runtime = standard_runtime_with_transport_and_host(
        transport,
        test_host_config_with_trace_path(trace_path.clone()),
    )
    .await;

    let serializations_before = lash_core::trace::composition_schema_serialization_count();
    run_composition_probe_turn(&mut runtime, &TurnId::from("first-composition")).await;
    let serializations_after_first = lash_core::trace::composition_schema_serialization_count();
    assert!(
        serializations_after_first > serializations_before,
        "the first composition fingerprints and materializes its tool contracts"
    );
    run_composition_probe_turn(&mut runtime, &TurnId::from("same-composition")).await;
    assert_eq!(
        lash_core::trace::composition_schema_serialization_count(),
        serializations_after_first,
        "an identical composition must not serialize schemas or allocate a fresh schema Vec"
    );
    runtime
        .update_session_config(lash_core::facade_support::SessionConfigPatch {
            model: Some(
                lash_core::ModelSpec::builder("different-route")
                    .context_window_tokens(150_000)
                    .build()
                    .expect("route-noise model"),
            ),
            ..Default::default()
        })
        .await
        .expect("apply route-noise model");
    run_composition_probe_turn(&mut runtime, &TurnId::from("route-capacity-noise")).await;
    runtime
        .add_prompt_contribution(lash_core::PromptContribution::guidance(
            "Changed policy",
            "This text proves the rendered prompt changed.",
        ))
        .await
        .expect("change session prompt layer");
    run_composition_probe_turn(&mut runtime, &TurnId::from("changed-prompt")).await;

    let entries = composition_change_entries(&trace_path);
    assert_eq!(
        entries.len(),
        2,
        "initial composition and one genuine prompt change emit exactly once: {entries:?}"
    );
    assert_ne!(entries[0]["fingerprint"], entries[1]["fingerprint"]);
    assert!(
        entries[1]["rendered_system_prompt"]
            .as_str()
            .is_some_and(|prompt| prompt.contains("This text proves the rendered prompt changed."))
    );
    assert_eq!(
        entries[0]["tool_schemas"], entries[1]["tool_schemas"],
        "a prompt-only change retains the complete ordered tool schemas"
    );

    let _ = std::fs::remove_file(trace_path);
}

#[tokio::test]
async fn composition_trace_fires_once_when_tool_membership_changes_with_full_ordered_schemas() {
    let transport = mock_provider(vec![
        completed_text_call("tool present"),
        completed_text_call("tool absent"),
        completed_text_call("still absent"),
    ]);
    let trace_path = std::env::temp_dir().join(format!(
        "lash-tool-composition-trace-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EchoTool),
        transport,
        test_host_config_with_trace_path(trace_path.clone()),
    )
    .await;

    run_composition_probe_turn(&mut runtime, &TurnId::from("tool-member")).await;
    let mut tool_state = runtime.tool_state().expect("live tool state");
    tool_state
        .set_membership(&lash_core::ToolId::from("tool:echo_tool"), false)
        .expect("hide echo tool");
    runtime
        .apply_tool_state(tool_state)
        .await
        .expect("apply tool membership change");
    run_composition_probe_turn(&mut runtime, &TurnId::from("tool-removed")).await;
    run_composition_probe_turn(&mut runtime, &TurnId::from("tool-still-removed")).await;

    let entries = composition_change_entries(&trace_path);
    assert_eq!(
        entries.len(),
        2,
        "initial membership and one genuine removal emit exactly once: {entries:?}"
    );
    let initial_tools = entries[0]["tool_schemas"]
        .as_array()
        .expect("initial ordered tool schemas");
    let echo = initial_tools
        .iter()
        .find(|tool| tool["name"] == "echo_tool")
        .expect("echo tool has a full schema snapshot");
    assert_eq!(echo["input_schema"]["canonical"]["required"][0], "value");
    assert!(echo["output_schema"]["canonical"].is_object());
    let changed_tools = entries[1]["tool_schemas"]
        .as_array()
        .expect("changed ordered tool schemas");
    assert_eq!(initial_tools.len(), changed_tools.len() + 1);
    assert!(
        changed_tools.iter().all(|tool| tool["name"] != "echo_tool"),
        "the changed snapshot must carry the complete catalog without the removed member"
    );
    assert_ne!(entries[0]["fingerprint"], entries[1]["fingerprint"]);

    let _ = std::fs::remove_file(trace_path);
}

struct SchemaChangingTool {
    revision: Mutex<u64>,
}

impl SchemaChangingTool {
    fn definition(&self) -> lash_core::ToolDefinition {
        let field = if *self.revision.lock_recover() == 1 {
            "first_value"
        } else {
            "second_value"
        };
        lash_core::ToolDefinition::raw(
            "tool:schema_changing",
            "schema_changing",
            "A stable member whose contract can refresh",
            serde_json::json!({
                "type": "object",
                "properties": { field: { "type": "string" } },
                "required": [field],
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object", "additionalProperties": true }),
        )
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for SchemaChangingTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![self.definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "schema_changing").then(|| Arc::new(self.definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::Value::Null).into()
    }
}

#[tokio::test]
async fn composition_trace_fires_once_when_same_member_tool_schema_changes() {
    let transport = mock_provider(vec![
        completed_text_call("first schema"),
        completed_text_call("second schema"),
        completed_text_call("unchanged second schema"),
    ]);
    let trace_path = std::env::temp_dir().join(format!(
        "lash-tool-schema-composition-trace-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let tool = Arc::new(SchemaChangingTool {
        revision: Mutex::new(1),
    });
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        tool.clone() as Arc<dyn lash_core::ToolProvider>,
        transport,
        test_host_config_with_trace_path(trace_path.clone()),
    )
    .await;

    run_composition_probe_turn(&mut runtime, &TurnId::from("schema-one")).await;
    *tool.revision.lock_recover() = 2;
    runtime
        .refresh_session_tool_catalog()
        .await
        .expect("refresh changed tool schema");
    run_composition_probe_turn(&mut runtime, &TurnId::from("schema-two")).await;
    run_composition_probe_turn(&mut runtime, &TurnId::from("schema-two-unchanged")).await;

    let entries = composition_change_entries(&trace_path);
    assert_eq!(entries.len(), 2, "one schema change emits exactly once");
    assert_eq!(
        composition_tool_contract(&entries[0], "schema_changing")["input_schema"]["canonical"]["required"]
            [0],
        "first_value"
    );
    assert_eq!(
        composition_tool_contract(&entries[1], "schema_changing")["input_schema"]["canonical"]["required"]
            [0],
        "second_value"
    );
    assert_ne!(entries[0]["fingerprint"], entries[1]["fingerprint"]);

    let _ = std::fs::remove_file(trace_path);
}

#[derive(Clone, Debug, Default)]
struct SpanCapture {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CapturedSpan {
    name: String,
    parent: Option<String>,
}

impl SpanCapture {
    fn snapshot(&self) -> Vec<CapturedSpan> {
        self.spans.lock_recover().clone()
    }
}

impl<S> Layer<S> for SpanCapture
where
    S: ::tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(
        &self,
        _attrs: &::tracing::span::Attributes<'_>,
        id: &::tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let span = ctx.span(id).expect("new span present in registry");
        let captured = CapturedSpan {
            name: span.metadata().name().to_string(),
            parent: span
                .parent()
                .map(|parent| parent.metadata().name().to_string()),
        };
        self.spans.lock_recover().push(captured);
    }
}

#[tokio::test]
async fn runtime_session_graph_service_routes_standard_compaction_event_to_real_sink() {
    let trace_path = std::env::temp_dir().join(format!(
        "lash-runtime-plugin-trace-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let runtime = standard_runtime_with_transport_and_host(
        mock_provider(Vec::new()),
        test_host_config_with_trace_path(trace_path.clone()),
    )
    .await;
    let graph = runtime
        .session_graph_service()
        .expect("resident runtime exposes its graph service");

    graph
        .emit_trace_event(
            lash_core::TraceContext::default().for_session("emitter-supplied-id"),
            lash_core::TraceEvent::CompactionCompleted { summary_nodes: 0 },
        )
        .await
        .expect("runtime graph service should route trace records");

    let logged = std::fs::read_to_string(&trace_path).expect("read runtime trace sink");
    let record: lash_core::facade_support::TraceRecord = lash_trace::parse_jsonl_records(&logged)
        .expect("typed trace records")
        .into_iter()
        .next()
        .expect("one trace record");
    assert_eq!(record.context.session_id.as_deref(), Some("root"));
    assert!(matches!(
        record.event,
        lash_core::TraceEvent::CompactionCompleted { summary_nodes: 0 }
    ));
    let _ = std::fs::remove_file(trace_path);
}

#[tokio::test]
async fn provider_spans_are_children_of_the_turn_span() {
    let provider = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|_request| async {
            let provider_span = ::tracing::info_span!("provider.complete");
            drop(provider_span);
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "done".to_string(),
                    response_meta: None,
                }],
                ..LlmResponse::default()
            })
        })
        .build();
    let mut runtime = standard_runtime_with_transport(provider).await;
    let capture = SpanCapture::default();
    let subscriber = Registry::default().with(capture.clone());
    ::tracing::subscriber::set_global_default(subscriber).expect("install capture subscriber");
    let turn_span = ::tracing::info_span!("runtime.turn");

    runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: lash_core::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("provider-span-parentage"),
            ),
        )
        .instrument(turn_span)
        .await
        .expect("turn");

    let spans = capture.snapshot();
    assert_eq!(
        spans
            .iter()
            .find(|span| span.name == "provider.complete")
            .and_then(|span| span.parent.as_deref()),
        Some("runtime.turn"),
        "provider span must inherit the turn span; captured spans: {spans:?}"
    );
}

async fn assert_standard_tool_lifecycle(
    call_id: &str,
    tool_name: &str,
    input_json: &str,
    expected_success: bool,
    plugins: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
) {
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: call_id.to_string(),
                    tool_name: tool_name.to_string(),
                    input_json: input_json.to_string(),
                    replay: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "done".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let trace_path = std::env::temp_dir().join(format!(
        "lash-standard-tool-trace-{}-{}-{}.jsonl",
        std::process::id(),
        call_id,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        plugins,
        Arc::new(EchoTool),
        transport,
        test_host_config_with_trace_path(trace_path.clone()),
    )
    .await;
    let turn_events = RecordingTurnEvents::default();

    let turn = runtime
        .stream_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "call the tool".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: lash_core::TurnContext::default(),
            },
            TurnOptions::new(
                CancellationToken::new(),
                host_turn_scope(
                    &runtime.host.core,
                    &SessionId::from("root"),
                    &TurnId::from("trace-standard-tool-turn"),
                ),
            )
            .with_turn_events(&turn_events),
        )
        .await
        .expect("turn");

    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. }
    ));
    assert_eq!(turn.tool_calls.len(), 1, "one accounting record per call");
    assert_eq!(turn.tool_calls[0].call_id.as_deref(), Some(call_id));
    assert_eq!(turn.tool_calls[0].tool, tool_name);
    assert_eq!(turn.tool_calls[0].output.is_success(), expected_success);

    let logged = std::fs::read_to_string(&trace_path).expect("read trace");
    let entries =
        lash_trace::parse_jsonl_records::<serde_json::Value>(&logged).expect("json log entries");

    let started = entries
        .iter()
        .filter(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("tool_call_started"))
        .collect::<Vec<_>>();
    let completed = entries
        .iter()
        .filter(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("tool_call_completed"))
        .collect::<Vec<_>>();
    assert_eq!(
        started.len(),
        1,
        "expected exactly one ToolCallStarted trace: {entries:?}"
    );
    assert_eq!(
        completed.len(),
        1,
        "expected exactly one ToolCallCompleted trace: {entries:?}"
    );
    assert_eq!(
        started[0].get("call_id").and_then(|v| v.as_str()),
        Some(call_id)
    );
    assert_eq!(
        started[0].get("name").and_then(|v| v.as_str()),
        Some(tool_name)
    );
    let started_position = entries
        .iter()
        .position(|entry| {
            entry.get("type").and_then(|value| value.as_str()) == Some("tool_call_started")
                && entry.get("call_id").and_then(|value| value.as_str()) == Some(call_id)
        })
        .expect("started position");
    let completed_position = entries
        .iter()
        .position(|entry| {
            entry.get("type").and_then(|value| value.as_str()) == Some("tool_call_completed")
                && entry.get("call_id").and_then(|value| value.as_str()) == Some(call_id)
        })
        .expect("completed position");
    assert!(
        started_position < completed_position,
        "Started must precede Completed: {entries:?}"
    );

    let activities = turn_events.snapshot();
    let lifecycle = activities
        .iter()
        .filter_map(|activity| match &activity.event {
            lash_core::TurnEvent::ToolCallStarted {
                call_id: Some(observed),
                ..
            } if observed == call_id => Some("started"),
            lash_core::TurnEvent::ToolCallCompleted {
                call_id: Some(observed),
                ..
            } if observed == call_id => Some("completed"),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        lifecycle,
        ["started", "completed"],
        "exactly one ordered activity pair keyed by {call_id}: {activities:?}"
    );
    let expected_correlation = lash_core::TurnActivityId::new(format!("tool:{call_id}"));
    assert!(
        activities
            .iter()
            .filter(|activity| {
                matches!(
                    &activity.event,
                    lash_core::TurnEvent::ToolCallStarted { call_id: Some(observed), .. }
                        | lash_core::TurnEvent::ToolCallCompleted { call_id: Some(observed), .. }
                        if observed == call_id
                )
            })
            .all(|activity| activity.correlation_id == expected_correlation),
        "tool activity correlation remains keyed by call id: {activities:?}"
    );
    // Span identity is stamped from session/turn context so the tool nests
    // under its turn as `tool:<call_id>`.
    let expected_graph_node_id = format!("tool:{call_id}");
    assert_eq!(
        completed[0]
            .get("context")
            .and_then(|context| context.get("graph_node_id"))
            .and_then(|v| v.as_str()),
        Some(expected_graph_node_id.as_str())
    );

    let _ = std::fs::remove_file(&trace_path);
}

#[tokio::test]
async fn standard_runtime_emits_single_tool_call_trace_pair_per_call() {
    // Successful prepared calls keep the one-pair contract: the reporting
    // repair must not duplicate the start already emitted by batch execution.
    Box::pin(assert_standard_tool_lifecycle(
        "call-success",
        "echo_tool",
        r#"{"value":"sample"}"#,
        true,
        Vec::new(),
    ))
    .await;
}

/// A controller that hosts groups on an embedded native substrate, so the
/// turn driver's `ToolInvocation` group children open, defer, and settle
/// against the same registry the test resolves out of band.
struct PendingToolResolutionController {
    inner: Arc<lash_core::facade_support::NativeRuntimeEffectController>,
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for PendingToolResolutionController {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        Some(format!("pending-resolution-controller:{:p}", self))
    }

    async fn await_event_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> Result<lash_core::AwaitEventKey, lash_core::RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveOutcome, lash_core::RuntimeError> {
        self.inner.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> Result<Option<lash_core::Resolution>, lash_core::RuntimeError> {
        self.inner.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<lash_core::Resolution, lash_core::RuntimeError> {
        self.inner.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), lash_core::RuntimeError> {
        self.inner.revoke_await_events_for_session(session_id).await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), lash_core::RuntimeError> {
        self.inner.cancel_await_events_for_session(session_id).await
    }
}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for PendingToolResolutionController {
    async fn execute_effect(
        &self,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        // The turn's cancel watch issues peek effects the local executor does
        // not cover; the embedded registry answers them.
        if let lash_core::RuntimeEffectCommand::PeekAwaitEvent { key } = &envelope.command {
            return Ok(lash_core::RuntimeEffectOutcome::PeekAwaitEvent {
                resolution: self.inner.peek_await_event(key).await?,
            });
        }
        local_executor.execute(envelope).await
    }

    async fn open_effect_group(
        &self,
        group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        self.inner.open_effect_group(group).await
    }

    fn register_group_executors(
        &self,
        executors: Arc<dyn lash_core::GroupExecutors>,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.inner.register_group_executors(executors)
    }

    async fn await_next_settlement(
        &self,
        handle: &mut lash_core::EffectGroupHandle,
        cancel: lash_core::CancellationToken,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        self.inner.await_next_settlement(handle, cancel).await
    }

    async fn close_effect_group(
        &self,
        handle: lash_core::EffectGroupHandle,
        disposition: lash_core::LoserPolicy,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.inner.close_effect_group(handle, disposition).await
    }

    async fn commit_group_child_final(
        &self,
        commit: lash_core::facade_support::effect_replay_driver::GroupChildFinalCommit,
    ) -> Result<
        lash_core::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome,
        lash_core::RuntimeEffectControllerError,
    > {
        self.inner.commit_group_child_final(commit).await
    }

    async fn group_child_drain_blocked(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<bool, lash_core::RuntimeEffectControllerError> {
        self.inner
            .group_child_drain_blocked(group_key, commit_seq)
            .await
    }
}

/// An `echo_tool` that parks on its issued completion key and lets the test
/// resolve it out of band — the group-path shape of "pending then resolved".
struct PendingEchoTool {
    resolver: Arc<lash_core::facade_support::NativeRuntimeEffectController>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for PendingEchoTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![pending_echo_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "echo_tool").then(|| Arc::new(pending_echo_tool_definition().contract()))
    }

    fn attempt_may_defer(&self, tool_id: &lash_core::ToolId) -> bool {
        *tool_id == pending_echo_tool_definition().manifest().id
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let key = call
            .context
            .completion_key()
            .expect("the group child carries an issued completion key");
        let resolver = Arc::clone(&self.resolver);
        let value = call
            .args
            .get("value")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        tokio::task::yield_now().await;
        let _ = resolver
            .resolve_await_event(
                &key,
                lash_core::Resolution::Ok(serde_json::json!({
                    "payload": format!("raw:{value}")
                })),
            )
            .await;
        lash_core::ToolAttemptOutcome::Pending(lash_core::PendingCompletion::default())
    }
}

fn pending_echo_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:echo_tool",
        "echo_tool",
        "Return a tool payload",
        serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"],
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
}

#[tokio::test]
async fn pending_then_resolved_tool_call_emits_one_completion_per_channel() {
    let call_id = "call-pending";
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: call_id.to_string(),
                    tool_name: "echo_tool".to_string(),
                    input_json: r#"{"value":"pending"}"#.to_string(),
                    replay: None,
                }],
                ..LlmResponse::default()
            }),
        },
        completed_text_call("done"),
    ]);
    let trace_path = std::env::temp_dir().join(format!(
        "lash-pending-tool-trace-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let inner = Arc::new(
        lash_core::facade_support::NativeRuntimeEffectController::default()
            .allow_process_lifetime_completion_keys(),
    );
    let controller: Arc<dyn lash_core::RuntimeEffectController> =
        Arc::new(PendingToolResolutionController {
            inner: Arc::clone(&inner),
        });
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(PendingEchoTool {
        resolver: Arc::clone(&inner),
    });
    let mut config = test_runtime_host_config();
    config.tracing.trace_sink = Some(Arc::new(lash_trace::JsonlTraceSink::new(
        trace_path.clone(),
    )));
    config.control.effect_host =
        crate::runtime_support::effect_recording_authority::controller_effect_host(Arc::clone(
            &controller,
        ));
    crate::runtime_support::effect_recording_authority::reinstall_tool_child_host(&mut config);
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        tools,
        transport,
        EmbeddedRuntimeHost::new(config),
    )
    .await;
    let turn_events = RecordingTurnEvents::default();

    let turn = runtime
        .stream_turn(
            TurnInput::text("call the pending tool"),
            TurnOptions::new(
                CancellationToken::new(),
                lash_core::ScopedEffectController::shared(
                    controller,
                    lash_core::AdmittedScope::turn("root", "pending-tool-turn"),
                )
                .expect("scoped controller"),
            )
            .with_turn_events(&turn_events),
        )
        .await
        .expect("turn");

    assert_eq!(turn.tool_calls.len(), 1);
    assert_eq!(turn.tool_calls[0].call_id.as_deref(), Some(call_id));
    let entries = lash_trace::parse_jsonl_records::<serde_json::Value>(
        &std::fs::read_to_string(&trace_path).expect("read pending trace"),
    )
    .expect("trace entries");
    assert_eq!(
        entries
            .iter()
            .filter(|entry| {
                entry.get("type").and_then(serde_json::Value::as_str) == Some("tool_call_completed")
                    && entry.get("call_id").and_then(serde_json::Value::as_str) == Some(call_id)
            })
            .count(),
        1,
        "pending resolution owns exactly one trace completion: {entries:?}"
    );
    let activities = turn_events.snapshot();
    let completions = activities
        .iter()
        .filter(|activity| {
            matches!(
                &activity.event,
                lash_core::TurnEvent::ToolCallCompleted { call_id: Some(observed), .. }
                    if observed == call_id
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        completions.len(),
        1,
        "pending resolution owns exactly one activity completion: {activities:?}"
    );
    assert_eq!(
        completions[0].correlation_id,
        lash_core::TurnActivityId::new(format!("tool:{call_id}"))
    );

    let _ = std::fs::remove_file(trace_path);
}

#[tokio::test]
async fn unavailable_tool_name_emits_an_ordered_lifecycle_pair() {
    Box::pin(assert_standard_tool_lifecycle(
        "call-missing-name",
        "missing_tool",
        r#"{"value":1}"#,
        false,
        Vec::new(),
    ))
    .await;
}

#[tokio::test]
async fn invalid_tool_arguments_emit_an_ordered_lifecycle_pair() {
    Box::pin(assert_standard_tool_lifecycle(
        "call-invalid-args",
        "echo_tool",
        r#"{"other":true}"#,
        false,
        Vec::new(),
    ))
    .await;
}

#[tokio::test]
async fn before_tool_hook_refusal_emits_an_ordered_lifecycle_pair() {
    let refusal = Arc::new(lash_core::plugin::StaticPluginFactory::new(
        "tool-refusal",
        lash_core::facade_support::PluginSpec::new().with_before_tool_call(Arc::new(|_ctx| {
            Box::pin(async {
                Ok(vec![
                    lash_core::facade_support::BeforeToolCallPluginDirective::short_circuit(
                        lash_core::ToolOutcome::err_fmt("refused by test hook"),
                    ),
                ])
            })
        })),
    ));
    Box::pin(assert_standard_tool_lifecycle(
        "call-hook-refusal",
        "echo_tool",
        r#"{"value":"blocked"}"#,
        false,
        vec![refusal],
    ))
    .await;
}

#[tokio::test]
async fn standard_runtime_trace_records_stream_event_entries() {
    let transport = mock_provider(vec![MockCall {
        stream_events: vec![
            LlmStreamEvent::Delta {
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                text: "Hello ".to_string(),
            },
            LlmStreamEvent::Delta {
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                text: "world".to_string(),
            },
            LlmStreamEvent::Part(LlmOutputPart::Text {
                text: "Hello world".to_string(),
                response_meta: None,
            }),
            LlmStreamEvent::Usage(LlmUsage {
                input_tokens: 10,
                output_tokens: 2,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            }),
        ],
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "Hello world".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            execution_evidence: Some(lash_core::ExecutionEvidence {
                served_model: Some("served-model".to_string()),
                provider_response_id: Some("provider-response-1".to_string()),
                reasoning_output_tokens: Some(0),
                provider_finish_reason: Some("stop".to_string()),
                ..lash_core::ExecutionEvidence::default()
            }),
            ..LlmResponse::default()
        }),
    }]);
    let trace_path = std::env::temp_dir().join(format!(
        "lash-standard-trace-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let mut runtime = standard_runtime_with_transport_and_host(
        transport,
        test_host_config_with_trace_path_and_stream_events(trace_path.clone()),
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: lash_core::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("trace-stream-events-turn"),
            ),
        )
        .await
        .expect("turn");

    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. }
    ));
    let attempt_evidence = turn.llm_calls[0].attempts[0]
        .evidence
        .as_ref()
        .expect("response evidence reaches the sealed attempt ledger");
    assert_eq!(
        attempt_evidence.provider_response_id.as_deref(),
        Some("provider-response-1")
    );

    let logged = std::fs::read_to_string(&trace_path).expect("read trace");
    let entries =
        lash_trace::parse_jsonl_records::<serde_json::Value>(&logged).expect("json log entries");

    assert!(
        entries
            .iter()
            .any(|entry| entry.get("type").and_then(|v| v.as_str())
                == Some("runtime_stream_event")
                && entry
                    .get("event")
                    .and_then(|payload| payload.get("event_name"))
                    .and_then(|v| v.as_str())
                    == Some("delta")
                && entry
                    .get("event")
                    .and_then(|payload| payload.get("raw_text"))
                    .and_then(|v| v.as_str())
                    == Some("Hello ")),
        "expected delta stream event in trace: {entries:?}"
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.get("type").and_then(|v| v.as_str())
                == Some("runtime_stream_event")
                && entry
                    .get("event")
                    .and_then(|payload| payload.get("event_name"))
                    .and_then(|v| v.as_str())
                    == Some("text_part")
                && entry
                    .get("event")
                    .and_then(|payload| payload.get("raw_text"))
                    .and_then(|v| v.as_str())
                    == Some("Hello world")
                && entry
                    .get("event")
                    .and_then(|payload| payload.get("visible_text"))
                    .is_none_or(|v| v.is_null())),
        "expected text_part stream event in trace: {entries:?}"
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("llm_call_completed")),
        "expected final llm trace entry in trace: {entries:?}"
    );
    let response_entry = entries
        .iter()
        .find(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("llm_call_completed"))
        .expect("completed llm call entry");
    assert_eq!(
        response_entry["response"]["request_model"].as_str(),
        Some("mock-model")
    );
    assert_eq!(
        response_entry["attempts"][0]["execution_evidence"]["served_model"].as_str(),
        Some("served-model")
    );
    assert_eq!(
        response_entry["attempts"][0]["execution_evidence"]["reasoning_output_tokens"].as_u64(),
        Some(0)
    );
    let stream_summary = response_entry
        .get("stream_summary")
        .and_then(|value| value.as_object())
        .expect("stream summary");
    assert_eq!(
        stream_summary
            .get("text_delta_count")
            .and_then(|value| value.as_u64()),
        Some(2)
    );
    assert_eq!(
        stream_summary
            .get("visible_chunk_count")
            .and_then(|value| value.as_u64()),
        Some(2)
    );
    assert_eq!(
        stream_summary
            .get("max_visible_chunk_chars")
            .and_then(|value| value.as_u64()),
        Some(6)
    );
    let avg_chunk_chars = stream_summary
        .get("avg_visible_chunk_chars")
        .and_then(|value| value.as_f64())
        .expect("avg visible chunk chars");
    assert!((avg_chunk_chars - 5.5).abs() < f64::EPSILON);
    assert!(
        stream_summary
            .get("first_visible_token_latency_ms")
            .is_some_and(|value| !value.is_null())
    );
    assert!(
        stream_summary
            .get("stream_duration_ms")
            .is_some_and(|value| !value.is_null())
    );

    let _ = std::fs::remove_file(&trace_path);
}

#[tokio::test]
async fn extended_runtime_trace_records_provider_request_and_stream_events() {
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|req| async move {
            if let Some(tx) = req.provider_trace.as_ref() {
                tx.send(LlmProviderTraceEvent::request(
                    "mock",
                    "responses",
                    serde_json::json!({
                        "model": "mock-model",
                        "input": "x".repeat(3_000),
                    })
                    .to_string(),
                ));
                tx.send(LlmProviderTraceEvent::request(
                    "mock",
                    "chat/completions",
                    r#"{"model":"small"}"#.to_string(),
                ));
                tx.send(LlmProviderTraceEvent::request(
                    "mock",
                    "invalid",
                    "not-json".to_string(),
                ));
                tx.send(LlmProviderTraceEvent {
                    provider: "mock",
                    event_name: "response.output_item.done".to_string(),
                    raw: serde_json::json!({
                        "type": "response.output_item.done",
                        "output_index": 0,
                        "item": { "id": "msg_1" }
                    })
                    .to_string(),
                });
                tx.send(LlmProviderTraceEvent {
                    provider: "mock",
                    event_name: "response.output_item.done".to_string(),
                    raw: serde_json::json!({
                        "type": "response.output_item.done",
                        "output_index": 1,
                        "item": { "id": "msg_2" }
                    })
                    .to_string(),
                });
            }
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "Hello".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build();
    let trace_path = std::env::temp_dir().join(format!(
        "lash-provider-trace-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let mut runtime = standard_runtime_with_transport_and_host(
        transport,
        test_host_config_with_trace_path_and_stream_events(trace_path.clone()),
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: lash_core::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("trace-provider-stream-turn"),
            ),
        )
        .await
        .expect("turn");

    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. }
    ));

    let logged = std::fs::read_to_string(&trace_path).expect("read trace");
    let entries =
        lash_trace::parse_jsonl_records::<serde_json::Value>(&logged).expect("json log entries");
    let provider_events = entries
        .iter()
        .filter(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("provider_stream_event"))
        .collect::<Vec<_>>();
    let provider_requests = entries
        .iter()
        .filter(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("provider_request"))
        .collect::<Vec<_>>();
    assert_eq!(provider_requests.len(), 3, "provider traces: {entries:?}");
    let request_event = provider_requests
        .iter()
        .find(|entry| entry["event"]["endpoint"] == "responses")
        .expect("large provider request")["event"]
        .clone();
    let expected_body = serde_json::json!({
        "model": "mock-model",
        "input": "x".repeat(3_000),
    });
    let expected_serialized = expected_body.to_string();
    assert_eq!(request_event["provider"], "mock");
    assert_eq!(request_event["endpoint"], "responses");
    assert!(request_event.get("body_json").is_none());
    assert_eq!(request_event["body_json_omitted_reason"], "size_limit");
    assert_eq!(request_event["body_len"], expected_serialized.len());
    assert!(request_event["body_len"].as_u64().unwrap() > 2_048);
    assert_eq!(
        request_event["body_sha256"],
        lash_trace::sha256_hex(expected_serialized.as_bytes())
    );
    let small_request = provider_requests
        .iter()
        .find(|entry| entry["event"]["endpoint"] == "chat/completions")
        .expect("small provider request");
    assert_eq!(small_request["event"]["body_json"]["model"], "small");
    assert!(
        small_request["event"]
            .get("body_json_omitted_reason")
            .is_none()
    );
    let invalid_request = provider_requests
        .iter()
        .find(|entry| entry["event"]["endpoint"] == "invalid")
        .expect("invalid provider request");
    assert!(invalid_request["event"].get("body_json").is_none());
    assert_eq!(
        invalid_request["event"]["body_json_omitted_reason"],
        "invalid_json"
    );
    assert_eq!(invalid_request["event"]["body_len"], "not-json".len());
    assert_eq!(
        invalid_request["event"]["body_sha256"],
        lash_trace::sha256_hex(b"not-json")
    );
    assert_eq!(
        provider_events.len(),
        2,
        "provider trace entries: {entries:?}"
    );
    assert_eq!(provider_events[0]["event"]["item_id"], "msg_1");
    assert_eq!(provider_events[0]["event"]["output_index"], 0);
    assert_eq!(provider_events[1]["event"]["item_id"], "msg_2");
    assert_eq!(
        provider_events[1]["event"]["raw_json"]["item"]["id"],
        "msg_2"
    );
    assert!(
        provider_events[1]["event"]["raw_sha256"]
            .as_str()
            .is_some_and(|hash| !hash.is_empty())
    );

    let _ = std::fs::remove_file(&trace_path);
}

#[tokio::test]
async fn provider_request_trace_sender_requires_extended_level_and_sink() {
    async fn assert_sender_absent(host: EmbeddedRuntimeHost, turn_id: &TurnId) {
        let transport = TestProvider::builder()
            .kind("mock")
            .requires_streaming(true)
            .complete(|req| async move {
                assert!(req.provider_trace.is_none());
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "Hello".to_string(),
                        response_meta: None,
                    }],
                    ..LlmResponse::default()
                })
            })
            .build();
        let mut runtime = standard_runtime_with_transport_and_host(transport, host).await;
        runtime
            .run_turn_assembled(
                TurnInput {
                    items: vec![InputItem::Text {
                        text: "hello".to_string(),
                    }],
                    protocol_turn_options: None,
                    trace_turn_id: None,
                    protocol_extension: None,
                    turn_context: lash_core::TurnContext::default(),
                },
                CancellationToken::new(),
                named_turn_scope(&SessionId::from("root"), turn_id),
            )
            .await
            .expect("turn");
    }

    let trace_path = std::env::temp_dir().join(format!(
        "lash-provider-gate-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    Box::pin(assert_sender_absent(
        test_host_config_with_trace_path(trace_path.clone()),
        &TurnId::from("standard-trace-level"),
    ))
    .await;

    let mut no_sink = test_host_config();
    no_sink.core.tracing.trace_level = lash_trace::TraceLevel::Extended;
    Box::pin(assert_sender_absent(
        no_sink,
        &TurnId::from("extended-without-sink"),
    ))
    .await;

    let _ = std::fs::remove_file(trace_path);
}

#[tokio::test]
async fn standard_runtime_trace_omits_stream_event_entries_by_default() {
    let transport = mock_provider(vec![MockCall {
        stream_events: vec![
            LlmStreamEvent::Delta {
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                text: "Hello ".to_string(),
            },
            LlmStreamEvent::Delta {
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                text: "world".to_string(),
            },
        ],
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "Hello world".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let trace_path = std::env::temp_dir().join(format!(
        "lash-standard-trace-summary-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let mut runtime = standard_runtime_with_transport_and_host(
        transport,
        test_host_config_with_trace_path(trace_path.clone()),
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: lash_core::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("trace-standard-turn"),
            ),
        )
        .await
        .expect("turn");

    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. }
    ));

    let logged = std::fs::read_to_string(&trace_path).expect("read trace");
    let entries =
        lash_trace::parse_jsonl_records::<serde_json::Value>(&logged).expect("json log entries");

    assert!(
        !entries.iter().any(
            |entry| entry.get("type").and_then(|v| v.as_str()) == Some("runtime_stream_event")
        ),
        "stream event entries should be opt-in: {entries:?}"
    );
    let response_entry = entries
        .iter()
        .find(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("llm_call_completed"))
        .expect("completed llm call entry");
    assert!(
        response_entry
            .get("stream_summary")
            .is_some_and(|value| !value.is_null()),
        "stream summary should remain in completed LLM trace: {response_entry:?}"
    );

    let _ = std::fs::remove_file(&trace_path);
}

#[tokio::test]
async fn standard_runtime_trace_records_failed_llm_calls() {
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Err(lash_core::llm::transport::LlmTransportError::new(
            "HTTP request failed: builder error",
        )
        .with_code(FailureCode::provider("builder"))
        .with_raw("transport raw body")
        .with_request_body("{\"model\":\"mock-model\"}")),
    }]);
    let trace_path = std::env::temp_dir().join(format!(
        "lash-standard-trace-error-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let mut runtime = standard_runtime_with_transport_and_host(
        transport,
        test_host_config_with_trace_path(trace_path.clone()),
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: lash_core::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("trace-failed-llm-turn"),
            ),
        )
        .await
        .expect("turn");

    assert!(matches!(&turn.outcome, TurnOutcome::Stopped(_)));
    assert_eq!(turn.errors.len(), 1);
    assert_eq!(turn.errors[0].raw.as_deref(), Some("transport raw body"));

    let logged = std::fs::read_to_string(&trace_path).expect("read trace");
    let entries =
        lash_trace::parse_jsonl_records::<serde_json::Value>(&logged).expect("json log entries");
    let error_entry = entries
        .iter()
        .find(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("llm_call_failed"))
        .expect("llm error entry");
    assert_eq!(
        error_entry["error"]["message"].as_str(),
        Some("HTTP request failed: builder error")
    );
    assert_eq!(error_entry["error"]["code"].as_str(), Some("builder"));
    assert_eq!(
        error_entry["error"]["code_namespace"].as_str(),
        Some("provider")
    );
    assert_eq!(
        error_entry["error"]["raw"].as_str(),
        Some("transport raw body")
    );
    let request_entry = entries
        .iter()
        .find(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("llm_call_started"))
        .expect("llm request entry");
    assert_eq!(
        request_entry["request"]["model"].as_str(),
        Some("mock-model")
    );
}

#[test]
fn nonzero_usage_carries_the_last_call_verbatim() {
    let usage = TokenUsage {
        input_tokens: 80,
        output_tokens: 0,
        cache_read_input_tokens: 20,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: 0,
    };
    let carried = nonzero_usage(usage.clone()).expect("nonzero usage is carried");
    assert_eq!(carried, usage);
    assert_eq!(carried.input_total(), 100);
    assert_eq!(carried.total(), 100);
    assert_eq!(nonzero_usage(TokenUsage::default()), None);
}
