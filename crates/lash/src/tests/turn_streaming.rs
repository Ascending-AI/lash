use super::*;
#[cfg(feature = "rlm")]
use crate::rlm::RlmTurnBuilderExt as _;
use futures_util::StreamExt as _;
use lash_core::QueuedWorkStore as _;
use lash_core::SessionExecutionLeaseStore as _;
use lash_sansio::sync::{LockResultExt, MutexExt};
#[cfg(feature = "rlm")]
use sha2::Digest as _;
use std::collections::BTreeSet;

struct QueuedWorkHydrationProbeFactory {
    builds: Arc<AtomicUsize>,
}

impl lash_core::facade_support::PluginFactory for QueuedWorkHydrationProbeFactory {
    fn id(&self) -> &'static str {
        "queued-work-hydration-probe"
    }

    fn build(
        &self,
        _ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        self.builds.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(QueuedWorkHydrationProbePlugin))
    }
}

struct QueuedWorkHydrationProbePlugin;

impl lash_core::facade_support::SessionPlugin for QueuedWorkHydrationProbePlugin {
    fn id(&self) -> &'static str {
        "queued-work-hydration-probe"
    }

    fn register(
        &self,
        _reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        Ok(())
    }
}

#[cfg(feature = "rlm")]
struct TurnPersistedGraphAppendFactory {
    append_count: Arc<AtomicUsize>,
    max_appends: usize,
}

#[cfg(feature = "rlm")]
impl lash_core::facade_support::PluginFactory for TurnPersistedGraphAppendFactory {
    fn id(&self) -> &'static str {
        "turn-persisted-graph-append"
    }

    fn build(
        &self,
        _ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        Ok(Arc::new(TurnPersistedGraphAppendPlugin {
            append_count: Arc::clone(&self.append_count),
            max_appends: self.max_appends,
        }))
    }
}

#[cfg(feature = "rlm")]
struct TurnPersistedGraphAppendPlugin {
    append_count: Arc<AtomicUsize>,
    max_appends: usize,
}

#[cfg(feature = "rlm")]
struct StopAfterFrameSwitchCommitFactory;

#[cfg(feature = "rlm")]
impl lash_core::facade_support::PluginFactory for StopAfterFrameSwitchCommitFactory {
    fn id(&self) -> &'static str {
        "stop-after-frame-switch-commit"
    }

    fn build(
        &self,
        _ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        Ok(Arc::new(StopAfterFrameSwitchCommitPlugin))
    }
}

#[cfg(feature = "rlm")]
struct StopAfterFrameSwitchCommitPlugin;

#[cfg(feature = "rlm")]
impl lash_core::facade_support::SessionPlugin for StopAfterFrameSwitchCommitPlugin {
    fn id(&self) -> &'static str {
        "stop-after-frame-switch-commit"
    }

    fn register(
        &self,
        reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        reg.session().on_event(Arc::new(|event| {
            Box::pin(async move {
                if matches!(
                    event,
                    lash_core::facade_support::PluginLifecycleEvent::TurnPersisted(_)
                ) {
                    return Err(lash_core::PluginError::Session(
                        "stop after the accepted frame-switch commit".to_string(),
                    ));
                }
                Ok(())
            })
        }));
        Ok(())
    }
}

#[cfg(feature = "rlm")]
fn frame_state_probe_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:frame_state_probe",
        "frame_state_probe",
        "Record a deferred resolution in the current execution state.",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "string" }),
    )
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(
        ["fixture"],
        "probe",
    ))
}

#[cfg(feature = "rlm")]
struct FrameStateDeferredResolver;

#[cfg(feature = "rlm")]
#[async_trait]
impl lash_lashlang_runtime::DeferredToolResolver for FrameStateDeferredResolver {
    async fn resolve(
        &self,
        paths: &[&str],
    ) -> std::collections::BTreeMap<String, lash_lashlang_runtime::Resolution> {
        paths
            .iter()
            .map(|path| {
                let resolution = if *path == "fixture.probe" {
                    lash_lashlang_runtime::Resolution::Resolved(Box::new(
                        lash_lashlang_runtime::ToolGrant::new(frame_state_probe_definition())
                            .with_source_id(lash_core::facade_support::PLUGIN_TOOL_SOURCE_ID),
                    ))
                } else {
                    lash_lashlang_runtime::Resolution::NotAvailable
                };
                ((*path).to_string(), resolution)
            })
            .collect()
    }
}

#[cfg(feature = "rlm")]
struct FrameStateDeferredTools;

#[cfg(feature = "rlm")]
#[async_trait]
impl lash_core::ToolProvider for FrameStateDeferredTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        Vec::new()
    }

    fn resolve_manifest_by_id(&self, id: &lash_core::ToolId) -> Option<lash_core::ToolManifest> {
        (id == &lash_core::ToolId::from("tool:frame_state_probe"))
            .then(|| frame_state_probe_definition().manifest())
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<lash_core::ToolContract>> {
        None
    }

    async fn prepare_granted_tool_call(
        &self,
        _grant: &lash_core::ToolExecutionGrant,
        call: lash_core::ToolPrepareCall<'_>,
    ) -> std::result::Result<lash_core::PreparedToolCall, lash_core::ToolOutcome> {
        Ok(lash_core::PreparedToolCall::identity(
            call.tool_id,
            call.pending,
        ))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        assert_eq!(call.name, "frame_state_probe");
        lash_core::ToolOutcome::ok(serde_json::json!("recorded"))
    }

    async fn execute_granted(
        &self,
        grant: &lash_core::ToolExecutionGrant,
        args: &serde_json::Value,
        context: &lash_core::AttemptContext<'_>,
    ) -> lash_core::ToolOutcome {
        self.execute_by_id(&grant.manifest.id, args, context).await
    }
}

#[cfg(feature = "rlm")]
fn assert_sqlite_session_lane_free_at_generation(
    store_factory: &lash_sqlite_store::SqliteSessionStoreFactory,
    session_id: &str,
    expected_generation: u64,
) {
    let conn = rusqlite::Connection::open(store_factory.catalog_path())
        .expect("open SQLite session catalog");
    let (owner, generation) = conn
        .query_row(
            "SELECT lease_owner_id, lease_fencing_token FROM session_execution_leases WHERE session_id = ?1",
            [session_id],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, u64>(1)?)),
        )
        .expect("read session execution lease row");
    assert!(
        owner.is_none(),
        "completed handoff must leave the lane free"
    );
    assert_eq!(
        generation, expected_generation,
        "nested borrowed commits must not rotate the outer lane generation"
    );
}

#[cfg(feature = "rlm")]
impl lash_core::facade_support::SessionPlugin for TurnPersistedGraphAppendPlugin {
    fn id(&self) -> &'static str {
        "turn-persisted-graph-append"
    }

    fn register(
        &self,
        reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        let append_count = Arc::clone(&self.append_count);
        let max_appends = self.max_appends;
        reg.session().on_event(Arc::new(move |event| {
            let append_count = Arc::clone(&append_count);
            Box::pin(async move {
                let lash_core::facade_support::PluginLifecycleEvent::TurnPersisted(ctx) = event
                else {
                    return Ok(());
                };
                let Ok(append_index) =
                    append_count.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                        (current < max_appends).then_some(current + 1)
                    })
                else {
                    return Ok(());
                };
                let _ = ctx
                    .session_graph
                    .append_session_nodes(
                        &ctx.session_id,
                        lash_core::AppendSessionNodesRequest {
                            operation_id: format!("turn-persisted-graph-append-{append_index}"),
                            nodes: vec![lash_core::SessionAppendNode::plugin(
                                "test.turn-persisted",
                                serde_json::json!({ "committed": true }),
                            )],
                            requires_ancestor_node_id: None,
                        },
                    )
                    .await;
                Ok(())
            })
        }));
        Ok(())
    }
}

struct CreateOnlySessionStoreFactory {
    inner: lash_core::facade_support::InMemorySessionStoreFactory,
}

// The fixture narrows the factory surface, but attachment ownership remains
// with the real inner factory and must be delegated unchanged.
#[async_trait]
impl lash_core::AttachmentRootSet for CreateOnlySessionStoreFactory {
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> std::result::Result<
        std::collections::BTreeSet<lash_core::AttachmentId>,
        lash_core::StoreError,
    > {
        lash_core::AttachmentRootSet::live_attachment_refs(
            &self.inner,
            intent_grace_cutoff_epoch_ms,
        )
        .await
    }

    async fn has_live_attachment_ref(
        &self,
        id: &lash_core::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> std::result::Result<bool, lash_core::StoreError> {
        lash_core::AttachmentRootSet::has_live_attachment_ref(
            &self.inner,
            id,
            intent_grace_cutoff_epoch_ms,
        )
        .await
    }
}

#[async_trait]
impl lash_core::SessionStoreFactory for CreateOnlySessionStoreFactory {
    async fn create_store(
        &self,
        request: &lash_core::SessionStoreCreateRequest,
    ) -> std::result::Result<Arc<dyn lash_core::RuntimePersistence>, lash_core::StoreError> {
        self.inner.create_store(request).await
    }

    async fn session_was_deleted(&self, session_id: &str) -> std::result::Result<bool, String> {
        lash_core::SessionStoreFactory::session_was_deleted(&self.inner, session_id).await
    }

    async fn delete_session(&self, session_id: &str) -> std::result::Result<(), String> {
        self.inner.delete_session(session_id).await
    }
}

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
        self.invocations.lock_recover().clone()
    }
}

impl lash_core::AwaitEventResolver for RecordingDurableEffectController {
    fn replay_ownership(&self) -> lash_core::EffectReplayOwnership {
        lash_core::EffectReplayOwnership::Controller
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
            .lock_recover()
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
    persisted_outcomes: StdMutex<Vec<String>>,
    inline: lash_core::facade_support::InlineRuntimeEffectController,
}

impl RecordingInlineEffectController {
    fn invocations(&self) -> Vec<DurableEffectInvocation> {
        self.invocations.lock_recover().clone()
    }

    #[cfg(feature = "rlm")]
    fn persisted_outcomes(&self) -> Vec<lash_core::RuntimeEffectOutcome> {
        self.persisted_outcomes
            .lock_recover()
            .iter()
            .map(|outcome| serde_json::from_str(outcome).expect("deserialize effect outcome"))
            .collect()
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
            .lock_recover()
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
        let outcome = local_executor.execute(envelope).await;
        if let Ok(outcome) = &outcome {
            self.persisted_outcomes
                .lock_recover()
                .push(serde_json::to_string(outcome).expect("serialize effect outcome"));
        }
        outcome
    }
}

#[derive(Default)]
struct DurableInMemoryProcessEnvStore {
    inner: lash_core::facade_support::InMemoryProcessExecutionEnvStore,
}

#[async_trait]
impl lash_core::ProcessExecutionEnvStore for DurableInMemoryProcessEnvStore {
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
    fn replay_ownership(&self) -> lash_core::EffectReplayOwnership {
        lash_core::EffectReplayOwnership::Controller
    }
}

impl lash_core::EffectHost for DurableNoopEffectHost {
    fn scoped<'run>(
        &'run self,
        scope: lash_core::ExecutionScope,
    ) -> std::result::Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        lash_core::ScopedEffectController::shared(
            Arc::new(lash_core::facade_support::InlineRuntimeEffectController::default()),
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
            Arc::new(lash_core::facade_support::InlineRuntimeEffectController::default()),
            scope,
        )?))
    }
}

#[cfg(feature = "rlm")]
struct BlockingAppTools {
    entered_tx: StdMutex<Option<oneshot::Sender<()>>>,
    release_rx: TokioMutex<Option<oneshot::Receiver<()>>>,
}

#[derive(Clone, Default)]
struct ContractRecordingTools {
    resolved: Arc<StdMutex<Vec<serde_json::Value>>>,
}

impl ContractRecordingTools {
    fn take_resolved(&self) -> Vec<serde_json::Value> {
        std::mem::take(&mut *self.resolved.lock_recover())
    }
}

#[async_trait]
impl ToolProvider for ContractRecordingTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![app_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        if name != "app_lookup" {
            return None;
        }
        let contract = Arc::new(app_tool_definition().contract());
        self.resolved
            .lock_recover()
            .push(serde_json::to_value(contract.as_ref()).expect("serialize tool contract"));
        Some(contract)
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true }))
    }
}

#[cfg(feature = "rlm")]
impl BlockingAppTools {
    fn new(entered_tx: oneshot::Sender<()>, release_rx: oneshot::Receiver<()>) -> Self {
        Self {
            entered_tx: StdMutex::new(Some(entered_tx)),
            release_rx: TokioMutex::new(Some(release_rx)),
        }
    }
}

#[cfg(feature = "rlm")]
#[async_trait]
impl ToolProvider for BlockingAppTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![app_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(app_tool_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        assert_eq!(call.name, "app_lookup");
        if let Some(tx) = self.entered_tx.lock_recover().take() {
            let _ = tx.send(());
        }
        if let Some(rx) = self.release_rx.lock().await.take() {
            let _ = rx.await;
        }
        lash_core::ToolOutcome::ok(serde_json::json!({ "answer": "ready" }))
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
        self.windows.lock_recover().clone()
    }
}

#[async_trait]
impl ToolProvider for RuntimeBatchTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![
            runtime_probe_tool_definition("first").manifest(),
            runtime_probe_tool_definition("formerly_serial").manifest(),
            runtime_probe_tool_definition("last").manifest(),
        ]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        match name {
            "first" => Some(Arc::new(runtime_probe_tool_definition("first").contract())),
            "formerly_serial" => Some(Arc::new(
                runtime_probe_tool_definition("formerly_serial").contract(),
            )),
            "last" => Some(Arc::new(runtime_probe_tool_definition("last").contract())),
            _ => None,
        }
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        match call.name {
            "first" | "formerly_serial" | "last" => {
                let start = std::time::Instant::now();
                let waited = tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    self.barrier.wait(),
                )
                .await;
                let end = std::time::Instant::now();
                self.windows
                    .lock_recover()
                    .push((call.name.to_string(), start, end));
                match waited {
                    Ok(_) => lash_core::ToolOutcome::ok(serde_json::json!(call.name)),
                    Err(_) => lash_core::ToolOutcome::err_fmt(format!(
                        "{} did not overlap with the rest of the batch",
                        call.name
                    )),
                }
            }
            other => lash_core::ToolOutcome::err_fmt(format!("Unknown tool: {other}")),
        }
    }
}

/// `runtime_batch` fans out to other tools from inside its own body, which is
/// journal-capable orchestration rather than leaf work. It therefore lives in
/// the orchestrating lane: a recorded leaf attempt receives an `AttemptContext`
/// and has no route to nested dispatch.
struct RuntimeBatchOrchestratingTool;

#[async_trait]
impl lash_core::facade_support::OrchestratingToolImplementation for RuntimeBatchOrchestratingTool {
    fn manifest(&self) -> lash_core::ToolManifest {
        runtime_batch_tool_definition().manifest()
    }

    fn contract(&self) -> Arc<lash_core::ToolContract> {
        Arc::new(runtime_batch_tool_definition().contract())
    }

    async fn execute(
        &self,
        args: &serde_json::Value,
        context: &lash_core::facade_support::OrchestrationContext<'_>,
    ) -> lash_core::ToolOutcome {
        execute_runtime_batch_tool(context, args).await
    }
}

fn runtime_batch_orchestrating_tool() -> lash_core::facade_support::OrchestratingToolDef {
    let implementation: Arc<dyn lash_core::facade_support::OrchestratingToolImplementation> =
        Arc::new(RuntimeBatchOrchestratingTool);
    // SAFETY: this crate's test module owns the `runtime_batch` contract and body.
    unsafe { lash_core::facade_support::OrchestratingToolDef::from_first_party(implementation) }
}

fn runtime_batch_plugin() -> Arc<StaticPluginFactory> {
    Arc::new(StaticPluginFactory::new(
        "runtime-batch-tool",
        lash_core::facade_support::PluginSpec::new()
            .with_orchestrating_tool(runtime_batch_orchestrating_tool()),
    ))
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
    context: &lash_core::facade_support::OrchestrationContext<'_>,
    args: &serde_json::Value,
) -> lash_core::ToolOutcome {
    let Some(raw_calls) = args.get("tool_calls").and_then(serde_json::Value::as_array) else {
        return lash_core::ToolOutcome::err_fmt("Missing required parameter: tool_calls");
    };
    let mut invocations = Vec::with_capacity(raw_calls.len());
    let mut immediate_results = Vec::new();
    for (index, item) in raw_calls.iter().enumerate() {
        let Some(tool_name) = item.get("tool").and_then(serde_json::Value::as_str) else {
            return lash_core::ToolOutcome::err_fmt(format!("Invalid tool_calls[{index}].tool"));
        };
        let Some(manifest) = context.callable_tool_manifest(tool_name) else {
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
            lash_core::facade_support::ToolInvocation::new(
                format!("runtime-batch:{index}"),
                manifest.id,
                item.get("parameters")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
            ),
        ));
    }

    let replies = context
        .call_tool_batch(
            invocations
                .iter()
                .map(|(_, invocation)| invocation.clone())
                .collect(),
        )
        .await;
    let mut results = invocations
        .into_iter()
        .zip(replies)
        .map(|((index, invocation), reply)| {
            let tool = reply
                .record
                .as_ref()
                .map(|record| record.tool.clone())
                .unwrap_or_else(|| invocation.tool_id.to_string());
            serde_json::json!({
                "index": index,
                "tool": tool,
                "success": reply.output.is_success(),
                "value": reply.output.value_for_projection(),
            })
        })
        .collect::<Vec<_>>();
    results.extend(immediate_results);
    lash_core::ToolOutcome::ok(serde_json::json!({ "results": results }))
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .effect_host(Arc::new(lash_core::facade_support::InlineEffectHost::new(
            effect_controller,
        )))
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
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
    let core = LashCore::standard_builder(crate::TurnBudget::Unbounded)
        .attachment_store(Arc::new(crate::persistence::FileAttachmentStore::new(
            dir.path().join("attachments"),
        )))
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .process_env_store(Arc::new(DurableInMemoryProcessEnvStore::default()))
        .effect_host(Arc::new(DurableNoopEffectHost))
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .effect_host(Arc::new(lash_core::facade_support::InlineEffectHost::new(
            effect_controller,
        )))
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
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
                    .lock_recover()
                    .push(request.messages.clone());
                Ok(text_response("echo: queued work"))
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .disable_queued_work_driver()
        .build(crate::testing::runtime_lease_owner())?;
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
        let requests = requests.lock_recover();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            serde_json::to_string(&requests[0][1..])
                .expect("serialize queued-next request user messages"),
            r#"[{"role":"User","blocks":[{"Text":{"text":"queued work","response_meta":null,"cache_breakpoint":false}}]}]"#
        );
    }
    assert!(session.queued_turn().run().await?.ran().is_none());
    Ok(())
}

/// FIG-1575: an exhausted queue and an unreachable one are opposite answers.
/// Only the exhausted queue is terminal, so the drain names which one it hit.
#[tokio::test]
async fn an_exhausted_queue_reports_an_empty_claim_refusal() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(
            crate::testing::TestProvider::builder()
                .kind("empty-drain-reason")
                .complete(|_| async { Ok(text_response("echo")) })
                .build()
                .into_handle(),
        )
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .disable_queued_work_driver()
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("empty-drain-reason").open().await?;

    let drain = session.queued_turn().run().await?;

    assert!(
        matches!(
            drain,
            crate::QueuedTurnDrain::Empty(crate::EmptyQueuedDrainReason::ClaimRefused(
                crate::QueuedWorkClaimRefusal::Empty
            ))
        ),
        "an exhausted queue must report an empty claim refusal, got {drain:?}"
    );
    Ok(())
}

