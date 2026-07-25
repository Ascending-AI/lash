use super::*;
use crate::rlm::RlmTurnBuilderExt as _;
use futures_util::StreamExt as _;
use std::collections::BTreeSet;

#[derive(Clone, Debug)]
struct DurableEffectInvocation {
    kind: lash_core::RuntimeEffectKind,
    turn_id: Option<String>,
    replay_key: Option<String>,
}

#[derive(Default)]
struct RecordingDurableEffectController {
    invocations: StdMutex<Vec<DurableEffectInvocation>>,
}

impl RecordingDurableEffectController {
    fn invocations(&self) -> Vec<DurableEffectInvocation> {
        self.invocations
            .lock()
            .expect("durable effect invocations")
            .clone()
    }
}

impl lash_core::AwaitEventResolver for RecordingDurableEffectController {
    fn durability_tier(&self) -> DurabilityTier {
        DurabilityTier::Durable
    }
}

#[async_trait]
impl lash_core::RuntimeEffectController for RecordingDurableEffectController {
    async fn execute_effect(
        &self,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> std::result::Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError>
    {
        self.invocations
            .lock()
            .expect("durable effect invocations")
            .push(DurableEffectInvocation {
                kind: envelope.invocation.effect_kind().expect("effect kind"),
                turn_id: envelope.invocation.scope.turn_id.clone(),
                replay_key: envelope.invocation.replay_key().map(ToOwned::to_owned),
            });
        if matches!(
            &envelope.command,
            lash_core::RuntimeEffectCommand::PeekAwaitEvent { .. }
        ) {
            return Ok(lash_core::RuntimeEffectOutcome::PeekAwaitEvent { resolution: None });
        }
        local_executor.execute(envelope).await
    }
}

#[derive(Default)]
struct RecordingInlineEffectController {
    invocations: StdMutex<Vec<DurableEffectInvocation>>,
    inline: lash_core::InlineRuntimeEffectController,
}

impl RecordingInlineEffectController {
    fn invocations(&self) -> Vec<DurableEffectInvocation> {
        self.invocations
            .lock()
            .expect("inline effect invocations")
            .clone()
    }
}

#[async_trait]
impl lash_core::AwaitEventResolver for RecordingInlineEffectController {
    async fn await_event_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> std::result::Result<lash_core::AwaitEventKey, lash_core::RuntimeError> {
        self.inline.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> std::result::Result<lash_core::ResolveOutcome, lash_core::RuntimeError> {
        self.inline.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> std::result::Result<Option<lash_core::Resolution>, lash_core::RuntimeError> {
        self.inline.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> std::result::Result<lash_core::Resolution, lash_core::RuntimeError> {
        self.inline.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &str,
    ) -> std::result::Result<(), lash_core::RuntimeError> {
        self.inline
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &str,
    ) -> std::result::Result<(), lash_core::RuntimeError> {
        self.inline
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait]
impl lash_core::RuntimeEffectController for RecordingInlineEffectController {
    async fn execute_effect(
        &self,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> std::result::Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError>
    {
        self.invocations
            .lock()
            .expect("inline effect invocations")
            .push(DurableEffectInvocation {
                kind: envelope.invocation.effect_kind().expect("effect kind"),
                turn_id: envelope.invocation.scope.turn_id.clone(),
                replay_key: envelope.invocation.replay_key().map(ToOwned::to_owned),
            });
        if matches!(
            &envelope.command,
            lash_core::RuntimeEffectCommand::PeekAwaitEvent { .. }
        ) {
            return Ok(lash_core::RuntimeEffectOutcome::PeekAwaitEvent { resolution: None });
        }
        local_executor.execute(envelope).await
    }
}

#[derive(Default)]
struct DurableInMemoryProcessEnvStore {
    inner: lash_core::InMemoryProcessExecutionEnvStore,
}

#[async_trait]
impl lash_core::ProcessExecutionEnvStore for DurableInMemoryProcessEnvStore {
    fn durability_tier(&self) -> DurabilityTier {
        DurabilityTier::Durable
    }

    async fn put_process_execution_env(
        &self,
        env_ref: &lash_core::ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> std::result::Result<(), lash_core::PluginError> {
        self.inner.put_process_execution_env(env_ref, bytes).await
    }

    async fn get_process_execution_env(
        &self,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> std::result::Result<Option<Vec<u8>>, lash_core::PluginError> {
        self.inner.get_process_execution_env(env_ref).await
    }
}

#[derive(Default)]
struct DurableNoopEffectHost;

impl lash_core::AwaitEventResolver for DurableNoopEffectHost {
    fn durability_tier(&self) -> DurabilityTier {
        DurabilityTier::Durable
    }
}

impl lash_core::EffectHost for DurableNoopEffectHost {
    fn scoped<'run>(
        &'run self,
        scope: lash_core::ExecutionScope,
    ) -> std::result::Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        lash_core::ScopedEffectController::shared(
            Arc::new(lash_core::InlineRuntimeEffectController::default()),
            scope,
        )
    }

    fn scoped_static(
        &self,
        scope: lash_core::ExecutionScope,
    ) -> std::result::Result<
        Option<lash_core::ScopedEffectController<'static>>,
        lash_core::RuntimeError,
    > {
        Ok(Some(lash_core::ScopedEffectController::shared(
            Arc::new(lash_core::InlineRuntimeEffectController::default()),
            scope,
        )?))
    }
}

struct BlockingAppTools {
    entered_tx: StdMutex<Option<oneshot::Sender<()>>>,
    release_rx: TokioMutex<Option<oneshot::Receiver<()>>>,
}

impl BlockingAppTools {
    fn new(entered_tx: oneshot::Sender<()>, release_rx: oneshot::Receiver<()>) -> Self {
        Self {
            entered_tx: StdMutex::new(Some(entered_tx)),
            release_rx: TokioMutex::new(Some(release_rx)),
        }
    }
}

#[async_trait]
impl ToolProvider for BlockingAppTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![app_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(app_tool_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
        assert_eq!(call.name, "app_lookup");
        if let Some(tx) = self.entered_tx.lock().expect("entered tx").take() {
            let _ = tx.send(());
        }
        if let Some(rx) = self.release_rx.lock().await.take() {
            let _ = rx.await;
        }
        lash_core::ToolResult::ok(serde_json::json!({ "answer": "ready" }))
    }
}

struct RuntimeBatchTools {
    barrier: Arc<tokio::sync::Barrier>,
    windows: Arc<StdMutex<Vec<(String, std::time::Instant, std::time::Instant)>>>,
}

impl RuntimeBatchTools {
    fn new() -> Self {
        Self {
            barrier: Arc::new(tokio::sync::Barrier::new(3)),
            windows: Arc::new(StdMutex::new(Vec::new())),
        }
    }

    fn windows(&self) -> Vec<(String, std::time::Instant, std::time::Instant)> {
        self.windows.lock().expect("execution windows").clone()
    }
}

#[async_trait]
impl ToolProvider for RuntimeBatchTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![
            runtime_batch_tool_definition().manifest(),
            runtime_probe_tool_definition("first").manifest(),
            runtime_probe_tool_definition("formerly_serial").manifest(),
            runtime_probe_tool_definition("last").manifest(),
        ]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        match name {
            "runtime_batch" => Some(Arc::new(runtime_batch_tool_definition().contract())),
            "first" => Some(Arc::new(runtime_probe_tool_definition("first").contract())),
            "formerly_serial" => Some(Arc::new(
                runtime_probe_tool_definition("formerly_serial").contract(),
            )),
            "last" => Some(Arc::new(runtime_probe_tool_definition("last").contract())),
            _ => None,
        }
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
        match call.name {
            "runtime_batch" => execute_runtime_batch_tool(call.context, call.args).await,
            "first" | "formerly_serial" | "last" => {
                let start = std::time::Instant::now();
                let waited = tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    self.barrier.wait(),
                )
                .await;
                let end = std::time::Instant::now();
                self.windows.lock().expect("execution windows").push((
                    call.name.to_string(),
                    start,
                    end,
                ));
                match waited {
                    Ok(_) => lash_core::ToolResult::ok(serde_json::json!(call.name)),
                    Err(_) => lash_core::ToolResult::err_fmt(format!(
                        "{} did not overlap with the rest of the batch",
                        call.name
                    )),
                }
            }
            other => lash_core::ToolResult::err_fmt(format!("Unknown tool: {other}")),
        }
    }
}

fn runtime_batch_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:runtime_batch",
        "runtime_batch",
        "Execute a batch of tool calls.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "tool_calls": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "tool": { "type": "string" },
                            "parameters": { "type": "object", "additionalProperties": true }
                        },
                        "required": ["tool", "parameters"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["tool_calls"],
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
}

fn runtime_probe_tool_definition(name: &'static str) -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        format!("Probe tool {name}."),
        serde_json::json!({ "type": "object", "additionalProperties": false }),
        serde_json::json!({}),
    )
}

async fn execute_runtime_batch_tool(
    context: &lash_core::ToolContext<'_>,
    args: &serde_json::Value,
) -> lash_core::ToolResult {
    let Some(raw_calls) = args.get("tool_calls").and_then(serde_json::Value::as_array) else {
        return lash_core::ToolResult::err_fmt("Missing required parameter: tool_calls");
    };
    let mut invocations = Vec::with_capacity(raw_calls.len());
    let mut immediate_results = Vec::new();
    let dispatch = context.dispatch();
    for (index, item) in raw_calls.iter().enumerate() {
        let Some(tool_name) = item.get("tool").and_then(serde_json::Value::as_str) else {
            return lash_core::ToolResult::err_fmt(format!("Invalid tool_calls[{index}].tool"));
        };
        let Some(manifest) = dispatch.callable_tool_manifest(tool_name) else {
            immediate_results.push(serde_json::json!({
                "index": index,
                "tool": tool_name,
                "success": false,
                "value": format!("Tool '{tool_name}' is unavailable in this session"),
            }));
            continue;
        };
        invocations.push((
            index,
            lash_core::ToolInvocation::new(
                format!("runtime-batch:{index}"),
                manifest.id,
                item.get("parameters")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
            ),
        ));
    }

    let outcomes = dispatch
        .batch(
            invocations
                .iter()
                .map(|(_, invocation)| invocation.clone())
                .collect(),
        )
        .await;
    let mut results = invocations
        .into_iter()
        .zip(outcomes)
        .map(|((index, invocation), outcome)| {
            let tool = outcome
                .record
                .as_ref()
                .map(|record| record.tool.clone())
                .unwrap_or_else(|| invocation.label());
            let output = outcome.output;
            serde_json::json!({
                "index": index,
                "tool": tool,
                "success": output.is_success(),
                "value": output.value_for_projection(),
            })
        })
        .collect::<Vec<_>>();
    results.extend(immediate_results);
    lash_core::ToolResult::ok(serde_json::json!({ "results": results }))
}

fn runtime_batch_provider() -> ProviderHandle {
    let responses = Arc::new(TokioMutex::new(VecDeque::from([
        LlmResponse {
            parts: vec![LlmOutputPart::ToolCall {
                call_id: "batch-call".to_string(),
                tool_name: "runtime_batch".to_string(),
                input_json: serde_json::json!({
                    "tool_calls": [
                        { "tool": "first", "parameters": {} },
                        { "tool": "formerly_serial", "parameters": {} },
                        { "tool": "last", "parameters": {} }
                    ]
                })
                .to_string(),
                replay: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        },
        LlmResponse {
            full_text: "done".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "done".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        },
    ])));
    crate::testing::TestProvider::builder()
        .kind("runtime-batch-test")
        .complete(move |_request| {
            let responses = Arc::clone(&responses);
            async move { Ok(responses.lock().await.pop_front().expect("queued response")) }
        })
        .build()
        .into_handle()
}

#[tokio::test]
async fn turn_run_uses_configured_inline_effect_host_without_explicit_effects() -> Result<()> {
    let recorder = Arc::new(RecordingInlineEffectController::default());
    let effect_controller: Arc<dyn lash_core::RuntimeEffectController> = recorder.clone();
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .effect_host(Arc::new(lash_core::InlineEffectHost::new(
            effect_controller,
        )))
        .provider(mock_provider())
        .model(mock_model_spec())
        .build()?;
    let session = core.session("inline-default-effect-host").open().await?;

    let output = session.turn(TurnInput::text("inline")).run().await?;

    assert_eq!(output.assistant_message(), Some("echo: inline"));
    let invocations = recorder.invocations();
    assert!(
        invocations
            .iter()
            .any(|record| record.kind == lash_core::RuntimeEffectKind::LlmCall)
    );
    assert!(invocations.iter().all(|record| {
        record
            .turn_id
            .as_deref()
            .is_some_and(|turn_id| !turn_id.trim().is_empty())
    }));
    Ok(())
}

#[tokio::test]
async fn durable_configured_effect_host_requires_explicit_handler_effects() -> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = LashCore::rlm_builder(rlm_factory())
        .attachment_store(Arc::new(crate::persistence::FileAttachmentStore::new(
            dir.path().join("attachments"),
        )))
        .process_env_store(Arc::new(DurableInMemoryProcessEnvStore::default()))
        .effect_host(Arc::new(DurableNoopEffectHost))
        .provider(mock_provider())
        .model(mock_model_spec())
        .build()?;
    let session = core.session("durable-default-effect-host").open().await?;

    let err = session
        .turn(TurnInput::text("should fail before provider"))
        .run()
        .await
        .expect_err("durable deployment host should require handler context");

    assert!(matches!(
        err,
        EmbedError::DurableEffectHostRequiresHandlerContext { operation: "turn" }
    ));
    Ok(())
}

#[tokio::test]
async fn turn_id_sets_execution_scope_and_trace_identity() -> Result<()> {
    let recorder = Arc::new(RecordingInlineEffectController::default());
    let effect_controller: Arc<dyn lash_core::RuntimeEffectController> = recorder.clone();
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .effect_host(Arc::new(lash_core::InlineEffectHost::new(
            effect_controller,
        )))
        .provider(mock_provider())
        .model(mock_model_spec())
        .build()?;
    let session = core.session("stable-turn-id").open().await?;

    session
        .turn(TurnInput::text("stable"))
        .turn_id("stable-turn")
        .run()
        .await?;

    let llm_invocation = recorder
        .invocations()
        .into_iter()
        .find(|record| record.kind == lash_core::RuntimeEffectKind::LlmCall)
        .expect("llm effect");
    assert_eq!(llm_invocation.turn_id.as_deref(), Some("stable-turn"));
    assert!(
        llm_invocation
            .replay_key
            .as_deref()
            .is_some_and(|key| key.contains("stable-turn"))
    );
    Ok(())
}

#[tokio::test]
async fn explicit_effect_controller_creates_turn_scope_internally() -> Result<()> {
    let recorder = RecordingInlineEffectController::default();
    let core = standard_core();
    let session = core.session("explicit-handler-effects").open().await?;

    session
        .turn(TurnInput::text("handler"))
        .turn_id("handler-turn")
        .effects(&recorder)
        .run()
        .await?;

    let llm_invocation = recorder
        .invocations()
        .into_iter()
        .find(|record| record.kind == lash_core::RuntimeEffectKind::LlmCall)
        .expect("llm effect");
    assert_eq!(llm_invocation.turn_id.as_deref(), Some("handler-turn"));
    Ok(())
}

#[tokio::test]
async fn queued_turn_run_drains_ready_work_and_returns_none_when_idle() -> Result<()> {
    let requests = Arc::new(StdMutex::new(
        Vec::<Vec<lash_core::llm::types::LlmMessage>>::new(),
    ));
    let captured_requests = Arc::clone(&requests);
    let provider = crate::testing::TestProvider::builder()
        .kind("queued-next-prompt-shape")
        .complete(move |request| {
            let captured_requests = Arc::clone(&captured_requests);
            async move {
                captured_requests
                    .lock()
                    .expect("request capture")
                    .push(request.messages.clone());
                Ok(text_response("echo: queued work"))
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .disable_queued_work_driver()
        .build()?;
    let session = core.session("queued-turn-run").open().await?;
    session
        .enqueue(TurnInput::text("queued work"))
        .id("queued-request")
        .send()
        .await?;

    let output = session
        .queued_turn()
        .run()
        .await?
        .expect("queued turn should run");

    assert_eq!(output.assistant_message(), Some("echo: queued work"));
    {
        let requests = requests.lock().expect("request capture");
        assert_eq!(requests.len(), 1);
        assert_eq!(
            serde_json::to_string(&requests[0][1..])
                .expect("serialize queued-next request user messages"),
            r#"[{"role":"User","blocks":[{"Text":{"text":"queued work","response_meta":null,"cache_breakpoint":false}}]}]"#
        );
    }
    assert!(session.queued_turn().run().await?.is_none());
    Ok(())
}

#[tokio::test]
async fn idle_queued_input_emits_typed_remote_application_and_durable_identity() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .disable_queued_work_driver()
        .build()?;
    let session = core.session("idle-input-application").open().await?;
    let cursor = session.observe().current_remote_observation().cursor;
    let empty_admission = session
        .enqueue(TurnInput::text(""))
        .id("idle-empty-source")
        .send()
        .await?;
    let admission = session
        .enqueue(TurnInput::text("queued canonical input"))
        .id("idle-source")
        .send()
        .await?;

    session
        .queued_turn()
        .drain_id("idle-application-turn")
        .run()
        .await?
        .expect("queued input should run");

    let crate::observe::RemoteSessionObservationSubscription::Subscribed(mut subscription) =
        session.observe().subscribe_from_remote_cursor(
            &crate::remote::observations::RemoteSessionCursor::new(cursor),
        )?
    else {
        panic!("recent cursor should replay typed application");
    };
    let live = loop {
        let event =
            tokio::time::timeout(std::time::Duration::from_secs(2), subscription.next_event())
                .await
                .expect("timed out waiting for typed idle application")
                .expect("remote observation event");
        let crate::remote::observations::RemoteSessionObservationEventPayload::TurnActivity {
            activity,
        } = event.event
        else {
            continue;
        };
        if let crate::remote::usage::RemoteTurnEvent::TurnInputApplied { applications } =
            activity.event
        {
            break applications;
        }
    };
    assert_eq!(
        live.len(),
        1,
        "only inputs materialized into the canonical message receive application evidence"
    );
    let live = &live[0];
    assert_ne!(live.input_id, empty_admission.input_id);
    assert_eq!(live.input_id, admission.input_id);
    assert_eq!(live.source_key.as_deref(), Some("host:idle-source"));
    assert_eq!(live.turn_id, "idle-application-turn");
    assert_eq!(live.checkpoint, None);
    assert!(
        session
            .read_view()
            .messages()
            .iter()
            .any(|message| message.id == live.committed_message_id),
        "typed evidence must identify the canonical committed message"
    );

    let durable = session.remote_turn_input_applications().await?;
    assert_eq!(durable, vec![live.clone()]);
    Ok(())
}

#[tokio::test]
async fn durable_application_read_survives_a_trimmed_live_replay_window() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .live_replay_store(Arc::new(lash_core::InMemoryLiveReplayStore::new(
            lash_core::InMemoryLiveReplayStoreConfig {
                max_events_per_session: 1,
                ..lash_core::InMemoryLiveReplayStoreConfig::default()
            },
        )))
        .disable_queued_work_driver()
        .build()?;
    let session = core.session("durable-input-application-gap").open().await?;
    let stale_cursor = session.observe().current_remote_observation().cursor;
    let admission = session
        .enqueue(TurnInput::text("survives replay gap"))
        .id("gap-source")
        .send()
        .await?;
    session
        .queued_turn()
        .drain_id("gap-application-turn")
        .run()
        .await?
        .expect("queued input should run");

    let mut recovery = session.observe().subscribe_and_recover_remote(
        crate::remote::observations::RemoteSessionCursor::new(stale_cursor),
    )?;
    let item = tokio::time::timeout(std::time::Duration::from_secs(2), recovery.next())
        .await
        .expect("timed out waiting for replay gap")
        .expect("recovery stream item")?;
    assert!(matches!(
        item,
        crate::observe::RemoteSessionObservationStreamItem::Gap { .. }
    ));

    let applications = session.remote_turn_input_applications().await?;
    assert!(matches!(
        applications.as_slice(),
        [application]
            if application.input_id == admission.input_id
                && application.source_key.as_deref() == Some("host:gap-source")
                && application.turn_id == "gap-application-turn"
                && application.checkpoint.is_none()
                && session
                    .read_view()
                    .messages()
                    .iter()
                    .any(|message| message.id == application.committed_message_id)
    ));
    Ok(())
}

#[tokio::test]
async fn queued_turn_explicit_effects_create_queue_drain_scope_internally() -> Result<()> {
    let recorder = RecordingInlineEffectController::default();
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .disable_queued_work_driver()
        .build()?;
    let session = core.session("queued-explicit-effects").open().await?;
    session
        .enqueue(TurnInput::text("queued handler"))
        .send()
        .await?;

    let output = session
        .queued_turn()
        .drain_id("handler-drain")
        .effects(&recorder)
        .run()
        .await?
        .expect("queued turn should run");

    assert_eq!(output.assistant_message(), Some("echo: queued handler"));
    let llm_invocation = recorder
        .invocations()
        .into_iter()
        .find(|record| record.kind == lash_core::RuntimeEffectKind::LlmCall)
        .expect("llm effect");
    assert_eq!(llm_invocation.turn_id.as_deref(), Some("handler-drain"));
    Ok(())
}

#[tokio::test]
async fn turn_builder_stream_emits_activities_and_finishes() -> Result<()> {
    let core = standard_core();
    let session = core.session("turn-stream").open().await?;
    let mut stream = session.turn(TurnInput::text("stream me")).stream()?;

    let mut activities = Vec::new();
    while let Some(activity) = stream.next().await {
        activities.push(activity?);
    }
    let result = stream.finish().await?;

    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::TurnFinish::AssistantMessage { .. })
    ));
    assert_eq!(assistant_prose(&activities), "echo: stream me");
    assert!(
        activities
            .iter()
            .any(|activity| matches!(&activity.event, TurnEvent::AssistantProseDelta { .. }))
    );
    Ok(())
}

