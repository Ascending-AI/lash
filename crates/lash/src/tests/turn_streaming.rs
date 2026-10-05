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

impl lash_core::plugin::PluginDefinition for QueuedWorkHydrationProbeFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("queued-work-hydration-probe")
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
struct TurnPersistedObserverFactory {
    observation_count: Arc<AtomicUsize>,
    max_failures: usize,
}

#[cfg(feature = "rlm")]
impl lash_core::facade_support::PluginFactory for TurnPersistedObserverFactory {
    fn id(&self) -> &'static str {
        "turn-persisted-observer"
    }

    fn build(
        &self,
        _ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        Ok(Arc::new(TurnPersistedObserverPlugin {
            observation_count: Arc::clone(&self.observation_count),
            max_failures: self.max_failures,
        }))
    }
}

#[cfg(feature = "rlm")]
impl lash_core::plugin::PluginDefinition for TurnPersistedObserverFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("turn-persisted-observer")
    }
}

#[cfg(feature = "rlm")]
struct TurnPersistedObserverPlugin {
    observation_count: Arc<AtomicUsize>,
    max_failures: usize,
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
    .expect("valid declared tool schemas")
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
        _cx: &lash_lashlang_runtime::DeferredResolveContext<'_>,
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
impl lash_core::facade_support::SessionPlugin for TurnPersistedObserverPlugin {
    fn id(&self) -> &'static str {
        "turn-persisted-observer"
    }

    fn register(
        &self,
        reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        let observation_count = Arc::clone(&self.observation_count);
        let max_failures = self.max_failures;
        reg.session().on_event(
            crate::hook_key!("session-on-event-1"),
            Arc::new(move |event| {
                let observation_count = Arc::clone(&observation_count);
                Box::pin(async move {
                    let lash_core::facade_support::PluginLifecycleEvent::TurnPersisted(ctx) = event
                    else {
                        return Ok(());
                    };
                    let Ok(_observation_index) = observation_count.fetch_update(
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                        |current| (current < max_failures).then_some(current + 1),
                    ) else {
                        return Ok(());
                    };
                    assert_eq!(ctx.state.session_id(), &ctx.session_id);
                    Err(lash_core::PluginError::Session(
                        "observer sink unavailable".into(),
                    ))
                })
            }),
        )?;
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
