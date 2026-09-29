use super::*;
#[cfg(feature = "rlm")]
use crate::rlm::RlmSendBuilderExt as _;
use futures_util::StreamExt as _;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use lash_sansio::sync::{LockResultExt, MutexExt};

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

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            assert_eq!(call.name(), "frame_state_probe");
            lash_core::ToolOutcome::ok(serde_json::json!("recorded"))
        })
        .await
        .into()
    }
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
    inner: Arc<dyn lash_core::SessionStoreFactory>,
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
            self.inner.as_ref(),
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
            self.inner.as_ref(),
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

    // A Durable Session acquires through the non-creating by-id seam. This
    // fixture still leaves `open_existing_store` unimplemented, so the queued
    // driver's admissibility read stays "unknown" — the property under test.
    async fn open_existing_store_by_id(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<Option<Arc<dyn lash_core::RuntimePersistence>>, lash_core::StoreError>
    {
        lash_core::SessionStoreFactory::open_existing_store_by_id(self.inner.as_ref(), session_id)
            .await
    }

    async fn session_was_deleted(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<bool, String> {
        lash_core::SessionStoreFactory::session_was_deleted(self.inner.as_ref(), session_id).await
    }

    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> lash_core::MaintenanceResult<lash_core::SessionBlobReclaimReport> {
        self.inner.delete_session(session_id).await
    }

    // A decorator forwards the deployment turn count to the catalog it wraps.
    async fn count_unsettled_turns(
        &self,
    ) -> std::result::Result<lash_core::store::UnsettledTurnCounts, lash_core::StoreError> {
        self.inner.count_unsettled_turns().await
    }

    async fn list_turn_parks(
        &self,
        query: &lash_core::store::TurnParkQuery,
    ) -> std::result::Result<Vec<lash_core::store::TurnPark>, lash_core::StoreError> {
        self.inner.list_turn_parks(query).await
    }

    async fn turn_park_feed(
        &self,
        after: lash_core::store::ParkFeedCursor,
        limit: std::num::NonZeroUsize,
    ) -> std::result::Result<
        lash_core::store::ParkFeedPage<lash_core::store::TurnParkTarget>,
        lash_core::StoreError,
    > {
        self.inner.turn_park_feed(after, limit).await
    }

    async fn root_terminal(
        &self,
        session_id: &lash_core::SessionId,
        root: &lash_core::TurnId,
    ) -> std::result::Result<Option<lash_core::store::RootTerminal>, lash_core::StoreError> {
        self.inner.root_terminal(session_id, root).await
    }

    async fn compact_turn_park_feed(
        &self,
        through: lash_core::store::ParkFeedCursor,
    ) -> std::result::Result<(), lash_core::StoreError> {
        self.inner.compact_turn_park_feed(through).await
    }
}

#[async_trait::async_trait]
impl lash_core::store::ControlIntentStore for CreateOnlySessionStoreFactory {
    async fn begin_session_close(
        &self,
        session_id: &SessionId,
        at_ms: u64,
    ) -> std::result::Result<Option<lash_core::store::ControlIntent>, lash_core::StoreError> {
        self.inner.begin_session_close(session_id, at_ms).await
    }

    async fn claim_intent_application(
        &self,
        id: lash_core::store::ControlIntentId,
        at_ms: u64,
    ) -> std::result::Result<lash_core::store::IntentApplication, lash_core::StoreError> {
        self.inner.claim_intent_application(id, at_ms).await
    }

    async fn acknowledge_intent(
        &self,
        id: lash_core::store::ControlIntentId,
        claim: &lash_core::store::ClaimToken,
        at_ms: u64,
    ) -> std::result::Result<lash_core::store::IntentSettle, lash_core::StoreError> {
        self.inner.acknowledge_intent(id, claim, at_ms).await
    }

    async fn record_intent_failure(
        &self,
        id: lash_core::store::ControlIntentId,
        claim: &lash_core::store::ClaimToken,
        error: &str,
        retryable: bool,
        at_ms: u64,
    ) -> std::result::Result<lash_core::store::IntentSettle, lash_core::StoreError> {
        self.inner
            .record_intent_failure(id, claim, error, retryable, at_ms)
            .await
    }

    async fn load_intent(
        &self,
        id: lash_core::store::ControlIntentId,
    ) -> std::result::Result<Option<lash_core::store::ControlIntent>, lash_core::StoreError> {
        self.inner.load_intent(id).await
    }
}

/// The replay keys of every effect run `double` journaled, across all its
/// invocations: the engine journals each effect as a `ctx.run` named
/// `lash:<replay key>`.
pub(super) fn journaled_run_keys(double: &lash_restate_test::RestateTestBackend) -> Vec<String> {
    let server = double.server();
    server
        .invocations()
        .into_iter()
        .flat_map(|invocation| server.journal(&invocation.id).unwrap_or_default())
        .filter(|entry| entry.ty == lash_restate_test::protocol::MessageType::RunCommand)
        .filter_map(|entry| {
            entry
                .name
                .as_deref()
                .and_then(|name| name.strip_prefix("lash:"))
                .map(str::to_owned)
        })
        .collect()
}

/// The journaled model-call keys among [`journaled_run_keys`].
pub(super) fn journaled_llm_call_keys(
    double: &lash_restate_test::RestateTestBackend,
) -> Vec<String> {
    journaled_run_keys(double)
        .into_iter()
        .filter(|key| key.contains(":llm_call:"))
        .collect()
}

/// Every effect outcome `double`'s engine journaled, in journal order across
/// its invocations: each effect's `ctx.run` completes with its recorded
/// envelope and its outcome, a `Result` whose `Ok` is the effect's outcome.
#[cfg(feature = "rlm")]
fn journaled_effect_outcomes(
    double: &lash_restate_test::RestateTestBackend,
) -> Vec<lash_core::RuntimeEffectOutcome> {
    let server = double.server();
    server
        .invocations()
        .into_iter()
        .flat_map(|invocation| server.journal(&invocation.id).unwrap_or_default())
        .filter_map(|entry| entry.run_completion()?.ok())
        .filter_map(|value| {
            let mut record = serde_json::from_slice::<serde_json::Value>(&value).ok()?;
            serde_json::from_value(record.get_mut("outcome")?.get_mut("Ok")?.take()).ok()
        })
        .collect()
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

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async { lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true })) })
            .await
            .into()
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

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            assert_eq!(call.name(), "app_lookup");
            if let Some(tx) = self.entered_tx.lock_recover().take() {
                let _ = tx.send(());
            }
            if let Some(rx) = self.release_rx.lock().await.take() {
                let _ = rx.await;
            }
            lash_core::ToolOutcome::ok(serde_json::json!({ "answer": "ready" }))
        })
        .await
        .into()
    }
}

mod builders_and_queue;
mod control_and_cancel;
mod observations;
mod rlm_processes;
mod rlm_streaming;