#[tokio::test]
async fn session_observation_replays_live_activity_and_commit() -> Result<()> {
    let core = standard_core();
    let session = core.session("session-observation-replay").open().await?;
    let cursor = session.observe().current_observation().cursor;

    let output = session.turn(TurnInput::text("observe me")).run().await?;
    assert_eq!(assistant_prose(&output.activities), "echo: observe me");

    let replay = session.observe().resume_from_cursor(&cursor)?;
    let SessionResume::Replayed { events } = replay else {
        panic!("recent cursor should replay live events");
    };
    assert!(events.iter().any(|event| {
        matches!(
            &event.payload,
            lash_core::SessionObservationEventPayload::TurnActivity(activity)
                if matches!(
                    &activity.event,
                    TurnEvent::AssistantProseDelta { text } if text.as_ref() == "echo: observe me"
                )
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            &event.payload,
            lash_core::SessionObservationEventPayload::Committed { .. }
        )
    }));
    Ok(())
}

fn retrying_visible_stream_provider() -> ProviderHandle {
    let attempts = Arc::new(AtomicUsize::new(0));
    crate::testing::TestProvider::builder()
        .kind("retrying-visible-stream")
        .requires_streaming(true)
        .options(lash_core::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(3)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..lash_core::ProviderOptions::default()
        })
        .complete(move |request| {
            let attempts = Arc::clone(&attempts);
            async move {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                let stream = request.stream_events.expect("stream events");
                stream.send(LlmStreamEvent::ReasoningDelta(format!(
                    "reasoning-{attempt}"
                )));
                stream.send(LlmStreamEvent::Delta(format!("prose-{attempt}")));
                if attempt < 3 {
                    return Err(
                        LlmTransportError::new(format!("retry attempt {attempt}")).retryable(true)
                    );
                }
                Ok(LlmResponse {
                    full_text: "prose-3".to_string(),
                    parts: vec![LlmOutputPart::Text {
                        text: "prose-3".to_string(),
                        response_meta: None,
                    }],
                    terminal_reason: lash_core::LlmTerminalReason::Stop,
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

fn retrying_rlm_prose_provider(
    transport_calls: Arc<AtomicUsize>,
    requests: Arc<StdMutex<Vec<lash_core::LlmRequest>>>,
) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("retrying-rlm-prose")
        .requires_streaming(true)
        .options(lash_core::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..lash_core::ProviderOptions::default()
        })
        .complete(move |request| {
            let transport_calls = Arc::clone(&transport_calls);
            let requests = Arc::clone(&requests);
            async move {
                requests
                    .lock()
                    .expect("retrying RLM requests")
                    .push(request.clone());
                let call = transport_calls.fetch_add(1, Ordering::SeqCst);
                let stream = request.stream_events.expect("stream events");
                if call == 0 {
                    stream.send(LlmStreamEvent::Delta(
                        "retry observer single-copy marker\n<lashlang>\n".to_string(),
                    ));
                    return Err(
                        LlmTransportError::new("deterministic rate limit")
                            .with_status(429)
                            .retryable(true),
                    );
                }
                let text = match call {
                    1 => {
                        "retry observer single-copy marker\n<lashlang>\nretry_missing_name\n</lashlang>"
                    }
                    2 => "<lashlang>\nfinish \"provider retry succeeded\"\n</lashlang>",
                    _ => "<lashlang>\nfinish \"subsequent turn succeeded\"\n</lashlang>",
                };
                stream.send(LlmStreamEvent::Delta(text.to_string()));
                Ok(LlmResponse {
                    full_text: text.to_string(),
                    parts: vec![LlmOutputPart::Text {
                        text: text.to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

fn provider_request_text(request: &lash_core::LlmRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn rlm_provider_retry_commits_only_surviving_prose() -> Result<()> {
    run_async_test_on_stack_budget("rlm-provider-retry-prose-test", || async {
        const MARKER: &str = "retry observer single-copy marker";
        const EXEC_ERROR: &str = "unknown name `retry_missing_name`\n--> line 1, column 1\nretry_missing_name\n^~~~~~~~~~~~~~~~~~";
        let transport_calls = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let core = explicit_ephemeral_facets(LashCore::rlm_builder(rlm_factory()))
            .provider(retrying_rlm_prose_provider(
                Arc::clone(&transport_calls),
                Arc::clone(&requests),
            ))
            .model(mock_model_spec())
            .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
            .process_registry(Arc::new(TestLocalProcessRegistry::default()))
            .build()?;
        let session = core.session("rlm-provider-retry-prose").open().await?;

        let first = session
            .turn(TurnInput::text("trigger deterministic rate limit retry"))
            .run()
            .await?;

        assert_eq!(
            transport_calls.load(Ordering::SeqCst),
            3,
            "one failed attempt, its retry, and one error continuation must finish the turn"
        );
        assert_eq!(
            first.final_value(),
            Some(&serde_json::json!("provider retry succeeded"))
        );
        let rlm_marker_events = first
            .result
            .state
            .read_view()
            .active_events()
            .iter()
            .filter(|event| {
                let lash_core::SessionHistoryRecord::Protocol(event) = event else {
                    return false;
                };
                matches!(
                    lash_protocol_rlm::decode_rlm_protocol_event(event),
                    Some(lash_rlm_types::RlmProtocolEvent::RlmAssistantContent(content))
                        if content.prose.contains(MARKER)
                )
            })
            .count();
        assert_eq!(
            rlm_marker_events, 1,
            "RLM semantic history retained aborted-attempt prose"
        );
        {
            let requests = requests.lock().expect("retrying RLM requests");
            assert_eq!(requests.len(), 3, "RLM scheduled a spurious iteration");
            let continuation_request = requests.last().expect("post-error continuation request");
            let continuation = provider_request_text(continuation_request);
            assert_eq!(
                continuation.matches(MARKER).count(),
                1,
                "provider continuation duplicated retry prose: {continuation}"
            );
            assert_eq!(
                continuation.matches(EXEC_ERROR).count(),
                1,
                "provider continuation duplicated exec error: {continuation}"
            );
            assert_eq!(
                continuation
                    .matches(&format!("Error:\n{EXEC_ERROR}"))
                    .count(),
                1,
                "provider continuation duplicated the rendered Error block: {continuation}"
            );
        }

        session
            .admin()
            .state()
            .append_messages(vec![
                lash_core::PluginMessage::text(
                    lash_core::MessageRole::Assistant,
                    format!("{MARKER}\n\nprovider retry succeeded"),
                )
                .with_id("workbench-assistant:retry-turn"),
            ])
            .await?;

        let marker_messages = || {
            session
                .read_view()
                .messages()
                .iter()
                .filter(|message| {
                    message.role == lash_core::MessageRole::Assistant
                        && message
                            .parts
                            .iter()
                            .any(|part| part.content.contains(MARKER))
                })
                .count()
        };
        assert_eq!(
            marker_messages(),
            1,
            "conversation history should contain only the host transcript"
        );
        let persisted = session.admin().state().persist_current().await?;
        session.close().await?;

        let reopened = core.session("rlm-provider-retry-prose").open().await?;
        reopened.admin().state().set_persisted(persisted).await?;
        assert_eq!(
            reopened
                .read_view()
                .messages()
                .iter()
                .filter(|message| {
                    message.role == lash_core::MessageRole::Assistant
                        && message
                            .parts
                            .iter()
                            .any(|part| part.content.contains(MARKER))
                })
                .count(),
            1,
            "reloaded conversation history should contain one transcript"
        );

        reopened
            .turn(TurnInput::text("check retry history"))
            .run()
            .await?;
        let requests = requests.lock().expect("retrying RLM requests");
        assert_eq!(requests.len(), 4);
        let subsequent_history = requests.last().expect("subsequent request");
        let subsequent_history_json = serde_json::to_string(subsequent_history)?;
        let subsequent_history_text = provider_request_text(subsequent_history);
        let marker_count = subsequent_history_text.matches(MARKER).count();
        assert_eq!(
            marker_count, 1,
            "provider-visible history duplicated retry prose: {subsequent_history_text}"
        );
        assert_eq!(
            subsequent_history_text.matches(EXEC_ERROR).count(),
            1,
            "provider-visible history duplicated exec error: {subsequent_history_json}"
        );
        Ok(())
    })
}

fn render_observed_attempt_text(
    events: &[Arc<lash_core::SessionObservationEvent>],
) -> (String, String) {
    let mut prose = Vec::new();
    let mut reasoning = Vec::new();
    for event in events {
        let lash_core::SessionObservationEventPayload::TurnActivity(activity) = &event.payload
        else {
            continue;
        };
        match &activity.event {
            TurnEvent::AssistantProseDelta { text } => {
                prose.push((activity.correlation_id.clone(), text.clone()));
            }
            TurnEvent::ReasoningDelta { text } => {
                reasoning.push((activity.correlation_id.clone(), text.clone()));
            }
            TurnEvent::ModelAttemptReset {
                assistant_prose_correlation_ids,
                reasoning_correlation_ids,
            } => {
                prose.retain(|(correlation_id, _)| {
                    !assistant_prose_correlation_ids.contains(correlation_id)
                });
                reasoning.retain(|(correlation_id, _)| {
                    !reasoning_correlation_ids.contains(correlation_id)
                });
            }
            _ => {}
        }
    }
    (
        prose
            .into_iter()
            .map(|(_, text)| text.to_string())
            .collect(),
        reasoning
            .into_iter()
            .map(|(_, text)| text.to_string())
            .collect(),
    )
}

fn model_attempt_resets(events: &[Arc<lash_core::SessionObservationEvent>]) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(
                &event.payload,
                lash_core::SessionObservationEventPayload::TurnActivity(activity)
                    if matches!(&activity.event, TurnEvent::ModelAttemptReset { .. })
            )
        })
        .count()
}

#[tokio::test]
async fn session_observation_envelopes_scope_activity_and_commit_to_the_turn() -> Result<()> {
    let core = standard_core();
    let session = core
        .session("session-observation-turn-identity")
        .open()
        .await?;
    let cursor = session.observe().current_observation().cursor;

    session
        .turn(TurnInput::text("identify this turn"))
        .turn_id("observation-turn")
        .run()
        .await?;

    let lash_core::SessionResume::Replayed { events } =
        session.observe().resume_from_cursor(&cursor)?
    else {
        panic!("fresh turn observation cursor should remain replayable");
    };
    let turn_activity = events
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                lash_core::SessionObservationEventPayload::TurnActivity(_)
            )
        })
        .collect::<Vec<_>>();
    assert!(!turn_activity.is_empty(), "turn emitted no activity");
    assert!(
        turn_activity
            .iter()
            .all(|event| event.turn_id.as_deref() == Some("observation-turn")),
        "every turn activity must carry its producing turn identity"
    );
    let committed = events
        .iter()
        .find(|event| {
            matches!(
                event.payload,
                lash_core::SessionObservationEventPayload::Committed { .. }
            )
        })
        .expect("turn commit observation");
    assert_eq!(committed.turn_id.as_deref(), Some("observation-turn"));
    Ok(())
}

#[tokio::test]
async fn session_observation_retracts_two_retried_visible_attempts_live_and_on_replay() -> Result<()>
{
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(retrying_visible_stream_provider())
        .model(mock_model_spec())
        .build()?;
    let session = core.session("retry-visible-observation").open().await?;
    let cursor = session.observe().current_observation().cursor;
    let lash_core::SessionObservationSubscription::Subscribed(mut subscription) =
        session.observe().subscribe_from_cursor(&cursor)?
    else {
        panic!("fresh cursor should subscribe without a gap");
    };
    let live_collector = tokio::spawn(async move {
        let mut events = Vec::new();
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(2), subscription.next_event())
                    .await
                    .expect("timed out waiting for live observation")
                    .expect("live observation event");
            let committed = matches!(
                event.payload,
                lash_core::SessionObservationEventPayload::Committed { .. }
            );
            events.push(event);
            if committed {
                break;
            }
        }
        events
    });

    let output = session
        .turn(TurnInput::text("retry twice after visible output"))
        .run()
        .await?;
    assert_eq!(output.assistant_message(), Some("prose-3"));
    let live_events = live_collector.await.expect("live collector task");

    let lash_core::SessionResume::Replayed {
        events: replay_events,
    } = session.observe().resume_from_cursor(&cursor)?
    else {
        panic!("recent cursor should replay all attempt activity");
    };

    assert_eq!(
        render_observed_attempt_text(&live_events),
        ("prose-3".to_string(), "reasoning-3".to_string())
    );
    assert_eq!(
        render_observed_attempt_text(&replay_events),
        ("prose-3".to_string(), "reasoning-3".to_string())
    );
    assert_eq!(model_attempt_resets(&live_events), 2);
    assert_eq!(model_attempt_resets(&replay_events), 2);
    Ok(())
}

#[tokio::test]
async fn session_observation_rejects_cursor_from_another_session() -> Result<()> {
    let core = standard_core();
    let session = core.session("session-observation-a").open().await?;
    let other = core.session("session-observation-b").open().await?;
    let other_cursor = other.observe().current_observation().cursor;

    let err = session
        .observe()
        .resume_from_cursor(&other_cursor)
        .expect_err("cursor from another session should be rejected");
    assert!(
        err.to_string().contains("session-observation-b")
            && err.to_string().contains("session-observation-a"),
        "unexpected error: {err}"
    );
    Ok(())
}

#[tokio::test]
async fn session_observation_subscription_replays_buffered_events_before_live_events() -> Result<()>
{
    let core = standard_core();
    let session = core
        .session("session-observation-subscribe-replay")
        .open()
        .await?;
    let cursor = session.observe().current_observation().cursor;

    session
        .turn(TurnInput::text("first observed"))
        .run()
        .await?;
    let SessionObservationSubscription::Subscribed(mut subscription) =
        session.observe().subscribe_from_cursor(&cursor)?
    else {
        panic!("recent cursor should subscribe without a gap");
    };

    loop {
        let event =
            tokio::time::timeout(std::time::Duration::from_secs(2), subscription.next_event())
                .await
                .expect("timed out waiting for replayed event")
                .expect("replayed event");
        if observation_assistant_delta(&event).as_deref() == Some("echo: first observed") {
            break;
        }
    }

    session
        .turn(TurnInput::text("second observed"))
        .run()
        .await?;
    loop {
        let event =
            tokio::time::timeout(std::time::Duration::from_secs(2), subscription.next_event())
                .await
                .expect("timed out waiting for live event")
                .expect("live event");
        if observation_assistant_delta(&event).as_deref() == Some("echo: second observed") {
            break;
        }
    }
    Ok(())
}

#[tokio::test]
async fn session_observation_recovery_stream_replays_buffered_events_before_live_events()
-> Result<()> {
    let core = standard_core();
    let session = core
        .session("session-observation-recovered-stream")
        .open()
        .await?;
    let cursor = session.observe().current_observation().cursor;

    session
        .turn(TurnInput::text("first recovered"))
        .run()
        .await?;
    let mut stream = session.observe().subscribe_and_recover(cursor);

    loop {
        let item = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .expect("timed out waiting for replayed stream item")
            .expect("replayed stream should stay open")?;
        if let crate::observe::SessionObservationStreamItem::Event(event) = item
            && observation_assistant_delta(&event).as_deref() == Some("echo: first recovered")
        {
            break;
        }
    }

    session
        .turn(TurnInput::text("second recovered"))
        .run()
        .await?;
    loop {
        let item = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .expect("timed out waiting for live stream item")
            .expect("live stream should stay open")?;
        if let crate::observe::SessionObservationStreamItem::Event(event) = item
            && observation_assistant_delta(&event).as_deref() == Some("echo: second recovered")
        {
            break;
        }
    }
    Ok(())
}

#[tokio::test]
async fn session_observation_remote_subscription_replays_dto_events() -> Result<()> {
    let core = standard_core();
    let session = core
        .session("session-observation-remote-subscribe")
        .open()
        .await?;
    let observation = session.observe().current_remote_observation();
    assert_eq!(
        observation.session_id,
        "session-observation-remote-subscribe"
    );

    session
        .turn(TurnInput::text("remote observed"))
        .run()
        .await?;
    let crate::observe::RemoteSessionObservationSubscription::Subscribed(mut subscription) =
        session.observe().subscribe_from_remote_cursor(
            &crate::remote::observations::RemoteSessionCursor::new(observation.cursor.clone()),
        )?
    else {
        panic!("recent remote cursor should subscribe without a gap");
    };

    loop {
        let event =
            tokio::time::timeout(std::time::Duration::from_secs(2), subscription.next_event())
                .await
                .expect("timed out waiting for remote replayed event")
                .expect("remote replayed event");
        if remote_observation_assistant_delta(&event).as_deref() == Some("echo: remote observed") {
            assert_eq!(
                event.protocol_version,
                crate::remote::REMOTE_PROTOCOL_VERSION
            );
            assert_eq!(event.session_id, "session-observation-remote-subscribe");
            break;
        }
    }
    Ok(())
}

#[tokio::test]
async fn session_observation_remote_recovery_stream_yields_dto_gap() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .live_replay_store(Arc::new(lash_core::InMemoryLiveReplayStore::new(
            lash_core::InMemoryLiveReplayStoreConfig {
                max_events_per_session: 1,
                ..lash_core::InMemoryLiveReplayStoreConfig::default()
            },
        )))
        .build()?;
    let session = core
        .session("session-observation-remote-gap")
        .open()
        .await?;
    let observation = session.observe().current_remote_observation();

    session
        .turn(TurnInput::text("trimmed before remote subscribe"))
        .run()
        .await?;
    let mut stream = session.observe().subscribe_and_recover_remote(
        crate::remote::observations::RemoteSessionCursor::new(observation.cursor),
    )?;
    let item = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .expect("timed out waiting for remote gap stream item")
        .expect("remote recovery stream should stay open")?;
    let crate::observe::RemoteSessionObservationStreamItem::Gap { observation, gap } = item else {
        panic!("trimmed remote cursor should yield a gap item");
    };

    assert_eq!(
        gap.reason,
        crate::remote::observations::RemoteLiveReplayGapReason::Trimmed
    );
    assert_eq!(gap.latest_cursor, observation.cursor);
    assert_eq!(observation.session_id, "session-observation-remote-gap");
    Ok(())
}

#[tokio::test]
async fn session_observation_recovery_stream_yields_gap_for_trimmed_cursor() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .live_replay_store(Arc::new(lash_core::InMemoryLiveReplayStore::new(
            lash_core::InMemoryLiveReplayStoreConfig {
                max_events_per_session: 1,
                ..lash_core::InMemoryLiveReplayStoreConfig::default()
            },
        )))
        .build()?;
    let session = core
        .session("session-observation-recovered-gap")
        .open()
        .await?;
    let cursor = session.observe().current_observation().cursor;

    session
        .turn(TurnInput::text("trimmed before subscribe"))
        .run()
        .await?;
    let mut stream = session.observe().subscribe_and_recover(cursor);
    let item = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .expect("timed out waiting for gap stream item")
        .expect("recovery stream should stay open")?;
    let crate::observe::SessionObservationStreamItem::Gap { observation, gap } = item else {
        panic!("trimmed cursor should yield a gap item");
    };

    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Trimmed);
    assert_eq!(gap.latest_cursor, observation.cursor);
    Ok(())
}

#[tokio::test]
async fn recoverable_chat_conformance_snapshot_subscription_and_terminal_replacement() -> Result<()>
{
    let core = standard_core();
    let session = core.session("recoverable-chat-terminal").open().await?;
    let snapshot = session.observe().recoverable_chat_snapshot();
    assert!(snapshot.read_view.messages().is_empty());
    let mut stream = session
        .observe()
        .subscribe_recoverable_chat(snapshot.cursor);

    session
        .turn(TurnInput::text("terminal replacement"))
        .turn_id("recoverable-terminal-turn")
        .run()
        .await?;

    let terminal = loop {
        let update = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .expect("recoverable chat terminal timeout")
            .expect("recoverable chat stream stays open")?;
        if let crate::recoverable_chat::RecoverableChatUpdate::TerminalReplacement {
            snapshot,
            ..
        } = update
        {
            break snapshot;
        }
    };
    assert!(
        terminal
            .read_view
            .messages()
            .iter()
            .any(|message| crate::message_text(message).contains("terminal replacement")),
        "terminal replacement must carry the authoritative committed transcript"
    );
    Ok(())
}

#[derive(Debug)]
struct PausedCommitReplayStore {
    inner: lash_core::InMemoryLiveReplayStore,
    commit_appended: std::sync::atomic::AtomicBool,
    release_commit: std::sync::atomic::AtomicBool,
    pause_lock: StdMutex<()>,
    pause_changed: std::sync::Condvar,
}

impl PausedCommitReplayStore {
    fn new() -> Self {
        Self {
            inner: lash_core::InMemoryLiveReplayStore::default(),
            commit_appended: std::sync::atomic::AtomicBool::new(false),
            release_commit: std::sync::atomic::AtomicBool::new(false),
            pause_lock: StdMutex::new(()),
            pause_changed: std::sync::Condvar::new(),
        }
    }

    async fn wait_for_commit_append(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !self
                .commit_appended
                .load(std::sync::atomic::Ordering::Acquire)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("turn never reached the post-append observation-install seam");
    }

    fn release_commit_install(&self) {
        self.release_commit
            .store(true, std::sync::atomic::Ordering::Release);
        self.pause_changed.notify_all();
    }
}

impl lash_core::LiveReplayStore for PausedCommitReplayStore {
    fn append(
        &self,
        session_id: &str,
        revision: lash_core::SessionRevision,
        turn_id: Option<&str>,
        payload: lash_core::SessionObservationEventPayload,
    ) -> std::result::Result<Arc<lash_core::SessionObservationEvent>, lash_core::LiveReplayStoreError>
    {
        let pause = matches!(
            payload,
            lash_core::SessionObservationEventPayload::Committed { .. }
        );
        let event = self.inner.append(session_id, revision, turn_id, payload)?;
        if pause
            && !self
                .commit_appended
                .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            let mut guard = self.pause_lock.lock().expect("commit pause lock");
            while !self
                .release_commit
                .load(std::sync::atomic::Ordering::Acquire)
            {
                guard = self
                    .pause_changed
                    .wait(guard)
                    .expect("commit pause condition");
            }
        }
        Ok(event)
    }

    fn replay_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplayResult, lash_core::LiveReplayStoreError> {
        self.inner.replay_after_cursor(cursor)
    }

    fn subscribe_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplaySubscribeResult, lash_core::LiveReplayStoreError>
    {
        self.inner.subscribe_after_cursor(cursor)
    }

    fn current_cursor(
        &self,
        session_id: &str,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.inner.current_cursor(session_id, revision)
    }

    fn trim_session(
        &self,
        session_id: &str,
    ) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.trim_session(session_id)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recoverable_chat_snapshot_handoff_never_loses_concurrent_terminal_commit() -> Result<()> {
    let replay_store = Arc::new(PausedCommitReplayStore::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .live_replay_store(replay_store.clone())
        .build()?;
    let session = core
        .session("recoverable-chat-atomic-handoff")
        .open()
        .await?;
    let turn_session = session.clone();
    let turn = tokio::spawn(async move {
        turn_session
            .turn(TurnInput::text("atomic terminal handoff"))
            .turn_id("recoverable-atomic-handoff-turn")
            .run()
            .await
    });

    replay_store.wait_for_commit_append().await;
    let snapshot = session.observe().recoverable_chat_snapshot();
    replay_store.release_commit_install();
    turn.await.expect("join publishing turn")?;

    let already_captured = snapshot
        .read_view
        .messages()
        .iter()
        .any(|message| crate::message_text(message).contains("atomic terminal handoff"));
    let delivered_after_snapshot = if already_captured {
        false
    } else {
        let mut stream = session
            .observe()
            .subscribe_recoverable_chat(snapshot.cursor);
        tokio::time::timeout(std::time::Duration::from_millis(500), async {
            loop {
                let update = stream.next().await.expect("handoff stream remains open")?;
                if let crate::recoverable_chat::RecoverableChatUpdate::TerminalReplacement {
                    snapshot,
                    ..
                } = update
                    && snapshot.read_view.messages().iter().any(|message| {
                        crate::message_text(message).contains("atomic terminal handoff")
                    })
                {
                    break Ok::<_, crate::EmbedError>(true);
                }
            }
        })
        .await
        .unwrap_or(Ok(false))?
    };

    assert!(
        already_captured || delivered_after_snapshot,
        "a terminal commit concurrent with snapshot capture must be present in the snapshot or replayed after its cursor"
    );
    Ok(())
}

#[tokio::test]
async fn recoverable_chat_conformance_deduplicates_redelivery_identity() -> Result<()> {
    let core = standard_core();
    let session = core.session("recoverable-chat-redelivery").open().await?;
    let cursor = session.observe().recoverable_chat_snapshot().cursor;
    session
        .turn(TurnInput::text("redelivery identity"))
        .turn_id("recoverable-redelivery-turn")
        .run()
        .await?;

    let mut first_delivery = session.observe().subscribe_recoverable_chat(cursor.clone());
    let first_id = match first_delivery.next().await.expect("first replay event")? {
        crate::recoverable_chat::RecoverableChatUpdate::Event { id, .. }
        | crate::recoverable_chat::RecoverableChatUpdate::TerminalReplacement { id, .. } => id,
        crate::recoverable_chat::RecoverableChatUpdate::ReplayGap { .. } => {
            panic!("fresh cursor unexpectedly gapped")
        }
    };

    let mut redelivery = session
        .observe()
        .subscribe_recoverable_chat(cursor)
        .with_applied_event_ids([first_id.clone()]);
    let next_id = match redelivery.next().await.expect("next replay event")? {
        crate::recoverable_chat::RecoverableChatUpdate::Event { id, .. }
        | crate::recoverable_chat::RecoverableChatUpdate::TerminalReplacement { id, .. } => id,
        crate::recoverable_chat::RecoverableChatUpdate::ReplayGap { .. } => {
            panic!("fresh cursor unexpectedly gapped")
        }
    };
    assert_ne!(
        next_id, first_id,
        "an already-applied event identity must not be delivered twice"
    );
    Ok(())
}

#[tokio::test]
async fn recoverable_chat_restart_identity_does_not_depend_on_gap_clearing() -> Result<()> {
    let session_id = "recoverable-chat-restart-cursor";
    let store_factory = Arc::new(lash_core::InMemorySessionStoreFactory::new());
    let bootstrap_core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .build()?;
    bootstrap_core
        .session(session_id)
        .open()
        .await?
        .close()
        .await?;
    drop(bootstrap_core);

    let first_core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .live_replay_store(Arc::new(lash_core::InMemoryLiveReplayStore::default()))
        .build()?;
    let first_session = first_core.session(session_id).open().await?;
    let initial_cursor = first_session.observe().recoverable_chat_snapshot().cursor;
    first_session.observe().runtime.record_turn_activity(
        Some("before-restart-turn"),
        TurnActivity::independent(TurnEvent::AssistantProseDelta {
            text: "before replay-store restart".into(),
        }),
    );
    let mut first_stream = first_session
        .observe()
        .subscribe_recoverable_chat(initial_cursor);
    let old_id = match first_stream.next().await.expect("pre-restart event")? {
        crate::recoverable_chat::RecoverableChatUpdate::Event { id, .. } => id,
        other => panic!("expected pre-restart provisional event, got {other:?}"),
    };
    let old_cursor = first_stream.cursor().clone();
    drop(first_stream);
    drop(first_session);
    drop(first_core);

    let second_core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(store_factory)
        .live_replay_store(Arc::new(lash_core::InMemoryLiveReplayStore::default()))
        .build()?;
    let second_session = second_core.session(session_id).open().await?;
    let restarted_at = second_session.observe().recoverable_chat_snapshot().cursor;
    let mut retained_applied_ids = second_session
        .observe()
        .subscribe_recoverable_chat(restarted_at)
        .with_applied_event_ids([old_id.clone()]);
    let mut recovered = second_session
        .observe()
        .subscribe_recoverable_chat(old_cursor)
        .with_applied_event_ids([old_id.clone()]);
    let gap = recovered.next().await.expect("restart gap")?;
    assert!(matches!(
        gap,
        crate::recoverable_chat::RecoverableChatUpdate::ReplayGap {
            gap: lash_core::LiveReplayGap {
                reason: lash_core::LiveReplayGapReason::Unavailable,
                ..
            },
            ..
        }
    ));

    second_session.observe().runtime.record_turn_activity(
        Some("after-restart-turn"),
        TurnActivity::independent(TurnEvent::AssistantProseDelta {
            text: "after replay-store restart".into(),
        }),
    );
    let gap_continuation =
        tokio::time::timeout(std::time::Duration::from_millis(500), recovered.next())
            .await
            .expect("gap stream did not continue with the new event")
            .expect("recovered stream remains open")?;
    let crate::recoverable_chat::RecoverableChatUpdate::Event {
        id: gap_continuation_id,
        event: gap_continuation_event,
    } = gap_continuation
    else {
        panic!("expected post-gap provisional event");
    };
    let update = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        retained_applied_ids.next(),
    )
    .await
    .expect("retained pre-restart identity incorrectly suppressed the new event")
    .expect("recovered stream remains open")?;
    let crate::recoverable_chat::RecoverableChatUpdate::Event { id, event } = update else {
        panic!("expected post-restart provisional event");
    };
    assert_eq!(
        id.cursor, old_id.cursor,
        "the in-memory replay store deliberately reuses the pre-restart cursor"
    );
    assert_ne!(
        id, old_id,
        "a fresh replay-store incarnation must distinguish a reused cursor without relying on gap clearing"
    );
    assert_eq!(gap_continuation_id, id);
    assert_eq!(
        observation_assistant_delta(&gap_continuation_event).as_deref(),
        Some("after replay-store restart")
    );
    assert_eq!(
        observation_assistant_delta(&event).as_deref(),
        Some("after replay-store restart")
    );
    Ok(())
}

#[tokio::test]
async fn recoverable_chat_conformance_forwards_trimmed_gap_and_continues() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .live_replay_store(Arc::new(lash_core::InMemoryLiveReplayStore::new(
            lash_core::InMemoryLiveReplayStoreConfig {
                max_events_per_session: 1,
                ..lash_core::InMemoryLiveReplayStoreConfig::default()
            },
        )))
        .build()?;
    let session = core.session("recoverable-chat-gap").open().await?;
    let cursor = session.observe().recoverable_chat_snapshot().cursor;
    session
        .turn(TurnInput::text("trim the initial cursor"))
        .run()
        .await?;
    let mut stream = session.observe().subscribe_recoverable_chat(cursor);
    let update = stream.next().await.expect("gap update")?;
    let crate::recoverable_chat::RecoverableChatUpdate::ReplayGap { snapshot, gap } = update else {
        panic!("trimmed cursor must be forwarded as a recoverable gap");
    };
    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Trimmed);
    assert_eq!(gap.latest_cursor, snapshot.cursor);

    session
        .turn(TurnInput::text("live after gap"))
        .run()
        .await?;
    let read_view = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match stream.next().await.expect("post-gap live update")? {
                crate::recoverable_chat::RecoverableChatUpdate::ReplayGap { snapshot, .. }
                | crate::recoverable_chat::RecoverableChatUpdate::TerminalReplacement {
                    snapshot,
                    ..
                } => break Ok::<_, crate::EmbedError>(snapshot.read_view),
                crate::recoverable_chat::RecoverableChatUpdate::Event { .. } => {}
            }
        }
    })
    .await
    .expect("post-gap continuation timeout")?;
    assert!(
        read_view
            .messages()
            .iter()
            .any(|message| crate::message_text(message).contains("live after gap")),
        "continued recovery must replace from a snapshot containing the next turn"
    );
    Ok(())
}

