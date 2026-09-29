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