/// A session with no durable store has no queue at all. Reporting that as a
/// busy lane would invite a host to retry forever.
#[tokio::test]
async fn a_storeless_session_reports_no_durable_queue() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(
            crate::testing::TestProvider::builder()
                .kind("storeless-drain-reason")
                .complete(|_| async { Ok(text_response("echo")) })
                .build()
                .into_handle(),
        )
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("storeless-drain-reason").open().await?;

    let drain = session.queued_turn().run().await?;

    assert!(
        matches!(
            drain,
            crate::QueuedTurnDrain::Empty(crate::EmptyQueuedDrainReason::NoDurableQueue)
        ),
        "a storeless session must report no durable queue, got {drain:?}"
    );
    Ok(())
}

/// An automatic drain names why it ran no turn, and a row that can never fit is
/// not such a reason: it is a terminal fault. Before FIG-1575 this path reached
/// a selected-drain refusal on a drain that selected nothing, and panicked.
#[tokio::test]
async fn an_oversized_queued_row_fails_an_automatic_drain_by_name() -> Result<()> {
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(
            crate::testing::TestProvider::builder()
                .kind("oversized-queued-row")
                .complete(|_| async { Ok(text_response("echo")) })
                .build()
                .into_handle(),
        )
        .model(crate::tests::harness::model_spec("mock-model", None, 1_024))
        .store_factory(
            Arc::clone(&store_factory) as Arc<dyn crate::persistence::SessionStoreFactory>
        )
        .disable_queued_work_driver()
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("oversized-queued-row").open().await?;
    {
        use crate::persistence::SessionStoreFactory as _;

        let store = store_factory
            .create_store(&crate::persistence::SessionStoreCreateRequest {
                session_id: session.session_id().to_string(),
                relation: crate::persistence::SessionRelation::Root,
                policy: session.policy_snapshot(),
            })
            .await?;
        store
            .enqueue_queued_work(crate::persistence::QueuedWorkBatchDraft::new(
                session.session_id(),
                crate::persistence::DeliveryPolicy::EarliestSafeBoundary,
                vec![crate::persistence::QueuedWorkPayload::agent_frame_task(
                    "oversized-frame",
                    "w".repeat(64 * 1024),
                    None,
                )],
            ))
            .await?;
    }

    let error = session
        .queued_turn()
        .run()
        .await
        .expect_err("a row larger than the window cannot drain automatically");

    let EmbedError::Runtime(runtime) = &error else {
        panic!("expected a runtime error naming the oversized row, got {error:?}");
    };
    assert_eq!(
        runtime.code,
        lash_core::RuntimeErrorCode::QueuedWorkRowExceedsContextWindow,
        "the oversized row must be named, not panicked on: {error:?}"
    );
    Ok(())
}

/// The wedge FIG-1575 exists to prevent: a drain that could not take the lane
/// consumed nothing, and must never read as an exhausted queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_busy_execution_lane_is_never_reported_as_an_exhausted_queue() -> Result<()> {
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let provider = hang_on_signal_provider(Arc::new(StdMutex::new(vec![started_tx])));
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory)
        .disable_queued_work_driver()
        .build(crate::testing::runtime_lease_owner())?;
    let holder = core.session("busy-lane-drain-reason").open().await?;
    holder
        .enqueue(TurnInput::text("hang queued"))
        .send()
        .await?;
    let drainer = holder.clone();
    let drain = tokio::spawn(async move { drainer.queued_turn().run().await });
    started_rx.await.expect("queued drain reached the provider");

    // A second handle over the same durable session cannot take the lane the
    // hung drain still holds.
    let peer = core.session("busy-lane-drain-reason").open().await?;
    let peer_drain = peer.queued_turn().run().await?;

    assert!(
        matches!(
            peer_drain,
            crate::QueuedTurnDrain::Empty(crate::EmptyQueuedDrainReason::ExecutionLaneBusy)
        ),
        "a busy execution lane must never be reported as an exhausted queue, got {peer_drain:?}"
    );
    assert_eq!(holder.cancel_running_turns(), 1);
    drain.await.expect("drain task")?;
    Ok(())
}