#[tokio::test]
async fn recoverable_chat_conformance_disconnect_does_not_cancel_server_work() -> Result<()> {
    let (entered_tx, entered_rx) = oneshot::channel();
    let entered_tx = Arc::new(StdMutex::new(Some(entered_tx)));
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = crate::testing::TestProvider::builder()
        .kind("recoverable-chat-disconnect")
        .complete({
            let entered_tx = Arc::clone(&entered_tx);
            let release = Arc::clone(&release);
            move |_request| {
                let entered_tx = Arc::clone(&entered_tx);
                let release = Arc::clone(&release);
                async move {
                    if let Some(tx) = entered_tx.lock().expect("entered sender").take() {
                        let _ = tx.send(());
                    }
                    release.notified().await;
                    Ok(text_response("completed after observer disconnect"))
                }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(provider)
        .model(mock_model_spec())
        .build()?;
    let session = core.session("recoverable-chat-disconnect").open().await?;
    let cursor = session.observe().recoverable_chat_snapshot().cursor;
    let stream = session.observe().subscribe_recoverable_chat(cursor);
    let run_session = session.clone();
    let mut turn = tokio::spawn(async move {
        run_session
            .turn(TurnInput::text("keep running"))
            .turn_id("disconnect-is-not-cancel")
            .run()
            .await
    });
    entered_rx.await.expect("provider entered");
    drop(stream);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut turn)
            .await
            .is_err(),
        "disconnecting observation must not cancel server work"
    );
    release.notify_one();
    let result = turn.await.expect("join turn")?;
    assert!(matches!(result.result.outcome, TurnOutcome::Finished(_)));
    Ok(())
}

fn observation_assistant_delta(event: &lash_core::SessionObservationEvent) -> Option<String> {
    match &event.payload {
        lash_core::SessionObservationEventPayload::TurnActivity(activity) => {
            match &activity.event {
                TurnEvent::AssistantProseDelta { text } => Some(text.to_string()),
                _ => None,
            }
        }
        _ => None,
    }
}

fn remote_observation_assistant_delta(
    event: &crate::remote::observations::RemoteSessionObservationEvent,
) -> Option<String> {
    match &event.event {
        crate::remote::observations::RemoteSessionObservationEventPayload::TurnActivity {
            activity,
        } => match &activity.event {
            crate::remote::usage::RemoteTurnEvent::AssistantProseDelta { text } => {
                Some(text.clone())
            }
            _ => None,
        },
        _ => None,
    }
}

#[tokio::test]
async fn turn_stream_finish_returns_committed_assistant_prose() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(semantic_group_provider())
        .model(mock_model_spec())
        .build()?;
    let session = core.session("turn-stream-last-group").open().await?;
    let mut stream = session.turn(TurnInput::text("stream groups")).stream()?;

    let mut activities = Vec::new();
    while let Some(activity) = stream.next_activity().await {
        activities.push(activity?);
    }
    let result = stream.finish().await?;

    assert_eq!(assistant_prose(&activities), "firstsecond");
    assert_eq!(result.assistant_message(), Some("first\n\nsecond"));
    assert_eq!(result.assistant_output.safe_text, "first\n\nsecond");
    assert!(result.is_success());
    Ok(())
}

