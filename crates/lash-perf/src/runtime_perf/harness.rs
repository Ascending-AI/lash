use lash_core::facade_support::TurnActivityIdFacadeOps;
use std::{
    collections::HashMap,
    fmt::Write as _,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use lash::{
    LashCore, TurnOutcome,
    messages::MessageRole,
    plugins::{
        PluginError, PluginExtensionContribution, PluginFactory, PluginMessage, PluginRegistrar,
        PluginSessionContext, PluginSpec, SessionPlugin, StaticPluginFactory,
    },
    provider::{ProviderHandle, ProviderOptions, ProviderReliability},
    runtime::SessionSnapshot,
};
use lash_core::SessionHistoryRecord;
use lash_llm_tools::LlmToolsPluginFactory;
use lash_protocol_rlm::RlmTurnInputExt;
use lash_provider_openai::OpenAiCompatibleProvider;
use lash_rlm_types::{RlmProtocolEvent, RlmTrajectoryEntry};
use lash_standard_plugins::{StandardToolStackOptions, standard_tool_stack};
use tokio_util::sync::CancellationToken;

use super::openai_compat::OpenAiCompatBenchServer;
use super::providers::{
    BENCHMARK_MAIL_RECEIVED_SOURCE_TYPE, BenchmarkEchoTool, BenchmarkLargeToolCatalog,
    BenchmarkObliqueTools, BenchmarkProviderControl, BenchmarkWorkbenchMailTool,
    benchmark_provider, benchmark_provider_with_control, benchmark_stream_profile,
};
use super::scenarios::{ExecutionMode, RuntimePerfScenario};
use super::store::{RuntimePerfStore, RuntimePerfStoreFactory};

const DEFAULT_PROMPT: &str =
    "Inspect the current state and reply with exactly: runtime perf benchmark ok";
const HISTORY_EXCHANGES: usize = 18;
const RUNTIME_PERF_MAX_TURNS: usize = 1;

const BENCHMARK_MAIL_RESOURCE: &str = "Mail";
const BENCHMARK_MAIL_ALIAS: &str = "mail";
const BENCHMARK_MAIL_EVENT: &str = "received";

fn benchmark_model_spec() -> lash::ModelSpec {
    lash::ModelSpec::from_token_limits("mock-model", Default::default(), 200_000, None)
        .expect("valid benchmark model spec")
}

trait ExplicitEphemeralFacets: Sized {
    fn with_explicit_ephemeral_facets(self) -> Self;
}

impl ExplicitEphemeralFacets for lash::LashCoreBuilder {
    fn with_explicit_ephemeral_facets(self) -> Self {
        self.effect_host(Arc::new(
            lash::durability::InlineEffectHost::default().allow_process_lifetime_completion_keys(),
        ))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
    }
}

#[derive(Clone)]
pub(crate) enum BenchmarkCore {
    Standard(lash::LashCore),
    Rlm(lash::LashCore),
}

impl BenchmarkCore {
    pub(crate) fn as_lash_core(&self) -> LashCore {
        match self {
            Self::Standard(core) => core.clone(),
            Self::Rlm(core) => core.clone(),
        }
    }

    pub(crate) async fn open_session(&self, session_id: String) -> lash::Result<lash::LashSession> {
        match self {
            Self::Standard(core) => core.session(session_id).open().await,
            Self::Rlm(core) => core.session(session_id).open().await,
        }
    }

    async fn open_session_with_state(
        &self,
        session_id: String,
        store: Arc<dyn lash::persistence::RuntimePersistence>,
        state: lash::persistence::RuntimeSessionState,
    ) -> lash::Result<lash::LashSession> {
        match self {
            Self::Standard(core) => {
                core.session(session_id)
                    .store(store)
                    .open_with_state(state)
                    .await
            }
            Self::Rlm(core) => {
                core.session(session_id)
                    .store(store)
                    .open_with_state(state)
                    .await
            }
        }
    }
}

pub(crate) struct BenchmarkRuntime {
    core: BenchmarkCore,
    session: Option<lash::LashSession>,
    store: Option<Arc<RuntimePerfStore>>,
    provider_control: Option<Arc<BenchmarkProviderControl>>,
    _openai_compat_server: Option<OpenAiCompatBenchServer>,
}

pub(crate) struct RuntimePerfTraceConfig {
    pub(crate) trace_jsonl_path: Option<PathBuf>,
    pub(crate) lashlang_execution_jsonl_path: Option<PathBuf>,
    pub(crate) trace_level: lash::tracing::TraceLevel,
}

impl BenchmarkRuntime {
    pub(crate) fn usage_report(&self) -> lash::usage::SessionUsageReport {
        self.session
            .as_ref()
            .expect("benchmark session")
            .usage_report()
    }

    pub(crate) fn store(&self) -> Arc<RuntimePerfStore> {
        Arc::clone(self.store.as_ref().expect("runtime perf in-memory store"))
    }

    pub(crate) fn core(&self) -> LashCore {
        self.core.as_lash_core()
    }

    pub(crate) async fn reopen_with_state(
        &mut self,
        scenario: RuntimePerfScenario,
        state: lash::persistence::RuntimeSessionState,
    ) -> anyhow::Result<()> {
        if let Some(session) = self.session.take() {
            session.close().await?;
        }
        let store = self.store() as Arc<dyn lash::persistence::RuntimePersistence>;
        self.session = Some(
            self.core
                .open_session_with_state(format!("runtime-perf-{}", scenario.name()), store, state)
                .await?,
        );
        Ok(())
    }

    pub(crate) async fn reopen_session(
        &mut self,
        scenario: RuntimePerfScenario,
    ) -> anyhow::Result<()> {
        if let Some(session) = self.session.take() {
            session.close().await?;
        }
        self.session = Some(
            self.core
                .open_session(format!("runtime-perf-{}", scenario.name()))
                .await?,
        );
        Ok(())
    }

    pub(crate) async fn close(&mut self) -> anyhow::Result<()> {
        if let Some(session) = self.session.take() {
            session.close().await?;
        }
        Ok(())
    }

    pub(crate) async fn set_turn_phase_probe(
        &self,
        probe: Arc<dyn lash::runtime::RuntimeTurnPhaseProbe>,
    ) {
        self.session
            .as_ref()
            .expect("benchmark session")
            .set_turn_phase_probe(probe)
            .await;
    }

    pub(crate) fn turn_scope(&self, turn_id: impl Into<String>) -> lash::runtime::ExecutionScope {
        self.session
            .as_ref()
            .expect("benchmark session")
            .turn_scope(turn_id)
    }

    pub(crate) async fn run_turn(
        &self,
        input: lash::TurnInput,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<lash::TurnResult> {
        let session = self.session.as_ref().expect("benchmark session");
        let effect_host = session.effect_host();
        let scoped_effect_controller = effect_host
            .scoped(session.turn_scope(lash_core::TurnActivityId::fresh().0.to_string()))
            .map_err(anyhow::Error::from)?;
        session
            .turn(input)
            .cancel(cancel)
            .advanced()
            .collect_session_events_with_scope(
                &lash::runtime::NoopEventSink,
                scoped_effect_controller,
            )
            .await
            .map_err(anyhow::Error::from)
    }

    pub(crate) async fn run_turn_with_id(
        &self,
        input: lash::TurnInput,
        turn_id: &str,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<lash::TurnResult> {
        let session = self.session.as_ref().expect("benchmark session");
        let effect_host = session.effect_host();
        let scoped_effect_controller = effect_host
            .scoped(session.turn_scope(turn_id))
            .map_err(anyhow::Error::from)?;
        session
            .turn(input)
            .turn_id(turn_id)
            .cancel(cancel)
            .advanced()
            .collect_session_events_with_scope(
                &lash::runtime::NoopEventSink,
                scoped_effect_controller,
            )
            .await
            .map_err(anyhow::Error::from)
    }

    pub(crate) async fn enqueue_active_turn_input(
        &self,
        turn_id: &str,
        input: lash::TurnInput,
        source_id: &str,
    ) -> anyhow::Result<lash_core::facade_support::TurnInputAcceptanceReceipt> {
        self.session
            .as_ref()
            .expect("benchmark session")
            .enqueue(input)
            .id(source_id)
            .ingress(lash_core::TurnInputIngress::active_turn(
                turn_id,
                lash_core::TurnInputCheckpointBoundary::AfterWork,
            ))
            .send()
            .await
            .map_err(anyhow::Error::from)
    }

    pub(crate) async fn run_cancel_round_trip(
        &self,
        input: lash::TurnInput,
        turn_id: &str,
        cancel: tokio_util::sync::CancellationToken,
        request_id: &str,
    ) -> anyhow::Result<(lash::TurnResult, std::time::Duration)> {
        let control = self
            .provider_control
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("cancel round-trip provider control missing"))?;
        let driver = self.core().turn_work_driver();
        let address = self
            .session
            .as_ref()
            .expect("benchmark session")
            .turn_address(turn_id);
        let turn = self.run_turn_with_id(input, turn_id, cancel);
        tokio::pin!(turn);
        tokio::select! {
            () = control.provider_started.notified() => {}
            result = &mut turn => {
                return result.and_then(|_| Err(anyhow::anyhow!(
                    "cancel round-trip turn completed before the provider parked"
                )));
            }
        }
        let round_trip_started = std::time::Instant::now();
        let receipt = driver
            .request_cancel(
                lash_core::facade_support::TurnCancelRequest::new(
                    address,
                    request_id,
                    Some("runtime-perf".into()),
                )
                .with_reason("measure request-to-token-to-seal"),
            )
            .await?;
        if !matches!(
            receipt.outcome,
            lash_core::facade_support::TurnCancelOutcome::Requested(_)
        ) {
            anyhow::bail!(
                "cancel round-trip request did not win the gate: {:?}",
                receipt.outcome
            );
        }
        turn.await.map(|turn| (turn, round_trip_started.elapsed()))
    }

    pub(crate) async fn run_ingress_claim_projection(
        &self,
        input: lash::TurnInput,
        turn_id: &str,
        cancel: tokio_util::sync::CancellationToken,
        source_id: &str,
    ) -> anyhow::Result<(lash::TurnResult, std::time::Duration)> {
        let control = self
            .provider_control
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("ingress projection provider control missing"))?;
        let turn = self.run_turn_with_id(input, turn_id, cancel);
        tokio::pin!(turn);
        tokio::select! {
            () = control.provider_started.notified() => {}
            result = &mut turn => {
                return result.and_then(|_| Err(anyhow::anyhow!(
                    "ingress projection turn completed before the first provider call parked"
                )));
            }
        }
        let projection_started = std::time::Instant::now();
        self.enqueue_active_turn_input(
            turn_id,
            lash::TurnInput::text("ingress projection marker"),
            source_id,
        )
        .await?;
        control.release_provider.notify_one();
        turn.await.map(|turn| (turn, projection_started.elapsed()))
    }

    pub(crate) async fn run_turn_with_execution_scope(
        &self,
        input: lash::TurnInput,
        cancel: tokio_util::sync::CancellationToken,
        scoped_effect_controller: lash::runtime::ScopedEffectController<'_>,
    ) -> anyhow::Result<lash::TurnResult> {
        self.session
            .as_ref()
            .expect("benchmark session")
            .turn(input)
            .cancel(cancel)
            .advanced()
            .run_with_scope(scoped_effect_controller)
            .await
            .map(|output| output.result)
            .map_err(anyhow::Error::from)
    }

    pub(crate) async fn await_background_work(&self) -> anyhow::Result<()> {
        self.session
            .as_ref()
            .expect("benchmark session")
            .refresh_background_graph()
            .await?;
        Ok(())
    }

    pub(crate) async fn export_state(&self) -> SessionSnapshot {
        self.session
            .as_ref()
            .expect("benchmark session")
            .admin()
            .state()
            .export()
            .await
    }
}
pub(crate) fn validate_runtime_perf_turn(
    scenario: RuntimePerfScenario,
    turn_index: usize,
    turn: &lash::TurnResult,
) -> anyhow::Result<()> {
    let expected = "runtime perf benchmark ok";
    let diagnostics = runtime_perf_turn_diagnostics(turn);
    if !rlm_trajectory_errors(turn).is_empty() {
        anyhow::bail!(
            "runtime perf scenario {} turn {} surfaced RLM execution error:\n{}",
            scenario.name(),
            turn_index + 1,
            diagnostics
        );
    }
    if !turn.errors.is_empty() {
        anyhow::bail!(
            "runtime perf scenario {} turn {} emitted runtime errors:\n{}",
            scenario.name(),
            turn_index + 1,
            diagnostics
        );
    }
    if scenario.execution_mode().is_rlm()
        && matches!(
            turn.outcome,
            TurnOutcome::Finished(lash::TurnFinish::AssistantMessage { .. })
        )
    {
        anyhow::bail!(
            "runtime perf scenario {} turn {} finished through assistant prose; RLM perf scenarios must complete through finish so fixture errors cannot be hidden.\n{}",
            scenario.name(),
            turn_index + 1,
            diagnostics
        );
    }
    match &turn.outcome {
        TurnOutcome::Finished(lash::TurnFinish::AssistantMessage { text }) => {
            let valid = if matches!(scenario, RuntimePerfScenario::OpenAiCompatStream) {
                text.contains(expected) || turn.assistant_output.safe_text.contains(expected)
            } else {
                text.trim() == expected || turn.assistant_output.safe_text.trim() == expected
            };
            if valid {
                return Ok(());
            }
            anyhow::bail!(
                "runtime perf scenario {} turn {} produced unexpected assistant text: {:?}",
                scenario.name(),
                turn_index + 1,
                text
            );
        }
        TurnOutcome::Finished(lash::TurnFinish::FinalValue { value }) => {
            if value.as_str() == Some(expected) {
                return Ok(());
            }
            anyhow::bail!(
                "runtime perf scenario {} turn {} submitted unexpected value: {}",
                scenario.name(),
                turn_index + 1,
                value
            );
        }
        TurnOutcome::Finished(lash::TurnFinish::ToolValue { tool_name, value }) => {
            anyhow::bail!(
                "runtime perf scenario {} turn {} finished with tool value from {}: {}",
                scenario.name(),
                turn_index + 1,
                tool_name,
                value
            );
        }
        TurnOutcome::AgentFrameSwitch { frame_id, .. } => {
            anyhow::bail!(
                "runtime perf scenario {} turn {} unexpectedly switched to agent frame {}",
                scenario.name(),
                turn_index + 1,
                frame_id
            );
        }
        TurnOutcome::Stopped(stop) => {
            anyhow::bail!(
                "runtime perf scenario {} turn {} stopped with {:?}; assistant_output={:?}",
                scenario.name(),
                turn_index + 1,
                stop,
                turn.assistant_output
            );
        }
    }
}

fn rlm_trajectory_errors(turn: &lash::TurnResult) -> Vec<RlmTrajectoryEntry> {
    rlm_trajectory_entries(turn)
        .into_iter()
        .filter(|entry| {
            entry
                .error
                .as_deref()
                .is_some_and(|error| !error.trim().is_empty())
        })
        .collect()
}

fn rlm_trajectory_entries(turn: &lash::TurnResult) -> Vec<RlmTrajectoryEntry> {
    turn.state
        .read_view()
        .active_events()
        .iter()
        .filter_map(|event| {
            let SessionHistoryRecord::Protocol(event) = event else {
                return None;
            };
            match event.decode::<RlmProtocolEvent>(lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID) {
                Ok(Some(RlmProtocolEvent::RlmTrajectoryEntry(entry))) => Some(entry),
                Ok(Some(
                    RlmProtocolEvent::RlmAssistantContent(_)
                    | RlmProtocolEvent::RlmDiagnostic(_)
                    | RlmProtocolEvent::RlmGlobalsPatch(_)
                    | RlmProtocolEvent::RlmSeed(_),
                ))
                | Ok(None)
                | Err(_) => None,
            }
        })
        .collect()
}

fn runtime_perf_turn_diagnostics(turn: &lash::TurnResult) -> String {
    let mut out = String::new();
    if !turn.errors.is_empty() {
        let _ = writeln!(out, "turn_errors:");
        for issue in &turn.errors {
            let code = issue.code.as_deref().unwrap_or("none");
            let _ = writeln!(
                out,
                "- kind={} code={} message={}",
                issue.kind,
                code,
                preview(&issue.message, 600)
            );
        }
    }

    let entries = rlm_trajectory_entries(turn);
    let errors = entries
        .iter()
        .filter(|entry| {
            entry
                .error
                .as_deref()
                .is_some_and(|error| !error.trim().is_empty())
        })
        .collect::<Vec<_>>();
    if !errors.is_empty() {
        let _ = writeln!(out, "rlm_execution_errors:");
        for entry in errors {
            let _ = writeln!(
                out,
                "- iteration={} error={}",
                entry.protocol_iteration,
                preview(entry.error.as_deref().unwrap_or_default(), 900)
            );
            if !entry.code.trim().is_empty() {
                let _ = writeln!(out, "  code={}", preview(&entry.code, 900));
            }
        }
    } else if let Some(entry) = entries.last() {
        let _ = writeln!(
            out,
            "last_rlm_step: iteration={} final_output={}",
            entry.protocol_iteration,
            entry
                .final_output
                .as_ref()
                .map_or_else(|| "none".to_string(), serde_json::Value::to_string)
        );
        if !entry.code.trim().is_empty() {
            let _ = writeln!(out, "last_rlm_code={}", preview(&entry.code, 900));
        }
    }

    if out.trim().is_empty() {
        "no captured turn errors or RLM trajectory entries".to_string()
    } else {
        out
    }
}

fn preview(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let mut preview = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        preview.push_str("...");
    }
    preview.replace('\n', "\\n")
}