#[tokio::test]
async fn selected_queued_turn_refuses_partial_key_break_without_settling_rows() -> Result<()> {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let observed_provider_calls = Arc::clone(&provider_calls);
    let provider = crate::testing::TestProvider::builder()
        .kind("selected-queued-turn-refusal")
        .complete(move |_request| {
            let observed_provider_calls = Arc::clone(&observed_provider_calls);
            async move {
                observed_provider_calls.fetch_add(1, Ordering::SeqCst);
                Ok(text_response("selected queued turn must not execute"))
            }
        })
        .build()
        .into_handle();
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .disable_queued_work_driver()
        .build(crate::testing::runtime_lease_owner())?;
    let session_id = "selected-queued-turn-key-break-refusal";
    let session = core.session(session_id).open().await?;
    let store = store_factory
        .raw_store_for_testing(session_id)
        .expect("opened session retains its in-memory store");
    let enqueue = |source_key: &'static str, merge_key: &'static str| {
        let store = Arc::clone(&store);
        async move {
            store
                .enqueue_queued_work(
                    crate::persistence::QueuedWorkBatchDraft::new(
                        session_id,
                        lash_core::DeliveryPolicy::EarliestSafeBoundary,
                        vec![crate::persistence::QueuedWorkPayload::agent_frame_task(
                            "selected-refusal-frame",
                            source_key,
                            None,
                        )],
                    )
                    .with_source_key(source_key)
                    .with_merge_key(merge_key),
                )
                .await
                .expect("enqueue selected-refusal row")
        }
    };
    let a1 = enqueue("selected-a1", "a").await;
    let _b1 = enqueue("selected-b1", "b").await;
    let a2 = enqueue("selected-a2", "a").await;

    let error = session
        .queued_turn()
        .batch_ids([a1.batch_id.clone(), a2.batch_id.clone()])
        .run()
        .await
        .expect_err("A1,B1,A2 cannot satisfy selected [A1,A2] atomically");
    match error {
        EmbedError::SelectedQueuedWorkDrainRefused {
            cause:
                SelectedQueuedWorkDrainRefusalCause::UnclaimableTogether {
                    unclaimed_batch_ids,
                },
        } => assert_eq!(unclaimed_batch_ids, vec![a2.batch_id]),
        other => panic!("expected typed selected-drain refusal, got {other:?}"),
    }
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        session
            .queued_work()
            .await?
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![
            (Some("selected-a1"), 1),
            (Some("selected-b1"), 2),
            (Some("selected-a2"), 3),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn selected_queued_turn_redrives_an_interrupted_composition_exactly_or_not_at_all()
-> Result<()> {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let observed_provider_calls = Arc::clone(&provider_calls);
    let provider = crate::testing::TestProvider::builder()
        .kind("selected-interrupted-composition")
        .complete(move |_request| {
            let observed_provider_calls = Arc::clone(&observed_provider_calls);
            async move {
                observed_provider_calls.fetch_add(1, Ordering::SeqCst);
                Ok(text_response("redrove interrupted composition"))
            }
        })
        .build()
        .into_handle();
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .disable_queued_work_driver()
        .build(crate::testing::runtime_lease_owner())?;
    let session_id = "selected-interrupted-composition";
    let session = core.session(session_id).open().await?;
    let store = store_factory
        .raw_store_for_testing(session_id)
        .expect("opened session retains its in-memory store");
    for source_key in ["interrupted-w1", "interrupted-w2"] {
        store
            .enqueue_queued_work(
                crate::persistence::QueuedWorkBatchDraft::new(
                    session_id,
                    lash_core::DeliveryPolicy::EarliestSafeBoundary,
                    vec![crate::persistence::QueuedWorkPayload::agent_frame_task(
                        "interrupted-frame",
                        source_key,
                        None,
                    )],
                )
                .with_source_key(source_key)
                .with_merge_key("interrupted-key"),
            )
            .await
            .expect("enqueue interrupted composition row");
    }
    let owner_a = lash_core::LeaseOwnerIdentity::opaque(
        "selected-interrupted-owner-a",
        "selected-interrupted-owner-a:incarnation",
    );
    let lease_a = store
        .try_claim_session_execution_lease(session_id, &owner_a, "owner-a-executor", 60_000)
        .await
        .expect("claim predecessor session execution lease")
        .acquired()
        .expect("predecessor session execution lane is free");
    let claim_a = store
        .claim_ready_queued_work(
            session_id,
            &lease_a.fence(),
            &owner_a,
            crate::persistence::QueuedWorkClaimBoundary::Idle,
            lash_core::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim predecessor composition")
        .claim()
        .expect("predecessor composition exists");
    assert_eq!(
        claim_a
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec!["recording-qwb-1", "recording-qwb-2"]
    );
    assert_eq!(claim_a.claim_id, "recording-qwc:1:1");
    store
        .release_session_execution_lease(&lease_a.completion())
        .await
        .expect("release predecessor session execution lease");

    let error = session
        .queued_turn()
        .batch_ids(["recording-qwb-1"])
        .run()
        .await
        .expect_err("a selected drain cannot split an interrupted composition");
    match error {
        EmbedError::SelectedQueuedWorkDrainRefused { cause } => assert_eq!(
            cause,
            SelectedQueuedWorkDrainRefusalCause::InterruptedBatchRequiresFullComposition {
                required_batch_ids: vec![
                    "recording-qwb-1".to_string(),
                    "recording-qwb-2".to_string(),
                ],
            }
        ),
        other => panic!("expected interrupted-composition refusal, got {other:?}"),
    }
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        store
            .raw_queued_work_for_testing()
            .into_iter()
            .map(|(batch, claim_id, _, _, _, _)| (batch.batch_id, claim_id))
            .collect::<Vec<_>>(),
        vec![
            (
                "recording-qwb-1".to_string(),
                Some("recording-qwc:1:1".to_string()),
            ),
            (
                "recording-qwb-2".to_string(),
                Some("recording-qwc:1:1".to_string()),
            ),
        ]
    );

    let output = session
        .queued_turn()
        .batch_ids(["recording-qwb-1", "recording-qwb-2"])
        .run()
        .await?
        .expect("the complete interrupted composition executes");
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        output
            .activities
            .iter()
            .find_map(|activity| match &activity.event {
                TurnEvent::QueuedWorkStarted { batch_ids, .. } => Some(batch_ids.clone()),
                _ => None,
            }),
        Some(vec![
            "recording-qwb-1".to_string(),
            "recording-qwb-2".to_string(),
        ])
    );
    assert!(store.raw_queued_work_for_testing().is_empty());
    Ok(())
}

#[tokio::test]
async fn selected_queued_turn_reports_claimed_now_and_already_satisfied_ids() -> Result<()> {
    let provider = crate::testing::TestProvider::builder()
        .kind("selected-idempotent-outcome")
        .complete(|_| async { Ok(text_response("selected outcome")) })
        .build()
        .into_handle();
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .disable_queued_work_driver()
        .build(crate::testing::runtime_lease_owner())?;
    let session_id = "selected-idempotent-outcome";
    let session = core.session(session_id).open().await?;
    let store = store_factory
        .raw_store_for_testing(session_id)
        .expect("opened session retains its in-memory store");
    let batch = store
        .enqueue_queued_work(
            crate::persistence::QueuedWorkBatchDraft::new(
                session_id,
                lash_core::DeliveryPolicy::EarliestSafeBoundary,
                vec![crate::persistence::QueuedWorkPayload::agent_frame_task(
                    "selected-outcome-frame",
                    "selected-outcome-task",
                    None,
                )],
            )
            .with_source_key("selected-outcome-source"),
        )
        .await
        .expect("enqueue selected outcome row");

    let claimed = session
        .queued_turn()
        .batch_ids([batch.batch_id.clone()])
        .run()
        .await?;
    assert!(claimed.turn.is_some());
    assert_eq!(
        claimed.satisfied,
        vec![crate::SelectedQueuedWorkBatchSatisfaction::ClaimedNow {
            batch_id: batch.batch_id.clone(),
        }]
    );

    let replay = session
        .queued_turn()
        .batch_ids([batch.batch_id.clone()])
        .run()
        .await?;
    assert!(replay.turn.is_none());
    assert_eq!(
        replay.satisfied,
        vec![
            crate::SelectedQueuedWorkBatchSatisfaction::AlreadySatisfied {
                batch_id: batch.batch_id,
            },
        ]
    );
    Ok(())
}

#[tokio::test]
async fn selected_queued_turn_deduplicates_absent_ids_with_free_or_busy_lane() -> Result<()> {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let observed_provider_calls = Arc::clone(&provider_calls);
    let provider = crate::testing::TestProvider::builder()
        .kind("selected-duplicate-absent")
        .complete(move |_| {
            let observed_provider_calls = Arc::clone(&observed_provider_calls);
            async move {
                observed_provider_calls.fetch_add(1, Ordering::SeqCst);
                Ok(text_response("absent selection must not execute"))
            }
        })
        .build()
        .into_handle();
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .disable_queued_work_driver()
        .build(crate::testing::runtime_lease_owner())?;
    let session_id = "selected-duplicate-absent";
    let session = core.session(session_id).open().await?;
    let store = store_factory
        .raw_store_for_testing(session_id)
        .expect("opened session retains its in-memory store");

    let expected = vec![
        crate::SelectedQueuedWorkBatchSatisfaction::AlreadySatisfied {
            batch_id: "absent-batch".to_string(),
        },
    ];
    let lane_free = session
        .queued_turn()
        .batch_ids(["absent-batch", "absent-batch"])
        .run()
        .await?;
    assert!(lane_free.turn.is_none());
    assert_eq!(lane_free.satisfied, expected);

    let held_owner = lash_core::LeaseOwnerIdentity::opaque(
        "selected-duplicate-absent-holder",
        "selected-duplicate-absent-holder:incarnation",
    );
    let held_lease = store
        .try_claim_session_execution_lease(session_id, &held_owner, "held-executor", 60_000)
        .await
        .expect("claim held session execution lease")
        .acquired()
        .expect("session execution lane is initially free");
    let lane_busy = session
        .queued_turn()
        .batch_ids(["absent-batch", "absent-batch"])
        .run()
        .await?;
    assert!(lane_busy.turn.is_none());
    assert_eq!(lane_busy.satisfied, expected);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    store
        .release_session_execution_lease(&held_lease.completion())
        .await
        .expect("release held session execution lease");
    Ok(())
}

#[tokio::test]
async fn selected_queued_turn_deduplicates_present_claimable_id() -> Result<()> {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let observed_provider_calls = Arc::clone(&provider_calls);
    let provider = crate::testing::TestProvider::builder()
        .kind("selected-duplicate-present")
        .complete(move |_| {
            let observed_provider_calls = Arc::clone(&observed_provider_calls);
            async move {
                observed_provider_calls.fetch_add(1, Ordering::SeqCst);
                Ok(text_response("selected duplicate present outcome"))
            }
        })
        .build()
        .into_handle();
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .disable_queued_work_driver()
        .build(crate::testing::runtime_lease_owner())?;
    let session_id = "selected-duplicate-present";
    let session = core.session(session_id).open().await?;
    let store = store_factory
        .raw_store_for_testing(session_id)
        .expect("opened session retains its in-memory store");
    let batch = store
        .enqueue_queued_work(
            crate::persistence::QueuedWorkBatchDraft::new(
                session_id,
                lash_core::DeliveryPolicy::EarliestSafeBoundary,
                vec![crate::persistence::QueuedWorkPayload::agent_frame_task(
                    "selected-duplicate-present-frame",
                    "selected-duplicate-present-task",
                    None,
                )],
            )
            .with_source_key("selected-duplicate-present-source"),
        )
        .await
        .expect("enqueue duplicate-selected row");

    let outcome = session
        .queued_turn()
        .batch_ids([batch.batch_id.clone(), batch.batch_id.clone()])
        .run()
        .await?;
    assert!(outcome.turn.is_some());
    assert_eq!(
        outcome.satisfied,
        vec![crate::SelectedQueuedWorkBatchSatisfaction::ClaimedNow {
            batch_id: batch.batch_id,
        }]
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn selected_queued_turn_empty_selection_is_satisfied_noop() -> Result<()> {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let observed_provider_calls = Arc::clone(&provider_calls);
    let provider = crate::testing::TestProvider::builder()
        .kind("selected-empty-noop")
        .complete(move |_| {
            let observed_provider_calls = Arc::clone(&observed_provider_calls);
            async move {
                observed_provider_calls.fetch_add(1, Ordering::SeqCst);
                Ok(text_response("empty selection must not execute"))
            }
        })
        .build()
        .into_handle();
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory)
        .disable_queued_work_driver()
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("selected-empty-noop").open().await?;
    session
        .enqueue(TurnInput::text("must remain queued"))
        .id("selected-empty-noop-input")
        .send()
        .await?;

    let outcome = session
        .queued_turn()
        .batch_ids(std::iter::empty::<String>())
        .run()
        .await?;
    assert!(outcome.turn.is_none());
    assert_eq!(outcome.satisfied, Vec::new());
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    assert!(
        session.queued_turn().run().await?.ran().is_some(),
        "the empty selection must leave unrestricted queued input pending"
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn selected_queued_turn_validates_every_interrupted_composition_before_mutating() -> Result<()>
{
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let observed_provider_calls = Arc::clone(&provider_calls);
    let provider = crate::testing::TestProvider::builder()
        .kind("selected-two-interrupted-compositions")
        .complete(move |_request| {
            let observed_provider_calls = Arc::clone(&observed_provider_calls);
            async move {
                observed_provider_calls.fetch_add(1, Ordering::SeqCst);
                Ok(text_response("refused selections must not execute"))
            }
        })
        .build()
        .into_handle();
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .disable_queued_work_driver()
        .build(crate::testing::runtime_lease_owner())?;
    let session_id = "selected-two-interrupted-compositions";
    let session = core.session(session_id).open().await?;
    let store = store_factory
        .raw_store_for_testing(session_id)
        .expect("opened session retains its in-memory store");
    for source_key in ["claim-a1", "claim-a2", "claim-b1", "claim-b2"] {
        store
            .enqueue_queued_work(
                crate::persistence::QueuedWorkBatchDraft::new(
                    session_id,
                    lash_core::DeliveryPolicy::EarliestSafeBoundary,
                    vec![crate::persistence::QueuedWorkPayload::agent_frame_task(
                        "two-claims-frame",
                        source_key,
                        None,
                    )],
                )
                .with_source_key(source_key)
                .with_merge_key("two-claims-key"),
            )
            .await
            .expect("enqueue two-claim row");
    }
    let predecessor_owner = lash_core::LeaseOwnerIdentity::opaque(
        "selected-two-claims-predecessor",
        "selected-two-claims-predecessor:incarnation",
    );
    let predecessor_lease = store
        .try_claim_session_execution_lease(
            session_id,
            &predecessor_owner,
            "predecessor-executor",
            60_000,
        )
        .await
        .expect("claim predecessor session execution lease")
        .acquired()
        .expect("predecessor session execution lane is free");
    let claim_a = store
        .claim_ready_queued_work(
            session_id,
            &predecessor_lease.fence(),
            &predecessor_owner,
            crate::persistence::QueuedWorkClaimBoundary::Idle,
            lash_core::testing::queued_work_claim_policy(2),
        )
        .await
        .expect("claim predecessor A")
        .claim()
        .expect("predecessor A exists");
    let claim_b = store
        .claim_ready_queued_work(
            session_id,
            &predecessor_lease.fence(),
            &predecessor_owner,
            crate::persistence::QueuedWorkClaimBoundary::Idle,
            lash_core::testing::queued_work_claim_policy(2),
        )
        .await
        .expect("claim predecessor B")
        .claim()
        .expect("predecessor B exists");
    assert_eq!(
        claim_a
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec!["recording-qwb-1", "recording-qwb-2"]
    );
    assert_eq!(claim_a.claim_id, "recording-qwc:1:1");
    assert_eq!(
        claim_b
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec!["recording-qwb-3", "recording-qwb-4"]
    );
    assert_eq!(claim_b.claim_id, "recording-qwc:3:1");
    store
        .release_session_execution_lease(&predecessor_lease.completion())
        .await
        .expect("release predecessor session execution lease");

    let partial_error = session
        .queued_turn()
        .batch_ids(["recording-qwb-1", "recording-qwb-2", "recording-qwb-3"])
        .run()
        .await
        .expect_err("full A plus partial B must refuse before reclaiming A");
    match partial_error {
        EmbedError::SelectedQueuedWorkDrainRefused { cause } => assert_eq!(
            cause,
            SelectedQueuedWorkDrainRefusalCause::InterruptedBatchRequiresFullComposition {
                required_batch_ids: vec![
                    "recording-qwb-3".to_string(),
                    "recording-qwb-4".to_string(),
                ],
            }
        ),
        other => panic!("expected incomplete-B refusal, got {other:?}"),
    }
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        store
            .raw_queued_work_for_testing()
            .into_iter()
            .map(|(batch, claim_id, _, _, _, _)| (batch.batch_id, claim_id))
            .collect::<Vec<_>>(),
        vec![
            (
                "recording-qwb-1".to_string(),
                Some("recording-qwc:1:1".to_string()),
            ),
            (
                "recording-qwb-2".to_string(),
                Some("recording-qwc:1:1".to_string()),
            ),
            (
                "recording-qwb-3".to_string(),
                Some("recording-qwc:3:1".to_string()),
            ),
            (
                "recording-qwb-4".to_string(),
                Some("recording-qwc:3:1".to_string()),
            ),
        ]
    );

    let complete_error = session
        .queued_turn()
        .batch_ids([
            "recording-qwb-1",
            "recording-qwb-2",
            "recording-qwb-3",
            "recording-qwb-4",
        ])
        .run()
        .await
        .expect_err("one selected drain claims exactly the earliest interrupted composition");
    match complete_error {
        EmbedError::SelectedQueuedWorkDrainRefused { cause } => assert_eq!(
            cause,
            SelectedQueuedWorkDrainRefusalCause::UnclaimableTogether {
                unclaimed_batch_ids: vec![
                    "recording-qwb-3".to_string(),
                    "recording-qwb-4".to_string(),
                ],
            }
        ),
        other => panic!("expected second-composition refusal, got {other:?}"),
    }
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        store
            .raw_queued_work_for_testing()
            .into_iter()
            .map(|(batch, claim_id, _, _, _, _)| (batch.batch_id, claim_id))
            .collect::<Vec<_>>(),
        vec![
            (
                "recording-qwb-1".to_string(),
                Some("recording-qwc:1:1".to_string()),
            ),
            (
                "recording-qwb-2".to_string(),
                Some("recording-qwc:1:1".to_string()),
            ),
            (
                "recording-qwb-3".to_string(),
                Some("recording-qwc:3:1".to_string()),
            ),
            (
                "recording-qwb-4".to_string(),
                Some("recording-qwc:3:1".to_string()),
            ),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn selected_queued_turn_redrive_ignores_successor_max_rows() -> Result<()> {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let observed_provider_calls = Arc::clone(&provider_calls);
    let provider = crate::testing::TestProvider::builder()
        .kind("selected-redrive-over-row-limit")
        .complete(move |_request| {
            let observed_provider_calls = Arc::clone(&observed_provider_calls);
            async move {
                observed_provider_calls.fetch_add(1, Ordering::SeqCst);
                Ok(text_response("redrove over successor row limit"))
            }
        })
        .build()
        .into_handle();
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(2))
        .disable_queued_work_driver()
        .build(crate::testing::runtime_lease_owner())?;
    let session_id = "selected-redrive-over-row-limit";
    let session = core.session(session_id).open().await?;
    let store = store_factory
        .raw_store_for_testing(session_id)
        .expect("opened session retains its in-memory store");
    for source_key in [
        "selected-limit-w1",
        "selected-limit-w2",
        "selected-limit-w3",
    ] {
        store
            .enqueue_queued_work(
                crate::persistence::QueuedWorkBatchDraft::new(
                    session_id,
                    lash_core::DeliveryPolicy::EarliestSafeBoundary,
                    vec![crate::persistence::QueuedWorkPayload::agent_frame_task(
                        "selected-limit-frame",
                        source_key,
                        None,
                    )],
                )
                .with_source_key(source_key)
                .with_merge_key("selected-limit-key"),
            )
            .await
            .expect("enqueue selected row-limit row");
    }
    let predecessor_owner = lash_core::LeaseOwnerIdentity::opaque(
        "selected-limit-predecessor",
        "selected-limit-predecessor:incarnation",
    );
    let predecessor_lease = store
        .try_claim_session_execution_lease(
            session_id,
            &predecessor_owner,
            "predecessor-executor",
            60_000,
        )
        .await
        .expect("claim selected row-limit predecessor lease")
        .acquired()
        .expect("selected row-limit predecessor lane is free");
    let predecessor_claim = store
        .claim_ready_queued_work(
            session_id,
            &predecessor_lease.fence(),
            &predecessor_owner,
            crate::persistence::QueuedWorkClaimBoundary::Idle,
            lash_core::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim selected row-limit predecessor")
        .claim()
        .expect("selected row-limit predecessor exists");
    assert_eq!(
        predecessor_claim
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec!["recording-qwb-1", "recording-qwb-2", "recording-qwb-3"]
    );
    store
        .release_session_execution_lease(&predecessor_lease.completion())
        .await
        .expect("release selected row-limit predecessor lease");

    let output = session
        .queued_turn()
        .batch_ids(["recording-qwb-1", "recording-qwb-2", "recording-qwb-3"])
        .run()
        .await?
        .expect("selected predecessor composition ignores successor max_rows=2");
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        output
            .activities
            .iter()
            .find_map(|activity| match &activity.event {
                TurnEvent::QueuedWorkStarted { batch_ids, .. } => Some(batch_ids.clone()),
                _ => None,
            }),
        Some(vec![
            "recording-qwb-1".to_string(),
            "recording-qwb-2".to_string(),
            "recording-qwb-3".to_string(),
        ])
    );
    Ok(())
}

#[tokio::test]
async fn selected_queued_turn_reports_execution_lane_contention() -> Result<()> {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let observed_provider_calls = Arc::clone(&provider_calls);
    let provider = crate::testing::TestProvider::builder()
        .kind("selected-execution-lane-busy")
        .complete(move |_request| {
            let observed_provider_calls = Arc::clone(&observed_provider_calls);
            async move {
                observed_provider_calls.fetch_add(1, Ordering::SeqCst);
                Ok(text_response("busy selection must not execute"))
            }
        })
        .build()
        .into_handle();
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .disable_queued_work_driver()
        .build(crate::testing::runtime_lease_owner())?;
    let session_id = "selected-execution-lane-busy";
    let session = core.session(session_id).open().await?;
    let store = store_factory
        .raw_store_for_testing(session_id)
        .expect("opened session retains its in-memory store");
    store
        .enqueue_queued_work(
            crate::persistence::QueuedWorkBatchDraft::new(
                session_id,
                lash_core::DeliveryPolicy::EarliestSafeBoundary,
                vec![crate::persistence::QueuedWorkPayload::agent_frame_task(
                    "busy-frame",
                    "busy-w1",
                    None,
                )],
            )
            .with_source_key("busy-w1"),
        )
        .await
        .expect("enqueue busy selected row");
    let held_owner = lash_core::LeaseOwnerIdentity::opaque(
        "selected-busy-holder",
        "selected-busy-holder:incarnation",
    );
    let held_lease = store
        .try_claim_session_execution_lease(session_id, &held_owner, "held-executor", 60_000)
        .await
        .expect("claim held session execution lease")
        .acquired()
        .expect("session execution lane is initially free");

    let error = session
        .queued_turn()
        .batch_ids(["recording-qwb-1"])
        .run()
        .await
        .expect_err("selected drain under a held lease is typed contention");
    match error {
        EmbedError::SelectedQueuedWorkDrainRefused { cause } => assert_eq!(
            cause,
            SelectedQueuedWorkDrainRefusalCause::ExecutionLaneBusy
        ),
        other => panic!("expected execution-lane-busy refusal, got {other:?}"),
    }
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        session
            .queued_work()
            .await?
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec!["recording-qwb-1"]
    );
    store
        .release_session_execution_lease(&held_lease.completion())
        .await
        .expect("release held session execution lease");
    Ok(())
}

#[tokio::test]
async fn idle_queued_input_emits_typed_remote_application_and_durable_identity() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .disable_queued_work_driver()
        .build(crate::testing::runtime_lease_owner())?;
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .live_replay_store(Arc::new(
            lash_core::facade_support::InMemoryLiveReplayStore::new(
                lash_core::facade_support::InMemoryLiveReplayStoreConfig {
                    max_events_per_session: 1,
                    ..lash_core::facade_support::InMemoryLiveReplayStoreConfig::default()
                },
            ),
        ))
        .disable_queued_work_driver()
        .build(crate::testing::runtime_lease_owner())?;
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
                && application.turn_id.as_str() == "gap-application-turn"
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .disable_queued_work_driver()
        .build(crate::testing::runtime_lease_owner())?;
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
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { .. })
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
        .options(lash_core::facade_support::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(3)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..lash_core::facade_support::ProviderOptions::default()
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

#[cfg(feature = "rlm")]
fn output_then_failing_rlm_prose_provider(
    transport_calls: Arc<AtomicUsize>,
    requests: Arc<StdMutex<Vec<lash_core::LlmRequest>>>,
) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("retrying-rlm-prose")
        .requires_streaming(true)
        .options(lash_core::facade_support::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..lash_core::facade_support::ProviderOptions::default()
        })
        .complete(move |request| {
            let transport_calls = Arc::clone(&transport_calls);
            let requests = Arc::clone(&requests);
            async move {
                requests
                    .lock_recover()
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
                            .retryable(true)
                            .with_output_started(true),
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

#[cfg(feature = "rlm")]
fn natural_prose_reasoning_provider(
    requests: Arc<StdMutex<Vec<lash_core::LlmRequest>>>,
) -> ProviderHandle {
    let calls = Arc::new(AtomicUsize::new(0));
    crate::testing::TestProvider::builder()
        .kind("natural-rlm-prose-reasoning")
        .complete(move |request| {
            let requests = Arc::clone(&requests);
            let calls = Arc::clone(&calls);
            async move {
                requests.lock_recover().push(request);
                let call = calls.fetch_add(1, Ordering::SeqCst);
                let text = match call {
                    0 => "natural completion single-copy marker",
                    1 => "subsequent natural answer",
                    other => panic!("unexpected natural RLM provider request {other}"),
                };
                let mut parts = Vec::new();
                if call == 0 {
                    parts.push(LlmOutputPart::Reasoning {
                        text: "reasoning retained for replay".to_string(),
                        replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                            item_id: Some("natural-reasoning".to_string()),
                            encrypted_content: Some("opaque-natural-replay".to_string()),
                            signature: None,
                            redacted: false,
                            summary: Vec::new(),
                            ..Default::default()
                        }),
                    });
                }
                parts.push(LlmOutputPart::Text {
                    text: text.to_string(),
                    response_meta: None,
                });
                Ok(LlmResponse {
                    full_text: text.to_string(),
                    parts,
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

#[cfg(feature = "rlm")]
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

#[cfg(feature = "rlm")]
#[test]
fn rlm_provider_failure_after_prose_is_not_retried_or_committed() -> Result<()> {
    run_async_test_on_stack_budget("rlm-provider-output-failure-test", || async {
        const MARKER: &str = "retry observer single-copy marker";
        let transport_calls = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let core = explicit_ephemeral_facets(LashCore::rlm_builder(
            crate::TurnBudget::Unbounded,
            rlm_factory(),
        ))
        .provider(output_then_failing_rlm_prose_provider(
            Arc::clone(&transport_calls),
            Arc::clone(&requests),
        ))
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build(crate::testing::runtime_lease_owner())?;
        let session = core.session("rlm-provider-retry-prose").open().await?;

        let first = session
            .turn(TurnInput::text("trigger deterministic rate limit retry"))
            .run()
            .await?;

        assert_eq!(
            transport_calls.load(Ordering::SeqCst),
            1,
            "provider output must not be re-bought after the failed attempt"
        );
        assert!(matches!(
            first.result.outcome,
            TurnOutcome::Stopped(lash_core::facade_support::TurnStop::ProviderError)
        ));
        assert!(first.result.assistant_output.safe_text.is_empty());
        assert!(first.result.assistant_output.raw_text.is_empty());
        assert!(first.activities.iter().any(|activity| matches!(
            &activity.event,
            TurnEvent::AssistantProseDelta { text } if text.contains(MARKER)
        )));
        assert!(
            first
                .activities
                .iter()
                .all(|activity| !matches!(activity.event, TurnEvent::ModelAttemptReset { .. }))
        );
        let rlm_marker_records = first
            .result
            .state
            .read_view()
            .active_events()
            .iter()
            .filter(|record| match record {
                lash_core::SessionHistoryRecord::Conversation(message) => {
                    matches!(
                        message.origin.as_ref(),
                        Some(lash_core::MessageOrigin::Plugin {
                            plugin_id,
                            transient: false,
                        }) if plugin_id == lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID
                    ) && message
                        .parts
                        .iter()
                        .any(|part| part.content.contains(MARKER))
                }
                lash_core::SessionHistoryRecord::Protocol(event) => matches!(
                    lash_protocol_rlm::decode_rlm_protocol_event(event),
                    Some(lash_rlm_types::RlmProtocolEvent::RlmAssistantContent(content))
                        if content.prose.contains(MARKER)
                ),
            })
            .count();
        assert_eq!(
            rlm_marker_records, 0,
            "failed-attempt prose is preview output, not committed RLM history"
        );
        {
            let requests = requests.lock_recover();
            assert_eq!(requests.len(), 1, "RLM scheduled a spurious iteration");
        }
        assert_eq!(first.result.llm_calls.len(), 1);
        assert_eq!(first.result.llm_calls[0].attempts.len(), 1);
        let attempt = &first.result.llm_calls[0].attempts[0];
        assert_eq!(
            attempt.protocol_position,
            lash_core::ProtocolPosition::OutputStarted
        );
        assert_eq!(
            attempt
                .retry_decision
                .as_ref()
                .map(|decision| decision.scheduled),
            Some(false)
        );
        assert_eq!(
            attempt
                .retry_decision
                .as_ref()
                .and_then(|decision| decision.reason.as_deref()),
            Some("output_started_without_retry_guarantee")
        );
        let issue = first.result.errors.first().expect("typed provider issue");
        assert_eq!(
            issue.code.as_deref(),
            Some("unsafe_retry_after_output_started")
        );
        assert_eq!(issue.retryable, Some(false));

        let persisted = session.admin().state().persist_current().await?;
        session.close().await?;

        let reopened = core.session("rlm-provider-retry-prose").open().await?;
        reopened.admin().state().set_persisted(persisted).await?;
        assert_eq!(
            reopened
                .read_view()
                .active_events()
                .iter()
                .filter(|record| match record {
                    lash_core::SessionHistoryRecord::Conversation(message) => message
                        .parts
                        .iter()
                        .any(|part| part.content.contains(MARKER)),
                    lash_core::SessionHistoryRecord::Protocol(event) => matches!(
                        lash_protocol_rlm::decode_rlm_protocol_event(event),
                        Some(lash_rlm_types::RlmProtocolEvent::RlmAssistantContent(content))
                            if content.prose.contains(MARKER)
                    ),
                })
                .count(),
            0,
            "reloaded history retained failed-attempt prose"
        );
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
fn rlm_natural_prose_completion_is_single_copy_in_next_request() -> Result<()> {
    run_async_test_on_stack_budget("rlm-natural-prose-single-copy-test", || async {
        const MARKER: &str = "natural completion single-copy marker";
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let core = explicit_ephemeral_facets(LashCore::rlm_builder(
            lash_core::TurnBudget::Unbounded,
            rlm_factory(),
        ))
        .provider(natural_prose_reasoning_provider(Arc::clone(&requests)))
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build(crate::testing::runtime_lease_owner())?;
        let session = core.session("rlm-natural-prose-single-copy").open().await?;

        let first = session
            .turn(TurnInput::text("answer naturally"))
            .run()
            .await?;
        assert_eq!(first.assistant_message(), Some(MARKER));
        session
            .admin()
            .state()
            .append_messages(vec![
                lash_core::PluginMessage::text(lash_core::MessageRole::Assistant, MARKER)
                    .with_id("workbench-assistant:natural-turn"),
            ])
            .await?;

        session
            .turn(TurnInput::text("check natural completion history"))
            .run()
            .await?;

        let requests = requests.lock_recover();
        assert_eq!(requests.len(), 2);
        let next_request = provider_request_text(&requests[1]);
        assert_eq!(
            next_request.matches(MARKER).count(),
            1,
            "provider-visible history duplicated natural prose: {next_request}"
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

    let lash_core::facade_support::SessionResume::Replayed { events } =
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(retrying_visible_stream_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("retry-visible-observation").open().await?;
    let cursor = session.observe().current_observation().cursor;
    let lash_core::facade_support::SessionObservationSubscription::Subscribed(mut subscription) =
        session.observe().subscribe_from_cursor(&cursor)?
    else {
        panic!("fresh cursor should subscribe without a gap");
    };
    let live_collector = tokio::spawn(async move {
        let mut events = Vec::new();
        loop {
            let event = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                futures_util::StreamExt::next(&mut subscription),
            )
            .await
            .expect("timed out waiting for live observation")
            .expect("live observation subscription closed")
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

    let lash_core::facade_support::SessionResume::Replayed {
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
        let event = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            futures_util::StreamExt::next(&mut subscription),
        )
        .await
        .expect("timed out waiting for replayed event")
        .expect("replay subscription closed")
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
        let event = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            futures_util::StreamExt::next(&mut subscription),
        )
        .await
        .expect("timed out waiting for live event")
        .expect("live subscription closed")
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .live_replay_store(Arc::new(
            lash_core::facade_support::InMemoryLiveReplayStore::new(
                lash_core::facade_support::InMemoryLiveReplayStoreConfig {
                    max_events_per_session: 1,
                    ..lash_core::facade_support::InMemoryLiveReplayStoreConfig::default()
                },
            ),
        ))
        .build(crate::testing::runtime_lease_owner())?;
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
async fn capacity_and_age_trim_force_snapshot_with_matching_observation_cursor() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .live_replay_store(Arc::new(
            lash_core::facade_support::InMemoryLiveReplayStore::new(
                lash_core::facade_support::InMemoryLiveReplayStoreConfig {
                    max_events_per_session: 1,
                    ..lash_core::facade_support::InMemoryLiveReplayStoreConfig::default()
                },
            ),
        ))
        .build(crate::testing::runtime_lease_owner())?;
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
async fn trimmed_gap_replacement_cursor_preserves_unseen_auxiliary_event() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .live_replay_store(Arc::new(
            lash_core::facade_support::InMemoryLiveReplayStore::new(
                lash_core::facade_support::InMemoryLiveReplayStoreConfig {
                    max_events_per_session: 1,
                    ..lash_core::facade_support::InMemoryLiveReplayStoreConfig::default()
                },
            ),
        ))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("trimmed-gap-unseen-auxiliary-event")
        .open()
        .await?;
    let stale_cursor = session.observe().current_observation().cursor;

    session
        .turn(TurnInput::text("install replacement projection"))
        .run()
        .await?;
    let installed_projection = session.observe().current_observation();
    session.observe().runtime.record_queue_changed(
        lash_core::SessionQueueEventKind::Enqueued,
        vec!["unseen-batch".to_string()],
    );

    let SessionResume::Gap { gap, .. } = session.observe().resume_from_cursor(&stale_cursor)?
    else {
        panic!("the trimmed cursor must yield a replacement gap");
    };
    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Trimmed);
    assert_eq!(
        gap.latest_cursor, installed_projection.cursor,
        "the replacement cursor must stay before auxiliary events absent from the projection"
    );

    let SessionResume::Replayed { events } =
        session.observe().resume_from_cursor(&gap.latest_cursor)?
    else {
        panic!("the replacement cursor must retain a replayable auxiliary suffix");
    };
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0].payload,
        lash_core::SessionObservationEventPayload::QueueChanged { kind, batch_ids }
            if *kind == lash_core::SessionQueueEventKind::Enqueued
                && batch_ids == &["unseen-batch"]
    ));
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
    inner: lash_core::facade_support::InMemoryLiveReplayStore,
    boundary: PublicationBoundary,
    pause: Arc<PublicationPause>,
}

#[derive(Debug)]
struct PublicationPause {
    boundary_reached: std::sync::atomic::AtomicBool,
    release_boundary: std::sync::atomic::AtomicBool,
    pause_lock: StdMutex<()>,
    pause_changed: std::sync::Condvar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublicationBoundary {
    BeforeReservation,
    AfterReservation,
    AfterInstall,
    BeforeNotification,
}

#[derive(Debug)]
struct NoopTurnPhaseProbe;

impl lash_core::runtime::RuntimeTurnPhaseProbe for NoopTurnPhaseProbe {
    fn begin(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}
}

#[derive(Debug)]
struct FailingAppendReplayStore {
    inner: lash_core::facade_support::InMemoryLiveReplayStore,
}

impl FailingAppendReplayStore {
    fn new() -> Self {
        Self {
            inner: lash_core::facade_support::InMemoryLiveReplayStore::default(),
        }
    }
}

impl lash_core::LiveReplayStore for FailingAppendReplayStore {
    fn prepare_publication(
        &self,
        _session_id: &str,
        _revision: lash_core::SessionRevision,
        _events: Vec<lash_core::LiveReplayEventDraft>,
    ) -> std::result::Result<
        lash_core::PreparedLiveReplayPublication,
        lash_core::LiveReplayStoreError,
    > {
        Err(lash_core::LiveReplayStoreError::Store(
            "injected live-replay append failure".to_string(),
        ))
    }

    fn publish_prepared(
        &self,
        _prepared: lash_core::PreparedLiveReplayPublication,
    ) -> std::result::Result<
        Vec<Arc<lash_core::SessionObservationEvent>>,
        lash_core::LiveReplayStoreError,
    > {
        unreachable!("failed preparations cannot be published")
    }

    fn replay_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplayOutcome, lash_core::LiveReplayStoreError> {
        self.inner.replay_after_cursor(cursor)
    }

    fn subscribe_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplaySubscribeOutcome, lash_core::LiveReplayStoreError>
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

#[tokio::test]
async fn durable_revision_requires_replacement_evidence() -> Result<()> {
    let replay_store = Arc::new(FailingAppendReplayStore::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .live_replay_store(replay_store)
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("failed-commit-observation-reconciliation")
        .open()
        .await?;
    let before = session.observe().current_observation();

    let output = session
        .turn(TurnInput::text("commit despite replay failure"))
        .run()
        .await?;
    assert_eq!(
        output.assistant_message(),
        Some("echo: commit despite replay failure"),
        "the durable turn must still commit"
    );

    let SessionResume::Gap { observation, gap } =
        session.observe().resume_from_cursor(&before.cursor)?
    else {
        panic!("a pre-commit cursor without replacement evidence must not replay cleanly");
    };
    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Unavailable);
    assert_eq!(gap.latest_revision, lash_core::SessionRevision::new(1));
    assert_eq!(gap.requested_cursor, before.cursor);
    assert_eq!(gap.latest_cursor, observation.cursor);
    assert_ne!(
        gap.latest_cursor, before.cursor,
        "the unchanged live position must still carry the new durable revision"
    );

    let SessionObservationSubscription::Gap { observation, gap } =
        session.observe().subscribe_from_cursor(&before.cursor)?
    else {
        panic!("a pre-commit cursor without replacement evidence must not subscribe cleanly");
    };
    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Unavailable);
    assert_eq!(gap.latest_revision, lash_core::SessionRevision::new(1));
    assert_eq!(gap.requested_cursor, before.cursor);
    assert_eq!(gap.latest_cursor, observation.cursor);
    assert_ne!(
        gap.latest_cursor, before.cursor,
        "subscribe must adopt the revision-stamped replacement cursor"
    );
    Ok(())
}

#[tokio::test]
async fn idle_session_reconnect_after_failed_append_yields_gap_without_another_commit() -> Result<()>
{
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .live_replay_store(Arc::new(FailingAppendReplayStore::new()))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("idle-failed-commit-observation-reconciliation")
        .open()
        .await?;
    let cursor = session.observe().current_observation().cursor;

    session
        .turn(TurnInput::text("commit before becoming idle"))
        .run()
        .await?;

    let mut reconnect = session.observe().subscribe_and_recover(cursor);
    let item = tokio::time::timeout(std::time::Duration::from_millis(250), reconnect.next())
        .await
        .expect("an idle reconnect must not wait for a future commit")
        .expect("recovery stream remains open")?;
    assert!(matches!(
        item,
        crate::observe::SessionObservationStreamItem::Gap {
            gap: lash_core::facade_support::LiveReplayGap {
                reason: lash_core::LiveReplayGapReason::Unavailable,
                ..
            },
            ..
        }
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_subscribe_has_only_two_histories() -> Result<()> {
    for boundary in [
        PublicationBoundary::BeforeReservation,
        PublicationBoundary::AfterReservation,
        PublicationBoundary::AfterInstall,
        PublicationBoundary::BeforeNotification,
    ] {
        let replay_store = Arc::new(PausedCommitReplayStore::at(boundary));
        let core =
            explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
                .provider(mock_provider())
                .model(mock_model_spec())
                .live_replay_store(replay_store.clone())
                .build(crate::testing::runtime_lease_owner())?;
        let session_id = format!("two-histories-{boundary:?}");
        let session = core.session(session_id).open().await?;
        let before = session.observe().recoverable_chat_snapshot();
        let turn_session = session.clone();
        let turn = tokio::spawn(async move {
            turn_session
                .turn(TurnInput::text("exactly once across the cut"))
                .run()
                .await
        });

        replay_store.wait_for_commit_append().await;
        let batch_is_visible = match lash_core::LiveReplayStore::replay_after_cursor(
            replay_store.as_ref(),
            &before.cursor,
        )
        .expect("boundary visibility probe must read replay")
        {
            lash_core::LiveReplayOutcome::Replayed(events) => events.iter().any(|event| {
                matches!(
                    event.payload,
                    lash_core::SessionObservationEventPayload::Committed { .. }
                )
            }),
            lash_core::LiveReplayOutcome::Gap(reason) => {
                panic!("{boundary:?}: boundary probe unexpectedly gapped: {reason:?}")
            }
        };
        assert_eq!(
            batch_is_visible,
            boundary == PublicationBoundary::BeforeNotification,
            "{boundary:?}: only the pre-notification cut may expose the batch to replay"
        );
        let snapshot = session.observe().recoverable_chat_snapshot();
        let snapshot_is_new =
            snapshot.read_view.messages().iter().any(|message| {
                crate::message_text(message).contains("exactly once across the cut")
            });
        assert_eq!(
            snapshot_is_new,
            matches!(
                boundary,
                PublicationBoundary::AfterInstall | PublicationBoundary::BeforeNotification
            ),
            "{boundary:?}: projection installation must divide the two allowed histories"
        );
        let mut stream = session
            .observe()
            .subscribe_recoverable_chat(snapshot.cursor);
        replay_store.release_commit_install();
        turn.await.expect("join publishing turn")?;

        if snapshot_is_new {
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(50), stream.next())
                    .await
                    .is_err(),
                "{boundary:?}: a new snapshot must not redeliver its reserved publication"
            );
        } else {
            let mut replacements = 0;
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while let Some(update) = stream.next().await {
                    if matches!(
                        update?,
                        crate::recoverable_chat::RecoverableChatUpdate::TerminalReplacement { .. }
                    ) {
                        replacements += 1;
                        break;
                    }
                }
                Ok::<_, crate::EmbedError>(())
            })
            .await
            .expect("old snapshot did not receive its complete publication")?;
            assert_eq!(
                replacements, 1,
                "{boundary:?}: an old snapshot must receive the batch exactly once"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn incarnation_change_invalidates_cursor() {
    let original = crate::observe::InMemoryLiveReplayStore::default();
    let preserved = original.reopen_preserving_history();
    crate::testing::conformance::incarnation_change_invalidates_cursor(
        Arc::new(original),
        Arc::new(crate::observe::InMemoryLiveReplayStore::default()),
        Arc::new(preserved),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_observes_installed_projection() -> Result<()> {
    let replay_store = Arc::new(PausedCommitReplayStore::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .live_replay_store(replay_store.clone())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("notification-observes-installed-projection")
        .open()
        .await?;
    let cursor = session.observe().current_observation().cursor;
    let SessionObservationSubscription::Subscribed(mut subscription) =
        session.observe().subscribe_from_cursor(&cursor)?
    else {
        panic!("a fresh cursor must subscribe without a gap");
    };

    let turn_session = session.clone();
    let turn = tokio::spawn(async move {
        turn_session
            .turn(TurnInput::text("projection before notification"))
            .run()
            .await
    });
    replay_store.wait_for_commit_append().await;
    let installed_before_notification = session.observe().current_observation();
    let committed_escaped = tokio::time::timeout(std::time::Duration::from_millis(25), async {
        loop {
            let event = subscription
                .next()
                .await
                .expect("notification subscription remains open")
                .expect("notification before committed publication");
            if matches!(
                event.payload,
                lash_core::SessionObservationEventPayload::Committed { .. }
            ) {
                return;
            }
        }
    })
    .await
    .is_ok();
    replay_store.release_commit_install();
    let notification = loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), subscription.next())
            .await
            .expect("timed out waiting for committed notification")
            .expect("notification subscription remains open")
            .expect("committed notification");
        if matches!(
            &event.payload,
            lash_core::SessionObservationEventPayload::Committed { .. }
        ) {
            break event;
        }
    };
    let projection_at_notification = session.observe().current_observation();
    turn.await.expect("join publishing turn")?;

    assert_eq!(installed_before_notification.read_view.turn_index(), 1);
    assert!(
        installed_before_notification
            .read_view
            .messages()
            .iter()
            .any(|message| crate::message_text(message).contains("projection before notification")),
        "the authoritative projection must be installed before publication enters notify"
    );
    assert!(
        !committed_escaped,
        "no committed notification may escape while publish_prepared is gated"
    );
    assert_eq!(
        projection_at_notification.cursor, notification.cursor,
        "a Committed notification must not be observable before its authoritative projection is installed"
    );

    replay_store.arm_pause();
    let resident_cursor = projection_at_notification.cursor.clone();
    let SessionObservationSubscription::Subscribed(mut resident_subscription) =
        session.observe().subscribe_from_cursor(&resident_cursor)?
    else {
        panic!("the committed cursor must remain subscribable");
    };
    let resident_session = session.clone();
    let resident = tokio::spawn(async move {
        resident_session
            .set_turn_phase_probe(Arc::new(NoopTurnPhaseProbe))
            .await;
    });
    replay_store.wait_for_commit_append().await;
    let installed_resident = session.observe().current_observation();
    assert_eq!(
        installed_resident.read_view.turn_index(),
        projection_at_notification.read_view.turn_index(),
        "resident publication must not claim a durable revision transition"
    );
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(25),
            resident_subscription.next(),
        )
        .await
        .is_err(),
        "no resident notification may escape before its projection is installed"
    );
    replay_store.release_commit_install();
    let resident_event = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        resident_subscription.next(),
    )
    .await
    .expect("resident notification timeout")
    .expect("resident subscription stays open")
    .expect("resident notification");
    resident.await.expect("join resident publication");
    assert!(matches!(
        resident_event.payload,
        lash_core::SessionObservationEventPayload::ResidentChanged { .. }
    ));
    assert_eq!(
        session.observe().current_observation().cursor,
        resident_event.cursor,
        "a resident notification must observe its installed projection"
    );
    Ok(())
}

#[tokio::test]
async fn payload_authority_matches_revision_transition() -> Result<()> {
    let core = standard_core();
    let session = core.session("payload-authority-transition").open().await?;
    let initial = session.observe().current_observation();

    session
        .turn(TurnInput::text("durable transition"))
        .run()
        .await?;
    let committed = session.observe().resume_from_cursor(&initial.cursor)?;
    let SessionResume::Replayed { events } = committed else {
        panic!("durable transition must replay its committed evidence");
    };
    let committed = events
        .iter()
        .find(|event| {
            matches!(
                event.payload,
                lash_core::SessionObservationEventPayload::Committed { .. }
            )
        })
        .expect("durable transition emitted Committed");
    assert_eq!(committed.revision, lash_core::SessionRevision::new(1));
    let lash_core::SessionObservationEventPayload::Committed { read_view } = &committed.payload
    else {
        unreachable!()
    };
    assert_eq!(read_view.turn_index(), 1);

    let committed_cursor = committed.cursor.clone();
    let probe: Arc<dyn lash_core::runtime::RuntimeTurnPhaseProbe> = Arc::new(NoopTurnPhaseProbe);
    session.set_turn_phase_probe(Arc::clone(&probe)).await;
    let SessionResume::Replayed { events } =
        session.observe().resume_from_cursor(&committed_cursor)?
    else {
        panic!("resident transition must remain replayable");
    };
    assert!(matches!(
        events.as_slice(),
        [event]
            if event.revision == lash_core::SessionRevision::new(1)
                && matches!(
                    event.payload,
                    lash_core::SessionObservationEventPayload::ResidentChanged { .. }
                )
    ));

    let resident_cursor = events[0].cursor.clone();
    session.set_turn_phase_probe(probe).await;
    let SessionResume::Replayed { events } =
        session.observe().resume_from_cursor(&resident_cursor)?
    else {
        panic!("a no-op publication must preserve clean continuity");
    };
    assert!(events.is_empty(), "a no-op publication must emit no event");
    Ok(())
}

impl PausedCommitReplayStore {
    fn new() -> Self {
        Self::at(PublicationBoundary::AfterInstall)
    }

    fn at(boundary: PublicationBoundary) -> Self {
        let pause = Arc::new(PublicationPause {
            boundary_reached: std::sync::atomic::AtomicBool::new(false),
            release_boundary: std::sync::atomic::AtomicBool::new(false),
            pause_lock: StdMutex::new(()),
            pause_changed: std::sync::Condvar::new(),
        });
        let inner = if boundary == PublicationBoundary::BeforeNotification {
            let notification_pause = Arc::clone(&pause);
            lash_core::facade_support::InMemoryLiveReplayStore::with_before_notification_gate_for_testing(
                lash_core::facade_support::InMemoryLiveReplayStore::default(),
                move |events| {
                    if Self::is_authoritative_events(events) {
                        notification_pause.pause();
                    }
                },
            )
        } else {
            lash_core::facade_support::InMemoryLiveReplayStore::default()
        };
        Self {
            inner,
            boundary,
            pause,
        }
    }

    fn is_authoritative_events(events: &[Arc<lash_core::SessionObservationEvent>]) -> bool {
        events.iter().any(|event| {
            matches!(
                &event.payload,
                lash_core::SessionObservationEventPayload::Committed { .. }
                    | lash_core::SessionObservationEventPayload::ResidentChanged { .. }
            )
        })
    }

    fn arm_pause(&self) {
        self.pause
            .release_boundary
            .store(false, std::sync::atomic::Ordering::Release);
        self.pause
            .boundary_reached
            .store(false, std::sync::atomic::Ordering::Release);
    }

    fn pause(&self) {
        self.pause.pause();
    }

    async fn wait_for_commit_append(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !self
                .pause
                .boundary_reached
                .load(std::sync::atomic::Ordering::Acquire)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("turn never reached the post-append observation-install seam");
    }

    fn release_commit_install(&self) {
        self.pause
            .release_boundary
            .store(true, std::sync::atomic::Ordering::Release);
        self.pause.pause_changed.notify_all();
    }
}

impl PublicationPause {
    fn pause(&self) {
        self.boundary_reached
            .store(true, std::sync::atomic::Ordering::Release);
        let mut guard = self.pause_lock.lock_recover();
        while !self
            .release_boundary
            .load(std::sync::atomic::Ordering::Acquire)
        {
            guard = self.pause_changed.wait(guard).recover();
        }
    }
}

impl lash_core::LiveReplayStore for PausedCommitReplayStore {
    fn prepare_publication(
        &self,
        session_id: &str,
        revision: lash_core::SessionRevision,
        events: Vec<lash_core::LiveReplayEventDraft>,
    ) -> std::result::Result<
        lash_core::PreparedLiveReplayPublication,
        lash_core::LiveReplayStoreError,
    > {
        let authoritative = events.iter().any(|event| {
            matches!(
                &event.payload,
                lash_core::SessionObservationEventPayload::Committed { .. }
                    | lash_core::SessionObservationEventPayload::ResidentChanged { .. }
            )
        });
        if authoritative && self.boundary == PublicationBoundary::BeforeReservation {
            self.pause();
        }
        let prepared = self
            .inner
            .prepare_publication(session_id, revision, events)?;
        if authoritative && self.boundary == PublicationBoundary::AfterReservation {
            self.pause();
        }
        Ok(prepared)
    }

    fn publish_prepared(
        &self,
        prepared: lash_core::PreparedLiveReplayPublication,
    ) -> std::result::Result<
        Vec<Arc<lash_core::SessionObservationEvent>>,
        lash_core::LiveReplayStoreError,
    > {
        let pause = Self::is_authoritative_events(prepared.events());
        if pause && self.boundary == PublicationBoundary::AfterInstall {
            self.pause();
        }
        self.inner.publish_prepared(prepared)
    }

    fn replay_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplayOutcome, lash_core::LiveReplayStoreError> {
        self.inner.replay_after_cursor(cursor)
    }

    fn subscribe_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplaySubscribeOutcome, lash_core::LiveReplayStoreError>
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
        | crate::recoverable_chat::RecoverableChatUpdate::TerminalReplacement { id, .. }
        | crate::recoverable_chat::RecoverableChatUpdate::ResidentReplacement { id, .. } => id,
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
        | crate::recoverable_chat::RecoverableChatUpdate::TerminalReplacement { id, .. }
        | crate::recoverable_chat::RecoverableChatUpdate::ResidentReplacement { id, .. } => id,
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
async fn gap_replacement_then_continuation_after_unavailable_history() -> Result<()> {
    let session_id = "recoverable-chat-restart-cursor";
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let bootstrap_core =
        explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
            .provider(mock_provider())
            .model(mock_model_spec())
            .store_factory(store_factory.clone())
            .build(crate::testing::runtime_lease_owner())?;
    bootstrap_core
        .session(session_id)
        .open()
        .await?
        .close()
        .await?;
    drop(bootstrap_core);

    let first_core =
        explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
            .provider(mock_provider())
            .model(mock_model_spec())
            .store_factory(store_factory.clone())
            .live_replay_store(Arc::new(
                lash_core::facade_support::InMemoryLiveReplayStore::default(),
            ))
            .build(crate::testing::runtime_lease_owner())?;
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

    let second_core =
        explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
            .provider(mock_provider())
            .model(mock_model_spec())
            .store_factory(store_factory)
            .live_replay_store(Arc::new(
                lash_core::facade_support::InMemoryLiveReplayStore::default(),
            ))
            .build(crate::testing::runtime_lease_owner())?;
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
            gap: lash_core::facade_support::LiveReplayGap {
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
    assert_ne!(
        id.cursor, old_id.cursor,
        "a fresh replay-store incarnation must change the opaque cursor even at the same numeric position"
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
async fn gap_replacement_then_continuation_after_trimmed_history() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .live_replay_store(Arc::new(
            lash_core::facade_support::InMemoryLiveReplayStore::new(
                lash_core::facade_support::InMemoryLiveReplayStoreConfig {
                    max_events_per_session: 1,
                    ..lash_core::facade_support::InMemoryLiveReplayStoreConfig::default()
                },
            ),
        ))
        .build(crate::testing::runtime_lease_owner())?;
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
                }
                | crate::recoverable_chat::RecoverableChatUpdate::ResidentReplacement {
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
async fn subscriber_lag_with_trimmed_suffix_forces_gap_then_continues() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .live_replay_store(Arc::new(
            lash_core::facade_support::InMemoryLiveReplayStore::new(
                lash_core::facade_support::InMemoryLiveReplayStoreConfig {
                    max_events_per_session: 1,
                    ..lash_core::facade_support::InMemoryLiveReplayStoreConfig::default()
                },
            ),
        ))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("subscriber-lag-trimmed-recovery")
        .open()
        .await?;
    let cursor = session.observe().current_observation().cursor;
    let mut stream = session.observe().subscribe_and_recover(cursor);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), stream.next())
            .await
            .is_err(),
        "the initial poll must install the live receiver and wait"
    );

    for text in ["lag one", "lag two", "lag three"] {
        session.observe().runtime.record_turn_activity(
            Some("lagged-turn"),
            TurnActivity::independent(TurnEvent::AssistantProseDelta { text: text.into() }),
        );
    }

    let gap = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
        .await
        .expect("lag recovery timed out")
        .expect("lag recovery stream remains open")?;
    assert!(matches!(
        gap,
        crate::observe::SessionObservationStreamItem::Gap {
            gap: lash_core::facade_support::LiveReplayGap {
                reason: lash_core::LiveReplayGapReason::Trimmed,
                ..
            },
            ..
        }
    ));

    session.observe().runtime.record_turn_activity(
        Some("after-lag-turn"),
        TurnActivity::independent(TurnEvent::AssistantProseDelta {
            text: "after lag".into(),
        }),
    );
    let continued = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
        .await
        .expect("post-lag continuation timed out")
        .expect("post-lag stream remains open")?;
    let crate::observe::SessionObservationStreamItem::Event(event) = continued else {
        panic!("lag recovery must continue with the next live event");
    };
    assert_eq!(
        observation_assistant_delta(&event).as_deref(),
        Some("after lag")
    );
    Ok(())
}

#[tokio::test]
#[ignore = "the reservation surface still couples broadcast capacity to replay retention, so an independently retained missed suffix is not constructible"]
async fn subscriber_lag_recovers_from_last_delivered_cursor() {
    unreachable!("documented conformance-law placeholder")
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
                    if let Some(tx) = entered_tx.lock_recover().take() {
                        let _ = tx.send(());
                    }
                    release.notified().await;
                    Ok(text_response("completed after observer disconnect"))
                }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(semantic_group_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(semantic_group_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(retry_once_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("retry-status").open().await?;
    let events = RecordingEvents::default();

    let result = session
        .turn(TurnInput::text("hello"))
        .stream_to(&events)
        .await?;

    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { .. })
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(checkpoint_gated_provider(entered_tx, release_rx))
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .build(crate::testing::runtime_lease_owner())?;
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
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { .. })
    ));
    let events = events.snapshot().await;
    assert!(events.iter().any(|event| matches!(
        &event.event,
        TurnEvent::QueuedInputAccepted {
            applications,
        } if applications.iter().any(|application| {
            application.source_key.as_deref() == Some("injection:queue-1")
                && application.turn_id.as_str() == "queued-input-turn"
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
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled)
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
        Some(lash_core::facade_support::TurnCancellationEvidence {
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
                if let Some(tx) = started_tx.lock_recover().take() {
                    let _ = tx.send(());
                }
                // Hang until the turn is cancelled out from under us.
                std::future::pending::<()>().await;
                unreachable!("provider future should be dropped by cancellation")
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())
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
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled)
    ));
    assert!(matches!(
        result.cancellation,
        Some(lash_core::facade_support::TurnCancellationEvidence {
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
                    if let Some(tx) = started_tx.lock_recover().pop() {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn next_turn_notification_during_a_live_turn_has_bounded_hydrations() -> Result<()> {
    let builds = Arc::new(AtomicUsize::new(0));
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let first_entered = Arc::new(tokio::sync::Notify::new());
    let release_first = Arc::new(tokio::sync::Semaphore::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            let first_entered = Arc::clone(&first_entered);
            let release_first = Arc::clone(&release_first);
            move |_request| {
                let provider_calls = Arc::clone(&provider_calls);
                let first_entered = Arc::clone(&first_entered);
                let release_first = Arc::clone(&release_first);
                async move {
                    if provider_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        first_entered.notify_one();
                        release_first
                            .acquire()
                            .await
                            .expect("release semaphore remains open")
                            .forget();
                    }
                    Ok(text_response("live turn complete"))
                }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .plugin(Arc::new(QueuedWorkHydrationProbeFactory {
            builds: Arc::clone(&builds),
        }))
        .queued_work_execution_concurrency(1)
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("queued-work-live-lease").open().await?;
    let entered = first_entered.notified();
    let foreground = session.turn(TurnInput::text("foreground turn")).stream()?;
    entered.await;
    let baseline_builds = builds.load(Ordering::SeqCst);

    core.enqueue_turn_input(
        "queued-work-live-lease",
        TurnInput::text("queued while foreground owns the lease"),
        lash_core::TurnInputIngress::NextTurn,
        Some("queued-during-live-turn".to_string()),
    )
    .await?;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let hydrations = builds
        .load(Ordering::SeqCst)
        .saturating_sub(baseline_builds);
    assert!(
        hydrations <= 5,
        "one live-lease notification must keep hydrations bounded, got {hydrations}"
    );

    release_first.add_permits(1);
    let foreground_result =
        tokio::time::timeout(std::time::Duration::from_secs(2), foreground.finish())
            .await
            .expect("foreground turn completes after release");
    match foreground_result {
        Ok(_) => {}
        Err(EmbedError::Runtime(error)) => {
            assert_eq!(error.code, lash_core::RuntimeErrorCode::StoreCommitFailed);
            assert!(
                error.message.contains("store head revision conflict"),
                "the only accepted concurrent-writer loss is head CAS, got: {error}"
            );
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

#[tokio::test]
async fn create_only_factory_returns_to_idle_after_draining_unknown_claimability() -> Result<()> {
    const MAX_TRANSIENT_HYDRATIONS_PER_NOTIFICATION: usize =
        lash_core::runtime::QUEUED_WORK_MAX_TRANSIENT_ATTEMPTS;

    async fn wait_for_stable_build_count(builds: &AtomicUsize) -> usize {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut observed = builds.load(Ordering::SeqCst);
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
                let current = builds.load(Ordering::SeqCst);
                if current == observed {
                    return current;
                }
                observed = current;
            }
        })
        .await
        .expect("unknown-claimability hydration reaches an idle steady state")
    }

    let builds = Arc::new(AtomicUsize::new(0));
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            move |_request| {
                let provider_calls = Arc::clone(&provider_calls);
                async move {
                    provider_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(text_response("create-only queued work complete"))
                }
            }
        })
        .build()
        .into_handle();
    let store_factory = Arc::new(CreateOnlySessionStoreFactory {
        inner: lash_core::facade_support::InMemorySessionStoreFactory::new(),
    });
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .plugin(Arc::new(QueuedWorkHydrationProbeFactory {
            builds: Arc::clone(&builds),
        }))
        .queued_work_execution_concurrency(1)
        .build(crate::testing::runtime_lease_owner())?;
    let baseline_builds = builds.load(Ordering::SeqCst);

    core.enqueue_turn_input(
        "create-only-factory-idles",
        TurnInput::text("queued through create-only factory"),
        lash_core::TurnInputIngress::NextTurn,
        Some("create-only-idle".to_string()),
    )
    .await?;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while provider_calls.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the conservatively admitted queued turn reaches the provider");

    let request = lash_core::SessionStoreCreateRequest {
        session_id: "create-only-factory-idles".to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };
    let store = lash_core::SessionStoreFactory::open_existing_store(&store_factory.inner, &request)
        .await
        .expect("open the create-only factory's inner store")
        .expect("the queued session exists");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let read = store
                .load_session()
                .await
                .expect("load the queued session")
                .expect("the queued session state exists");
            if read
                .checkpoint
                .as_ref()
                .is_some_and(|checkpoint| checkpoint.turn_state.turn_index >= 1)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the queued turn commits durably");

    let first_settled_builds = wait_for_stable_build_count(&builds).await;
    let first_hydrations = first_settled_builds.saturating_sub(baseline_builds);
    assert!(
        (1..=MAX_TRANSIENT_HYDRATIONS_PER_NOTIFICATION).contains(&first_hydrations),
        "one conservative notification must use one bounded hydration ladder, got {first_hydrations}"
    );

    core.enqueue_turn_input(
        "create-only-factory-idles",
        TurnInput::text("queued after the create-only factory idled"),
        lash_core::TurnInputIngress::NextTurn,
        Some("create-only-rearm".to_string()),
    )
    .await?;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while provider_calls.load(Ordering::SeqCst) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("enqueue and notify re-arm the idled create-only factory");

    let second_settled_builds = wait_for_stable_build_count(&builds).await;
    let second_hydrations = second_settled_builds.saturating_sub(first_settled_builds);
    assert!(
        (1..=MAX_TRANSIENT_HYDRATIONS_PER_NOTIFICATION).contains(&second_hydrations),
        "the re-armed notification must use one fresh bounded hydration ladder, got {second_hydrations}"
    );
    Ok(())
}

#[tokio::test]
async fn inline_queued_work_burst_reuses_one_hydrated_runtime() -> Result<()> {
    const INPUTS: usize = 8;
    let builds = Arc::new(AtomicUsize::new(0));
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let seen_inputs = Arc::new(StdMutex::new(Vec::<String>::new()));
    let first_entered = Arc::new(tokio::sync::Notify::new());
    let release_first = Arc::new(tokio::sync::Semaphore::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            let seen_inputs = Arc::clone(&seen_inputs);
            let first_entered = Arc::clone(&first_entered);
            let release_first = Arc::clone(&release_first);
            move |request| {
                let provider_calls = Arc::clone(&provider_calls);
                let seen_inputs = Arc::clone(&seen_inputs);
                let first_entered = Arc::clone(&first_entered);
                let release_first = Arc::clone(&release_first);
                async move {
                    seen_inputs.lock_recover().push(last_user_text(&request));
                    if provider_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        first_entered.notify_one();
                        release_first
                            .acquire()
                            .await
                            .expect("release semaphore remains open")
                            .forget();
                    }
                    Ok(text_response("queued work complete"))
                }
            }
        })
        .build()
        .into_handle();
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .plugin(Arc::new(QueuedWorkHydrationProbeFactory {
            builds: Arc::clone(&builds),
        }))
        .build(crate::testing::runtime_lease_owner())?;
    assert_eq!(builds.load(Ordering::SeqCst), 1, "build-time validation");

    let entered = first_entered.notified();
    core.enqueue_turn_input(
        "queued-work-hydration-burst",
        TurnInput::text("queued input 0"),
        lash_core::TurnInputIngress::NextTurn,
        Some("queued-input-0".to_string()),
    )
    .await?;
    tokio::time::timeout(std::time::Duration::from_secs(1), entered)
        .await
        .expect("the first queued turn reaches the provider");
    for index in 1..INPUTS {
        core.enqueue_turn_input(
            "queued-work-hydration-burst",
            TurnInput::text(format!("queued input {index}")),
            lash_core::TurnInputIngress::NextTurn,
            Some(format!("queued-input-{index}")),
        )
        .await?;
    }

    assert_eq!(
        builds.load(Ordering::SeqCst),
        2,
        "the blocked run must be the only runtime hydration admitted for the session burst"
    );
    release_first.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let observed = seen_inputs.lock_recover().join("\n");
            if (0..INPUTS).all(|index| observed.contains(&format!("queued input {index}"))) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the hydrated runtime drains every queued input");
    let request = lash_core::SessionStoreCreateRequest {
        session_id: "queued-work-hydration-burst".to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };
    let store =
        lash_core::SessionStoreFactory::open_existing_store(store_factory.as_ref(), &request)
            .await
            .expect("open the queued-work burst store")
            .expect("the queued-work burst store exists");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let read = store
                .load_session()
                .await
                .expect("load queued-work burst state")
                .expect("queued-work burst state exists");
            if read
                .checkpoint
                .as_ref()
                .is_some_and(|checkpoint| checkpoint.turn_state.turn_index >= 2)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the hydrated runtime durably commits the full burst");

    let observed = seen_inputs.lock_recover().join("\n");
    let mut previous = 0;
    for index in 0..INPUTS {
        let position = observed
            .find(&format!("queued input {index}"))
            .expect("every queued input reached the provider");
        assert!(position >= previous, "queued input order changed");
        previous = position;
    }
    assert_eq!(
        builds.load(Ordering::SeqCst),
        2,
        "one hydrated runtime must serve the whole ordered burst"
    );
    Ok(())
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())
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
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled)
    ));
    assert!(matches!(
        second.outcome,
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled)
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .build(crate::testing::runtime_lease_owner())
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
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled)
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .disable_queued_work_driver()
        .build(crate::testing::runtime_lease_owner())
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
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled)
    ));
    assert!(matches!(
        output.result.cancellation,
        Some(lash_core::facade_support::TurnCancellationEvidence {
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
                    .lock_recover()
                    .push((user_text.clone(), request.messages.clone()));
                if user_text == "primary hangs" {
                    if let Some(tx) = started_tx.lock_recover().take() {
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .disable_queued_work_driver()
        .build(crate::testing::runtime_lease_owner())?;
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
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled)
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
    let requests = requests.lock_recover().clone();
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
                captured_requests.lock_recover().push(user_text.clone());
                if user_text == "primary waits for active steer" {
                    if let Some(tx) = first_started_tx.lock_recover().take() {
                        let _ = tx.send(());
                    }
                    if let Some(rx) = release_first_rx.lock().await.take() {
                        let _ = rx.await;
                    }
                    return Ok(text_response("first response"));
                }
                if user_text == "accepted active steer" {
                    if let Some(tx) = second_started_tx.lock_recover().take() {
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .disable_queued_work_driver()
        .build(crate::testing::runtime_lease_owner())?;
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
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled)
    ));
    assert!(
        session.pending_turn_inputs().await?.is_empty(),
        "accepted active steer `{}` must be completed, not deferred after interrupt",
        active.input_id
    );
    assert!(
        session.queued_turn().run().await?.ran().is_none(),
        "accepted active steer must not replay as a later queued turn"
    );
    let requests = requests.lock_recover().clone();
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

#[cfg(feature = "rlm")]
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
                        .lock_recover()
                        .push(request.messages.clone());
                    match call_index.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                        0 => {
                            if let Some(tx) = first_started_tx.lock_recover().take() {
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
        let core = explicit_ephemeral_facets(LashCore::rlm_builder(
            crate::TurnBudget::Unbounded,
            rlm_factory(),
        ))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .disable_queued_work_driver()
        .build(crate::testing::runtime_lease_owner())?;
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

        let requests = requests.lock_recover().clone();
        assert_eq!(requests.len(), 3, "two turns must execute three calls");
        let first_messages = serde_json::to_string(&requests[0]).expect("serialize first request");
        let second_messages =
            serde_json::to_string(&requests[1]).expect("serialize second request");
        assert!(!first_messages.contains("mid-turn injection marker"));
        assert!(
            second_messages.contains("mid-turn injection marker"),
            "active input was claimed but omitted from the next RLM provider request: {second_messages}"
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .disable_queued_work_driver()
        .build(crate::testing::runtime_lease_owner())
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
        session.queued_turn().run().await?.ran().is_none(),
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .build(crate::testing::runtime_lease_owner())
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
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { .. })
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
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { .. })
    ));
    assert_eq!(result.result.usage.output_tokens, 2);
    Ok(())
}

#[tokio::test]
async fn core_catalog_and_actual_turn_resolve_the_identical_contract() -> Result<()> {
    let tools = ContractRecordingTools::default();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(tool_roundtrip_provider())
        .model(mock_model_spec())
        .tools(Arc::new(tools.clone()))
        .build(crate::testing::runtime_lease_owner())?;
    let core_contract = core
        .tool_catalog()
        .resolve_contract("app_lookup")
        .expect("core catalog contract");
    let expected = serde_json::to_value(core_contract.as_ref()).expect("serialize core contract");
    let session = core.session("catalog-agreement").open().await?;
    tools.take_resolved();

    let output = session
        .turn(TurnInput::text("use the lookup tool"))
        .run()
        .await?;

    assert!(output.is_success());
    let turn_contracts = tools.take_resolved();
    assert!(
        !turn_contracts.is_empty(),
        "the actual turn must resolve the tool contract"
    );
    assert!(
        turn_contracts.iter().all(|contract| contract == &expected),
        "the core projection and actual turn path must use identical contracts"
    );
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(tool_roundtrip_provider())
        .model(mock_model_spec())
        .tools(Arc::new(AppTools))
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("fanout-tool-events").open().await?;

    let output = session
        .turn(TurnInput::text("use tool"))
        .advanced()
        .collect_with_scope(live.as_ref(), turn_scope(&session.session_id()))
        .await?;

    assert!(matches!(
        output.result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { .. })
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
        let core =
            explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
                .provider(runtime_batch_provider())
                .model(mock_model_spec())
                .tools(tool_provider)
                .plugin(runtime_batch_plugin())
                .store_factory(Arc::new(
                    lash_core::facade_support::InMemorySessionStoreFactory::new(),
                ))
                .process_registry(Arc::new(TestLocalProcessRegistry::default()))
                .build(crate::testing::runtime_lease_owner())?;
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
            let core =
                explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
                    .provider(runtime_batch_provider())
                    .model(mock_model_spec())
                    .tools(tool_provider)
                    .plugin(runtime_batch_plugin())
                    .store_factory(Arc::new(
                        lash_core::facade_support::InMemorySessionStoreFactory::new(),
                    ))
                    .process_registry(Arc::new(TestLocalProcessRegistry::default()))
                    .build(crate::testing::runtime_lease_owner())?;
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(tool_roundtrip_provider())
        .model(mock_model_spec())
        .tools(Arc::new(PendingAppTools::new(key_tx)))
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build(crate::testing::runtime_lease_owner())?;
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
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { .. })
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
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { .. })
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(tool_roundtrip_provider())
        .model(mock_model_spec())
        .tools(Arc::new(AppTools))
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("tool-events").open().await?;
    let events = RecordingEvents::default();

    let collected = session
        .turn(TurnInput::text("use tool"))
        .stream_to(&events)
        .await?;

    assert!(matches!(
        collected.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { .. })
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

#[cfg(feature = "rlm")]
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

        let core = explicit_ephemeral_facets(LashCore::rlm_builder(
            crate::TurnBudget::Unbounded,
            rlm_factory(),
        ))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build(crate::testing::runtime_lease_owner())?;
        let session = core.session("rlm-streamed-raw-final-cell").open().await?;
        let events = Arc::new(RecordingEvents::default());

        let result = session
            .turn(TurnInput::text("say hi"))
            .stream_to(events.as_ref())
            .await?;

        assert!(matches!(
            result.outcome,
            TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue { .. })
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

#[cfg(feature = "rlm")]
fn rlm_abort_drain_core(provider: ProviderHandle) -> Result<LashCore> {
    explicit_ephemeral_facets(LashCore::rlm_builder(
        lash_core::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(provider)
    .model(mock_model_spec())
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .process_registry(Arc::new(TestLocalProcessRegistry::default()))
    .build(crate::testing::runtime_lease_owner())
}

#[cfg(feature = "rlm")]
#[test]
fn rlm_abort_drain_ignores_a_late_attempt_reset() -> Result<()> {
    run_async_test_on_stack_budget("rlm-abort-drain-attempt-reset", || async {
        let provider = crate::testing::TestProvider::builder()
            .kind("rlm-abort-reset")
            .requires_streaming(true)
            .complete(|request| async move {
                let stream = request.stream_events.expect("stream events");
                stream.send(LlmStreamEvent::Delta(
                    "<lashlang>\nfinish \"cell survived reset\"\n</lashlang>\n".to_string(),
                ));
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                stream.send(LlmStreamEvent::AttemptReset);
                std::future::pending::<std::result::Result<LlmResponse, LlmTransportError>>().await
            })
            .build()
            .into_handle();
        let core = rlm_abort_drain_core(provider)?;
        let session = core.session("rlm-abort-reset").open().await?;

        let result = session.turn(TurnInput::text("finish")).run().await?;

        assert_eq!(
            result.final_value(),
            Some(&serde_json::json!("cell survived reset"))
        );
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
fn rlm_abort_drain_preserves_late_reasoning_replay_and_usage() -> Result<()> {
    run_async_test_on_stack_budget("rlm-abort-drain-late-events", || async {
        let provider = crate::testing::TestProvider::builder()
            .kind("rlm-abort-late-events")
            .requires_streaming(true)
            .complete(|request| async move {
                let stream = request.stream_events.expect("stream events");
                stream.send(LlmStreamEvent::Evidence(lash_core::LlmStreamEvidence {
                    request_body: Some("{\"model\":\"rlm-evidence\"}".to_string()),
                    http_summary: Some(
                        "HTTP POST https://provider.test/v1/responses (stream)".to_string(),
                    ),
                    execution_evidence: Some(lash_core::ExecutionEvidence {
                        provider_request_id: Some("request-after-response-start".to_string()),
                        ..Default::default()
                    }),
                    generation_disposition: Some(lash_core::GenerationReceipt {
                        stop_sequences: lash_core::GenerationOptionOutcome::Applied,
                        ..Default::default()
                    }),
                    response_metadata: std::collections::BTreeMap::from([(
                        "header:x-request-cost".to_string(),
                        serde_json::json!("0.04"),
                    )]),
                    ..Default::default()
                }));
                stream.send(LlmStreamEvent::Delta(
                    "<lashlang>\nfinish \"late events survived\"\n</lashlang>\n".to_string(),
                ));
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                stream.send(LlmStreamEvent::Delta("provider suffix".to_string()));
                stream.send(LlmStreamEvent::Part(LlmOutputPart::Reasoning {
                    text: "signed reasoning".to_string(),
                    replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                        item_id: Some("reasoning-after-abort".to_string()),
                        encrypted_content: Some("encrypted-after-abort".to_string()),
                        signature: Some("signature-after-abort".to_string()),
                        redacted: false,
                        summary: vec!["signed reasoning".to_string()],
                        ..Default::default()
                    }),
                }));
                stream.send(LlmStreamEvent::Usage(lash_core::llm::types::LlmUsage {
                    input_tokens: 17,
                    output_tokens: 5,
                    reasoning_output_tokens: 2,
                    ..lash_core::llm::types::LlmUsage::default()
                }));
                std::future::pending::<std::result::Result<LlmResponse, LlmTransportError>>().await
            })
            .build()
            .into_handle();
        let recorder = Arc::new(RecordingInlineEffectController::default());
        let effect_controller: Arc<dyn lash_core::RuntimeEffectController> = recorder.clone();
        let core = explicit_ephemeral_facets(LashCore::rlm_builder(
            lash_core::TurnBudget::Unbounded,
            rlm_factory(),
        ))
        .generation(lash_core::GenerationOptions {
            stop_sequences: vec!["caller-owned-stop".to_string()],
            ..Default::default()
        })
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .effect_host(Arc::new(lash_core::facade_support::InlineEffectHost::new(
            effect_controller,
        )))
        .build(crate::testing::runtime_lease_owner())?;
        let session = core.session("rlm-abort-late-events").open().await?;

        let result = session.turn(TurnInput::text("finish")).run().await?;

        assert_eq!(result.result.usage.input_tokens, 17);
        assert_eq!(result.result.usage.output_tokens, 5);
        assert!(
            result
                .result
                .state
                .read_view()
                .messages()
                .iter()
                .any(|message| {
                    message.parts.iter().any(|part| {
                        part.reasoning_meta.as_ref().is_some_and(|meta| {
                            meta.signature.as_deref() == Some("signature-after-abort")
                                && meta.encrypted_content.as_deref()
                                    == Some("encrypted-after-abort")
                        })
                    })
                })
        );

        let attempt = result
            .result
            .llm_calls
            .first()
            .and_then(|record| record.attempts.first())
            .expect("persisted aborted attempt");
        assert_eq!(attempt.outcome, lash_core::AttemptOutcome::Aborted);
        assert_eq!(
            attempt
                .generation_disposition
                .expect("attempt disposition")
                .stop_sequences,
            lash_core::GenerationOptionOutcome::SuppressedProtocolOwned
        );
        assert_eq!(
            attempt
                .evidence
                .as_ref()
                .and_then(|evidence| evidence.provider_request_id.as_deref()),
            Some("request-after-response-start")
        );
        assert_eq!(
            attempt
                .evidence
                .as_ref()
                .and_then(|evidence| evidence.collection_interruption),
            Some(lash_core::ExecutionEvidenceCollectionInterruption::ProtocolAbort)
        );

        let journaled = recorder
            .persisted_outcomes()
            .into_iter()
            .find(|outcome| matches!(outcome, lash_core::RuntimeEffectOutcome::LlmCall { .. }))
            .expect("persisted LLM effect outcome");
        let lash_core::RuntimeEffectOutcome::LlmCall {
            result: journaled_result,
            call_record,
            ..
        } = journaled
        else {
            unreachable!("selected LLM outcome")
        };
        let response = journaled_result
            .as_ref()
            .as_ref()
            .expect("protocol abort is an accepted response");
        assert_eq!(
            response.request_body.as_deref(),
            Some("{\"model\":\"rlm-evidence\"}")
        );
        assert_eq!(
            response.http_summary.as_deref(),
            Some("HTTP POST https://provider.test/v1/responses (stream)")
        );
        assert_eq!(
            response.response_metadata.get("header:x-request-cost"),
            Some(&serde_json::json!("0.04"))
        );
        assert_eq!(
            response
                .generation_disposition
                .expect("response disposition")
                .stop_sequences,
            lash_core::GenerationOptionOutcome::SuppressedProtocolOwned
        );
        assert_eq!(
            response
                .execution_evidence
                .as_ref()
                .and_then(|evidence| evidence.collection_interruption),
            Some(lash_core::ExecutionEvidenceCollectionInterruption::ProtocolAbort)
        );
        let journaled_attempt = call_record
            .as_ref()
            .and_then(|record| record.attempts.first())
            .expect("journaled aborted attempt");
        assert_eq!(journaled_attempt, attempt);
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
fn rlm_abort_drain_deadline_proceeds_with_default_usage() -> Result<()> {
    run_async_test_on_stack_budget("rlm-abort-drain-no-usage", || async {
        let provider = crate::testing::TestProvider::builder()
            .kind("rlm-abort-no-usage")
            .requires_streaming(true)
            .complete(|request| async move {
                request
                    .stream_events
                    .expect("stream events")
                    .send(LlmStreamEvent::Delta(
                        "<lashlang>\nfinish \"deadline survived\"\n</lashlang>\n".to_string(),
                    ));
                std::future::pending::<std::result::Result<LlmResponse, LlmTransportError>>().await
            })
            .build()
            .into_handle();
        let core = rlm_abort_drain_core(provider)?;
        let session = core.session("rlm-abort-no-usage").open().await?;

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            session.turn(TurnInput::text("finish")).run(),
        )
        .await
        .expect("abort drain deadline must not wedge")?;

        assert_eq!(
            result.final_value(),
            Some(&serde_json::json!("deadline survived"))
        );
        assert_eq!(result.result.usage, lash_core::TokenUsage::default());
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
fn rlm_tool_calls_stream_from_live_exec_boundary() -> Result<()> {
    run_async_test_on_stack_budget("rlm-live-exec-boundary-test", || {
        rlm_tool_calls_stream_from_live_exec_boundary_inner()
    })
}

#[cfg(feature = "rlm")]
async fn rlm_tool_calls_stream_from_live_exec_boundary_inner() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![lashlang_block(
        r#"value = await tools.app_lookup({})?
finish "done""#,
    )]))
    .model(mock_model_spec())
    .tools(Arc::new(AppTools))
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .process_registry(Arc::new(TestLocalProcessRegistry::default()))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("rlm-live-tool-events").open().await?;
    let events = Arc::new(RecordingEvents::default());

    let result = session
        .turn(TurnInput::text("use tool"))
        .stream_to(events.as_ref())
        .await?;

    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue { .. })
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
            .session_graph()
            .clone()
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

#[cfg(feature = "rlm")]
#[test]
fn rlm_recovered_tool_failure_remains_in_turn_accounting() -> Result<()> {
    run_async_test_on_stack_budget("rlm-recovered-tool-failure-test", || async {
        let core = explicit_ephemeral_facets(LashCore::rlm_builder(
            crate::TurnBudget::Unbounded,
            rlm_factory(),
        ))
        .provider(queued_text_provider(vec![lashlang_block(
            r#"failure = await tools.app_lookup({})
finish "recovered""#,
        )]))
        .model(mock_model_spec())
        .tools(Arc::new(FailingAppTools))
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build(crate::testing::runtime_lease_owner())?;
        let session = core.session("rlm-recovered-tool-failure").open().await?;

        let result = session
            .turn(TurnInput::text("recover the tool failure"))
            .run()
            .await?
            .result;

        assert!(matches!(
            result.outcome,
            TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue { .. })
        ));
        assert_eq!(result.final_value(), Some(&serde_json::json!("recovered")));
        assert!(result.execution.had_tool_calls);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].tool, "app_lookup");
        assert!(!result.tool_calls[0].output.is_success());
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
fn rlm_code_block_aggregate_lists_every_collected_tool_call() -> Result<()> {
    run_async_test_on_stack_budget("rlm-aggregate-tool-ids-test", || {
        rlm_code_block_aggregate_lists_every_collected_tool_call_inner()
    })
}

#[cfg(feature = "rlm")]
async fn rlm_code_block_aggregate_lists_every_collected_tool_call_inner() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![lashlang_block(
        r#"a = await tools.app_lookup({})?
b = await tools.app_lookup({})?
finish "done""#,
    )]))
    .model(mock_model_spec())
    .tools(Arc::new(AppTools))
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .process_registry(Arc::new(TestLocalProcessRegistry::default()))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("rlm-aggregate-tool-ids").open().await?;
    let events = Arc::new(RecordingEvents::default());

    let result = session
        .turn(TurnInput::text("use tools"))
        .stream_to(events.as_ref())
        .await?;
    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue { .. })
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

#[cfg(feature = "rlm")]
#[test]
fn rlm_tool_calls_emit_typed_trace_pair_and_inline_boundary_protocol_step() -> Result<()> {
    run_async_test_on_stack_budget("rlm-tool-trace-test", || {
        rlm_tool_calls_emit_typed_trace_pair_and_inline_boundary_protocol_step_inner()
    })
}

#[cfg(feature = "rlm")]
async fn rlm_tool_calls_emit_typed_trace_pair_and_inline_boundary_protocol_step_inner() -> Result<()>
{
    let trace_path = std::env::temp_dir().join(format!(
        "lash-rlm-tool-trace-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![lashlang_block(
        r#"value = await tools.app_lookup({})?
finish "done""#,
    )]))
    .model(mock_model_spec())
    .tools(Arc::new(AppTools))
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .process_registry(Arc::new(TestLocalProcessRegistry::default()))
    .trace_jsonl_path(trace_path.clone())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("rlm-tool-trace").open().await?;

    let result = session.turn(TurnInput::text("use tool")).run().await?;
    assert!(matches!(
        result.result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue { .. })
    ));
    core.flush_trace_sink()?;

    let logged = std::fs::read_to_string(&trace_path).expect("read trace");
    let entries = logged
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json log entry"))
        .collect::<Vec<_>>();

    // The inline tier never persists progress boundaries, but the protocol
    // events returned by those boundaries must still reach the trace sink.
    // Runtime diagnostics use a separate emitter and do not prove this path.
    entries
        .iter()
        .find(|entry| {
            entry.get("type").and_then(|v| v.as_str()) == Some("protocol_step")
                && entry.get("plugin_id").and_then(|v| v.as_str()) == Some("rlm_protocol")
        })
        .expect("inline boundary-sourced RLM protocol step");

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

#[cfg(feature = "rlm")]
#[test]
fn rlm_native_provider_tool_call_is_a_traced_non_retryable_turn_issue() -> Result<()> {
    run_async_test_on_stack_budget("rlm-native-tool-contract-test", || async {
        let trace_path = std::env::temp_dir().join(format!(
            "lash-rlm-native-tool-contract-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let core = explicit_ephemeral_facets(LashCore::rlm_builder(
            lash_core::TurnBudget::Unbounded,
            rlm_factory(),
        ))
        .provider(native_tool_call_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .trace_jsonl_path(trace_path.clone())
        .build(crate::testing::runtime_lease_owner())?;
        let session = core.session("rlm-native-tool-contract").open().await?;

        let turn = session
            .turn(TurnInput::text("trigger native provider tool call"))
            .run()
            .await?;

        assert_eq!(
            turn.result.outcome,
            TurnOutcome::Stopped(lash_core::facade_support::TurnStop::RuntimeError)
        );
        let issue = turn
            .result
            .errors
            .iter()
            .find(|issue| issue.code.as_deref() == Some("native_tool_call_not_allowed"))
            .expect("typed RLM native-tool-call issue");
        assert_eq!(issue.kind, "rlm_protocol");
        assert_eq!(issue.retryable, Some(false));
        assert!(issue.message.contains("native_lookup"));
        assert!(issue.message.contains("must flow through Lashlang"));

        core.flush_trace_sink()?;
        let logged = std::fs::read_to_string(&trace_path).expect("read trace");
        let entries = logged
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("trace JSON"))
            .collect::<Vec<_>>();
        let diagnostic = entries
            .iter()
            .find(|entry| {
                entry.get("type").and_then(|value| value.as_str()) == Some("protocol_step")
                    && entry.get("plugin_id").and_then(|value| value.as_str())
                        == Some(lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID)
                    && entry
                        .pointer("/payload/RlmDiagnostic/phase")
                        .and_then(|value| value.as_str())
                        == Some("protocol_contract_violation")
            })
            .expect("RLM protocol-contract trace record");
        assert_eq!(
            diagnostic
                .pointer("/payload/RlmDiagnostic/payload/code")
                .and_then(|value| value.as_str()),
            Some("native_tool_call_not_allowed")
        );
        assert_eq!(
            diagnostic
                .pointer("/payload/RlmDiagnostic/payload/tool_name")
                .and_then(|value| value.as_str()),
            Some("native_lookup")
        );

        let _ = std::fs::remove_file(&trace_path);
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
fn rlm_pending_host_tool_completion_resumes_lashlang_await() -> Result<()> {
    run_async_test_on_stack_budget("rlm-pending-host-tool-test", || {
        rlm_pending_host_tool_completion_resumes_lashlang_await_inner()
    })
}

#[cfg(feature = "rlm")]
async fn rlm_pending_host_tool_completion_resumes_lashlang_await_inner() -> Result<()> {
    let (key_tx, key_rx) = oneshot::channel();
    let events = Arc::new(RecordingEvents::default());
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![lashlang_block(
        "value = await tools.app_lookup({})?\nfinish value",
    )]))
    .model(mock_model_spec())
    .tools(Arc::new(PendingAppTools::new(key_tx)))
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .process_registry(Arc::new(TestLocalProcessRegistry::default()))
    .build(crate::testing::runtime_lease_owner())?;
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
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue { .. })
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

#[cfg(feature = "rlm")]
#[test]
fn rlm_process_pending_host_tool_completion_resumes_process_await() -> Result<()> {
    run_async_test_on_stack_budget("rlm-process-pending-host-tool-test", || {
        rlm_process_pending_host_tool_completion_resumes_process_await_inner()
    })
}

#[cfg(feature = "rlm")]
async fn rlm_process_pending_host_tool_completion_resumes_process_await_inner() -> Result<()> {
    let (key_tx, key_rx) = oneshot::channel();
    let events = Arc::new(RecordingEvents::default());
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
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
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .process_registry(Arc::new(TestLocalProcessRegistry::default()))
    .build(crate::testing::runtime_lease_owner())?;
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
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue { .. })
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

#[cfg(feature = "rlm")]
#[test]
fn continue_as_observation_emits_frame_switch_then_commit() -> Result<()> {
    run_async_test_on_stack_budget("continue-as-observation-test", || {
        continue_as_observation_emits_frame_switch_then_commit_inner()
    })
}

#[cfg(feature = "rlm")]
async fn continue_as_observation_emits_frame_switch_then_commit_inner() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![
        lashlang_block(r#"await control.continue_as({ task: "finish in a fresh frame" })?"#),
        lashlang_block(r#"finish "done after continue_as""#),
    ]))
    .model(mock_model_spec())
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .build(crate::testing::runtime_lease_owner())?;
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

#[cfg(feature = "rlm")]
#[test]
fn lane_less_post_commit_from_plain_turn_does_not_affect_next_turn() -> Result<()> {
    run_async_test_on_stack_budget("lane-less-post-commit-plain-turn-test", || {
        lane_less_post_commit_from_plain_turn_does_not_affect_next_turn_inner()
    })
}

#[cfg(feature = "rlm")]
async fn lane_less_post_commit_from_plain_turn_does_not_affect_next_turn_inner() -> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "nested-release-turn-latch";
    let append_count = Arc::new(AtomicUsize::new(0));
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.path().join("sessions"),
    ));
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![
        lashlang_block(r#"finish "plain turn complete""#),
        lashlang_block(r#"await control.continue_as({ task: "finish turn two" })?"#),
        lashlang_block(r#"finish "turn two complete""#),
    ]))
    .model(mock_model_spec())
    .store_factory(store_factory.clone())
    .plugin(Arc::new(TurnPersistedGraphAppendFactory {
        append_count: Arc::clone(&append_count),
        max_appends: 1,
    }))
    .disable_queued_work_driver()
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(session_id).open().await?;

    let first = session
        .turn(TurnInput::text("plain finish with nested append"))
        .run()
        .await?;
    assert_eq!(
        first.final_value(),
        Some(&serde_json::json!("plain turn complete"))
    );
    assert_eq!(append_count.load(Ordering::SeqCst), 1);
    let second = session
        .turn(TurnInput::text("continue without another nested append"))
        .run()
        .await?;
    assert_eq!(
        second.final_value(),
        Some(&serde_json::json!("turn two complete"))
    );
    assert_eq!(append_count.load(Ordering::SeqCst), 1);
    // Main turn 1, its lane-less TurnPersisted append, and main turn 2 each
    // acquire once. No hidden transfer/reacquire occurs at either boundary.
    assert_sqlite_session_lane_free_at_generation(store_factory.as_ref(), session_id, 3);
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
fn probe_inprocess_continue_as_survives_post_commit_graph_append() -> Result<()> {
    run_async_test_on_stack_budget("inprocess-continue-as-authority-test", || {
        probe_inprocess_continue_as_survives_post_commit_graph_append_inner()
    })
}

#[cfg(feature = "rlm")]
async fn probe_inprocess_continue_as_survives_post_commit_graph_append_inner() -> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "inprocess-continue-as";
    let append_count = Arc::new(AtomicUsize::new(0));
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.path().join("sessions"),
    ));
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![
        lashlang_block(r#"await control.continue_as({ task: "finish in process" })?"#),
        lashlang_block(r#"finish "done after in-process handoff""#),
    ]))
    .model(mock_model_spec())
    .store_factory(store_factory.clone())
    .plugin(Arc::new(TurnPersistedGraphAppendFactory {
        append_count: Arc::clone(&append_count),
        max_appends: 1,
    }))
    .disable_queued_work_driver()
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(session_id).open().await?;

    let output = session
        .turn(TurnInput::text("switch frames in process"))
        .run()
        .await?;

    assert_eq!(append_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        output.final_value(),
        Some(&serde_json::json!("done after in-process handoff")),
        "post-commit graph writes must not strand the in-process frame handoff: {output:?}"
    );
    assert_sqlite_session_lane_free_at_generation(store_factory.as_ref(), session_id, 1);
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
fn durable_queued_continue_as_survives_post_commit_graph_append() -> Result<()> {
    run_async_test_on_stack_budget("durable-queued-continue-as-authority-test", || {
        durable_queued_continue_as_survives_post_commit_graph_append_inner()
    })
}

#[cfg(feature = "rlm")]
async fn durable_queued_continue_as_survives_post_commit_graph_append_inner() -> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "durable-queued-continue-as";
    let append_count = Arc::new(AtomicUsize::new(0));
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.path().join("sessions"),
    ));
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![
        lashlang_block(r#"await control.continue_as({ task: "finish from durable handoff" })?"#),
        lashlang_block(r#"finish "done after durable handoff""#),
    ]))
    .model(mock_model_spec())
    .store_factory(store_factory.clone())
    .plugin(Arc::new(TurnPersistedGraphAppendFactory {
        append_count: Arc::clone(&append_count),
        max_appends: 1,
    }))
    .disable_queued_work_driver()
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(session_id).open().await?;
    session
        .enqueue(TurnInput::text("switch frames from queued work"))
        .id("queued-continue-as")
        .send()
        .await?;

    let output = session
        .queued_turn()
        .run()
        .await?
        .expect("queued turn should run");

    assert_eq!(append_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        output.final_value(),
        Some(&serde_json::json!("done after durable handoff")),
        "post-commit graph writes must not strand the committed frame handoff: {output:?}"
    );
    assert_sqlite_session_lane_free_at_generation(store_factory.as_ref(), session_id, 1);
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
fn durable_queued_continue_as_seed_is_visible_to_follow_turn_linker() -> Result<()> {
    run_async_test_on_stack_budget("durable-queued-continue-as-seed-test", || {
        durable_queued_continue_as_seed_is_visible_to_follow_turn_linker_inner()
    })
}

#[cfg(feature = "rlm")]
async fn durable_queued_continue_as_seed_is_visible_to_follow_turn_linker_inner() -> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "durable-queued-continue-as-seed";
    let (first_provider_call_tx, first_provider_call_rx) = tokio::sync::oneshot::channel();
    let first_provider_call_tx = Arc::new(std::sync::Mutex::new(Some(first_provider_call_tx)));
    let release_first_provider_call = Arc::new(tokio::sync::Notify::new());
    let provider_call_count = Arc::new(AtomicUsize::new(0));
    let repair_request = Arc::new(std::sync::Mutex::new(None));
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete({
            let first_provider_call_tx = Arc::clone(&first_provider_call_tx);
            let release_first_provider_call = Arc::clone(&release_first_provider_call);
            let provider_call_count = Arc::clone(&provider_call_count);
            let repair_request = Arc::clone(&repair_request);
            move |request| {
                let first_provider_call_tx = Arc::clone(&first_provider_call_tx);
                let release_first_provider_call = Arc::clone(&release_first_provider_call);
                let provider_call_count = Arc::clone(&provider_call_count);
                let repair_request = Arc::clone(&repair_request);
                async move {
                    let call = provider_call_count.fetch_add(1, Ordering::SeqCst);
                    let text = match call {
                        0 => {
                            lashlang_block(
                                r#"control = { total: 28 }
finish { established: control.total }"#,
                            )
                        }
                        1 => {
                            if let Some(tx) = first_provider_call_tx
                                .lock_recover()
                                .take()
                            {
                                let _ = tx.send(());
                            }
                            release_first_provider_call.notified().await;
                            lashlang_block(
                                r#"await control.continue_as({ task: "finish from seeded durable handoff", seed: { baton: "seed:durable", session_chars: len(session_projection) } })?"#,
                            )
                        }
                        2 => lashlang_block(
                            r#"finish { seed_visible: baton, session_projection_chars: session_chars }"#,
                        ),
                        _ => {
                            *repair_request.lock_recover() = Some(format!("{request:?}"));
                            lashlang_block(r#"finish { unexpected_repair: true }"#)
                        }
                    };
                    Ok(text_response(&text))
                }
            }
        })
        .build()
        .into_handle();
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.path().join("sessions"),
    ));
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        lash_core::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(provider)
    .model(mock_model_spec())
    .store_factory(store_factory)
    .disable_queued_work_driver()
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(session_id).open().await?;
    let established = session
        .turn(TurnInput::text(
            "establish a durable global that collides with a module root",
        ))
        .run()
        .await?;
    assert_eq!(
        established.final_value(),
        Some(&serde_json::json!({ "established": 28 }))
    );
    session
        .admin()
        .protocol()
        .apply_session_extension(lash_protocol_rlm::rlm_session_projection_extension(
            lash_protocol_rlm::RlmProjectedBindings::new()
                .bind_json("session_projection", serde_json::json!("session:durable"))
                .expect("valid session projection"),
        ))
        .await?;
    session
        .enqueue(TurnInput::text("switch frames with a durable seed"))
        .id("queued-continue-as-seed")
        .send()
        .await?;

    let turn_session = session.clone();
    let turn = tokio::spawn(async move { turn_session.queued_turn().run().await });
    tokio::time::timeout(std::time::Duration::from_secs(1), first_provider_call_rx)
        .await
        .expect("first provider call should start")
        .expect("first provider call signal should arrive");
    session
        .enqueue(TurnInput::text("keep this pending across the frame switch"))
        .id("queued-after-continue-as")
        .send()
        .await?;
    release_first_provider_call.notify_one();
    let output = turn
        .await
        .expect("queued turn task")?
        .expect("queued turn should run");

    assert_eq!(
        output.final_value(),
        Some(&serde_json::json!({
            "seed_visible": "seed:durable",
            "session_projection_chars": 15
        })),
        "the committed frame seed must be installed before the follow turn links: {output:?}; repair_request={:?}",
        repair_request.lock_recover()
    );
    assert_eq!(provider_call_count.load(Ordering::SeqCst), 3);
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
fn leaf_bearing_rlm_append_stale_branch_rolls_back_projection() -> Result<()> {
    run_async_test_on_stack_budget("rlm-leaf-append-stale-rollback-test", || async {
        let retained_payload =
            "x".repeat(lash_core::plugin::EXECUTION_STATE_LEAF_MIN_BODY_BYTES * 2);
        let source =
            format!("retained = [{{ payload: {retained_payload:?} }}]\nfinish \"committed\"");
        let core = explicit_ephemeral_facets(LashCore::rlm_builder(
            crate::TurnBudget::Unbounded,
            rlm_factory(),
        ))
        .provider(queued_text_provider(vec![lashlang_block(&source)]))
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .disable_queued_work_driver()
        .build(crate::testing::runtime_lease_owner())?;
        let session = core
            .session("rlm-leaf-append-stale-rollback")
            .open()
            .await?;
        session
            .turn(TurnInput::text("commit leaf-bearing state"))
            .run()
            .await?;

        let execution_before = session
            .admin()
            .state()
            .snapshot_execution()
            .await?
            .expect("RLM has live execution state after the committed turn");
        assert!(
            !execution_before.components.is_empty(),
            "the committed RLM state must contain at least one keyed leaf"
        );

        const ROLLED_BACK_MARKER: &str = "must-not-survive-stale-append";
        let writer = session.runtime.writer();
        let mut runtime = writer.lock().await;
        let result = runtime
            .append_session_nodes(lash_core::AppendSessionNodesRequest {
                operation_id: "leaf-bearing-stale-append".to_string(),
                nodes: vec![lash_core::SessionAppendNode::message(
                    lash_core::PluginMessage::text(
                        lash_core::MessageRole::User,
                        ROLLED_BACK_MARKER,
                    )
                    .with_id("leaf-bearing-stale-append-message"),
                )],
                requires_ancestor_node_id: Some("inactive-ancestor".to_string()),
            })
            .await?;
        assert!(matches!(
            result,
            lash_core::AppendSessionNodesOutcome::StaleBranch { ref required_node_id }
                if required_node_id == "inactive-ancestor"
        ));
        assert!(
            runtime.read_view().messages().iter().all(|message| message
                .parts
                .iter()
                .all(|part| part.content != ROLLED_BACK_MARKER)),
            "the stale append must be absent from the reconciled RLM history projection"
        );
        session.runtime.publish_from(&runtime);
        drop(runtime);

        let execution_after = session
            .admin()
            .state()
            .snapshot_execution()
            .await?
            .expect("RLM execution state survives the stale append rollback");
        assert_eq!(
            execution_after, execution_before,
            "the stale append rollback must preserve the live leaf-bearing RLM projection"
        );
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct RlmExecutionSnapshotProbe {
    version: u32,
    engine: String,
    globals: std::collections::BTreeMap<String, RlmPersistedValueProbe>,
    files: std::collections::BTreeMap<String, RlmPersistedValueProbe>,
    deferred_resolutions: lash_lashlang_runtime::DeferredResolutionRecord,
}

#[cfg(feature = "rlm")]
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RlmPersistedValueProbe {
    Inline {
        #[serde(with = "serde_bytes")]
        body: Vec<u8>,
    },
    Leaf {
        component: String,
    },
}

#[cfg(feature = "rlm")]
impl RlmExecutionSnapshotProbe {
    fn global(
        &self,
        state: &lash_core::plugin::HydratedExecutionState,
        name: &str,
    ) -> Option<lashlang::Value> {
        let body = match self.globals.get(name)? {
            RlmPersistedValueProbe::Inline { body } => body.as_slice(),
            RlmPersistedValueProbe::Leaf { component } => {
                state.components.get(component)?.as_slice()
            }
        };
        lashlang::Snapshot::from_canonical_bytes(body)
            .ok()?
            .globals()
            .get("value")
            .cloned()
    }

    fn file<'a>(
        &'a self,
        state: &'a lash_core::plugin::HydratedExecutionState,
        path: &str,
    ) -> Option<&'a [u8]> {
        match self.files.get(path)? {
            RlmPersistedValueProbe::Inline { body } => Some(body),
            RlmPersistedValueProbe::Leaf { component } => {
                state.components.get(component).map(Vec::as_slice)
            }
        }
    }
}

#[cfg(feature = "rlm")]
struct ColdReopenFrameState {
    switch_checkpoint_budget_bytes: usize,
    resident_execution_state: lash_core::plugin::HydratedExecutionState,
    execution_state: lash_core::plugin::HydratedExecutionState,
}

#[cfg(feature = "rlm")]
async fn frame_switch_state_after_cold_reopen(
    session_id: &str,
    abandoned_global_bytes: usize,
) -> Result<ColdReopenFrameState> {
    let dir = tempfile::tempdir().expect("tempdir");
    let sqlite_store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.path().join("sessions"),
    ));
    let checkpoint_writes =
        lash_core::testing::checkpoint_observer::CheckpointWriteCollector::default();
    let store_factory = Arc::new(
        lash_core::testing::checkpoint_observer::ObservedSessionStoreFactory::new(
            sqlite_store_factory.clone() as Arc<dyn lash_core::SessionStoreFactory>,
            checkpoint_writes.clone(),
        ),
    );
    let abandoned_value = "x".repeat(abandoned_global_bytes);
    let switch_source = format!(
        r#"abandoned_global = {abandoned_value:?}
probe_result = await fixture.probe({{}})?
await control.continue_as({{ task: "finish after cold reopen", seed: {{ frame_seed: "seed:survives" }} }})?"#
    );
    let first_factory =
        rlm_factory().with_deferred_tool_resolver(Arc::new(FrameStateDeferredResolver));
    let first_core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        first_factory,
    ))
    .provider(queued_text_provider(vec![lashlang_block(&switch_source)]))
    .model(mock_model_spec())
    .store_factory(store_factory.clone())
    .tools(Arc::new(FrameStateDeferredTools))
    .plugin(Arc::new(StopAfterFrameSwitchCommitFactory))
    .disable_queued_work_driver()
    .build(crate::testing::runtime_lease_owner())?;
    let first_session = first_core.session(session_id).open().await?;

    let mut initial_execution_state = first_session
        .admin()
        .state()
        .snapshot_execution()
        .await?
        .expect("RLM has an execution snapshot");
    let mut initial_root: RlmExecutionSnapshotProbe =
        rmp_serde::from_slice(&initial_execution_state.root)
            .expect("decode RLM execution snapshot");
    let old_file_body = b"scratch:abandoned".to_vec();
    let old_file_component = format!(
        "execution_state/sha256/{:x}",
        sha2::Sha256::digest(&old_file_body)
    );
    initial_root.files.insert(
        "old-frame.txt".to_string(),
        RlmPersistedValueProbe::Leaf {
            component: old_file_component.clone(),
        },
    );
    initial_execution_state
        .components
        .insert(old_file_component, old_file_body);
    initial_execution_state.root =
        rmp_serde::to_vec_named(&initial_root).expect("encode mutated RLM execution snapshot");
    first_session
        .admin()
        .state()
        .restore_execution(&initial_execution_state)
        .await?;

    let switched = first_session
        .turn(TurnInput::text("switch away from the abandoned frame"))
        .run()
        .await?;
    assert!(matches!(
        switched.result.outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));
    assert!(
        switched.result.errors.iter().any(|issue| issue
            .message
            .contains("stop after the accepted frame-switch commit")),
        "the test hook must stop automatic follow-through only after the switch commit: {switched:?}"
    );
    let switch_turn_index = switched.result.state.turn_index;

    let resident_execution_state = first_session
        .admin()
        .state()
        .snapshot_execution()
        .await?
        .expect("resident switched RLM has an execution snapshot");

    drop(switched);
    drop(first_session);
    drop(first_core);

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let conn = rusqlite::Connection::open(sqlite_store_factory.catalog_path())
                .expect("open SQLite session catalog");
            let owner = conn
                .query_row(
                    "SELECT lease_owner_id FROM session_execution_leases WHERE session_id = ?1",
                    [session_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .expect("read session execution lease row");
            if owner.is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("dropped runtime releases its session lane");

    let store_request = lash_core::SessionStoreCreateRequest {
        session_id: session_id.to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(crate::TurnBudget::Unbounded),
    };
    let store = lash_core::SessionStoreFactory::open_existing_store(
        sqlite_store_factory.as_ref(),
        &store_request,
    )
    .await
    .expect("open durable session store")
    .expect("frame-switch session is durable");
    let durable = store
        .load_session()
        .await?
        .expect("frame-switch session has a durable head");
    let checkpoint = durable
        .checkpoint
        .as_ref()
        .expect("frame-switch commit has a checkpoint");
    assert!(
        checkpoint
            .component_ref(lash_core::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
            .is_none(),
        "the accepted switch commit and resident reset must agree on cleared execution state"
    );
    let switch_writes = checkpoint_writes
        .events()
        .into_iter()
        .filter(|event| event.session_id == session_id && event.turn_index == switch_turn_index)
        .collect::<Vec<_>>();
    assert_eq!(
        switch_writes.len(),
        1,
        "observe exactly one RuntimeCommit for the accepted frame-switch turn: {switch_writes:?}"
    );
    let switch_checkpoint_budget_bytes = checkpoint_writes
        .runtime_commit_budget(session_id, switch_writes[0].revision_before)
        .expect("observe the accepted frame-switch RuntimeCommit before its transaction")
        .checkpoint_bytes;
    drop(store);
    drop(durable);

    let reopened_core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![lashlang_block(
        r#"finish "unused after state inspection""#,
    )]))
    .model(mock_model_spec())
    .store_factory(sqlite_store_factory)
    .disable_queued_work_driver()
    .build(crate::testing::runtime_lease_owner())?;
    let reopened_session = reopened_core.session(session_id).open().await?;
    let execution_state = reopened_session
        .admin()
        .state()
        .snapshot_execution()
        .await?
        .expect("reopened RLM has an execution snapshot");

    Ok(ColdReopenFrameState {
        switch_checkpoint_budget_bytes,
        resident_execution_state,
        execution_state,
    })
}

#[cfg(feature = "rlm")]
#[test]
fn agent_frame_switch_clears_execution_state_across_cold_reopen() -> Result<()> {
    run_async_test_on_stack_budget("agent-frame-switch-cold-reopen-test", || async {
        let small = frame_switch_state_after_cold_reopen("frame-clear-small", 16).await?;
        let large = frame_switch_state_after_cold_reopen("frame-clear-large", 128 * 1024).await?;

        for (geometry, state) in [
            ("resident", &large.resident_execution_state),
            ("cold-reopened", &large.execution_state),
        ] {
            let execution_state: RlmExecutionSnapshotProbe =
                rmp_serde::from_slice(&state.root).expect("decode canonical RLM execution root");
            assert!(
                execution_state.global(state, "abandoned_global").is_none(),
                "the old frame's globals must not survive in the {geometry} executor"
            );
            assert!(matches!(
                execution_state.global(state, "frame_seed"),
                Some(lashlang::Value::String(value)) if value.as_str() == "seed:survives"
            ));

            assert!(
                !execution_state.files.contains_key("old-frame.txt"),
                "the old frame's scratch files must not survive in the {geometry} executor"
            );

            assert!(
                execution_state.deferred_resolutions.is_empty(),
                "the old frame's deferred resolutions must not survive in the {geometry} executor"
            );
        }

        let checkpoint_growth = large
            .switch_checkpoint_budget_bytes
            .abs_diff(small.switch_checkpoint_budget_bytes);
        assert!(
            checkpoint_growth < 1_024,
            "the budgeted RuntimeCommit checkpoint must not scale with 128 KiB of abandoned execution state: small={}, large={}, growth={checkpoint_growth}",
            small.switch_checkpoint_budget_bytes,
            large.switch_checkpoint_budget_bytes
        );
        Ok(())
    })
}

#[cfg(feature = "rlm")]
async fn assert_binary_scratch_files_survive_cold_reopen(
    backend: &str,
    store_factory: Arc<dyn lash_core::SessionStoreFactory>,
) -> Result<()> {
    let snapshot = lash_protocol_rlm::capture_scratch_files_for_testing(vec![
        (
            "inline-invalid-utf8.bin".to_string(),
            vec![0xff, 0xfe, 0x80, 0x00, 0x7f],
        ),
        ("leaf-invalid-utf8.bin".to_string(), vec![0xff; 513]),
    ])?;
    let root: RlmExecutionSnapshotProbe =
        rmp_serde::from_slice(&snapshot.root).expect("decode production-captured RLM root");
    assert!(matches!(
        root.files.get("inline-invalid-utf8.bin"),
        Some(RlmPersistedValueProbe::Inline { .. })
    ));
    assert!(matches!(
        root.files.get("leaf-invalid-utf8.bin"),
        Some(RlmPersistedValueProbe::Leaf { .. })
    ));
    assert_eq!(snapshot.components.len(), 1);

    let session_id = format!("binary-scratch-cold-{backend}-{}", uuid::Uuid::new_v4());
    let first_core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![lashlang_block(
        "checkpoint_marker = 1\nfinish checkpoint_marker",
    )]))
    .model(mock_model_spec())
    .store_factory(store_factory.clone())
    .disable_queued_work_driver()
    .build(crate::testing::runtime_lease_owner())?;
    let first_session = first_core.session(&session_id).open().await?;
    first_session
        .admin()
        .state()
        .restore_execution(&snapshot)
        .await?;
    first_session
        .turn(TurnInput::text("commit the restored scratch files"))
        .run()
        .await?;
    drop(first_session);
    drop(first_core);

    let reopened_core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(Vec::<String>::new()))
    .model(mock_model_spec())
    .store_factory(store_factory)
    .disable_queued_work_driver()
    .build(crate::testing::runtime_lease_owner())?;
    let reopened = reopened_core.session(&session_id).open().await?;
    let cold = reopened
        .admin()
        .state()
        .snapshot_execution()
        .await?
        .expect("cold-reopened RLM has execution state");
    let cold_root: RlmExecutionSnapshotProbe =
        rmp_serde::from_slice(&cold.root).expect("decode cold RLM root");
    assert!(matches!(
        cold_root.files.get("inline-invalid-utf8.bin"),
        Some(RlmPersistedValueProbe::Inline { .. })
    ));
    assert!(matches!(
        cold_root.files.get("leaf-invalid-utf8.bin"),
        Some(RlmPersistedValueProbe::Leaf { .. })
    ));
    assert_eq!(cold.components.len(), 1);
    assert_eq!(
        cold_root.file(&cold, "inline-invalid-utf8.bin"),
        Some([0xff, 0xfe, 0x80, 0x00, 0x7f].as_slice())
    );
    let leaf = cold_root
        .file(&cold, "leaf-invalid-utf8.bin")
        .expect("cold leaf file body");
    assert_eq!(leaf.len(), 513);
    assert_eq!(
        format!("{:x}", sha2::Sha256::digest(leaf)),
        "ea032debaa72c17dae01588597abe1bf263f08612fe41bd4a599e6b3480f0bec"
    );
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
fn binary_scratch_files_survive_store_backed_cold_reopen_byte_exactly() -> Result<()> {
    run_async_test_on_stack_budget("rlm-binary-scratch-cold-reopen-test", || async {
        let in_memory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
        assert_binary_scratch_files_survive_cold_reopen("in-memory", in_memory).await?;

        let dir = tempfile::tempdir().expect("tempdir");
        let sqlite = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            dir.path().join("sessions"),
        ));
        assert_binary_scratch_files_survive_cold_reopen("sqlite", sqlite).await?;

        match std::env::var("LASH_POSTGRES_DATABASE_URL") {
            Ok(database_url) if !database_url.is_empty() => {
                // Own database, not the shared one: the Postgres conformance
                // suites truncate every `lash_*` table, and this law's rows
                // would vanish mid-run beside them.
                let database =
                    lash_postgres_store::testing::IsolatedDatabase::create(&database_url).await;
                let storage = lash_postgres_store::PostgresStorage::connect(database.url()).await?;
                let postgres = Arc::new(storage.session_store_factory());
                assert_binary_scratch_files_survive_cold_reopen("postgres", postgres).await?;
                storage.pool().close().await;
            }
            _ if std::env::var("LASH_REQUIRE_POSTGRES").as_deref() == Ok("1") => {
                panic!("LASH_POSTGRES_DATABASE_URL must be set when LASH_REQUIRE_POSTGRES=1")
            }
            _ => eprintln!("skipping PostgreSQL binary scratch cold-reopen law: not configured"),
        }
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
fn durable_queued_chained_continue_as_survives_nested_commit_handoff() -> Result<()> {
    run_async_test_on_stack_budget("durable-queued-chained-continue-as-test", || {
        durable_queued_chained_continue_as_survives_nested_commit_handoff_inner()
    })
}

#[cfg(feature = "rlm")]
async fn durable_queued_chained_continue_as_survives_nested_commit_handoff_inner() -> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "durable-queued-chained-continue-as";
    let append_count = Arc::new(AtomicUsize::new(0));
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.path().join("sessions"),
    ));
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![
        lashlang_block(r#"await control.continue_as({ task: "switch again" })?"#),
        lashlang_block(r#"await control.continue_as({ task: "finish chain" })?"#),
        lashlang_block(r#"finish "done after chained handoffs""#),
    ]))
    .model(mock_model_spec())
    .store_factory(store_factory.clone())
    .plugin(Arc::new(TurnPersistedGraphAppendFactory {
        append_count: Arc::clone(&append_count),
        max_appends: 2,
    }))
    .disable_queued_work_driver()
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(session_id).open().await?;
    session
        .enqueue(TurnInput::text("start chained frame handoff"))
        .id("queued-chained-continue-as")
        .send()
        .await?;

    let output = session
        .queued_turn()
        .run()
        .await?
        .expect("queued chained turn should run");

    assert_eq!(append_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        output.final_value(),
        Some(&serde_json::json!("done after chained handoffs"))
    );
    assert_sqlite_session_lane_free_at_generation(store_factory.as_ref(), session_id, 1);
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
    let core = LashCore::standard_builder(crate::TurnBudget::Unbounded)
        .provider(agent_frame_switch_provider())
        .model(mock_model_spec())
        .tools(Arc::new(AgentFrameSwitchTools))
        .store_factory(store_factory.clone())
        .attachment_store(Arc::new(crate::persistence::FileAttachmentStore::new(
            dir.path().join("attachments"),
        )))
        .effect_host(Arc::new(
            lash_core::facade_support::InlineEffectHost::default(),
        ))
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .process_env_store(Arc::new(DurableInMemoryProcessEnvStore::default()))
        .build(crate::testing::runtime_lease_owner())?;
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

    let conn = rusqlite::Connection::open(store_factory.catalog_path())
        .expect("open session sqlite store");
    let mut stmt = conn
        .prepare(
            "SELECT turn_id FROM runtime_turn_commits
             WHERE session_id = ?1 ORDER BY turn_id ASC",
        )
        .expect("prepare turn commits");
    let turn_commit_ids = stmt
        .query_map([session_id], |row| row.get::<_, String>(0))
        .expect("query turn commits")
        .map(|row| row.expect("read turn commit row"))
        .map(|encoded| {
            serde_json::from_str::<lash_core::OperationId>(&encoded)
                .expect("decode commit operation")
                .scope
                .turn_id()
                .expect("turn-scoped commit")
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        turn_commit_ids,
        vec![root_turn_id.to_string(), follow_turn_id]
    );
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
fn processes_lists_started_lashlang_process_until_awaited() -> Result<()> {
    run_async_test_on_stack_budget("process-control-lashlang-process-test", || {
        processes_lists_started_lashlang_process_until_awaited_inner()
    })
}

#[cfg(feature = "rlm")]
async fn processes_lists_started_lashlang_process_until_awaited_inner() -> Result<()> {
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
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
    // A started (`start lookup(...)`) process runs in the lease-protected
    // worker's rebuilt runtime, which needs a session store factory; the
    // explicit in-memory factory backs ephemeral process execution.
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .process_registry(Arc::new(TestLocalProcessRegistry::default()))
    .build(crate::testing::runtime_lease_owner())?;
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

#[cfg(feature = "rlm")]
#[test]
fn lashlang_execution_graph_store_observes_lashlang_process_from_facade() -> Result<()> {
    run_async_test_on_stack_budget("lashlang-graph-store-facade-test", || {
        lashlang_execution_graph_store_observes_lashlang_process_from_facade_inner()
    })
}

#[cfg(feature = "rlm")]
async fn lashlang_execution_graph_store_observes_lashlang_process_from_facade_inner() -> Result<()>
{
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let graph_store = Arc::new(crate::tracing::TraceLashlangGraphStore::default());
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
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
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .process_registry(Arc::new(TestLocalProcessRegistry::default()))
    .build(crate::testing::runtime_lease_owner())?;
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
        lash_lashlang_runtime::TraceLanguageExecutionStatus::Running
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

#[cfg(feature = "rlm")]
#[tokio::test]
async fn natural_rlm_completion_emits_no_terminal_output() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec!["done in prose"]))
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("rlm-prose-completion").open().await?;
    let events = Arc::new(RecordingEvents::default());

    let result = session
        .turn(TurnInput::text("answer directly"))
        .allow_prose_or_finish()?
        .stream_to(events.as_ref())
        .await?;

    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { .. })
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

#[cfg(feature = "rlm")]
#[tokio::test]
async fn finish_required_rlm_completion_emits_terminal_output() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![lashlang_block(
        r#"finish "done via finish""#,
    )]))
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
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
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue { .. })
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

#[cfg(feature = "rlm")]
#[tokio::test]
async fn rlm_failed_code_emits_failed_code_completion_without_fake_tools() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![
        lashlang_block("this is not valid lashlang"),
        lashlang_block(r#"finish "recovered""#),
    ]))
    .model(mock_model_spec())
    .tools(Arc::new(AppTools))
    .build(crate::testing::runtime_lease_owner())?;
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

/// FIG-1573: a hard-killed host leaves a live session-execution-lease row; the
/// reopened process must claim its queued turn within one lease TTL.
///
/// Field shape (hirsel, durable SQLite): `send_message` was accepted onto the
/// queued-work path and the host process was killed before the drain claimed
/// it. The lease row the dead boot left behind cannot be released by anyone,
/// so the store's expiry check is the only thing that frees the lane. The
/// reopened process then drains every 30s and reports "claimed nothing
/// (session execution lease busy)" indefinitely.
#[tokio::test]
async fn fig1573_queued_turn_claims_after_a_hard_killed_boot_left_a_live_lane() -> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "fig1573-agent-g1";
    let clock = Arc::new(lash_core::testing::TestClock::new(1_700_000_000_000));
    let store_factory = Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new(dir.path().join("sessions"))
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>),
    );

    // Boot 1: the host accepts a queued turn, then is hard-killed.
    let first_core =
        explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
            .provider(
                crate::testing::TestProvider::builder()
                    .kind("fig1573-boot-1")
                    .complete(|_request| async { Ok(text_response("boot one must not answer")) })
                    .build()
                    .into_handle(),
            )
            .model(mock_model_spec())
            .clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>)
            .store_factory(store_factory.clone())
            .disable_queued_work_driver()
            .build(crate::testing::runtime_lease_owner())?;
    let first_session = first_core.session(session_id).open().await?;
    first_session
        .enqueue(TurnInput::text("what is the status of the migration?"))
        .id("fig1573-queued-request")
        .send()
        .await?;
    drop(first_session);
    drop(first_core);

    // The lane a SIGTERM leaves behind: a live lease row owned by a boot that
    // will never renew and never release it. Taking it on a bare store handle
    // and dropping the handle reproduces that row exactly - an in-process guard
    // drop would spawn the best-effort release a killed process never performs.
    let dead_boot_store = lash_core::SessionStoreFactory::create_store(
        store_factory.as_ref(),
        &lash_core::SessionStoreCreateRequest {
            session_id: session_id.to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(crate::TurnBudget::Unbounded),
        },
    )
    .await?;
    let dead_lane = dead_boot_store
        .try_claim_session_execution_lease(
            session_id,
            &lash_core::LeaseOwnerIdentity::opaque("fig1573-host", "fig1573-host:boot-1"),
            "fig1573-boot-1-executor",
            lash_core::facade_support::LeaseTimings::default().ttl_ms(),
        )
        .await?
        .acquired()
        .expect("the dying boot held the lane");
    let dead_lane_expiry = dead_lane.expires_at_epoch_ms;
    std::mem::forget(dead_lane);
    drop(dead_boot_store);

    // Boot 2 comes up 14s later and drains on the field's cadence.
    clock.advance(14_000);
    let second_core =
        explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
            .provider(
                crate::testing::TestProvider::builder()
                    .kind("fig1573-boot-2")
                    .complete(|_request| async { Ok(text_response("the migration is green")) })
                    .build()
                    .into_handle(),
            )
            .model(mock_model_spec())
            .clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>)
            .store_factory(store_factory.clone())
            .disable_queued_work_driver()
            .build(crate::testing::runtime_lease_owner())?;
    let second_session = second_core.session(session_id).open().await?;
    assert_eq!(
        second_session.pending_turn_inputs().await?.len(),
        1,
        "the queued turn is still pending after the reopen"
    );

    let mut claimed_at_ms = None;
    for _attempt in 0..8 {
        if let Some(output) = second_session.queued_turn().run().await?.ran() {
            assert_eq!(output.assistant_message(), Some("the migration is green"));
            claimed_at_ms = Some(lash_core::Clock::timestamp_ms(clock.as_ref()));
            break;
        }
        clock.advance(30_000);
    }

    let claimed_at_ms = claimed_at_ms.expect(
        "the queued turn must be claimed by the reopened process, not wedged behind the dead \
         boot's lease row",
    );
    assert!(
        claimed_at_ms >= dead_lane_expiry && claimed_at_ms < dead_lane_expiry + 60_000,
        "the drain must claim on the first probe after the dead boot's lease expires \
         (claimed at {claimed_at_ms}, dead lane expired at {dead_lane_expiry})"
    );
    Ok(())
}