#[tokio::test]
async fn turn_run_collects_activities_and_returns_committed_assistant_prose() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(semantic_group_provider())
        .model(mock_model_spec())
        .build()?;
    let session = core.session("turn-run-last-group").open().await?;

    let collected = session.turn(TurnInput::text("run groups")).run().await?;

    assert_eq!(assistant_prose(&collected.activities), "firstsecond");
    assert_eq!(
        collected.result.assistant_message(),
        Some("first\n\nsecond")
    );
    assert_eq!(
        collected.result.assistant_output.safe_text,
        "first\n\nsecond"
    );
    Ok(())
}

#[tokio::test]
async fn retry_status_streams_as_semantic_turn_event() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(retry_once_provider())
        .model(mock_model_spec())
        .build()?;
    let session = core.session("retry-status").open().await?;
    let events = RecordingEvents::default();

    let result = session
        .turn(TurnInput::text("hello"))
        .stream_to(&events)
        .await?;

    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::TurnFinish::AssistantMessage { .. })
    ));
    let retry = events
        .snapshot()
        .await
        .into_iter()
        .find(|event| matches!(&event.event, TurnEvent::RetryStatus { .. }))
        .expect("retry status event");
    let TurnEvent::RetryStatus {
        wait_seconds,
        attempt,
        max_attempts,
        reason,
    } = retry.event
    else {
        unreachable!();
    };
    assert_eq!(wait_seconds, 0);
    assert_eq!(attempt, 1);
    assert_eq!(max_attempts, 2);
    assert!(reason.contains("retry me"));
    Ok(())
}

#[tokio::test]
async fn control_turn_accepts_prebuilt_turn_input() -> Result<()> {
    let core = standard_core();
    let session = core.session("raw-turn").open().await?;
    let mut input = TurnInput::text("raw input");
    input.trace_turn_id = Some("host-trace-id".to_string());

    let result = session.turn(input).turn_id("host-trace-id").run().await?;

    assert_eq!(assistant_prose(&result.activities), "echo: raw input");
    Ok(())
}

#[tokio::test]
async fn queued_input_acceptance_streams_semantic_ack_with_id() -> Result<()> {
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(checkpoint_gated_provider(entered_tx, release_rx))
        .model(mock_model_spec())
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .build()?;
    let session = core.session("queued-input").open().await?;
    let events = Arc::new(RecordingEvents::default());
    let turn_session = session.clone();
    let turn_events = Arc::clone(&events);
    let turn = tokio::spawn(async move {
        turn_session
            .turn(TurnInput::text("hello"))
            .turn_id("queued-input-turn")
            .stream_to(turn_events.as_ref())
            .await
    });

    entered_rx.await.expect("provider entered first call");
    session
        .admin()
        .injection()
        .inject_turn_input(
            "queued-input-turn",
            Some("queue-1".to_string()),
            lash_core::PluginMessage::text(lash_core::MessageRole::User, "queued follow-up"),
        )
        .await?;
    release_tx.send(()).expect("release provider");
    let result = turn.await.expect("turn task")?;

    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::TurnFinish::AssistantMessage { .. })
    ));
    let events = events.snapshot().await;
    assert!(events.iter().any(|event| matches!(
        &event.event,
        TurnEvent::QueuedInputAccepted {
            applications,
        } if applications.iter().any(|application| {
            application.source_key.as_deref() == Some("injection:queue-1")
                && application.turn_id == "queued-input-turn"
                && application.checkpoint
                    == Some(lash_core::CheckpointKind::BeforeCompletion)
                && application.committed_message_id
                    == format!("m_ingress_{}", application.input_id)
        })
    )));
    let prose = events
        .into_iter()
        .filter_map(|event| match event.event {
            TurnEvent::AssistantProseDelta { text } => Some(text.to_string()),
            _ => None,
        })
        .collect::<String>();
    assert!(prose.contains("after queued follow-up"));
    Ok(())
}

#[tokio::test]
async fn pre_cancelled_token_yields_cancelled_outcome() -> Result<()> {
    let core = standard_core();
    let session = core.session("pre-cancelled").open().await?;
    let cancel = CancellationToken::new();
    cancel.cancel();

    let output = session
        .turn(TurnInput::text("never runs"))
        .cancel(cancel)
        .run()
        .await?;

    assert!(matches!(
        output.result.outcome,
        TurnOutcome::Stopped(lash_core::TurnStop::Cancelled)
    ));
    let evidence = output
        .result
        .cancellation
        .expect("local token cancellation evidence");
    assert_eq!(evidence.origin, None);
    assert_eq!(evidence.reason, None);
    Ok(())
}

#[tokio::test]
async fn local_cancel_token_preserves_explicit_origin_hint() -> Result<()> {
    let core = standard_core();
    let session = core.session("pre-cancelled-with-origin").open().await?;
    let cancel = CancellationToken::new();
    cancel.cancel();

    let output = session
        .turn(TurnInput::text("never runs"))
        .cancel_with_origin(cancel, Some("shutdown".to_string()))
        .run()
        .await?;

    assert!(matches!(
        output.result.cancellation,
        Some(lash_core::TurnCancellationEvidence {
            origin: Some(ref origin),
            ..
        }) if origin == "shutdown"
    ));
    Ok(())
}

#[tokio::test]
async fn cancel_running_turns_stops_inflight_turn() -> Result<()> {
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let started_tx = Arc::new(StdMutex::new(Some(started_tx)));
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |_request| {
            let started_tx = Arc::clone(&started_tx);
            async move {
                if let Some(tx) = started_tx.lock().expect("started signal").take() {
                    let _ = tx.send(());
                }
                // Hang until the turn is cancelled out from under us.
                std::future::pending::<()>().await;
                unreachable!("provider future should be dropped by cancellation")
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(provider)
        .model(mock_model_spec())
        .build()
        .expect("core");
    let session = core.session("cancel-inflight").open().await?;
    let stopper = session.clone();

    let stream = session.turn(TurnInput::text("hang forever")).stream()?;
    started_rx.await.expect("provider reached");
    assert_eq!(
        stopper.cancel_running_turns_with_origin(Some("user".to_string())),
        1
    );

    let result = stream.finish().await?;
    assert!(matches!(
        result.outcome,
        TurnOutcome::Stopped(lash_core::TurnStop::Cancelled)
    ));
    assert!(matches!(
        result.cancellation,
        Some(lash_core::TurnCancellationEvidence {
            origin: Some(ref origin),
            ..
        }) if origin == "user"
    ));
    // The registry entry is gone once the turn finished.
    assert_eq!(stopper.cancel_running_turns(), 0);
    Ok(())
}

fn hang_on_signal_provider(started_tx: Arc<StdMutex<Vec<oneshot::Sender<()>>>>) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |request| {
            let started_tx = Arc::clone(&started_tx);
            async move {
                let user_text = last_user_text(&request);
                if user_text.contains("hang") {
                    if let Some(tx) = started_tx.lock().expect("started signal").pop() {
                        let _ = tx.send(());
                    }
                    // Hang until the turn is cancelled out from under us.
                    std::future::pending::<()>().await;
                    unreachable!("provider future should be dropped by cancellation")
                }
                Ok(text_response(&format!("echo: {user_text}")))
            }
        })
        .build()
        .into_handle()
}