pub(crate) fn build_embed_core(
    scenario: RuntimePerfScenario,
    store: Arc<RuntimePerfStore>,
) -> anyhow::Result<BenchmarkCore> {
    let effect_host = Arc::new(
        lash::durability::InlineEffectHost::default().allow_process_lifetime_completion_keys(),
    );
    match scenario {
        RuntimePerfScenario::EmbedStandard => lash::LashCore::standard_builder()
            .with_explicit_ephemeral_facets()
            .effect_host(effect_host.clone())
            .provider(benchmark_provider(scenario).into_handle())
            .model(benchmark_model_spec())
            .store_factory(Arc::new(RuntimePerfStoreFactory::new(store)))
            .build()
            .map(BenchmarkCore::Standard)
            .map_err(anyhow::Error::from),
        RuntimePerfScenario::EmbedRlm => {
            let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
                lash_protocol_rlm::RlmProtocolPluginConfig::default(),
                Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
            );
            lash::LashCore::rlm_builder(factory)
                .with_explicit_ephemeral_facets()
                .effect_host(effect_host.clone())
                .tools(Arc::new(BenchmarkEchoTool::new(effect_host)))
                .provider(benchmark_provider(scenario).into_handle())
                .model(benchmark_model_spec())
                .store_factory(Arc::new(RuntimePerfStoreFactory::new(store)))
                .max_turns(RUNTIME_PERF_MAX_TURNS)
                .build()
                .map(BenchmarkCore::Rlm)
                .map_err(anyhow::Error::from)
        }
        _ => anyhow::bail!("{} is not an embed scenario", scenario.name()),
    }
}

