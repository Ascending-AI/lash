use lash_sansio::{SessionId, TurnId};
use std::{fmt::Write as _, future::IntoFuture as _, path::PathBuf, sync::Arc};

use lash::{
    LashCore, TurnOutcome,
    plugins::{PluginFactory, PluginSpec, StaticPluginFactory},
    provider::{ProviderHandle, ProviderOptions, ProviderReliability},
    runtime::SessionSnapshot,
};
use lash_core::SessionHistoryRecord;
use lash_llm_tools::LlmToolsPluginFactory;
use lash_provider_openai::OpenAiCompatibleProvider;
use lash_rlm_types::{RlmProtocolEvent, RlmTrajectoryEntry};

use super::openai_compat::OpenAiCompatBenchServer;
use super::plugin_stack::runtime_perf_plugin_stack;
use super::providers::{
    BenchmarkEchoTool, BenchmarkLargeToolCatalog, BenchmarkObliqueTools, BenchmarkProviderControl,
    BenchmarkSettlementControl, BenchmarkToolCatalogObserver, benchmark_provider,
    benchmark_provider_with_control, benchmark_stream_profile,
};
use super::scenarios::{ExecutionMode, RuntimePerfScenario};
use super::store::{RuntimePerfStore, RuntimePerfStoreFactory, RuntimePerfStoreMetrics};
pub(crate) use backend::{durable_backend, sqlite_memory_stores};

const HISTORY_EXCHANGES: usize = 18;
// `deep_turn_composition` performs two provider iterations: one runs the
// process/tool work, and the second incorporates queued active-turn input and finishes.
const RUNTIME_PERF_MAX_TURNS: usize = 2;

mod backend;
mod observation;

mod projection;
pub(crate) use projection::seed_runtime_state;

fn runtime_perf_owner() -> lash::persistence::LeaseOwnerIdentity {
    static INCARNATION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    lash::persistence::LeaseOwnerIdentity::opaque(
        "lash-perf",
        INCARNATION
            .get_or_init(|| uuid::Uuid::new_v4().to_string())
            .clone(),
    )
}

#[expect(
    clippy::expect_used,
    reason = "the mock model spec is built from fixed constants with no validation to fail"
)]
fn benchmark_llm_profile_spec() -> lash::LlmProfileMetadata {
    lash::LlmProfileMetadata::builder("mock-model")
        .context_window_tokens(200_000)
        .build()
        .expect("valid benchmark model spec")
}

trait ExplicitEphemeralFacets: Sized {
    fn with_explicit_ephemeral_facets(self) -> Self;
}