#[tokio::test]
async fn cancel_running_turns_sweeps_lock_queued_turns() -> Result<()> {
    // One opened session serializes turn execution on the runtime writer
    // lock, but a second turn is already registered while it waits for that
    // lock. A stop sweep must reach both: the executing turn aborts, and the
    // parked turn sees its cancelled token the moment it acquires the lock
    // instead of starting a fresh provider call after the user pressed stop.
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let provider = hang_on_signal_provider(Arc::new(StdMutex::new(vec![started_tx])));
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(provider)
        .model(mock_model_spec())
        .build()
        .expect("core");
    let session = core.session("cancel-lock-queue").open().await?;

    let first = session.turn(TurnInput::text("hang one")).stream()?;
    started_rx.await.expect("first turn reached the provider");
    let second = session.turn(TurnInput::text("hang two")).stream()?;

    assert_eq!(session.cancel_running_turns(), 2);

    let first = first.finish().await?;
    let second = second.finish().await?;
    assert!(matches!(
        first.outcome,
        TurnOutcome::Stopped(lash_core::TurnStop::Cancelled)
    ));
    assert!(matches!(
        second.outcome,
        TurnOutcome::Stopped(lash_core::TurnStop::Cancelled)
    ));
    assert_eq!(session.cancel_running_turns(), 0);
    Ok(())
}

#[tokio::test]
async fn cancel_running_turns_does_not_cross_separately_opened_handles() -> Result<()> {
    // Each open() builds its own runtime and cancel registry; the documented
    // scope of cancel_running_turns is the opened handle and its clones.
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let provider = hang_on_signal_provider(Arc::new(StdMutex::new(vec![started_tx])));
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(provider)
        .model(mock_model_spec())
        .build()
        .expect("core");
    let handle_a = core.session("cancel-scope").open().await?;
    let handle_b = core.session("cancel-scope").open().await?;

    let hanging = handle_a.turn(TurnInput::text("hang here")).stream()?;
    started_rx.await.expect("turn reached the provider");

    // The other handle has its own registry: nothing to cancel there.
    assert_eq!(handle_b.cancel_running_turns(), 0);
    assert_eq!(handle_a.cancel_running_turns(), 1);

    let result = hanging.finish().await?;
    assert!(matches!(
        result.outcome,
        TurnOutcome::Stopped(lash_core::TurnStop::Cancelled)
    ));

    // The untouched handle keeps working.
    let output = handle_b.turn(TurnInput::text("plain")).run().await?;
    assert_eq!(output.assistant_message(), Some("echo: plain"));
    Ok(())
}

#[tokio::test]
async fn cancel_running_turns_reaches_queued_turn_drains() -> Result<()> {
    // Queued drains register in the same session registry as foreground
    // turns, so a stop sweep reaches them too.
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let provider = hang_on_signal_provider(Arc::new(StdMutex::new(vec![started_tx])));
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .disable_queued_work_driver()
        .build()
        .expect("core");
    let session = core.session("cancel-queued-drain").open().await?;
    session
        .enqueue(TurnInput::text("hang queued"))
        .send()
        .await?;

    let drainer = session.clone();
    let drain = tokio::spawn(async move { drainer.queued_turn().run().await });
    started_rx.await.expect("queued drain reached the provider");
    assert_eq!(
        session.cancel_running_turns_with_origin(Some("user".to_string())),
        1
    );

    let output = drain
        .await
        .expect("drain task")?
        .expect("queued drain should produce a turn");
    assert!(matches!(
        output.result.outcome,
        TurnOutcome::Stopped(lash_core::TurnStop::Cancelled)
    ));
    assert!(matches!(
        output.result.cancellation,
        Some(lash_core::TurnCancellationEvidence {
            origin: Some(ref origin),
            ..
        }) if origin == "user"
    ));
    Ok(())
}

#[tokio::test]
async fn active_steer_after_last_call_defers_to_next_turn_first_call() -> Result<()> {
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let started_tx = Arc::new(StdMutex::new(Some(started_tx)));
    let requests = Arc::new(StdMutex::new(Vec::<(
        String,
        Vec<lash_core::llm::types::LlmMessage>,
    )>::new()));
    let captured_requests = Arc::clone(&requests);
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |request| {
            let started_tx = Arc::clone(&started_tx);
            let captured_requests = Arc::clone(&captured_requests);
            async move {
                let user_text = last_user_text(&request);
                captured_requests
                    .lock()
                    .expect("request log")
                    .push((user_text.clone(), request.messages.clone()));
                if user_text == "primary hangs" {
                    if let Some(tx) = started_tx.lock().expect("started signal").take() {
                        let _ = tx.send(());
                    }
                    std::future::pending::<()>().await;
                    unreachable!("provider future should be dropped by cancellation")
                }
                Ok(text_response(&format!("echo: {user_text}")))
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .disable_queued_work_driver()
        .build()?;
    let session = core.session("active-steer-interrupt-cancel").open().await?;
    let active_turn_id = "active-steer-interrupt-turn";
    let turn_session = session.clone();
    let turn = tokio::spawn(async move {
        let stream = turn_session
            .turn(TurnInput::text("primary hangs"))
            .turn_id(active_turn_id)
            .stream()?;
        stream.finish().await
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), started_rx)
        .await
        .expect("primary turn should reach provider")
        .expect("provider started signal");
    let active = session
        .enqueue(TurnInput::text("deferred active steer"))
        .id("active-steer")
        .ingress(lash_core::TurnInputIngress::active_turn(
            active_turn_id,
            lash_core::TurnInputCheckpointBoundary::AfterWork,
        ))
        .send()
        .await?;
    let queued = session
        .enqueue(TurnInput::text("cancelled next turn"))
        .id("cancelled-next")
        .send()
        .await?;
    let cancelled = session.cancel_pending_turn_input(&queued.input_id).await?;
    let crate::PendingTurnInputCancelOutcome::Cancelled(cancelled) = cancelled else {
        panic!("queued input should be cancellable before it is claimed: {cancelled:?}");
    };
    assert_eq!(cancelled.input_id, queued.input_id);

    assert_eq!(session.cancel_running_turns(), 1);
    let interrupted = turn.await.expect("turn task")?;
    assert!(matches!(
        interrupted.outcome,
        TurnOutcome::Stopped(lash_core::TurnStop::Cancelled)
    ));

    let pending = session.pending_turn_inputs().await?;
    assert_eq!(
        pending.len(),
        1,
        "only the unaccepted active steer should remain"
    );
    assert_eq!(pending[0].input_id, active.input_id);
    assert!(matches!(
        pending[0].ingress,
        lash_core::TurnInputIngress::NextTurn
    ));
    assert_eq!(
        pending[0].state,
        lash_core::TurnInputState::DeferredNextTurn
    );

    let drained = session
        .queued_turn()
        .run()
        .await?
        .expect("deferred active steer should run as the next turn");
    assert_eq!(
        drained.assistant_message(),
        Some("echo: deferred active steer")
    );
    assert!(session.pending_turn_inputs().await?.is_empty());
    let requests = requests.lock().expect("request log").clone();
    assert_eq!(
        requests
            .iter()
            .filter(|(text, _)| text == "deferred active steer")
            .count(),
        1,
        "deferred active steer must be sent exactly once"
    );
    assert!(
        !requests
            .iter()
            .any(|(text, _)| text == "cancelled next turn"),
        "cancelled queued turn must not reach the provider"
    );
    let deferred_request = requests
        .iter()
        .find(|(text, _)| text == "deferred active steer")
        .map(|(_, messages)| messages)
        .expect("deferred active input provider request");
    assert_eq!(
        serde_json::to_string(&deferred_request[1..])
            .expect("serialize deferred active-input request messages"),
        r#"[{"role":"User","blocks":[{"Text":{"text":"primary hangs","response_meta":null,"cache_breakpoint":false}}]},{"role":"User","blocks":[{"Text":{"text":"deferred active steer","response_meta":null,"cache_breakpoint":false}}]}]"#
    );
    Ok(())
}

#[tokio::test]
async fn accepted_active_steer_interrupt_is_not_requeued() -> Result<()> {
    let (first_started_tx, first_started_rx) = oneshot::channel::<()>();
    let (release_first_tx, release_first_rx) = oneshot::channel::<()>();
    let (second_started_tx, second_started_rx) = oneshot::channel::<()>();
    let first_started_tx = Arc::new(StdMutex::new(Some(first_started_tx)));
    let release_first_rx = Arc::new(TokioMutex::new(Some(release_first_rx)));
    let second_started_tx = Arc::new(StdMutex::new(Some(second_started_tx)));
    let requests = Arc::new(StdMutex::new(Vec::<String>::new()));
    let captured_requests = Arc::clone(&requests);
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |request| {
            let first_started_tx = Arc::clone(&first_started_tx);
            let release_first_rx = Arc::clone(&release_first_rx);
            let second_started_tx = Arc::clone(&second_started_tx);
            let captured_requests = Arc::clone(&captured_requests);
            async move {
                let user_text = last_user_text(&request);
                captured_requests
                    .lock()
                    .expect("request log")
                    .push(user_text.clone());
                if user_text == "primary waits for active steer" {
                    if let Some(tx) = first_started_tx.lock().expect("first signal").take() {
                        let _ = tx.send(());
                    }
                    if let Some(rx) = release_first_rx.lock().await.take() {
                        let _ = rx.await;
                    }
                    return Ok(text_response("first response"));
                }
                if user_text == "accepted active steer" {
                    if let Some(tx) = second_started_tx.lock().expect("second signal").take() {
                        let _ = tx.send(());
                    }
                    std::future::pending::<()>().await;
                    unreachable!("accepted steer provider call should be dropped by cancellation")
                }
                Ok(text_response(&format!("echo: {user_text}")))
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .disable_queued_work_driver()
        .build()?;
    let session = core
        .session("accepted-active-steer-interrupt")
        .open()
        .await?;
    let active_turn_id = "accepted-active-steer-turn";
    let turn_session = session.clone();
    let turn = tokio::spawn(async move {
        let stream = turn_session
            .turn(TurnInput::text("primary waits for active steer"))
            .turn_id(active_turn_id)
            .stream()?;
        stream.finish().await
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), first_started_rx)
        .await
        .expect("first provider call should start")
        .expect("first provider signal");
    let active = session
        .enqueue(TurnInput::text("accepted active steer"))
        .id("accepted-active-steer")
        .ingress(lash_core::TurnInputIngress::active_turn(
            active_turn_id,
            lash_core::TurnInputCheckpointBoundary::AfterWork,
        ))
        .send()
        .await?;
    release_first_tx
        .send(())
        .expect("release first provider response");
    tokio::time::timeout(std::time::Duration::from_secs(2), second_started_rx)
        .await
        .expect("accepted active steer should start the follow-up provider call")
        .expect("second provider signal");

    assert_eq!(session.cancel_running_turns(), 1);
    let interrupted = turn.await.expect("turn task")?;
    assert!(matches!(
        interrupted.outcome,
        TurnOutcome::Stopped(lash_core::TurnStop::Cancelled)
    ));
    assert!(
        session.pending_turn_inputs().await?.is_empty(),
        "accepted active steer `{}` must be completed, not deferred after interrupt",
        active.input_id
    );
    assert!(
        session.queued_turn().run().await?.is_none(),
        "accepted active steer must not replay as a later queued turn"
    );
    let requests = requests.lock().expect("request log").clone();
    assert_eq!(
        requests
            .iter()
            .filter(|text| text.as_str() == "accepted active steer")
            .count(),
        1,
        "accepted active steer should reach the provider once before cancellation"
    );
    Ok(())
}

#[test]
fn rlm_active_input_reaches_the_next_provider_iteration() -> Result<()> {
    run_async_test_on_stack_budget("rlm-active-input-next-iteration", || async {
        let (first_started_tx, first_started_rx) = oneshot::channel::<()>();
        let (release_first_tx, release_first_rx) = oneshot::channel::<()>();
        let first_started_tx = Arc::new(StdMutex::new(Some(first_started_tx)));
        let release_first_rx = Arc::new(TokioMutex::new(Some(release_first_rx)));
        let requests = Arc::new(StdMutex::new(
            Vec::<Vec<lash_core::llm::types::LlmMessage>>::new(),
        ));
        let captured_requests = Arc::clone(&requests);
        let call_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = crate::testing::TestProvider::builder()
            .kind("rlm-active-input-next-iteration")
            .complete(move |request| {
                let first_started_tx = Arc::clone(&first_started_tx);
                let release_first_rx = Arc::clone(&release_first_rx);
                let captured_requests = Arc::clone(&captured_requests);
                let call_index = Arc::clone(&call_index);
                async move {
                    captured_requests
                        .lock()
                        .expect("request capture")
                        .push(request.messages.clone());
                    match call_index.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                        0 => {
                            if let Some(tx) = first_started_tx.lock().expect("first signal").take()
                            {
                                let _ = tx.send(());
                            }
                            if let Some(rx) = release_first_rx.lock().await.take() {
                                let _ = rx.await;
                            }
                            Ok(text_response(&lashlang_block(
                                r#"print("first work complete")"#,
                            )))
                        }
                        1 => Ok(text_response(&lashlang_block(
                            r#"finish "active input delivered""#,
                        ))),
                        2 => Ok(text_response(&lashlang_block(r#"finish "later turn""#))),
                        other => panic!("unexpected provider call {other}"),
                    }
                }
            })
            .build()
            .into_handle();
        let core = explicit_ephemeral_facets(LashCore::rlm_builder(rlm_factory()))
            .provider(provider)
            .model(mock_model_spec())
            .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
            .process_registry(Arc::new(TestLocalProcessRegistry::default()))
            .disable_queued_work_driver()
            .build()?;
        let session = core
            .session("rlm-active-input-next-iteration")
            .open()
            .await?;
        let active_turn_id = "rlm-active-input-turn";
        let turn_session = session.clone();
        let turn = tokio::spawn(async move {
            turn_session
                .turn(TurnInput::text("perform two iterations"))
                .turn_id(active_turn_id)
                .require_finish()?
                .run()
                .await
        });

        first_started_rx.await.expect("first provider call started");
        session
            .enqueue(TurnInput::text("mid-turn injection marker"))
            .id("rlm-mid-turn-injection")
            .ingress(lash_core::TurnInputIngress::active_turn(
                active_turn_id,
                lash_core::TurnInputCheckpointBoundary::AfterWork,
            ))
            .send()
            .await?;
        release_first_tx.send(()).expect("release first response");
        turn.await.expect("turn task")?;
        let committed_marker_count = session
            .read_view()
            .messages()
            .iter()
            .filter(|message| crate::message_text(message) == "mid-turn injection marker")
            .count();
        assert_eq!(
            committed_marker_count, 1,
            "active input must be one normal committed transcript message"
        );
        session
            .turn(TurnInput::text("later turn input"))
            .turn_id("rlm-later-turn")
            .require_finish()?
            .run()
            .await?;

        let requests = requests.lock().expect("request capture").clone();
        assert_eq!(requests.len(), 3, "two turns must execute three calls");
        let first_messages = serde_json::to_string(&requests[0]).expect("serialize first request");
        let second_messages =
            serde_json::to_string(&requests[1]).expect("serialize second request");
        assert!(!first_messages.contains("mid-turn injection marker"));
        assert!(
            second_messages.contains("mid-turn injection marker"),
            "active input was claimed but omitted from the next RLM provider request"
        );
        assert_eq!(
            serde_json::to_string(&requests[1][1..requests[1].len() - 1])
                .expect("serialize stable request message prefix"),
            r#"[{"role":"User","blocks":[{"Text":{"text":"perform two iterations","response_meta":null,"cache_breakpoint":false}}]},{"role":"Assistant","blocks":[{"Text":{"text":"<lashlang>\nprint(\"first work complete\")\n</lashlang>","response_meta":null,"cache_breakpoint":false}}]},{"role":"User","blocks":[{"Text":{"text":"history[1].output[0] (19 chars):\nfirst work complete","response_meta":null,"cache_breakpoint":false}}]},{"role":"User","blocks":[{"Text":{"text":"mid-turn injection marker","response_meta":null,"cache_breakpoint":true}}]}]"#
        );
        assert_eq!(
            serde_json::to_string(&requests[2])?
                .matches("mid-turn injection marker")
                .count(),
            1,
            "later assembled history must contain the committed input exactly once"
        );
        assert!(session.pending_turn_inputs().await?.is_empty());
        Ok(())
    })
}

#[tokio::test]
async fn await_queued_work_batch_resolves_when_drained() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .disable_queued_work_driver()
        .build()
        .expect("core");
    let session = core.session("await-queued").open().await?;
    let receipt = session
        .commands()
        .refresh_tool_catalog("await queued work test", "await-queued-refresh")
        .await?;

    let waiter_session = session.clone();
    let waiter_batch = receipt.batch_id.clone();
    let waiter =
        tokio::spawn(async move { waiter_session.await_queued_work_batch(&waiter_batch).await });

    // Nothing has drained the batch yet, so the waiter must still be pending.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    assert!(!waiter.is_finished(), "waiter resolved before any drain");

    assert!(
        session.queued_turn().run().await?.is_none(),
        "a session-command-only drain should not produce a model turn"
    );

    tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
        .await
        .expect("waiter should resolve after the drain")
        .expect("waiter task")?;
    Ok(())
}

#[tokio::test]
async fn await_queued_work_batch_resolves_immediately_for_unknown_batch() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .build()
        .expect("core");
    let session = core.session("await-unknown").open().await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        session.await_queued_work_batch("qwb:never-existed"),
    )
    .await
    .expect("unknown batch must resolve immediately")?;
    Ok(())
}

#[tokio::test]
async fn turn_stream_receives_semantic_activities() -> Result<()> {
    let core = standard_core();
    let session = core.session("semantic-stream").open().await?;
    let turn_events = RecordingEvents::default();

    let result = session
        .turn(TurnInput::text("semantic stream"))
        .cancel(CancellationToken::new())
        .stream_to(&turn_events)
        .await?;

    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::TurnFinish::AssistantMessage { .. })
    ));
    assert!(
        turn_events
            .snapshot()
            .await
            .iter()
            .any(|event| matches!(&event.event, TurnEvent::AssistantProseDelta { .. }))
    );
    Ok(())
}