pub(crate) async fn build_runtime_with_store(
    scenario: RuntimePerfScenario,
    store: Option<Arc<RuntimePerfStore>>,
    trace_config: Option<RuntimePerfTraceConfig>,
) -> anyhow::Result<BenchmarkRuntime> {
    let execution_mode = scenario.execution_mode();
    let standard_context_approach = scenario.standard_context_approach();
    let openai_compat_server = if matches!(scenario, RuntimePerfScenario::OpenAiCompatStream) {
        Some(OpenAiCompatBenchServer::start(benchmark_stream_profile(scenario)).await?)
    } else {
        None
    };
    let base_url = openai_compat_server
        .as_ref()
        .map(|server| server.base_url.clone())
        .unwrap_or_else(|| "https://example.invalid/v1".to_string());
    let (provider, provider_control): (ProviderHandle, Option<Arc<BenchmarkProviderControl>>) =
        match scenario {
            RuntimePerfScenario::OpenAiCompatStream => (
                ProviderHandle::new(
                    OpenAiCompatibleProvider::new("test-key", base_url.clone())
                        .with_options(ProviderOptions {
                            reliability: ProviderReliability::disabled(),
                            ..ProviderOptions::default()
                        })
                        .into_components(),
                ),
                None,
            ),
            _ => {
                let (provider, control) = benchmark_provider_with_control(scenario);
                (provider.into_handle(), control)
            }
        };
    let effect_host: Arc<dyn lash_core::EffectHost> =
        if matches!(scenario, RuntimePerfScenario::TurnStartGate) {
            Arc::new(
                lash_core::facade_support::InlineEffectHost::new(Arc::new(
                    RetryingStartGateController::default(),
                ))
                .allow_process_lifetime_completion_keys(),
            )
        } else {
            Arc::new(
                lash_core::facade_support::InlineEffectHost::default()
                    .allow_process_lifetime_completion_keys(),
            )
        };
    let store = store.unwrap_or_else(|| Arc::new(RuntimePerfStore::default()));
    let mut plugin_stack = standard_tool_stack(StandardToolStackOptions {
        standard_context_approach: standard_context_approach.clone(),
        tavily_api_key: None,
        include_cancel_process: execution_mode.is_standard(),
    });
    plugin_stack.push(Arc::new(StaticPluginFactory::new(
        "runtime_perf_tools",
        PluginSpec::new()
            .with_tool_provider(Arc::new(BenchmarkEchoTool::new(Arc::clone(&effect_host)))),
    )));
    if matches!(scenario, RuntimePerfScenario::RlmLlmQuery) {
        plugin_stack.push(Arc::new(LlmToolsPluginFactory::default()));
    }
    if matches!(
        scenario,
        RuntimePerfScenario::RlmSubagentSpawn
            | RuntimePerfScenario::RlmObliqueStackMix
            | RuntimePerfScenario::DeepTurnComposition
    ) {
        plugin_stack.push(Arc::new(lash_subagents::SubagentsPluginFactory::new(
            Arc::new(lash_subagents::CapabilityRegistry::new().with(Arc::new(
                lash_subagents::StaticCapability::new(
                    "default",
                    lash_core::facade_support::SessionSpec::inherit(),
                ),
            ))),
        )));
    }
    if matches!(scenario, RuntimePerfScenario::RlmObliqueStackMix) {
        plugin_stack.push(Arc::new(StaticPluginFactory::new(
            "runtime_perf_oblique_tools",
            PluginSpec::new().with_tool_provider(Arc::new(BenchmarkObliqueTools)),
        )));
    }
    if matches!(
        scenario,
        RuntimePerfScenario::RlmLargeToolCatalog | RuntimePerfScenario::ToolDiscoverySearch
    ) {
        plugin_stack.push(Arc::new(StaticPluginFactory::new(
            "runtime_perf_large_tool_catalog",
            PluginSpec::new().with_tool_provider(Arc::new(BenchmarkLargeToolCatalog::default())),
        )));
    }
    if matches!(
        scenario,
        RuntimePerfScenario::RlmTriggerMailPipeline | RuntimePerfScenario::DeepTurnComposition
    ) {
        plugin_stack.push(Arc::new(BenchmarkWorkbenchTriggerPluginFactory));
    }
    let core = match execution_mode {
        ExecutionMode::Standard => {
            let mut builder = lash::LashCore::standard_builder()
                .with_explicit_ephemeral_facets()
                .effect_host(Arc::clone(&effect_host))
                .provider(provider)
                .model(benchmark_model_spec())
                .plugins(plugin_stack);
            if let Some(config) = trace_config {
                if let Some(path) = config.trace_jsonl_path {
                    builder = builder.trace_jsonl_path(path);
                }
                builder = builder.trace_level(config.trace_level);
            }
            if !matches!(scenario, RuntimePerfScenario::RlmGlobals) {
                builder = builder
                    .process_registry(Arc::new(lash_core::TestLocalProcessRegistry::default()));
            }
            if !matches!(scenario, RuntimePerfScenario::RlmGlobals) {
                builder = builder
                    .store_factory(Arc::new(RuntimePerfStoreFactory::new(Arc::clone(&store))));
            }
            BenchmarkCore::Standard(builder.build()?)
        }
        ExecutionMode::Rlm => {
            let mut factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
                lash_protocol_rlm::RlmProtocolPluginConfig::default(),
                Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
            );
            if let Some(path) = trace_config
                .as_ref()
                .and_then(|config| config.lashlang_execution_jsonl_path.clone())
            {
                factory = factory.with_lashlang_execution_jsonl_path(path);
            }
            let mut builder = lash::LashCore::rlm_builder(factory)
                .with_explicit_ephemeral_facets()
                .effect_host(Arc::clone(&effect_host))
                .provider(provider)
                .model(benchmark_model_spec())
                .plugins(plugin_stack)
                .max_turns(RUNTIME_PERF_MAX_TURNS);
            if let Some(config) = trace_config {
                if let Some(path) = config.trace_jsonl_path {
                    builder = builder.trace_jsonl_path(path);
                }
                builder = builder.trace_level(config.trace_level);
            }
            if !matches!(scenario, RuntimePerfScenario::RlmGlobals) {
                builder = builder
                    .process_registry(Arc::new(lash_core::TestLocalProcessRegistry::default()));
            }
            if !matches!(scenario, RuntimePerfScenario::RlmGlobals) {
                builder = builder
                    .store_factory(Arc::new(RuntimePerfStoreFactory::new(Arc::clone(&store))));
            }
            BenchmarkCore::Rlm(builder.build()?)
        }
    };
    let session = core
        .open_session(format!("runtime-perf-{}", scenario.name()))
        .await?;
    Ok(BenchmarkRuntime {
        core,
        session: Some(session),
        store: Some(store),
        provider_control,
        _openai_compat_server: openai_compat_server,
    })
}