impl ExplicitEphemeralFacets for lash::LashCoreBuilder {
    fn with_explicit_ephemeral_facets(self) -> Self {
        self.commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
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

    fn core(&self) -> &LashCore {
        match self {
            Self::Standard(core) | Self::Rlm(core) => core,
        }
    }

    /// The spec a benchmark's sessions are created from: the harness's own
    /// default for the core's protocol. A core keeps none.
    pub(crate) fn session_spec(&self) -> lash::SessionSpec {
        let turn_budget = match self {
            Self::Standard(_) => lash::TurnBudget::Unbounded,
            Self::Rlm(_) => lash::TurnBudget::bounded(RUNTIME_PERF_MAX_TURNS),
        };
        lash::SessionSpec::new(
            benchmark_llm_profile_spec().wire_model,
            turn_budget,
            lash::MaxToolCalls::new(1024),
        )
        .no_progress_budget(lash_core::NoProgressBudget::bounded(12))
    }

    /// Create a benchmark's fresh session with `creation`, then open it.
    pub(crate) async fn create_and_open_session(
        &self,
        session_id: SessionId,
        creation: lash::SessionCreation,
    ) -> lash::Result<lash::LashSession> {
        self.core()
            .session(session_id.clone())
            .create(creation)
            .await?;
        self.core().session(session_id).open().await
    }

    /// Open a session the benchmark already created.
    pub(crate) async fn open_session(
        &self,
        session_id: SessionId,
    ) -> lash::Result<lash::LashSession> {
        self.core().session(session_id).open().await
    }

    pub(crate) async fn create_and_open_child_session(
        &self,
        session_id: SessionId,
        parent_session_id: SessionId,
    ) -> lash::Result<lash::LashSession> {
        self.create_and_open_session(
            session_id,
            lash::SessionCreation::child_of(parent_session_id, self.session_spec()),
        )
        .await
    }

    async fn open_session_with_state(
        &self,
        session_id: SessionId,
        state: lash::persistence::RuntimeSessionState,
    ) -> lash::Result<lash::LashSession> {
        match self {
            Self::Standard(core) | Self::Rlm(core) => {
                core.session(session_id).open_with_state(state).await
            }
        }
    }
}

/// Where a benchmark turn's session lives. A turn is always sent to the
/// session and the engine's session shift runs it (FIG-3600): the host
/// waits on the sent input's answer and never runs the turn itself.
#[derive(Clone)]
pub(crate) enum TurnEntry {
    /// A lane on lash's durable engine.
    Durable,
}

impl TurnEntry {
    /// Send one turn to `session` and wait for its report. Without a
    /// `turn_id` the send names the run itself. `cancel` withdraws the
    /// input, or cancels its running run, when it fires.
    pub(crate) async fn run(
        &self,
        session: &lash::LashSession,
        input: lash::TurnInput,
        turn_id: Option<&TurnId>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<lash::TurnReport> {
        let mut send = session.send(input);
        if let Some(turn_id) = turn_id {
            send = send.id(turn_id.clone());
        }
        // Boxed: the send and wait futures are large, and every measured
        // scenario awaits this one.
        let handle = Box::pin(send.into_future()).await?;
        let stop = handle.cancel().reason("runtime perf turn cancelled");
        let mut output = Box::pin(handle.output());
        let output = tokio::select! {
            output = &mut output => output,
            () = cancel.cancelled() => {
                stop.await?;
                output.await
            }
        };
        Ok(output?.result)
    }
}

pub(crate) struct BenchmarkRuntime {
    turn_entry: TurnEntry,
    core: BenchmarkCore,
    session: Option<lash::LashSession>,
    store: Option<Arc<RuntimePerfStore>>,
    store_metrics: Arc<RuntimePerfStoreMetrics>,
    provider_control: Option<Arc<BenchmarkProviderControl>>,
    settlement_control: Option<Arc<BenchmarkSettlementControl>>,
    tool_catalog_observer: Option<Arc<BenchmarkToolCatalogObserver>>,
    _openai_compat_server: Option<OpenAiCompatBenchServer>,
}

pub(crate) struct RuntimePerfTraceConfig {
    pub(crate) trace_jsonl_path: Option<PathBuf>,
    pub(crate) lashlang_execution_jsonl_path: Option<PathBuf>,
    pub(crate) trace_level: lash::tracing::TraceLevel,
}

impl BenchmarkRuntime {
    #[expect(
        clippy::expect_used,
        reason = "the in-process lane installs its session store before measurement begins; the accessor is the panicking half of the Option field"
    )]
    pub(crate) fn store(&self) -> Arc<RuntimePerfStore> {
        Arc::clone(
            self.store
                .as_ref()
                .expect("runtime perf in-process lane store"),
        )
    }

    pub(crate) fn store_metrics(&self) -> Arc<RuntimePerfStoreMetrics> {
        Arc::clone(&self.store_metrics)
    }

    pub(crate) fn core(&self) -> LashCore {
        self.core.as_lash_core()
    }

    /// The spec this runtime's sessions are created from.
    pub(crate) fn session_spec(&self) -> lash::SessionSpec {
        self.core.session_spec()
    }