#[tokio::test]
async fn run_collects_ordered_assistant_prose_activity() -> Result<()> {
    let core = standard_core();
    let session = core.session("main").open().await?;

    let result = session.turn(TurnInput::text("visible")).run().await?;

    assert_eq!(assistant_prose(&result.activities), "echo: visible");
    assert!(
        result
            .activities
            .iter()
            .any(|activity| matches!(&activity.event, TurnEvent::AssistantProseDelta { .. }))
    );
    assert!(
        !result
            .activities
            .iter()
            .any(|activity| matches!(&activity.event, TurnEvent::ToolCallCompleted { .. }))
    );
    assert!(
        !result
            .activities
            .iter()
            .any(|activity| matches!(&activity.event, TurnEvent::CodeBlockCompleted { .. }))
    );
    assert!(matches!(
        result.result.outcome,
        TurnOutcome::Finished(lash_core::TurnFinish::AssistantMessage { .. })
    ));
    assert_eq!(result.result.usage.output_tokens, 2);
    Ok(())
}

#[tokio::test]
async fn private_run_collector_records_ordered_activities() -> Result<()> {
    let collector = RunActivityCollector::default();

    collector
        .emit(test_activity(
            "code-1",
            TurnEvent::CodeBlockStarted {
                language: "lashlang".to_string(),
                code: "x = await tools.app_lookup({})?".to_string(),
                graph_key: None,
            },
        ))
        .await;
    collector
        .emit(test_activity(
            "tool-1",
            TurnEvent::ToolCallCompleted {
                call_id: Some("call-1".to_string()),
                name: "app_lookup".to_string(),
                args: serde_json::json!({}),
                output: lash_core::ToolCallOutput::success(serde_json::json!({ "ok": true })),
                duration_ms: 3,
                graph_key: None,
                parent_call_id: None,
            },
        ))
        .await;
    collector
        .emit(test_activity(
            "code-1",
            TurnEvent::CodeBlockCompleted {
                language: "lashlang".to_string(),
                output: String::new(),
                error: None,
                success: true,
                duration_ms: 4,
                tool_call_ids: vec!["call-1".to_string()],
                graph_key: None,
            },
        ))
        .await;

    let activities = collector.snapshot();
    assert_eq!(activities.len(), 3);
    assert!(matches!(
        &activities[0].event,
        TurnEvent::CodeBlockStarted { language, code, .. }
            if language == "lashlang" && code == "x = await tools.app_lookup({})?"
    ));
    assert!(matches!(
        &activities[1].event,
        TurnEvent::ToolCallCompleted { name, output, .. }
            if name == "app_lookup" && output.value_for_projection() == serde_json::json!({ "ok": true })
    ));
    assert_eq!(activities[0].correlation_id, activities[2].correlation_id);
    assert!(matches!(
        &activities[2].event,
        TurnEvent::CodeBlockCompleted { language, success, .. }
            if language == "lashlang" && *success
    ));
    Ok(())
}

#[tokio::test]
async fn turn_event_fanout_streams_to_collector_and_live_sink() -> Result<()> {
    let live = Arc::new(RecordingEvents::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(tool_roundtrip_provider())
        .model(mock_model_spec())
        .tools(Arc::new(AppTools))
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build()?;
    let session = core.session("fanout-tool-events").open().await?;

    let output = session
        .turn(TurnInput::text("use tool"))
        .advanced()
        .collect_with_scope(live.as_ref(), turn_scope(&session.session_id()))
        .await?;

    assert!(matches!(
        output.result.outcome,
        TurnOutcome::Finished(lash_core::TurnFinish::AssistantMessage { .. })
    ));
    assert_eq!(
        serde_json::to_value(&output.activities).expect("recorded activities serialize"),
        serde_json::to_value(live.snapshot().await).expect("live activities serialize")
    );
    assert_eq!(assistant_prose(&output.activities), "done");
    assert_eq!(output.assistant_message(), Some("done"));
    assert!(output.is_success());
    let tool_completed = output
        .activities
        .iter()
        .find(|activity| matches!(&activity.event, TurnEvent::ToolCallCompleted { .. }))
        .expect("tool completion");
    assert!(matches!(
        &tool_completed.event,
        TurnEvent::ToolCallCompleted { name, output, .. }
            if name == "app_lookup" && output.value_for_projection() == serde_json::json!({ "ok": true })
    ));
    Ok(())
}

#[test]
fn turn_run_batch_tool_runs_every_call_concurrently_and_preserves_order() -> Result<()> {
    run_async_test_on_stack_size("runtime-batch-tool-order-test", 8 * 1024 * 1024, || async {
        let tools = Arc::new(RuntimeBatchTools::new());
        let tool_provider: Arc<dyn ToolProvider> = tools.clone();
        let core = explicit_ephemeral_facets(LashCore::standard_builder())
            .provider(runtime_batch_provider())
            .model(mock_model_spec())
            .tools(tool_provider)
            .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
            .process_registry(Arc::new(TestLocalProcessRegistry::default()))
            .build()?;
        let session = core.session("runtime-batch-tool-order").open().await?;

        let output = session.turn(TurnInput::text("run batch")).run().await?;

        assert_eq!(output.assistant_message(), Some("done"));
        let batch_completed = output
            .activities
            .iter()
            .find(|activity| {
                matches!(
                    &activity.event,
                    TurnEvent::ToolCallCompleted { name, .. } if name == "runtime_batch"
                )
            })
            .expect("batch completion");
        let TurnEvent::ToolCallCompleted {
            output: batch_output,
            ..
        } = &batch_completed.event
        else {
            unreachable!();
        };
        let batch_value = batch_output.value_for_projection();
        let results = batch_value
            .get("results")
            .and_then(serde_json::Value::as_array)
            .expect("batch results");
        let result_tools = results
            .iter()
            .map(|result| {
                assert_eq!(
                    result.get("success").and_then(serde_json::Value::as_bool),
                    Some(true)
                );
                result
                    .get("tool")
                    .and_then(serde_json::Value::as_str)
                    .expect("result tool")
            })
            .collect::<Vec<_>>();
        assert_eq!(result_tools, ["first", "formerly_serial", "last"]);

        let windows = tools.windows();
        assert_eq!(windows.len(), 3);
        let names = windows
            .iter()
            .map(|(name, _, _)| name.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(names, BTreeSet::from(["first", "formerly_serial", "last"]));
        Ok(())
    })
}

#[test]
fn batch_child_tool_calls_carry_parent_call_id_linkage() -> Result<()> {
    run_async_test_on_stack_size(
        "batch-child-parent-linkage-test",
        8 * 1024 * 1024,
        || async {
            let tools = Arc::new(RuntimeBatchTools::new());
            let tool_provider: Arc<dyn ToolProvider> = tools.clone();
            let core = explicit_ephemeral_facets(LashCore::standard_builder())
                .provider(runtime_batch_provider())
                .model(mock_model_spec())
                .tools(tool_provider)
                .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
                .process_registry(Arc::new(TestLocalProcessRegistry::default()))
                .build()?;
            let session = core.session("batch-child-parent-linkage").open().await?;

            let output = session.turn(TurnInput::text("run batch")).run().await?;

            // The batch container call itself is a top-level standard-mode call: no
            // parent linkage and no code-block graph key.
            let (batch_call_id, batch_parent, batch_graph_key) = output
                .activities
                .iter()
                .find_map(|activity| match &activity.event {
                    TurnEvent::ToolCallStarted {
                        name,
                        call_id,
                        parent_call_id,
                        graph_key,
                        ..
                    } if name == "runtime_batch" => {
                        Some((call_id.clone(), parent_call_id.clone(), graph_key.clone()))
                    }
                    _ => None,
                })
                .expect("batch container ToolCallStarted");
            assert_eq!(
                batch_parent, None,
                "batch container must not carry a parent"
            );
            assert_eq!(batch_graph_key, None, "standard-mode call has no graph key");
            let batch_call_id = batch_call_id.expect("batch container call id");

            // Each batch child is delivered as its own tool event pointing back at the
            // batch call, so consumers reconstruct containment from real events.
            let child_parents = output
                .activities
                .iter()
                .filter_map(|activity| match &activity.event {
                    TurnEvent::ToolCallCompleted {
                        name,
                        parent_call_id,
                        graph_key,
                        ..
                    } if matches!(name.as_str(), "first" | "formerly_serial" | "last") => {
                        Some((name.clone(), parent_call_id.clone(), graph_key.clone()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            let child_names = child_parents
                .iter()
                .map(|(name, _, _)| name.clone())
                .collect::<BTreeSet<_>>();
            assert_eq!(
                child_names,
                BTreeSet::from([
                    "first".to_string(),
                    "formerly_serial".to_string(),
                    "last".to_string(),
                ]),
                "every batch child must surface as its own tool event"
            );
            for (name, parent, graph_key) in &child_parents {
                assert_eq!(
                    parent.as_deref(),
                    Some(batch_call_id.as_str()),
                    "batch child {name} must link to the batch call"
                );
                assert_eq!(
                    graph_key, &None,
                    "standard-mode batch child {name} has no code-block graph key"
                );
            }
            Ok(())
        },
    )
}

#[tokio::test]
async fn pending_host_tool_completion_parks_turn_and_resolves_through_core_ingress() -> Result<()> {
    let (key_tx, key_rx) = oneshot::channel();
    let events = Arc::new(RecordingEvents::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(tool_roundtrip_provider())
        .model(mock_model_spec())
        .tools(Arc::new(PendingAppTools::new(key_tx)))
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build()?;
    let session = core.session("pending-host-tool").open().await?;
    let turn_session = session.clone();
    let turn_events = Arc::clone(&events);
    let mut turn = tokio::spawn(async move {
        turn_session
            .turn(TurnInput::text("use async tool"))
            .stream_to(turn_events.as_ref())
            .await
    });

    let key = tokio::time::timeout(std::time::Duration::from_secs(1), key_rx)
        .await
        .expect("pending tool should request completion key")
        .expect("pending tool should send completion key");
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut turn)
            .await
            .is_err(),
        "turn completed before external completion resolved"
    );
    assert!(
        !events
            .snapshot()
            .await
            .iter()
            .any(|activity| matches!(&activity.event, TurnEvent::ToolCallCompleted { .. })),
        "pending launch must not be projected as a completed tool result"
    );

    let resolution = serde_json::json!({ "ok": true, "async": true });
    let accepted = core
        .completions()
        .resolve(key.clone(), lash_core::Resolution::Ok(resolution.clone()))
        .await?;
    assert_eq!(accepted, lash_core::ResolveOutcome::Accepted);

    let result = turn.await.expect("turn task")?;
    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::TurnFinish::AssistantMessage { .. })
    ));
    assert_eq!(result.assistant_message(), Some("done"));
    let events = events.snapshot().await;
    assert_eq!(assistant_prose(&events), "done");
    let tool_started = events
        .iter()
        .position(|activity| matches!(&activity.event, TurnEvent::ToolCallStarted { .. }))
        .expect("tool start event");
    let tool_completed = events
        .iter()
        .position(|activity| matches!(&activity.event, TurnEvent::ToolCallCompleted { .. }))
        .expect("tool completion event");
    assert!(tool_started < tool_completed);
    let TurnEvent::ToolCallCompleted { output, .. } = &events[tool_completed].event else {
        unreachable!();
    };
    assert_eq!(output.value_for_projection(), resolution);

    let duplicate = core
        .completions()
        .resolve(
            key,
            lash_core::Resolution::Ok(serde_json::json!({ "ok": false })),
        )
        .await?;
    assert!(matches!(
        duplicate,
        lash_core::ResolveOutcome::AlreadyResolved {
            terminal: lash_core::Resolution::Ok(value)
        } if value == resolution
    ));
    Ok(())
}

#[tokio::test]
async fn stream_returns_terminal_metadata_without_prose() -> Result<()> {
    let core = standard_core();
    let session = core.session("semantic-events").open().await?;
    let events = RecordingEvents::default();

    let result = session
        .turn(TurnInput::text("stream"))
        .stream_to(&events)
        .await?;

    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::TurnFinish::AssistantMessage { .. })
    ));
    let prose = events
        .snapshot()
        .await
        .into_iter()
        .filter_map(|event| match event.event {
            TurnEvent::AssistantProseDelta { text } => Some(text.to_string()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(prose, "echo: stream");
    assert!(!events.snapshot().await.iter().any(|event| matches!(
        &event.event,
        TurnEvent::FinalValue { .. } | TurnEvent::ToolValue { .. }
    )));
    Ok(())
}

#[tokio::test]
async fn stream_emits_chronological_tool_events_without_prose_pollution() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(tool_roundtrip_provider())
        .model(mock_model_spec())
        .tools(Arc::new(AppTools))
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build()?;
    let session = core.session("tool-events").open().await?;
    let events = RecordingEvents::default();

    let collected = session
        .turn(TurnInput::text("use tool"))
        .stream_to(&events)
        .await?;

    assert!(matches!(
        collected.outcome,
        TurnOutcome::Finished(lash_core::TurnFinish::AssistantMessage { .. })
    ));
    let events = events.snapshot().await;
    let started = events
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::ToolCallStarted { .. }))
        .expect("tool start event");
    let completed = events
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::ToolCallCompleted { .. }))
        .expect("tool completed event");
    assert!(started < completed);
    let TurnEvent::ToolCallCompleted { output, .. } = &events[completed].event else {
        unreachable!();
    };
    assert_eq!(
        output.value_for_projection(),
        serde_json::json!({ "ok": true })
    );
    let prose = events
        .into_iter()
        .filter_map(|event| match event.event {
            TurnEvent::AssistantProseDelta { text } => Some(text.to_string()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(prose, "done");
    assert!(!prose.contains("ok"));
    Ok(())
}

#[test]
fn rlm_streamed_lashlang_cell_uses_captured_body_when_final_text_is_raw() -> Result<()> {
    run_async_test_on_stack_budget("rlm-streamed-cell-raw-final-test", || async {
        const RAW_FINAL: &str = "Visible before cell.\n<lashlang>\npayload = r\"\"\"```markdown\ninside\n```\"\"\"\nfinish \"streamed raw final ok\"\n</lashlang>";
        const EXPECTED_CODE: &str =
            "payload = r\"\"\"```markdown\ninside\n```\"\"\"\nfinish \"streamed raw final ok\"";

        let provider = crate::testing::TestProvider::builder()
            .kind("stream-raw-final-test")
            .requires_streaming(true)
            .complete(|request| async move {
                let stream = request
                    .stream_events
                    .expect("RLM streaming turn should request provider stream events");
                for chunk in [
                    "Visible before",
                    " cell.\n<lash",
                    "lang>\npayload = r\"\"\"",
                    "```markdown\ninside\n",
                    "```\"\"\"\nfinish ",
                    "\"streamed raw final ok\"\n</lashlang>",
                ] {
                    stream.send(LlmStreamEvent::Delta(chunk.to_string()));
                }
                Ok(LlmResponse {
                    full_text: RAW_FINAL.to_string(),
                    parts: vec![LlmOutputPart::Text {
                        text: RAW_FINAL.to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            })
            .build()
            .into_handle();

        let core = explicit_ephemeral_facets(LashCore::rlm_builder(rlm_factory()))
            .provider(provider)
            .model(mock_model_spec())
            .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
            .process_registry(Arc::new(TestLocalProcessRegistry::default()))
            .build()?;
        let session = core.session("rlm-streamed-raw-final-cell").open().await?;
        let events = Arc::new(RecordingEvents::default());

        let result = session
            .turn(TurnInput::text("say hi"))
            .stream_to(events.as_ref())
            .await?;

        assert!(matches!(
            result.outcome,
            TurnOutcome::Finished(lash_core::TurnFinish::FinalValue { .. })
        ));
        assert_eq!(
            result.final_value(),
            Some(&serde_json::json!("streamed raw final ok"))
        );

        let events = events.snapshot().await;
        let prose = assistant_prose(&events);
        assert_eq!(prose, "Visible before cell.\n");
        assert!(!prose.contains("<lashlang>"));
        assert!(!prose.contains("finish"));
        assert!(!prose.contains("```markdown"));

        let code_started = events
            .iter()
            .find(|event| matches!(&event.event, TurnEvent::CodeBlockStarted { .. }))
            .expect("code started");
        let TurnEvent::CodeBlockStarted { language, code, .. } = &code_started.event else {
            unreachable!();
        };
        assert_eq!(language, "lashlang");
        assert_eq!(code, EXPECTED_CODE);
        assert!(!code.contains("<lashlang>"));

        let code_completed = events
            .iter()
            .find(|event| matches!(&event.event, TurnEvent::CodeBlockCompleted { .. }))
            .expect("code completed");
        let TurnEvent::CodeBlockCompleted { success, error, .. } = &code_completed.event else {
            unreachable!();
        };
        assert!(*success);
        assert!(error.is_none());

        let terminal_output = events
            .iter()
            .find(|event| matches!(&event.event, TurnEvent::FinalValue { .. }))
            .expect("terminal output");
        let TurnEvent::FinalValue { value } = &terminal_output.event else {
            unreachable!();
        };
        assert_eq!(value, &serde_json::json!("streamed raw final ok"));
        Ok(())
    })
}

#[test]
fn rlm_tool_calls_stream_from_live_exec_boundary() -> Result<()> {
    run_async_test_on_stack_budget("rlm-live-exec-boundary-test", || {
        rlm_tool_calls_stream_from_live_exec_boundary_inner()
    })
}

async fn rlm_tool_calls_stream_from_live_exec_boundary_inner() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(rlm_factory()))
        .provider(queued_text_provider(vec![lashlang_block(
            r#"value = await tools.app_lookup({})?
finish "done""#,
        )]))
        .model(mock_model_spec())
        .tools(Arc::new(AppTools))
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build()?;
    let session = core.session("rlm-live-tool-events").open().await?;
    let events = Arc::new(RecordingEvents::default());

    let result = session
        .turn(TurnInput::text("use tool"))
        .stream_to(events.as_ref())
        .await?;

    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::TurnFinish::FinalValue { .. })
    ));
    assert!(result.execution.had_tool_calls);
    assert_eq!(result.tool_calls.len(), 1);
    assert_eq!(result.tool_calls[0].tool, "app_lookup");
    assert_eq!(result.tool_calls[0].args, serde_json::json!({}));
    assert_eq!(
        result.tool_calls[0].output.value_for_projection(),
        serde_json::json!({ "ok": true })
    );
    let events = events.snapshot().await;
    let code_started = events
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::CodeBlockStarted { .. }))
        .expect("code started");
    let tool_started = events
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::ToolCallStarted { .. }))
        .expect("tool started");
    let tool_completed = events
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::ToolCallCompleted { .. }))
        .expect("tool completed");
    let code_completed = events
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::CodeBlockCompleted { .. }))
        .expect("code completed");
    let terminal_output = events
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::FinalValue { .. }))
        .expect("terminal output");
    assert!(code_started < tool_started);
    assert!(tool_started < tool_completed);
    assert!(tool_completed < code_completed);
    assert!(code_completed < terminal_output);
    assert!(!events[code_completed + 1..].iter().any(|event| matches!(
        &event.event,
        TurnEvent::ToolCallStarted { .. } | TurnEvent::ToolCallCompleted { .. }
    )));

    let TurnEvent::ToolCallCompleted {
        call_id,
        output,
        graph_key: tool_completed_graph_key,
        parent_call_id: tool_completed_parent,
        ..
    } = &events[tool_completed].event
    else {
        unreachable!();
    };
    assert_eq!(
        output.value_for_projection(),
        serde_json::json!({ "ok": true })
    );
    let TurnEvent::CodeBlockStarted {
        graph_key: started_graph_key,
        ..
    } = &events[code_started].event
    else {
        unreachable!();
    };
    assert!(
        started_graph_key
            .as_deref()
            .is_some_and(|key| key.starts_with("effect:rlm-live-tool-events:")),
        "missing foreground graph key on CodeBlockStarted: {started_graph_key:?}"
    );
    let TurnEvent::CodeBlockCompleted {
        language,
        success,
        error,
        tool_call_ids,
        graph_key: completed_graph_key,
        ..
    } = &events[code_completed].event
    else {
        unreachable!();
    };
    assert_eq!(language, "lashlang");
    assert!(*success);
    assert!(error.is_none());
    assert_eq!(call_id.as_ref(), tool_call_ids.first());
    assert_eq!(tool_call_ids.len(), 1);
    assert_eq!(completed_graph_key, started_graph_key);
    // Task 4: the RLM tool call carries the enclosing block's graph_key for
    // structural containment, and no batch parent for a top-level call.
    let TurnEvent::ToolCallStarted {
        graph_key: tool_started_graph_key,
        parent_call_id: tool_started_parent,
        ..
    } = &events[tool_started].event
    else {
        unreachable!();
    };
    assert_eq!(tool_started_graph_key, started_graph_key);
    assert_eq!(tool_completed_graph_key, started_graph_key);
    assert_eq!(tool_started_parent, &None);
    assert_eq!(tool_completed_parent, &None);
    let read_view = result.state.read_view();
    assert!(
        read_view.messages().iter().all(|message| message
            .parts
            .iter()
            .all(|part| part.tool_call_id.as_ref() != tool_call_ids.first())),
        "live RLM tool calls should not be persisted as message history"
    );
    assert_eq!(
        read_view
            .materialized_session_graph()
            .active_path_nodes()
            .into_iter()
            .filter_map(|node| node.event())
            .filter(|event| matches!(event, lash_core::SessionHistoryRecord::Conversation(_)))
            .count(),
        read_view.messages().len()
    );
    let TurnEvent::FinalValue { value } = &events[terminal_output].event else {
        unreachable!();
    };
    assert_eq!(value, &serde_json::json!("done"));
    Ok(())
}