struct RetryingStartGateController {
    attempts_by_key: Mutex<HashMap<String, usize>>,
    delegate: lash_core::facade_support::InlineRuntimeEffectController,
}

impl Default for RetryingStartGateController {
    fn default() -> Self {
        Self {
            attempts_by_key: Mutex::new(HashMap::new()),
            delegate: lash_core::facade_support::InlineRuntimeEffectController::default(),
        }
    }
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for RetryingStartGateController {
    async fn await_event_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> Result<lash_core::AwaitEventKey, lash_core::RuntimeError> {
        self.delegate.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveOutcome, lash_core::RuntimeError> {
        self.delegate.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> Result<Option<lash_core::Resolution>, lash_core::RuntimeError> {
        if matches!(key.wait, lash_core::AwaitEventWaitIdentity::TurnCancelGate) {
            let mut attempts = self
                .attempts_by_key
                .lock()
                .expect("start gate attempt counter");
            let attempt = attempts.entry(key.key_id.clone()).or_default();
            *attempt += 1;
            if *attempt < 3 {
                return Err(lash_core::RuntimeError::new(
                    "runtime_perf_start_gate_retry",
                    "deterministic start-gate retry fixture",
                ));
            }
        }
        self.delegate.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<lash_core::Resolution, lash_core::RuntimeError> {
        self.delegate.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<(), lash_core::RuntimeError> {
        self.delegate
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<(), lash_core::RuntimeError> {
        self.delegate
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for RetryingStartGateController {
    async fn execute_effect(
        &self,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        self.delegate.execute_effect(envelope, local_executor).await
    }
}

struct BenchmarkWorkbenchTriggerPluginFactory;

impl PluginFactory for BenchmarkWorkbenchTriggerPluginFactory {
    fn id(&self) -> &'static str {
        "runtime_perf_workbench_trigger"
    }

    fn extension_contributions(&self) -> Vec<PluginExtensionContribution> {
        vec![
            PluginExtensionContribution::new(
                lash::rlm::LASHLANG_SURFACE_EXTENSION_ID,
                lash::rlm::LashlangSurfaceContribution::new(
                    lash::rlm::LashlangAbilities::default()
                        .with_processes()
                        .with_sleep()
                        .with_process_signals()
                        .with_triggers(),
                    lash::rlm::LashlangLanguageFeatures::default(),
                    benchmark_workbench_lashlang_resources(),
                ),
            )
            .expect("runtime perf lashlang surface serializes"),
        ]
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(BenchmarkWorkbenchTriggerPlugin))
    }
}

struct BenchmarkWorkbenchTriggerPlugin;

impl SessionPlugin for BenchmarkWorkbenchTriggerPlugin {
    fn id(&self) -> &'static str {
        "runtime_perf_workbench_trigger"
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        reg.triggers()
            .declare(lash_core::facade_support::TriggerEvent::new(
                BENCHMARK_MAIL_RESOURCE,
                BENCHMARK_MAIL_ALIAS,
                BENCHMARK_MAIL_EVENT,
                lash_core::LashSchema::new(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "account": { "type": "string" },
                        "title": { "type": "string" },
                        "text": { "type": "string" }
                    },
                    "required": ["account", "title", "text"],
                    "additionalProperties": false
                })),
            ))?;
        reg.tools().provider(Arc::new(BenchmarkWorkbenchMailTool))?;
        Ok(())
    }
}

fn benchmark_workbench_lashlang_resources() -> lash::rlm::LashlangHostCatalog {
    let mut resources = lash::rlm::LashlangHostCatalog::new();
    resources
        .add_trigger_source_constructor(
            BENCHMARK_MAIL_RECEIVED_SOURCE_TYPE.split('.'),
            lash::rlm::TypeExpr::Object(vec![]),
            benchmark_mail_received_event_type(),
        )
        .expect("valid benchmark mail trigger source");
    resources
}

fn benchmark_mail_received_event_type() -> lash::rlm::NamedDataType {
    lash::rlm::NamedDataType::object(
        "mail.Received",
        vec![
            benchmark_field("account", lash::rlm::TypeExpr::Str),
            benchmark_field("title", lash::rlm::TypeExpr::Str),
            benchmark_field("text", lash::rlm::TypeExpr::Str),
        ],
    )
    .expect("valid benchmark mail received type")
}

fn benchmark_field(name: &str, ty: lash::rlm::TypeExpr) -> lash::rlm::TypeField {
    lash::rlm::TypeField {
        name: name.into(),
        ty,
        optional: false,
    }
}

pub(crate) async fn build_runtime_with_sqlite_store(
    scenario: RuntimePerfScenario,
    root: PathBuf,
) -> anyhow::Result<BenchmarkRuntime> {
    let mode_id = scenario.execution_mode();
    let provider = benchmark_provider(scenario).into_handle();
    let mut plugin_stack = standard_tool_stack(StandardToolStackOptions {
        standard_context_approach: scenario.standard_context_approach(),
        tavily_api_key: None,
        include_cancel_process: mode_id.is_standard(),
    });
    let sessions_root = root.join("sessions");
    let attachments_root = root.join("attachments");
    let artifacts_db = root.join("artifacts.db");
    let effects_db = root.join("effects.db");
    let process_env_db = root.join("process-env.db");
    let process_db = root.join("processes.db");
    let triggers_db = root.join("triggers.db");
    let effect_host = Arc::new(
        lash_sqlite_store::SqliteEffectHost::open(&effects_db)
            .await
            .map_err(|err| anyhow::anyhow!(err.to_string()))?,
    );
    plugin_stack.push(Arc::new(StaticPluginFactory::new(
        "runtime_perf_tools",
        PluginSpec::new().with_tool_provider(Arc::new(BenchmarkEchoTool::new(effect_host.clone()))),
    )));
    let attachment_store = Arc::new(lash::persistence::FileAttachmentStore::new(
        attachments_root,
    ));
    let process_env_store = Arc::new(
        lash_sqlite_store::Store::open(&process_env_db)
            .await
            .map_err(|err| anyhow::anyhow!(err.to_string()))?,
    );
    let process_registry = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .map_err(|err| anyhow::anyhow!(err.to_string()))?,
    );
    let trigger_store = Arc::new(
        lash_sqlite_store::SqliteTriggerStore::open(&triggers_db)
            .await
            .map_err(|err| anyhow::anyhow!(err.to_string()))?,
    );
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        sessions_root,
    ));
    let core = match mode_id {
        ExecutionMode::Standard => BenchmarkCore::Standard(
            lash::LashCore::standard_builder()
                .provider(provider)
                .model(benchmark_model_spec())
                .effect_host(effect_host.clone())
                .attachment_store(attachment_store.clone())
                .process_env_store(process_env_store.clone())
                .process_registry(process_registry.clone())
                .trigger_store(trigger_store.clone())
                .store_factory(store_factory.clone())
                .plugins(plugin_stack)
                .build()?,
        ),
        ExecutionMode::Rlm => {
            let artifact_store = Arc::new(
                lash_sqlite_store::Store::open(&artifacts_db)
                    .await
                    .map_err(|err| anyhow::anyhow!(err.to_string()))?,
            );
            let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
                lash_protocol_rlm::RlmProtocolPluginConfig::default(),
                artifact_store,
            );
            BenchmarkCore::Rlm(
                lash::LashCore::rlm_builder(factory)
                    .provider(provider)
                    .model(benchmark_model_spec())
                    .effect_host(effect_host.clone())
                    .attachment_store(attachment_store.clone())
                    .process_env_store(process_env_store.clone())
                    .process_registry(process_registry.clone())
                    .trigger_store(trigger_store.clone())
                    .store_factory(store_factory.clone())
                    .plugins(plugin_stack)
                    .max_turns(RUNTIME_PERF_MAX_TURNS)
                    .build()?,
            )
        }
    };
    let session = core
        .open_session(format!("runtime-perf-{}", scenario.name()))
        .await?;
    Ok(BenchmarkRuntime {
        core,
        session: Some(session),
        store: None,
        provider_control: None,
        _openai_compat_server: None,
    })
}

