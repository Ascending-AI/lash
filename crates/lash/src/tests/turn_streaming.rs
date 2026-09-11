use super::*;
#[cfg(feature = "rlm")]
use crate::rlm::RlmTurnBuilderExt as _;
use futures_util::StreamExt as _;
use lash_core::QueuedWorkStore as _;
use lash_core::SessionExecutionLeaseStore as _;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use lash_sansio::sync::{LockResultExt, MutexExt};
use std::collections::BTreeSet;

#[derive(Default)]
struct RecordingTurnIds {
    turn_ids: TokioMutex<Vec<String>>,
}

impl RecordingTurnIds {
    async fn snapshot(&self) -> Vec<String> {
        self.turn_ids.lock().await.clone()
    }
}

#[async_trait]
impl TurnActivitySink for RecordingTurnIds {
    async fn emit(&self, _activity: TurnActivity) {}

    async fn emit_for_turn(&self, turn_id: &TurnId, _activity: TurnActivity) {
        self.turn_ids.lock().await.push(turn_id.to_string());
    }
}

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
        self.execute_by_id(&grant.manifest().id, args, context)
            .await
    }
}

#[cfg(feature = "rlm")]
fn assert_sqlite_session_lane_free_at_generation(
    store_factory: &lash_sqlite_store::SqliteSessionStoreFactory,
    session_id: &SessionId,
    expected_generation: u64,
) {
    let conn = rusqlite::Connection::open(store_factory.catalog_path())
        .expect("open SQLite session catalog");
    let (owner, generation) = conn
        .query_row(
            "SELECT lease_owner_id, lease_fencing_token FROM session_execution_leases WHERE session_id = ?1",
            [session_id.as_str()],
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

    async fn session_was_deleted(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<bool, String> {
        lash_core::SessionStoreFactory::session_was_deleted(&self.inner, session_id).await
    }

    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> lash_core::MaintenanceResult<lash_core::SessionBlobReclaimReport> {
        self.inner.delete_session(session_id).await
    }
}

#[derive(Clone, Debug)]
struct DurableEffectInvocation {
    kind: lash_core::RuntimeEffectKind,
    turn_id: Option<TurnId>,
    replay_key: Option<String>,
}

#[derive(Default)]
struct RecordingDurableEffectController {
    invocations: StdMutex<Vec<DurableEffectInvocation>>,
    native: lash_core::facade_support::NativeRuntimeEffectController,
}

impl RecordingDurableEffectController {
    fn invocations(&self) -> Vec<DurableEffectInvocation> {
        self.invocations.lock_recover().clone()
    }
}

#[async_trait]
impl lash_core::AwaitEventResolver for RecordingDurableEffectController {
    async fn await_event_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> std::result::Result<lash_core::AwaitEventKey, lash_core::RuntimeError> {
        self.native.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> std::result::Result<lash_core::ResolveOutcome, lash_core::RuntimeError> {
        self.native.resolve_await_event(key, resolution).await
    }
}

#[async_trait]
impl lash_core::RuntimeEffectController for RecordingDurableEffectController {
    async fn runtime_effect_failure_disposition(
        &self,
        _code: lash_core::RuntimeErrorCode,
    ) -> std::result::Result<lash_core::RuntimeEffectFailureDisposition, lash_core::RuntimeError>
    {
        Ok(lash_core::RuntimeEffectFailureDisposition::AbortInvocation)
    }

    async fn turn_control_participation(
        &self,
    ) -> std::result::Result<lash_core::TurnControlParticipation, lash_core::RuntimeError> {
        Ok(lash_core::TurnControlParticipation::DurableJournaled)
    }

    async fn execute_effect(
        &self,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> std::result::Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError>
    {
        self.invocations
            .lock_recover()
            .push(DurableEffectInvocation {
                kind: envelope.command.kind(),
                turn_id: envelope.invocation.attribution.turn_id.clone(),
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
struct RecordingNativeEffectController {
    invocations: StdMutex<Vec<DurableEffectInvocation>>,
    persisted_outcomes: StdMutex<Vec<String>>,
    native: lash_core::facade_support::NativeRuntimeEffectController,
}

impl RecordingNativeEffectController {
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
impl lash_core::AwaitEventResolver for RecordingNativeEffectController {
    async fn await_event_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> std::result::Result<lash_core::AwaitEventKey, lash_core::RuntimeError> {
        self.native.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> std::result::Result<lash_core::ResolveOutcome, lash_core::RuntimeError> {
        self.native.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> std::result::Result<Option<lash_core::Resolution>, lash_core::RuntimeError> {
        self.native.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> std::result::Result<lash_core::Resolution, lash_core::RuntimeError> {
        self.native.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), lash_core::RuntimeError> {
        self.native
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), lash_core::RuntimeError> {
        self.native
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait]
impl lash_core::RuntimeEffectController for RecordingNativeEffectController {
    async fn execute_effect(
        &self,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> std::result::Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError>
    {
        self.invocations
            .lock_recover()
            .push(DurableEffectInvocation {
                kind: envelope.command.kind(),
                turn_id: envelope.invocation.attribution.turn_id.clone(),
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
struct DurableNoopEffectHost {
    selected_scopes: StdMutex<Vec<lash_core::ExecutionScope>>,
    controller: Arc<RecordingDurableEffectController>,
}

impl DurableNoopEffectHost {
    fn selected_scopes(&self) -> Vec<lash_core::ExecutionScope> {
        self.selected_scopes.lock_recover().clone()
    }

    fn scoped_for<'run>(
        &self,
        scope: lash_core::ExecutionScope,
    ) -> std::result::Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        self.selected_scopes.lock_recover().push(scope.clone());
        let controller: Arc<dyn lash_core::RuntimeEffectController> = self.controller.clone();
        lash_core::ScopedEffectController::shared(controller, scope)
    }
}

#[async_trait]
impl lash_core::AwaitEventResolver for DurableNoopEffectHost {
    async fn prepare_completion_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> std::result::Result<lash_core::CompletionKeyPreparation, lash_core::RuntimeError> {
        self.controller
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }
}

#[async_trait]
impl lash_core::EffectHost for DurableNoopEffectHost {
    fn await_event_resolver(&self) -> &dyn lash_core::AwaitEventResolver {
        self
    }

    async fn prepare_tool_intent(
        &self,
        _sink: &dyn lash_core::ToolIntentOutcomeSink,
        _identity: &lash_core::ToolIntentIdentity,
        _intent: lash_core::ToolIntent,
    ) -> std::result::Result<lash_core::ToolIntentPreparation, lash_core::RuntimeError> {
        Ok(lash_core::ToolIntentPreparation::ControllerOwned)
    }

    async fn record_tool_intent_outcome(
        &self,
        sink: &dyn lash_core::ToolIntentOutcomeSink,
        identity: &lash_core::ToolIntentIdentity,
        submitted: lash_core::ToolIntent,
        outcome: lash_core::ToolIntentExecutionOutcome,
    ) -> std::result::Result<(), lash_core::RuntimeError> {
        sink.retain_in_journal(identity, submitted, outcome).await
    }

    fn scoped<'run>(
        &'run self,
        scope: lash_core::ExecutionScope,
    ) -> std::result::Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        self.scoped_for(scope)
    }

    fn scoped_static(
        &self,
        scope: lash_core::ExecutionScope,
    ) -> std::result::Result<
        Option<lash_core::ScopedEffectController<'static>>,
        lash_core::RuntimeError,
    > {
        Ok(Some(self.scoped_for(scope)?))
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

#[expect(
    unsafe_code,
    reason = "OrchestratingToolDef::from_first_party is lash-core's unsafe capability boundary, and this crate owns the tool contract it registers"
)]
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

mod builders_and_queue;
mod control_and_cancel;
mod observations;
mod rlm_processes;
mod rlm_streaming;

use control_and_cancel::*;