#[test]
fn rlm_recovered_tool_failure_remains_in_turn_accounting() -> Result<()> {
    run_async_test_on_stack_budget("rlm-recovered-tool-failure-test", || async {
        let core = explicit_ephemeral_facets(LashCore::rlm_builder(rlm_factory()))
            .provider(queued_text_provider(vec![lashlang_block(
                r#"failure = await tools.app_lookup({})
finish "recovered""#,
            )]))
            .model(mock_model_spec())
            .tools(Arc::new(FailingAppTools))
            .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
            .process_registry(Arc::new(TestLocalProcessRegistry::default()))
            .build()?;
        let session = core.session("rlm-recovered-tool-failure").open().await?;

        let result = session
            .turn(TurnInput::text("recover the tool failure"))
            .run()
            .await?
            .result;

        assert!(matches!(
            result.outcome,
            TurnOutcome::Finished(lash_core::TurnFinish::FinalValue { .. })
        ));
        assert_eq!(result.final_value(), Some(&serde_json::json!("recovered")));
        assert!(result.execution.had_tool_calls);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].tool, "app_lookup");
        assert!(!result.tool_calls[0].output.is_success());
        Ok(())
    })
}

#[test]
fn rlm_code_block_aggregate_lists_every_collected_tool_call() -> Result<()> {
    run_async_test_on_stack_budget("rlm-aggregate-tool-ids-test", || {
        rlm_code_block_aggregate_lists_every_collected_tool_call_inner()
    })
}

async fn rlm_code_block_aggregate_lists_every_collected_tool_call_inner() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(rlm_factory()))
        .provider(queued_text_provider(vec![lashlang_block(
            r#"a = await tools.app_lookup({})?
b = await tools.app_lookup({})?
finish "done""#,
        )]))
        .model(mock_model_spec())
        .tools(Arc::new(AppTools))
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build()?;
    let session = core.session("rlm-aggregate-tool-ids").open().await?;
    let events = Arc::new(RecordingEvents::default());

    let result = session
        .turn(TurnInput::text("use tools"))
        .stream_to(events.as_ref())
        .await?;
    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::TurnFinish::FinalValue { .. })
    ));
    let events = events.snapshot().await;

    // Every collected RLM tool record carries a call_id, so the code block's
    // `tool_call_ids` aggregate (which filters `Some(call_id)`) can never drop
    // a call and disagree with the trace's `tool_call_count`
    // (`output.tool_calls.len()`).
    let completed_ids = events
        .iter()
        .filter_map(|event| match &event.event {
            TurnEvent::ToolCallCompleted { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(completed_ids.len(), 2, "expected two tool completions");
    assert!(
        completed_ids.iter().all(Option::is_some),
        "every collected RLM tool record must carry a call_id"
    );

    let tool_call_ids = events
        .iter()
        .find_map(|event| match &event.event {
            TurnEvent::CodeBlockCompleted { tool_call_ids, .. } => Some(tool_call_ids.clone()),
            _ => None,
        })
        .expect("code block completed");
    assert_eq!(tool_call_ids.len(), completed_ids.len());
    for call_id in completed_ids.into_iter().flatten() {
        assert!(
            tool_call_ids.contains(&call_id),
            "code block aggregate must list collected tool call {call_id}"
        );
    }
    Ok(())
}

#[test]
fn rlm_tool_calls_emit_typed_trace_pair_and_structured_exec_diagnostic() -> Result<()> {
    run_async_test_on_stack_budget("rlm-tool-trace-test", || {
        rlm_tool_calls_emit_typed_trace_pair_and_structured_exec_diagnostic_inner()
    })
}

async fn rlm_tool_calls_emit_typed_trace_pair_and_structured_exec_diagnostic_inner() -> Result<()> {
    let trace_path = std::env::temp_dir().join(format!(
        "lash-rlm-tool-trace-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(rlm_factory()))
        .provider(queued_text_provider(vec![lashlang_block(
            r#"value = await tools.app_lookup({})?
finish "done""#,
        )]))
        .model(mock_model_spec())
        .tools(Arc::new(AppTools))
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .trace_jsonl_path(trace_path.clone())
        .build()?;
    let session = core.session("rlm-tool-trace").open().await?;

    let result = session.turn(TurnInput::text("use tool")).run().await?;
    assert!(matches!(
        result.result.outcome,
        TurnOutcome::Finished(lash_core::TurnFinish::FinalValue { .. })
    ));
    core.flush_trace_sink()?;

    let logged = std::fs::read_to_string(&trace_path).expect("read trace");
    let entries = logged
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json log entry"))
        .collect::<Vec<_>>();

    // Task 1: RLM tool calls emit a single typed Started/Completed trace pair,
    // with span identity stamped so each nests under its turn as tool:<call_id>.
    let started = entries
        .iter()
        .filter(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("tool_call_started"))
        .collect::<Vec<_>>();
    let completed = entries
        .iter()
        .filter(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("tool_call_completed"))
        .collect::<Vec<_>>();
    assert_eq!(started.len(), 1, "expected one RLM tool start: {entries:?}");
    assert_eq!(
        completed.len(),
        1,
        "expected one RLM tool completion: {entries:?}"
    );
    let call_id = completed[0]
        .get("call_id")
        .and_then(|v| v.as_str())
        .expect("completed tool trace call id");
    assert_eq!(
        completed[0].get("name").and_then(|v| v.as_str()),
        Some("app_lookup")
    );
    assert_eq!(
        completed[0]
            .get("context")
            .and_then(|context| context.get("graph_node_id"))
            .and_then(|v| v.as_str()),
        Some(format!("tool:{call_id}").as_str()),
        "RLM tool span identity must be tool:<call_id>"
    );

    // Task 2: the exec_code_completed runtime diagnostic carries a structured
    // tool_calls array whose length matches tool_call_count.
    let exec_completed = entries
        .iter()
        .find(|entry| {
            entry.get("type").and_then(|v| v.as_str()) == Some("protocol_step")
                && entry
                    .get("payload")
                    .and_then(|payload| payload.get("diagnostic"))
                    .and_then(|diagnostic| diagnostic.get("phase"))
                    .and_then(|v| v.as_str())
                    == Some("exec_code_completed")
        })
        .expect("exec_code_completed diagnostic");
    let diagnostic_payload = exec_completed
        .get("payload")
        .and_then(|payload| payload.get("diagnostic"))
        .and_then(|diagnostic| diagnostic.get("payload"))
        .expect("diagnostic payload");
    let tool_calls = diagnostic_payload
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .expect("structured tool_calls array");
    let tool_call_count = diagnostic_payload
        .get("tool_call_count")
        .and_then(|v| v.as_u64())
        .expect("tool_call_count");
    assert_eq!(tool_calls.len() as u64, tool_call_count);
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(
        tool_calls[0].get("call_id").and_then(|v| v.as_str()),
        Some(call_id)
    );
    assert_eq!(
        tool_calls[0].get("name").and_then(|v| v.as_str()),
        Some("app_lookup")
    );
    assert_eq!(
        tool_calls[0].get("status").and_then(|v| v.as_str()),
        Some("success")
    );
    assert!(tool_calls[0].get("duration_ms").is_some());

    let _ = std::fs::remove_file(&trace_path);
    Ok(())
}

#[test]
fn rlm_pending_host_tool_completion_resumes_lashlang_await() -> Result<()> {
    run_async_test_on_stack_budget("rlm-pending-host-tool-test", || {
        rlm_pending_host_tool_completion_resumes_lashlang_await_inner()
    })
}

async fn rlm_pending_host_tool_completion_resumes_lashlang_await_inner() -> Result<()> {
    let (key_tx, key_rx) = oneshot::channel();
    let events = Arc::new(RecordingEvents::default());
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(rlm_factory()))
        .provider(queued_text_provider(vec![lashlang_block(
            "value = await tools.app_lookup({})?\nfinish value",
        )]))
        .model(mock_model_spec())
        .tools(Arc::new(PendingAppTools::new(key_tx)))
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build()?;
    let session = core.session("rlm-pending-host-tool").open().await?;
    let turn_session = session.clone();
    let turn_events = Arc::clone(&events);
    let mut turn = tokio::spawn(async move {
        turn_session
            .turn(TurnInput::text("await async app lookup"))
            .stream_to(turn_events.as_ref())
            .await
    });

    let key = tokio::time::timeout(std::time::Duration::from_secs(1), key_rx)
        .await
        .expect("pending RLM tool should request completion key")
        .expect("pending RLM tool should send completion key");
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut turn)
            .await
            .is_err(),
        "RLM turn completed before external completion resolved"
    );
    assert!(
        !events
            .snapshot()
            .await
            .iter()
            .any(|activity| matches!(&activity.event, TurnEvent::ToolCallCompleted { .. })),
        "pending RLM launch must not emit a completed tool result"
    );

    let payload = serde_json::json!({ "ok": true, "async": "rlm" });
    let outcome = core
        .completions()
        .resolve(key, lash_core::Resolution::Ok(payload.clone()))
        .await?;
    assert_eq!(outcome, lash_core::ResolveOutcome::Accepted);

    let result = turn.await.expect("turn task")?;
    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::TurnFinish::FinalValue { .. })
    ));
    assert_eq!(result.final_value(), Some(&payload));
    let events = events.snapshot().await;
    let terminal_output = events
        .iter()
        .find_map(|activity| match &activity.event {
            TurnEvent::FinalValue { value } => Some(value),
            _ => None,
        })
        .expect("terminal final value");
    assert_eq!(terminal_output, &payload);
    Ok(())
}