pub(crate) async fn seed_runtime_state(
    runtime: &mut BenchmarkRuntime,
    scenario: RuntimePerfScenario,
) -> anyhow::Result<()> {
    let mut messages = Vec::with_capacity(HISTORY_EXCHANGES * 2);
    for index in 0..HISTORY_EXCHANGES {
        messages.push(PluginMessage::text(
            MessageRole::User,
            format!(
                "Historical user turn {index}: trace the performance-sensitive path through runtime/session graph/tool prep."
            ),
        ));
        messages.push(PluginMessage::text(
            MessageRole::Assistant,
            format!(
                "Historical assistant turn {index}: inspected runtime.rs, turn_runner.rs, and token ledger export surfaces."
            ),
        ));
    }

    runtime
        .session
        .as_ref()
        .expect("benchmark session")
        .admin()
        .state()
        .append_messages(messages)
        .await
        .map_err(|err| anyhow::anyhow!("seed historical messages: {err}"))?;

    if matches!(scenario, RuntimePerfScenario::RlmGlobals) {
        seed_rlm_live_globals(runtime).await?;
    }

    Ok(())
}

async fn seed_rlm_live_globals(runtime: &mut BenchmarkRuntime) -> anyhow::Result<()> {
    let turn_input =
        lash::TurnInput::text("Seed current working variables, then finish the benchmark marker.")
            .rlm_project(rlm_perf_projected_bindings(
                RuntimePerfScenario::RlmGlobals,
                0,
            )?)?;
    let turn = runtime
        .run_turn(turn_input, CancellationToken::new())
        .await?;
    validate_runtime_perf_turn(RuntimePerfScenario::RlmGlobals, 0, &turn)?;
    runtime.await_background_work().await?;
    Ok(())
}