/// FIG-1573: an active-turn-scoped input orphaned by a hard kill must become
/// drainable again in the reopened process.
///
/// A host that routes `send_message` into the turn currently running writes a
/// `pending_active` row scoped to that turn id. The only thing that ever moves
/// such a row back to `deferred_next_turn` is the interrupted-input re-defer
/// carried by that same turn's own final commit
/// (`RuntimeCommit::deferring_interrupted_turn_inputs`, applied by
/// `commit_runtime_turn`). A hard kill skips that commit, and nothing at
/// session reopen re-defers the row: the next-turn drain matches only
/// `state = 'deferred_next_turn'`, and an active-turn claim would have to name
/// a turn id that can never exist again. Before the fix the row stayed visible
/// to `pending_turn_inputs` forever while every drain claimed nothing - the
/// field signature in FIG-1573.
///
/// The regression law: the drain-time backstop repairs the row and the reopened
/// process delivers it in that same drain. Remove
/// `defer_orphaned_turn_inputs_before_drain` from `stream_queued_work` and this
/// test goes red again - the dying boot released its lease on the way out, so
/// the successor observes no displacement and nothing else in the reopened
/// process ever reaches those rows.
#[tokio::test]
async fn fig1573_active_turn_input_orphaned_by_a_hard_kill_is_drained_after_reopen() -> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "fig1573-orphaned-active-input";
    let interrupted_turn_id = "fig1573-interrupted-turn";
    let clock = Arc::new(lash_core::testing::TestClock::new(1_700_000_000_000));
    let store_factory = Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new(dir.path().join("sessions"))
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>),
    );

    // Boot 1: a turn is running and the host routes an input into it. The
    // provider never answers, so the turn never reaches its final commit - the
    // only writer of the interrupted-input re-defer.
    let provider_entered = Arc::new(tokio::sync::Notify::new());
    let entered = Arc::clone(&provider_entered);
    let first_core =
        explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
            .provider(
                crate::testing::TestProvider::builder()
                    .kind("fig1573-hung-boot-1")
                    .complete(move |_request| {
                        let entered = Arc::clone(&entered);
                        async move {
                            entered.notify_one();
                            std::future::pending::<()>().await;
                            unreachable!("the killed boot never answers")
                        }
                    })
                    .build()
                    .into_handle(),
            )
            .model(mock_model_spec())
            .clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>)
            .store_factory(store_factory.clone())
            .disable_queued_work_driver()
            .build(crate::testing::runtime_lease_owner())?;
    let first_session = first_core.session(session_id).open().await?;
    first_session
        .enqueue(TurnInput::text("what is the status of the migration?"))
        .id("fig1573-queued-request")
        .ingress(lash_core::TurnInputIngress::active_turn(
            interrupted_turn_id,
            lash_core::TurnInputCheckpointBoundary::default(),
        ))
        .send()
        .await?;
    {
        let running = first_session
            .turn(TurnInput::text("start the long turn"))
            .turn_id(interrupted_turn_id)
            .run();
        let mut running = std::pin::pin!(running);
        tokio::select! {
            _ = &mut running => panic!("the hung provider must not complete the turn"),
            () = provider_entered.notified() => {}
        }
        // Dropping the in-flight turn and the core is the hard kill: no final
        // commit, so no interrupted-input re-defer is ever written.
    }
    drop(first_session);
    drop(first_core);

    // Boot 2 reopens and drains on the field's cadence for well past any TTL.
    clock.advance(14_000);
    let second_core =
        explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
            .provider(
                crate::testing::TestProvider::builder()
                    .kind("fig1573-boot-2")
                    .complete(|_request| async { Ok(text_response("the migration is green")) })
                    .build()
                    .into_handle(),
            )
            .model(mock_model_spec())
            .clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>)
            .store_factory(store_factory.clone())
            .disable_queued_work_driver()
            .build(crate::testing::runtime_lease_owner())?;
    let second_session = second_core.session(session_id).open().await?;
    assert_eq!(
        second_session.pending_turn_inputs().await?.len(),
        1,
        "the orphaned input is still reported pending after the reopen"
    );

    let mut claimed = None;
    for _attempt in 0..10 {
        if let Some(output) = second_session.queued_turn().run().await?.ran() {
            claimed = Some(output);
            break;
        }
        clock.advance(30_000);
    }

    let output = claimed.expect(
        "the reopened process must drain the orphaned input; without the drain-time backstop \
         every drain claims nothing, because the row is stuck in pending_active with a turn id \
         that no longer exists",
    );
    assert_eq!(output.assistant_message(), Some("the migration is green"));
    assert!(
        second_session.pending_turn_inputs().await?.is_empty(),
        "the drained input must leave the pending queue"
    );
    Ok(())
}