#[test]
fn rlm_process_pending_host_tool_completion_resumes_process_await() -> Result<()> {
    run_async_test_on_stack_budget("rlm-process-pending-host-tool-test", || {
        rlm_process_pending_host_tool_completion_resumes_process_await_inner()
    })
}

async fn rlm_process_pending_host_tool_completion_resumes_process_await_inner() -> Result<()> {
    let (key_tx, key_rx) = oneshot::channel();
    let events = Arc::new(RecordingEvents::default());
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(rlm_factory()))
        .provider(queued_text_provider(vec![lashlang_block(
            r#"
process lookup(tools: Tools) {
  value = await tools.app_lookup({})?
  finish value
}
handle = start lookup(tools: tools)
result = (await handle)?
finish result"#,
        )]))
        .model(mock_model_spec())
        .tools(Arc::new(PendingAppTools::new(key_tx)))
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build()?;
    let session = core.session("rlm-process-pending-host-tool").open().await?;
    let turn_session = session.clone();
    let turn_events = Arc::clone(&events);
    let mut turn = tokio::spawn(async move {
        turn_session
            .turn(TurnInput::text("start process with async app lookup"))
            .stream_to(turn_events.as_ref())
            .await
    });

    let key = tokio::time::timeout(std::time::Duration::from_secs(1), key_rx)
        .await
        .expect("pending process tool should request completion key")
        .expect("pending process tool should send completion key");
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut turn)
            .await
            .is_err(),
        "process-backed turn completed before external completion resolved"
    );
    assert!(
        !events
            .snapshot()
            .await
            .iter()
            .any(|activity| matches!(&activity.event, TurnEvent::ToolCallCompleted { .. })),
        "pending process tool launch must not emit a completed tool result"
    );

    let payload = serde_json::json!({ "ok": true, "async": "process" });
    let outcome = core
        .completions()
        .resolve(key, lash_core::Resolution::Ok(payload.clone()))
        .await?;
    assert_eq!(outcome, lash_core::ResolveOutcome::Accepted);

    let result = turn.await.expect("turn task")?;
    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::TurnFinish::FinalValue { .. })
    ));
    assert_eq!(result.final_value(), Some(&payload));
    let events = events.snapshot().await;
    let terminal_output = events
        .iter()
        .find_map(|activity| match &activity.event {
            TurnEvent::FinalValue { value } => Some(value),
            _ => None,
        })
        .expect("terminal final value");
    assert_eq!(terminal_output, &payload);
    Ok(())
}

#[test]
fn continue_as_observation_emits_frame_switch_then_commit() -> Result<()> {
    run_async_test_on_stack_budget("continue-as-observation-test", || {
        continue_as_observation_emits_frame_switch_then_commit_inner()
    })
}

async fn continue_as_observation_emits_frame_switch_then_commit_inner() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(rlm_factory()))
        .provider(queued_text_provider(vec![
            lashlang_block(r#"await control.continue_as({ task: "finish in a fresh frame" })?"#),
            lashlang_block(r#"finish "done after continue_as""#),
        ]))
        .model(mock_model_spec())
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .build()?;
    let session = core.session("continue-as-observation").open().await?;
    let cursor = session.observe().current_observation().cursor;

    let output = session.turn(TurnInput::text("switch frames")).run().await?;
    assert_eq!(
        output.final_value(),
        Some(&serde_json::json!("done after continue_as"))
    );

    let SessionResume::Replayed { events } = session.observe().resume_from_cursor(&cursor)? else {
        panic!("recent cursor should replay continue_as observation events");
    };
    assert!(
        events.windows(2).any(|window| matches!(
            (&window[0].payload, &window[1].payload),
            (
                lash_core::SessionObservationEventPayload::AgentFrameSwitched { .. },
                lash_core::SessionObservationEventPayload::Committed { .. }
            )
        )),
        "expected AgentFrameSwitched immediately followed by Committed, got {events:?}"
    );
    Ok(())
}

#[test]
fn durable_agent_frame_follow_through_uses_distinct_turn_scopes_and_commits() -> Result<()> {
    run_async_test_on_stack_budget("durable-agent-frame-follow-through-test", || {
        durable_agent_frame_follow_through_uses_distinct_turn_scopes_and_commits_inner()
    })
}

async fn durable_agent_frame_follow_through_uses_distinct_turn_scopes_and_commits_inner()
-> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "agent-frame-durable";
    let root_turn_id = "agent-frame-root-turn";
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.path().join("sessions"),
    ));
    let controller = Arc::new(RecordingDurableEffectController::default());
    let scoped_effect_controller = ScopedEffectController::borrowed(
        controller.as_ref(),
        lash_core::ExecutionScope::turn(session_id, root_turn_id),
    )
    .expect("scoped durable effect controller");
    let core = LashCore::standard_builder()
        .provider(agent_frame_switch_provider())
        .model(mock_model_spec())
        .tools(Arc::new(AgentFrameSwitchTools))
        .store_factory(store_factory.clone())
        .attachment_store(Arc::new(crate::persistence::FileAttachmentStore::new(
            dir.path().join("attachments"),
        )))
        .effect_host(Arc::new(lash_core::InlineEffectHost::default()))
        .process_env_store(Arc::new(DurableInMemoryProcessEnvStore::default()))
        .build()?;
    let session = core.session(session_id).open().await?;
    let mut input = TurnInput::text("switch frames");
    input.trace_turn_id = Some(root_turn_id.to_string());

    let output = session
        .turn(input)
        .advanced()
        .run_with_scope(scoped_effect_controller)
        .await?;

    assert_eq!(output.assistant_message(), Some("done after frame switch"));
    let follow_turn_id = format!("{root_turn_id}:agent-frame:1");
    let mut llm_turn_ids = controller
        .invocations()
        .into_iter()
        .filter(|record| record.kind == lash_core::RuntimeEffectKind::LlmCall)
        .map(|record| record.turn_id.expect("turn-scoped LLM effect"))
        .collect::<Vec<_>>();
    llm_turn_ids.sort();
    llm_turn_ids.dedup();
    assert_eq!(
        llm_turn_ids,
        vec![root_turn_id.to_string(), follow_turn_id.clone()]
    );
    let replay_keys = controller
        .invocations()
        .into_iter()
        .filter_map(|record| record.replay_key)
        .collect::<Vec<_>>();
    assert!(
        replay_keys.iter().any(|key| key.contains(root_turn_id)),
        "root turn replay keys should include {root_turn_id}: {replay_keys:?}"
    );
    assert!(
        replay_keys.iter().any(|key| key.contains(&follow_turn_id)),
        "follow turn replay keys should include {follow_turn_id}: {replay_keys:?}"
    );

    let conn = rusqlite::Connection::open(store_factory.path_for_session(session_id))
        .expect("open session sqlite store");
    let mut stmt = conn
        .prepare("SELECT turn_id FROM runtime_turn_commits ORDER BY turn_id ASC")
        .expect("prepare turn commits");
    let turn_commit_ids = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query turn commits")
        .map(|row| row.expect("read turn commit row"))
        .collect::<Vec<_>>();
    assert_eq!(
        turn_commit_ids,
        vec![root_turn_id.to_string(), follow_turn_id]
    );
    Ok(())
}

#[test]
fn processes_lists_started_lashlang_process_until_awaited() -> Result<()> {
    run_async_test_on_stack_budget("process-control-lashlang-process-test", || {
        processes_lists_started_lashlang_process_until_awaited_inner()
    })
}

async fn processes_lists_started_lashlang_process_until_awaited_inner() -> Result<()> {
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(rlm_factory()))
        .provider(queued_text_provider(vec![lashlang_block(
            r#"
process lookup(tools: Tools) {
  value = await tools.app_lookup({})?
  finish value
}
h = start lookup(tools: tools)
value = await h
finish value"#,
        )]))
        .model(mock_model_spec())
        .tools(Arc::new(BlockingAppTools::new(entered_tx, release_rx)))
        // A started (`start lookup(...)`) process runs in the lease-protected
        // worker's rebuilt runtime, which needs a session store factory; the
        // explicit in-memory factory backs ephemeral process execution.
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build()?;
    let session = core.session("rlm-process-control-tool").open().await?;
    let turn_session = session.clone();
    let scoped_effect_controller = turn_scope(&turn_session.session_id());
    let turn = tokio::spawn(async move {
        turn_session
            .turn(TurnInput::text("start tool"))
            .advanced()
            .run_with_scope(scoped_effect_controller)
            .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx)
        .await
        .expect("tool process should start")
        .expect("tool provider entered");

    let processes = session.processes().list().await?;
    let running_app_lookup = processes.iter().any(|process| {
        process.kind == "lashlang" && process.label == "lookup" && !process.terminal
    });
    assert!(
        running_app_lookup,
        "expected running lookup lashlang process, got {processes:?}"
    );

    release_tx.send(()).expect("release tool provider");
    let result = turn.await.expect("turn task")?;
    assert_eq!(
        result.final_value(),
        Some(&serde_json::json!({
            "ok": true,
            "value": { "answer": "ready" },
        }))
    );
    Ok(())
}

#[test]
fn lashlang_execution_graph_store_observes_lashlang_process_from_facade() -> Result<()> {
    run_async_test_on_stack_budget("lashlang-graph-store-facade-test", || {
        lashlang_execution_graph_store_observes_lashlang_process_from_facade_inner()
    })
}

async fn lashlang_execution_graph_store_observes_lashlang_process_from_facade_inner() -> Result<()>
{
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let graph_store = Arc::new(crate::tracing::TraceLashlangGraphStore::default());
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        rlm_factory().with_lashlang_execution_sink(
            Arc::clone(&graph_store) as Arc<dyn crate::tracing::TraceSink>
        ),
    ))
    .provider(queued_text_provider(vec![lashlang_block(
        r#"
process lookup(tools: Tools) {
  value = await tools.app_lookup({})?
  finish value
}
h = start lookup(tools: tools)
value = await h
finish value"#,
    )]))
    .model(mock_model_spec())
    .tools(Arc::new(BlockingAppTools::new(entered_tx, release_rx)))
    .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
    .process_registry(Arc::new(TestLocalProcessRegistry::default()))
    .build()?;
    let session = core.session("rlm-lashlang-graph-store").open().await?;
    let turn_session = session.clone();
    let scoped_effect_controller = turn_scope(&turn_session.session_id());
    let turn = tokio::spawn(async move {
        turn_session
            .turn(TurnInput::text("start tool"))
            .advanced()
            .run_with_scope(scoped_effect_controller)
            .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx)
        .await
        .expect("tool process should start")
        .expect("tool provider entered");

    let processes = session.processes().list().await?;
    let running = processes
        .iter()
        .find(|process| process.label == "lookup")
        .expect("running lookup process");
    let graph = graph_store
        .graph(&format!("process:{}", running.process_id))
        .expect("Lashlang graph snapshot");
    assert_eq!(graph.graph_key, format!("process:{}", running.process_id));
    assert_eq!(graph.entry_kind, "process");
    assert_eq!(graph.entry_name, "lookup");
    assert_eq!(
        graph.status,
        lash_lashlang_runtime::TraceLashlangStatus::Running
    );
    assert!(!graph.nodes.is_empty());
    assert!(
        graph_store
            .graphs()
            .iter()
            .any(|graph| graph.entry_name == "lookup")
    );

    release_tx.send(()).expect("release tool provider");
    let _ = turn.await.expect("turn task")?;
    Ok(())
}

#[tokio::test]
async fn natural_rlm_completion_emits_no_terminal_output() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(rlm_factory()))
        .provider(queued_text_provider(vec!["done in prose"]))
        .model(mock_model_spec())
        .build()?;
    let session = core.session("rlm-prose-completion").open().await?;
    let events = Arc::new(RecordingEvents::default());

    let result = session
        .turn(TurnInput::text("answer directly"))
        .allow_prose_or_finish()?
        .stream_to(events.as_ref())
        .await?;

    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::TurnFinish::AssistantMessage { .. })
    ));
    let events = events.snapshot().await;
    assert!(!events.iter().any(|event| matches!(
        &event.event,
        TurnEvent::FinalValue { .. } | TurnEvent::ToolValue { .. }
    )));
    assert_eq!(assistant_prose(&events), "done in prose");
    let read_view = result.state.read_view();
    let assistant_messages = read_view
        .messages()
        .iter()
        .filter(|message| message.role == lash_core::MessageRole::Assistant)
        .collect::<Vec<_>>();
    assert_eq!(assistant_messages.len(), 1);
    assert_eq!(assistant_messages[0].parts[0].content, "done in prose");
    Ok(())
}

#[tokio::test]
async fn finish_required_rlm_completion_emits_terminal_output() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(rlm_factory()))
        .provider(queued_text_provider(vec![lashlang_block(
            r#"finish "done via finish""#,
        )]))
        .model(mock_model_spec())
        .build()?;
    let session = core
        .session("rlm-finish-required-completion")
        .open()
        .await?;
    let events = Arc::new(RecordingEvents::default());

    let result = session
        .turn(TurnInput::text("finish"))
        .require_finish()?
        .stream_to(events.as_ref())
        .await?;

    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::TurnFinish::FinalValue { .. })
    ));
    assert_eq!(
        result.final_value(),
        Some(&serde_json::json!("done via finish"))
    );
    let events = events.snapshot().await;
    let terminal_output = events
        .iter()
        .find(|event| matches!(&event.event, TurnEvent::FinalValue { .. }))
        .expect("terminal output");
    let TurnEvent::FinalValue { value } = &terminal_output.event else {
        unreachable!();
    };
    assert_eq!(value, &serde_json::json!("done via finish"));
    Ok(())
}

#[tokio::test]
async fn rlm_failed_code_emits_failed_code_completion_without_fake_tools() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(rlm_factory()))
        .provider(queued_text_provider(vec![
            lashlang_block("this is not valid lashlang"),
            lashlang_block(r#"finish "recovered""#),
        ]))
        .model(mock_model_spec())
        .tools(Arc::new(AppTools))
        .build()?;
    let session = core.session("rlm-failed-code-event").open().await?;
    let events = RecordingEvents::default();

    let _result = session
        .turn(TurnInput::text("bad code"))
        .stream_to(&events)
        .await?;

    let events = events.snapshot().await;
    let failed = events
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                TurnEvent::CodeBlockCompleted {
                    success: false,
                    error: Some(_),
                    ..
                }
            )
        })
        .expect("failed code completion");
    let next_code = events[failed + 1..]
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::CodeBlockStarted { .. }))
        .map(|offset| failed + 1 + offset)
        .unwrap_or(events.len());
    assert!(
        !events[failed + 1..next_code]
            .iter()
            .any(|event| matches!(&event.event, TurnEvent::ToolCallCompleted { .. }))
    );
    Ok(())
}