pub(crate) async fn prepare_turn(
    runtime: &mut BenchmarkRuntime,
    scenario: RuntimePerfScenario,
    turn_index: usize,
) -> anyhow::Result<()> {
    if !matches!(scenario, RuntimePerfScenario::RlmGlobals) {
        return Ok(());
    }

    let _ = runtime;
    let _ = turn_index;
    Ok(())
}

pub(crate) fn rlm_perf_projected_bindings(
    scenario: RuntimePerfScenario,
    turn_index: usize,
) -> anyhow::Result<lash_protocol_rlm::RlmProjectedBindings> {
    Ok(lash_protocol_rlm::RlmProjectedBindings::new()
        .bind_json(
            "benchmark",
            serde_json::json!({
                "name": "runtime_perf",
                "scenario": scenario.name(),
            }),
        )?
        .bind_json(
            "input",
            serde_json::json!({
                "turn": turn_index + 1,
                "goal": "measure runtime overhead across a longer same-session chat",
                "path": "crates/lash/src/runtime",
            }),
        )?
        .bind_json(
            "chat",
            serde_json::json!({
                "turn_count": turn_index + 1,
                "scenario": scenario.name(),
                "mode": "runtime_perf",
            }),
        )?)
}

pub(crate) fn benchmark_prompt(scenario: RuntimePerfScenario, turn_index: usize) -> String {
    match scenario {
        RuntimePerfScenario::Standard | RuntimePerfScenario::EmbedStandard => format!(
            "Turn {} of a longer runtime benchmark conversation. Inspect the state and reply with exactly: {}",
            turn_index + 1,
            DEFAULT_PROMPT
                .rsplit_once(": ")
                .map(|(_, text)| text)
                .unwrap_or("runtime perf benchmark ok")
        ),
        RuntimePerfScenario::Rlm | RuntimePerfScenario::EmbedRlm => format!(
            "Turn {} in RLM mode. Continue the benchmark chat and reply with exactly: {}",
            turn_index + 1,
            DEFAULT_PROMPT
                .rsplit_once(": ")
                .map(|(_, text)| text)
                .unwrap_or("runtime perf benchmark ok")
        ),
        RuntimePerfScenario::RlmLargeToolCatalog => format!(
            "Turn {} in RLM mode with a Gmail-sized callable tool catalog. Do not call a Gmail tool; reply with exactly: {}",
            turn_index + 1,
            DEFAULT_PROMPT
                .rsplit_once(": ")
                .map(|(_, text)| text)
                .unwrap_or("runtime perf benchmark ok")
        ),
        RuntimePerfScenario::RlmObliqueStackMix => format!(
            "Turn {} in RLM mode. Exercise the OBLIQ-style search, subagent, live-handle, direct judge, trace, and large print paths, then finish exactly: {}",
            turn_index + 1,
            DEFAULT_PROMPT
                .rsplit_once(": ")
                .map(|(_, text)| text)
                .unwrap_or("runtime perf benchmark ok")
        ),
        RuntimePerfScenario::RlmToolCalls => format!(
            "Turn {} in RLM mode. Exercise the benchmark_echo tool path and reply with exactly: {}",
            turn_index + 1,
            DEFAULT_PROMPT
                .rsplit_once(": ")
                .map(|(_, text)| text)
                .unwrap_or("runtime perf benchmark ok")
        ),
        RuntimePerfScenario::RlmAsyncToolCompletion => format!(
            "Turn {} in RLM mode. Exercise the pending benchmark_async tool completion path, then finish exactly: {}",
            turn_index + 1,
            DEFAULT_PROMPT
                .rsplit_once(": ")
                .map(|(_, text)| text)
                .unwrap_or("runtime perf benchmark ok")
        ),
        RuntimePerfScenario::StandardToolCalls => format!(
            "Turn {} in standard mode. Use the batch tool to exercise parallel benchmark_echo calls, then reply with exactly: {}",
            turn_index + 1,
            DEFAULT_PROMPT
                .rsplit_once(": ")
                .map(|(_, text)| text)
                .unwrap_or("runtime perf benchmark ok")
        ),
        RuntimePerfScenario::StandardAsyncToolCompletion => format!(
            "Turn {} in standard mode. Launch the async benchmark tool completion, then reply with exactly: {}",
            turn_index + 1,
            DEFAULT_PROMPT
                .rsplit_once(": ")
                .map(|(_, text)| text)
                .unwrap_or("runtime perf benchmark ok")
        ),
        RuntimePerfScenario::StandardShellOutput => format!(
            "Turn {} in standard mode. Exercise shell.exec output capture, then reply with exactly: runtime perf benchmark ok",
            turn_index + 1
        ),
        RuntimePerfScenario::ToolDiscoverySearch => format!(
            "Turn {} in standard mode. Search the catalog for Gmail email tools, then reply with exactly: runtime perf benchmark ok",
            turn_index + 1
        ),
        RuntimePerfScenario::OpenAiResponsesSseParse => format!(
            "Turn {} in OpenAI Responses SSE parser benchmark mode. Parse a local Responses stream and verify the benchmark marker.",
            turn_index + 1
        ),
        RuntimePerfScenario::DirectLlmClient => format!(
            "Turn {} in direct LLM client benchmark mode. Run a direct structured completion and verify the benchmark marker.",
            turn_index + 1
        ),
        RuntimePerfScenario::ProcessListStress => format!(
            "Turn {} in process-list stress benchmark mode. Compare live process listing with explicit full history and verify the benchmark marker.",
            turn_index + 1
        ),
        RuntimePerfScenario::RlmProcessHandles => format!(
            "Turn {} in RLM mode. Exercise start/await/cancel process handles, then finish exactly: {}",
            turn_index + 1,
            DEFAULT_PROMPT
                .rsplit_once(": ")
                .map(|(_, text)| text)
                .unwrap_or("runtime perf benchmark ok")
        ),
        RuntimePerfScenario::RlmTriggerMailPipeline => format!(
            "Turn {} in RLM mode. Ensure a mail trigger is registered, send through inbox.test, let the forwarder process run, and finish exactly: {}",
            turn_index + 1,
            DEFAULT_PROMPT
                .rsplit_once(": ")
                .map(|(_, text)| text)
                .unwrap_or("runtime perf benchmark ok")
        ),
        RuntimePerfScenario::RlmProcessAsyncToolCompletion => format!(
            "Turn {} in RLM mode. Exercise pending benchmark_async completion inside a started process, then finish exactly: {}",
            turn_index + 1,
            DEFAULT_PROMPT
                .rsplit_once(": ")
                .map(|(_, text)| text)
                .unwrap_or("runtime perf benchmark ok")
        ),
        RuntimePerfScenario::RlmSubagentSpawn => format!(
            "Turn {} in RLM mode. Start a process that spawns a default subagent with seeded input, await it, then finish exactly: {}",
            turn_index + 1,
            DEFAULT_PROMPT
                .rsplit_once(": ")
                .map(|(_, text)| text)
                .unwrap_or("runtime perf benchmark ok")
        ),
        RuntimePerfScenario::RlmLlmQuery => format!(
            "Turn {} in RLM mode. Exercise llm_query direct completion, then finish exactly: {}",
            turn_index + 1,
            DEFAULT_PROMPT
                .rsplit_once(": ")
                .map(|(_, text)| text)
                .unwrap_or("runtime perf benchmark ok")
        ),
        RuntimePerfScenario::RlmGlobals => format!(
            "Turn {} in RLM mode with bound variables updated for this turn. Inspect the current state and reply with exactly: {}",
            turn_index + 1,
            DEFAULT_PROMPT
                .rsplit_once(": ")
                .map(|(_, text)| text)
                .unwrap_or("runtime perf benchmark ok")
        ),
        RuntimePerfScenario::RlmLargePrint => format!(
            "Turn {} in RLM mode. Print a large structured tool result to exercise host-owned print projection, then finish exactly: {}",
            turn_index + 1,
            DEFAULT_PROMPT
                .rsplit_once(": ")
                .map(|(_, text)| text)
                .unwrap_or("runtime perf benchmark ok")
        ),
        RuntimePerfScenario::RlmStreamedPairedLashlang => format!(
            "Turn {} in RLM mode. Stream visible prose before a paired <lashlang> block, close it, ignore any suffix after the close tag, and finish exactly: {}",
            turn_index + 1,
            DEFAULT_PROMPT
                .rsplit_once(": ")
                .map(|(_, text)| text)
                .unwrap_or("runtime perf benchmark ok")
        ),
        RuntimePerfScenario::OpenAiCompatStream => format!(
            "Turn {} in OpenAI-compatible streaming benchmark mode. Continue the benchmark chat and reply with exactly: runtime perf benchmark ok",
            turn_index + 1
        ),
        RuntimePerfScenario::TurnCheckpoint => format!(
            "Turn {} in sans-IO turn checkpoint benchmark mode. Checkpoint and restore pending effects, then reply with exactly: runtime perf benchmark ok",
            turn_index + 1
        ),
        RuntimePerfScenario::ScopedEffectController => format!(
            "Turn {} in scoped effect-controller benchmark mode. Continue the benchmark chat and reply with exactly: runtime perf benchmark ok",
            turn_index + 1
        ),
        RuntimePerfScenario::StoreReopen | RuntimePerfScenario::SqliteStoreReopen => format!(
            "Turn {} in store reopen benchmark mode. Continue after persisted reload and reply with exactly: runtime perf benchmark ok",
            turn_index + 1
        ),
        RuntimePerfScenario::LiveReplayPressure => format!(
            "Turn {} in live replay pressure benchmark mode. Append, replay, subscribe, trim, and verify gap handling.",
            turn_index + 1
        ),
        RuntimePerfScenario::TraceJsonlStandard => format!(
            "Turn {} in standard JSONL trace benchmark mode. Continue the benchmark chat and reply with exactly: runtime perf benchmark ok",
            turn_index + 1
        ),
        RuntimePerfScenario::TraceJsonlExtended => format!(
            "Turn {} in extended JSONL trace benchmark mode. Run the Lashlang block and finish exactly: runtime perf benchmark ok",
            turn_index + 1
        ),
        RuntimePerfScenario::QueuedWorkClaimStress => format!(
            "Turn {} in queued-work claim stress benchmark mode. Claim, renew, complete, and verify queued work.",
            turn_index + 1
        ),
        RuntimePerfScenario::TurnInputIngressInterrupt => format!(
            "Turn {} in turn-input ingress interrupt benchmark mode. Claim, defer, reclaim, complete, and verify pending turn input.",
            turn_index + 1
        ),
        RuntimePerfScenario::DeepTurnComposition => format!(
            "Turn {} in the deep-composition stack benchmark. Run the parent process/tool loop and child session, then incorporate the injected active-turn input.",
            turn_index + 1
        ),
        RuntimePerfScenario::TurnStartGate => format!(
            "Turn {} in the cancellation start-gate benchmark. Reply with exactly: runtime perf benchmark ok",
            turn_index + 1
        ),
        RuntimePerfScenario::TurnCancelRoundTrip => format!(
            "Turn {} in the cancellation round-trip benchmark. Wait for the exact-turn cancellation request.",
            turn_index + 1
        ),
        RuntimePerfScenario::IngressClaimProjection => format!(
            "Turn {} in the active-ingress projection benchmark. Continue after the checkpoint and incorporate the injected marker.",
            turn_index + 1
        ),
    }
}