    #[expect(
        clippy::expect_used,
        reason = "the benchmark session is taken by set_up before any measurement begins; the accessor is the panicking half of the Option field"
    )]
    pub(crate) fn session(&self) -> lash::LashSession {
        self.session.as_ref().expect("benchmark session").clone()
    }

    pub(crate) async fn create_and_open_child_session(
        &self,
        session_id: SessionId,
    ) -> anyhow::Result<lash::LashSession> {
        let parent_session_id = self.session().session_id();
        self.core
            .create_and_open_child_session(session_id, parent_session_id)
            .await
            .map_err(anyhow::Error::from)
    }

    pub(crate) fn provider_control(&self) -> anyhow::Result<Arc<BenchmarkProviderControl>> {
        self.provider_control
            .clone()
            .ok_or_else(|| anyhow::anyhow!("benchmark provider control missing"))
    }

    pub(crate) fn settlement_control(&self) -> anyhow::Result<Arc<BenchmarkSettlementControl>> {
        self.settlement_control
            .clone()
            .ok_or_else(|| anyhow::anyhow!("benchmark settlement control missing"))
    }

    pub(crate) async fn reopen_with_state(
        &mut self,
        scenario: RuntimePerfScenario,
        state: lash::persistence::RuntimeSessionState,
    ) -> anyhow::Result<()> {
        if let Some(session) = self.session.take() {
            session.close().await?;
        }
        // The session's store and `self.store()` share one SQLite memory
        // database, so the seeded state is what `self.store()` reads.
        self.session = Some(
            self.core
                .open_session_with_state(
                    SessionId::fixture(format!("runtime-perf-{}", scenario.name())),
                    state,
                )
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
                .open_session(SessionId::fixture(format!(
                    "runtime-perf-{}",
                    scenario.name()
                )))
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

    #[expect(
        clippy::expect_used,
        reason = "take_session is the teardown-side accessor: the session is present exactly once and consumed here by contract"
    )]
    pub(crate) fn take_session(&mut self) -> lash::LashSession {
        self.session.take().expect("benchmark session")
    }

    #[expect(
        clippy::expect_used,
        reason = "the benchmark session is taken by set_up before any probe can be installed"
    )]
    pub(crate) async fn set_turn_phase_probe(
        &self,
        probe: Arc<dyn lash::testing::RuntimeTurnPhaseProbe>,
    ) {
        let session = self.session.as_ref().expect("benchmark session");
        session.set_turn_phase_probe(probe).await;
    }

    #[expect(
        clippy::expect_used,
        reason = "the benchmark session is taken by set_up before turn work runs"
    )]
    pub(crate) async fn run_turn(
        &self,
        input: lash::TurnInput,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<lash::TurnReport> {
        let session = self.session.as_ref().expect("benchmark session");
        self.turn_entry.run(session, input, None, cancel).await
    }

    #[expect(
        clippy::expect_used,
        reason = "the benchmark session is taken by set_up before turn work runs"
    )]
    pub(crate) async fn run_turn_with_id(
        &self,
        input: lash::TurnInput,
        turn_id: &TurnId,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<lash::TurnReport> {
        let session = self.session.as_ref().expect("benchmark session");
        self.turn_entry
            .run(session, input, Some(turn_id), cancel)
            .await
    }

    /// How this runtime's turns reach their effect controller.
    pub(crate) fn turn_entry(&self) -> TurnEntry {
        self.turn_entry.clone()
    }

    #[expect(
        clippy::expect_used,
        reason = "the benchmark session is taken by set_up before ingress enqueueing runs"
    )]
    pub(crate) async fn enqueue_active_turn_input(
        &self,
        turn_id: &TurnId,
        input: lash::TurnInput,
        source_id: &str,
    ) -> anyhow::Result<lash_core::facade_support::TurnInputAcceptanceReceipt> {
        self.session
            .as_ref()
            .expect("benchmark session")
            .send(input)
            .id(TurnId::parse(source_id)?)
            .ingress(lash_core::TurnInputIngress::active_turn(
                turn_id,
                lash_core::TurnInputCheckpointBoundary::AfterWork,
            ))
            .await
            .map(|handle| handle.receipt().clone())
            .map_err(anyhow::Error::from)
    }

    #[expect(
        clippy::expect_used,
        reason = "the benchmark session is taken by set_up; the provider control a few lines up propagates its absence, but the session never is"
    )]
    pub(crate) async fn run_cancel_round_trip(
        &self,
        input: lash::TurnInput,
        turn_id: &TurnId,
        cancel: tokio_util::sync::CancellationToken,
        request_id: &str,
    ) -> anyhow::Result<(lash::TurnReport, std::time::Duration)> {
        let control = self
            .provider_control
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("cancel round-trip provider control missing"))?;
        let driver = self.core().turn_work_driver();
        let address = self
            .session
            .as_ref()
            .expect("benchmark session")
            .turn_address((turn_id).clone());
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

    pub(crate) async fn run_ingress_admission_projection(
        &self,
        input: lash::TurnInput,
        turn_id: &TurnId,
        cancel: tokio_util::sync::CancellationToken,
        source_id: &str,
    ) -> anyhow::Result<(lash::TurnReport, std::time::Duration)> {
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

    #[expect(
        clippy::expect_used,
        reason = "the benchmark session is taken by set_up before background work can be awaited"
    )]
    pub(crate) async fn await_background_work(&self) -> anyhow::Result<()> {
        self.session
            .as_ref()
            .expect("benchmark session")
            .refresh_background_graph()
            .await?;
        Ok(())
    }

    #[expect(
        clippy::expect_used,
        reason = "the benchmark session is taken by set_up before state can be exported"
    )]
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
    turn: &lash::TurnReport,
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
                text.contains(expected)
            } else {
                text.trim() == expected
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
        TurnOutcome::AgentFrameSwitch { frame_key, .. } => {
            anyhow::bail!(
                "runtime perf scenario {} turn {} unexpectedly switched to agent frame {}",
                scenario.name(),
                turn_index + 1,
                frame_key.as_str()
            );
        }
        TurnOutcome::Stopped(stop) => {
            anyhow::bail!(
                "runtime perf scenario {} turn {} stopped with {:?}; errors={:?}",
                scenario.name(),
                turn_index + 1,
                stop,
                turn.errors
            );
        }
    }
}

fn rlm_trajectory_errors(turn: &lash::TurnReport) -> Vec<RlmTrajectoryEntry> {
    rlm_trajectory_entries(turn)
        .into_iter()
        .filter(|entry| {
            entry
                .outcome
                .error()
                .is_some_and(|failure| !failure.message.trim().is_empty())
        })
        .collect()
}

fn rlm_trajectory_entries(turn: &lash::TurnReport) -> Vec<RlmTrajectoryEntry> {
    turn.state
        .read_view()
        .active_events()
        .iter()
        .filter_map(|event| {
            let SessionHistoryRecord::Protocol(event) = event else {
                return None;
            };
            match lash_protocol_rlm::decode_rlm_protocol_event(event) {
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

fn runtime_perf_turn_diagnostics(turn: &lash::TurnReport) -> String {
    let mut out = String::new();
    if !turn.errors.is_empty() {
        let _ = writeln!(out, "turn_errors:");
        for issue in &turn.errors {
            let code = issue
                .code
                .as_ref()
                .map_or_else(|| "none".to_string(), |code| code.namespaced());
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
                .outcome
                .error()
                .is_some_and(|failure| !failure.message.trim().is_empty())
        })
        .collect::<Vec<_>>();
    if !errors.is_empty() {
        let _ = writeln!(out, "rlm_execution_errors:");
        for entry in errors {
            let _ = writeln!(
                out,
                "- iteration={} error={}",
                entry.protocol_iteration,
                preview(
                    entry
                        .outcome
                        .error()
                        .map_or("", |failure| failure.message.as_str()),
                    900,
                )
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
            entry.outcome.terminal_value().map_or_else(
                || "none".to_string(),
                |value| serde_json::json!(lash_rlm_types::HistoryValue::from(value)).to_string()
            )
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

fn benchmark_rlm_protocol_factory(
    backend: &lash::Backend,
) -> lash_protocol_rlm::RlmProtocolPluginFactory {
    lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        std::sync::Arc::new(lash_protocol_rlm::TypescriptDialect),
        backend,
    )
}

fn benchmark_standard_builder(
    backend: lash::Backend,
    provider: ProviderHandle,
) -> lash::LashCoreBuilder {
    lash::LashCore::standard_builder(backend)
        .serve_test_llm_profile(provider, benchmark_llm_profile_spec())
}

fn benchmark_rlm_builder(
    backend: lash::Backend,
    provider: ProviderHandle,
    factory: lash_protocol_rlm::RlmProtocolPluginFactory,
) -> lash::LashCoreBuilder {
    lash::LashCore::rlm_builder(backend, factory)
        .serve_test_llm_profile(provider, benchmark_llm_profile_spec())
}

// The benchmark plugin list, in push order. Every conditional reads the
// scenario's `ScenarioWiring` column; the builders only append the result.
fn benchmark_plugin_factories(
    scenario: RuntimePerfScenario,
    effect_host: &lash::runtime::ActorContext,
    settlement_control: Option<&Arc<BenchmarkSettlementControl>>,
    tool_catalog_observer: Option<&Arc<BenchmarkToolCatalogObserver>>,
) -> Vec<Arc<dyn PluginFactory>> {
    let wiring = scenario.wiring();
    let benchmark_tool = settlement_control.map_or_else(
        || BenchmarkEchoTool::new(effect_host.clone()),
        |control| {
            BenchmarkEchoTool::with_settlement_control(effect_host.clone(), Arc::clone(control))
        },
    );
    let mut factories: Vec<Arc<dyn PluginFactory>> = vec![Arc::new(StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("runtime_perf_tools"),
        PluginSpec::new().with_tool_provider(Arc::new(benchmark_tool)),
    ))];
    if wiring.llm_query_plugin {
        factories.push(Arc::new(LlmToolsPluginFactory::default()));
    }
    if wiring.delegation_plugin {
        // The host's delegation tool (`examples/delegation`): each child runs
        // the benchmark model under the harness's budgets, stated explicitly.
        let turn_budget = if scenario.execution_mode().is_rlm() {
            lash::TurnBudget::bounded(RUNTIME_PERF_MAX_TURNS)
        } else {
            lash::TurnBudget::Unbounded
        };
        let delegation = delegation::DelegationPluginFactory::new(
            lash::SessionSpec::new(
                benchmark_llm_profile_spec().wire_model,
                turn_budget,
                lash::MaxToolCalls::new(1024),
            )
            .no_progress_budget(lash_core::NoProgressBudget::bounded(12)),
            lash_core::lifetime::starter,
        );
        factories.push(Arc::new(if scenario.execution_mode().is_rlm() {
            delegation.with_rlm_children()
        } else {
            delegation
        }));
    }
    if wiring.oblique_tools_plugin {
        factories.push(Arc::new(StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("runtime_perf_oblique_tools"),
            PluginSpec::new().with_tool_provider(Arc::new(BenchmarkObliqueTools)),
        )));
    }
    if wiring.large_tool_catalog_plugin {
        factories.push(Arc::new(StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("runtime_perf_large_tool_catalog"),
            PluginSpec::new().with_tool_provider(Arc::new(BenchmarkLargeToolCatalog::default())),
        )));
    }
    if let Some(observer) = tool_catalog_observer {
        let composition_observer = Arc::clone(observer);
        factories.push(Arc::new(StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("runtime_perf_tool_catalog_observer"),
            PluginSpec::new().with_tool_catalog_contributor(
                lash_core::hook_key!("observe-catalog"),
                Arc::new(move |context| {
                    if let Some(session_id) = context.owner.session_id() {
                        composition_observer.observe_session_catalog_composition(session_id)?;
                    }
                    Ok(Default::default())
                }),
            ),
        )));
    }

    factories
}

/// The in-process lane: lash's durable engine over a fresh SQLite memory
/// store set, its session catalog behind the perf store decorator.
struct InProcessLane {
    backend: lash::Backend,
    stores: RuntimePerfStoreFactory,
}

fn measured_stores(
    stores: Arc<dyn lash_core::StoreSet>,
    catalog: Arc<dyn lash_core::DeploymentStore>,
    metrics: Arc<RuntimePerfStoreMetrics>,
    measure_commit_bytes: bool,
) -> Arc<dyn lash_core::StoreSet> {
    lash_core::testing::runtime_helpers::LayeredStores::over(stores)
        .map_session_store_factory(|_| catalog)
        .map_durable_store(|inner| {
            Arc::new(super::store::RuntimePerfDurableStore {
                inner,
                metrics,
                measure_commit_bytes,
            })
        })
        .into_store_set()
}

async fn in_process_lane() -> anyhow::Result<InProcessLane> {
    let raw: Arc<dyn lash_core::StoreSet> = Arc::new(sqlite_memory_stores().await?);
    let stores =
        RuntimePerfStoreFactory::decorating_without_commit_measurement(raw.session_store_factory());
    let layered = measured_stores(raw, Arc::new(stores.clone()), stores.metrics(), false);
    let backend = durable_backend(layered)?;
    Ok(InProcessLane { backend, stores })
}

/// A decorated root session store on a SQLite memory store set of its own,
/// for the store-level scenarios that execute no engine.
pub(crate) async fn memory_perf_store(
    session_id: &SessionId,
) -> anyhow::Result<Arc<RuntimePerfStore>> {
    let factory = RuntimePerfStoreFactory::decorating_without_commit_measurement(
        sqlite_memory_stores().await?.session_store_factory(),
    );
    Ok(factory.root_store(session_id).await?)
}

pub(crate) async fn build_embed_core(
    scenario: RuntimePerfScenario,
) -> anyhow::Result<(BenchmarkCore, RuntimePerfStoreFactory, TurnEntry)> {
    let InProcessLane { backend, stores } = in_process_lane().await?;
    let effect_host = lash::runtime::ActorContext::detached(backend.clone());
    let provider = benchmark_provider(scenario).into_handle();
    let core = match scenario.execution_mode() {
        ExecutionMode::Standard => benchmark_standard_builder(backend, provider)
            .with_explicit_ephemeral_facets()
            .build(runtime_perf_owner())
            .map(BenchmarkCore::Standard)
            .map_err(anyhow::Error::from),
        ExecutionMode::Rlm => benchmark_rlm_builder(
            backend.clone(),
            provider,
            benchmark_rlm_protocol_factory(&backend),
        )
        .with_explicit_ephemeral_facets()
        .tools(Arc::new(BenchmarkEchoTool::new(effect_host)))
        .build(runtime_perf_owner())
        .map(BenchmarkCore::Rlm)
        .map_err(anyhow::Error::from),
    }?;
    Ok((core, stores, TurnEntry::Durable))
}

pub(crate) async fn build_runtime(
    scenario: RuntimePerfScenario,
    trace_config: Option<RuntimePerfTraceConfig>,
) -> anyhow::Result<BenchmarkRuntime> {
    let wiring = scenario.wiring();
    let execution_mode = scenario.execution_mode();
    let openai_compat_server = if wiring.compat_stream_server {
        Some(OpenAiCompatBenchServer::start(benchmark_stream_profile(scenario)).await?)
    } else {
        None
    };
    let base_url = openai_compat_server
        .as_ref()
        .map(|server| server.base_url.clone())
        .unwrap_or_else(|| "https://example.invalid/v1".to_string());
    let (provider, provider_control): (ProviderHandle, Option<Arc<BenchmarkProviderControl>>) =
        if wiring.compat_stream_server {
            (
                ProviderHandle::new(
                    OpenAiCompatibleProvider::new("test-key", base_url.clone())
                        .with_options(ProviderOptions {
                            reliability: ProviderReliability::disabled(),
                            ..ProviderOptions::default()
                        })
                        .into_components(),
                ),
                None,
            )
        } else {
            let (provider, control) = benchmark_provider_with_control(scenario);
            (provider.into_handle(), control)
        };
    let InProcessLane {
        backend: perf_backend,
        stores: store_factory,
    } = in_process_lane().await?;
    let backend = perf_backend;
    let effect_host = lash::runtime::ActorContext::detached(backend.clone());
    let settlement_control = scenario
        .settlement_children()
        .map(|_| Arc::new(BenchmarkSettlementControl::new()));
    let tool_catalog_observer = wiring
        .tool_catalog_observer
        .then(|| Arc::new(BenchmarkToolCatalogObserver::default()));
    let mut plugin_stack = runtime_perf_plugin_stack(
        scenario.uses_standard_compaction(),
        execution_mode.is_standard(),
    );
    for factory in benchmark_plugin_factories(
        scenario,
        &effect_host,
        settlement_control.as_ref(),
        tool_catalog_observer.as_ref(),
    ) {
        plugin_stack.push(factory);
    }
    let core = match execution_mode {
        ExecutionMode::Standard => {
            let mut builder = benchmark_standard_builder(backend, provider)
                .with_explicit_ephemeral_facets()
                .plugins(plugin_stack);
            if let Some(config) = trace_config {
                if let Some(path) = config.trace_jsonl_path {
                    builder = builder.trace_jsonl_path(path);
                }
                builder = builder.trace_level(config.trace_level);
            }
            BenchmarkCore::Standard(builder.build(runtime_perf_owner())?)
        }
        ExecutionMode::Rlm => {
            let factory = benchmark_rlm_protocol_factory(&backend);
            let mut tracing = lash_core::trace::TraceRuntime::new(backend.clock());
            if let Some(path) = trace_config
                .as_ref()
                .and_then(|config| config.lashlang_execution_jsonl_path.clone())
            {
                tracing = tracing
                    .with_product_observer(Arc::new(lash::tracing::JsonlTraceSink::new(path)));
            }
            let mut builder = benchmark_rlm_builder(backend, provider, factory)
                .trace_runtime(tracing)
                .with_explicit_ephemeral_facets()
                .plugins(plugin_stack);
            if let Some(config) = trace_config {
                if let Some(path) = config.trace_jsonl_path {
                    builder = builder.trace_jsonl_path(path);
                }
                builder = builder.trace_level(config.trace_level);
            }
            BenchmarkCore::Rlm(builder.build(runtime_perf_owner())?)
        }
    };
    let session_id = SessionId::fixture(format!("runtime-perf-{}", scenario.name()));
    let session = core
        .create_and_open_session(
            session_id.clone(),
            lash::SessionCreation::root(core.session_spec()),
        )
        .await?;
    let store = store_factory
        .session_store(&session_id)
        .ok_or_else(|| anyhow::anyhow!("runtime perf session store was not opened"))?;
    Ok(BenchmarkRuntime {
        store_metrics: store_factory.metrics(),
        turn_entry: TurnEntry::Durable,
        core,
        session: Some(session),
        store: Some(store),
        provider_control,
        settlement_control,
        tool_catalog_observer,
        _openai_compat_server: openai_compat_server,
    })
}

pub(crate) async fn durable_sqlite_session_store_factory_without_commit_measurement(
    sessions_root: PathBuf,
    _process_registry_path: &std::path::Path,
) -> anyhow::Result<(
    Arc<dyn lash_core::DeploymentStore>,
    Arc<RuntimePerfStoreMetrics>,
)> {
    let sqlite = lash_sqlite_store::SqliteStoreSet::open(sessions_root.join("lash.db")).await?;
    let factory = RuntimePerfStoreFactory::decorating_without_commit_measurement(
        sqlite.session_store_factory(),
    );
    let metrics = factory.metrics();
    Ok((Arc::new(factory), metrics))
}

pub(crate) fn durable_postgres_session_store_factory_without_commit_measurement(
    postgres: &lash_postgres_store::PostgresStorage,
) -> (
    Arc<dyn lash_core::DeploymentStore>,
    Arc<RuntimePerfStoreMetrics>,
) {
    let factory =
        RuntimePerfStoreFactory::decorating_without_commit_measurement(Arc::new(postgres.store()));
    let metrics = factory.metrics();
    (Arc::new(factory), metrics)
}

pub(crate) async fn build_runtime_with_sqlite_store(
    scenario: RuntimePerfScenario,
    root: PathBuf,
) -> anyhow::Result<BenchmarkRuntime> {
    let wiring = scenario.wiring();
    let mode_id = scenario.execution_mode();
    let provider = benchmark_provider(scenario).into_handle();
    let mut plugin_stack =
        runtime_perf_plugin_stack(scenario.uses_standard_compaction(), mode_id.is_standard());
    let stores: Arc<dyn lash_core::StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(root.join("lash.db"))
            .await
            .map_err(|err| anyhow::anyhow!(err.to_string()))?,
    );
    let (store_factory, store_metrics): (
        Arc<dyn lash_core::DeploymentStore>,
        Arc<RuntimePerfStoreMetrics>,
    ) = if wiring.measure_commit_bytes {
        let factory = RuntimePerfStoreFactory::decorating(stores.session_store_factory());
        let metrics = factory.metrics();
        (Arc::new(factory), metrics)
    } else {
        (
            stores.session_store_factory(),
            Arc::new(RuntimePerfStoreMetrics::default()),
        )
    };
    let layered = measured_stores(
        stores,
        Arc::clone(&store_factory),
        Arc::clone(&store_metrics),
        wiring.measure_commit_bytes,
    );
    let backend = durable_backend(layered)?;
    let effect_host = lash::runtime::ActorContext::detached(backend.clone());
    for factory in benchmark_plugin_factories(scenario, &effect_host, None, None) {
        plugin_stack.push(factory);
    }
    let core = durable_benchmark_core(backend, mode_id, provider, plugin_stack)?;
    let session_id = SessionId::fixture(format!("runtime-perf-{}", scenario.name()));
    let session = core
        .create_and_open_session(
            session_id.clone(),
            lash::SessionCreation::root(core.session_spec()),
        )
        .await?;
    Ok(BenchmarkRuntime {
        store_metrics,
        turn_entry: TurnEntry::Durable,
        core,
        session: Some(session),
        store: None,
        provider_control: None,
        settlement_control: None,
        tool_catalog_observer: None,
        _openai_compat_server: None,
    })
}

/// A benchmark core on a durable backend, with the lane's plugin stack.
pub(crate) fn checkpoint_benchmark_core(
    backend: lash::Backend,
    factory: Arc<dyn PluginFactory>,
) -> anyhow::Result<LashCore> {
    let mut plugins = runtime_perf_plugin_stack(false, true);
    plugins.push(factory);
    Ok(benchmark_standard_builder(
        backend,
        benchmark_provider(RuntimePerfScenario::Standard).into_handle(),
    )
    .with_explicit_ephemeral_facets()
    .plugins(plugins)
    .build(lash::persistence::LeaseOwnerIdentity::opaque(
        format!("checkpoint-worker-{}", uuid::Uuid::new_v4()),
        uuid::Uuid::new_v4().to_string(),
    ))?)
}

fn durable_benchmark_core(
    backend: lash::Backend,
    mode_id: ExecutionMode,
    provider: ProviderHandle,
    plugin_stack: lash::PluginStack,
) -> anyhow::Result<BenchmarkCore> {
    let builder = match mode_id {
        ExecutionMode::Standard => benchmark_standard_builder(backend, provider),
        ExecutionMode::Rlm => {
            let factory = benchmark_rlm_protocol_factory(&backend);
            benchmark_rlm_builder(backend, provider, factory)
        }
    };
    let builder = builder
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .plugins(plugin_stack);
    let core = builder.build(runtime_perf_owner())?;
    Ok(match mode_id {
        ExecutionMode::Standard => BenchmarkCore::Standard(core),
        ExecutionMode::Rlm => BenchmarkCore::Rlm(core),
    })
}

#[cfg(test)]
mod tests;
