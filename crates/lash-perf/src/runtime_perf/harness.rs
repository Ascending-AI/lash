use lash_sansio::{SessionId, TurnId, sync::MutexExt};
use std::{
    fmt::Write as _,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use lash::{
    LashCore, TurnOutcome,
    plugins::{
        PluginError, PluginExtensionContribution, PluginFactory, PluginRegistrar,
        PluginSessionContext, PluginSpec, SessionPlugin, StaticPluginFactory,
    },
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
    BENCHMARK_MAIL_RECEIVED_SOURCE_TYPE, BenchmarkEchoTool, BenchmarkLargeToolCatalog,
    BenchmarkObliqueTools, BenchmarkProviderControl, BenchmarkSettlementControl,
    BenchmarkToolCatalogObserver, BenchmarkWorkbenchMailTool, benchmark_provider,
    benchmark_provider_with_control, benchmark_stream_profile,
};
use super::scenarios::{ExecutionMode, RuntimePerfScenario};
use super::store::{RuntimePerfStore, RuntimePerfStoreFactory, RuntimePerfStoreMetrics};
use backend::PerfBackend;
pub(crate) use backend::{memory_stores, restate_backend};

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

const BENCHMARK_MAIL_RESOURCE: &str = "Mail";
const BENCHMARK_MAIL_ALIAS: &str = "mail";
const BENCHMARK_MAIL_EVENT: &str = "received";

#[expect(
    clippy::expect_used,
    reason = "the mock model spec is built from fixed constants with no validation to fail"
)]
fn benchmark_model_spec() -> lash::ModelSpec {
    lash::ModelSpec::builder("mock-model")
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

    pub(crate) async fn open_session(
        &self,
        session_id: SessionId,
    ) -> lash::Result<lash::LashSession> {
        match self {
            Self::Standard(core) => core.session(session_id).open().await,
            Self::Rlm(core) => core.session(session_id).open().await,
        }
    }

    pub(crate) async fn open_child_session(
        &self,
        session_id: SessionId,
        parent_session_id: SessionId,
    ) -> lash::Result<lash::LashSession> {
        match self {
            Self::Standard(core) => {
                core.session(session_id)
                    .parent(parent_session_id)
                    .open()
                    .await
            }
            Self::Rlm(core) => {
                core.session(session_id)
                    .parent(parent_session_id)
                    .open()
                    .await
            }
        }
    }

    async fn open_session_with_state(
        &self,
        session_id: SessionId,
        state: lash::persistence::RuntimeSessionState,
    ) -> lash::Result<lash::LashSession> {
        match self {
            Self::Standard(core) => core.session(session_id).open_with_state(state).await,
            Self::Rlm(core) => core.session(session_id).open_with_state(state).await,
        }
    }
}

/// How a benchmark turn reaches its effect controller.
#[derive(Clone)]
pub(crate) enum TurnEntry {
    /// The backend's host scopes the turn itself: the durable SQLite and
    /// PostgreSQL lanes.
    Host,
    /// The turn runs inside a handler on the Restate server double, where a
    /// Restate deployment runs one. Restate re-runs the handler from the top
    /// on every replay, so a turn that suspends runs again under its turn id.
    RestateHandler(lash_restate_test::RestateTestBackend),
}

/// Which facade call drives a benchmark turn on its scoped controller.
#[derive(Clone, Copy)]
pub(crate) enum TurnDrive {
    /// `collect_session_events_with_scope`, the path every runtime scenario
    /// measures.
    CollectEvents,
    /// `run_with_scope`, the caller-supplied-controller path the
    /// scoped-effect scenario measures.
    RunWithScope,
}

async fn drive_turn(
    session: &lash::LashSession,
    input: lash::TurnInput,
    turn_id: Option<TurnId>,
    cancel: tokio_util::sync::CancellationToken,
    scoped: lash::runtime::ScopedEffectController<'_>,
    drive: TurnDrive,
) -> anyhow::Result<lash::TurnReport> {
    let mut turn = session.turn(input).cancel(cancel);
    if let Some(turn_id) = turn_id {
        turn = turn.turn_id(turn_id);
    }
    let turn = turn.advanced();
    match drive {
        TurnDrive::CollectEvents => turn
            .collect_session_events_with_scope(&lash::runtime::NoopEventSink, scoped)
            .await
            .map_err(anyhow::Error::from),
        TurnDrive::RunWithScope => turn
            .run_with_scope(scoped)
            .await
            .map(|output| output.result)
            .map_err(anyhow::Error::from),
    }
}

impl TurnEntry {
    /// Run one turn of `session` to its report. Without a `turn_id` the host
    /// lane lets the session name the turn; the Restate lane names it, since
    /// its handler is keyed by the turn scope.
    pub(crate) async fn run(
        &self,
        session: &lash::LashSession,
        input: lash::TurnInput,
        turn_id: Option<&TurnId>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<lash::TurnReport> {
        self.run_driven(session, input, turn_id, cancel, TurnDrive::CollectEvents)
            .await
    }

    pub(crate) async fn run_driven(
        &self,
        session: &lash::LashSession,
        input: lash::TurnInput,
        turn_id: Option<&TurnId>,
        cancel: tokio_util::sync::CancellationToken,
        drive: TurnDrive,
    ) -> anyhow::Result<lash::TurnReport> {
        match self {
            Self::Host => {
                let scope_turn_id = turn_id.cloned().unwrap_or_else(|| {
                    TurnId::from(
                        lash_core::TurnActivityId::new(uuid::Uuid::new_v4().to_string())
                            .0
                            .to_string(),
                    )
                });
                let effect_host = session.effect_host();
                let scoped = effect_host
                    .scoped(
                        lash_core::AdmittedScope::unpinned(session.turn_scope(scope_turn_id))
                            .map_err(anyhow::Error::from)?,
                    )
                    .map_err(anyhow::Error::from)?;
                drive_turn(session, input, turn_id.cloned(), cancel, scoped, drive).await
            }
            Self::RestateHandler(restate) => {
                let turn_id = turn_id.cloned().unwrap_or_else(|| {
                    TurnId::from(format!("runtime-perf-turn-{}", uuid::Uuid::new_v4()))
                });
                let admitted =
                    lash_core::AdmittedScope::unpinned(session.turn_scope(turn_id.clone()))
                        .map_err(anyhow::Error::from)?;
                let report: Arc<Mutex<Option<anyhow::Result<lash::TurnReport>>>> =
                    Arc::new(Mutex::new(None));
                let attempt: lash_restate_test::HandlerAttempt = {
                    let session = session.clone();
                    let report = Arc::clone(&report);
                    Arc::new(move |scoped| {
                        let session = session.clone();
                        let input = input.clone();
                        let turn_id = turn_id.clone();
                        let cancel = cancel.clone();
                        let report = Arc::clone(&report);
                        Box::pin(async move {
                            let result =
                                drive_turn(&session, input, Some(turn_id), cancel, scoped, drive)
                                    .await;
                            *report.lock_recover() = Some(result);
                        })
                    })
                };
                restate
                    .run_in_handler(admitted, attempt)
                    .await
                    .map_err(|err| anyhow::anyhow!("runtime perf turn handler: {err}"))?;
                report
                    .lock_recover()
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("the turn's handler recorded no report"))?
            }
        }
    }
}

pub(crate) struct BenchmarkRuntime {
    turn_entry: TurnEntry,
    /// The in-process lane's deployment worker probes; the durable lanes'
    /// cores carry their own.
    process_phase_probes: Option<lash::runtime::RuntimeTurnPhaseProbeSlot>,
    core: BenchmarkCore,
    session: Option<lash::LashSession>,
    store: Option<Arc<RuntimePerfStore>>,
    persistence: Option<Arc<dyn lash::persistence::RuntimePersistence>>,
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
        reason = "the benchmark session is taken by set_up before any measurement can read it; the accessor is the panicking half of the Option field"
    )]
    pub(crate) fn usage_report(&self) -> lash::usage::SessionUsageReport {
        self.session
            .as_ref()
            .expect("benchmark session")
            .usage_report()
    }

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

    #[expect(
        clippy::expect_used,
        reason = "the persistence handle is installed by set_up before measurement begins; the accessor is the panicking half of the Option field"
    )]
    pub(crate) fn persistence(&self) -> Arc<dyn lash::persistence::RuntimePersistence> {
        Arc::clone(
            self.persistence
                .as_ref()
                .expect("runtime perf persistence handle"),
        )
    }

    pub(crate) fn core(&self) -> LashCore {
        self.core.as_lash_core()
    }

    #[expect(
        clippy::expect_used,
        reason = "the benchmark session is taken by set_up before any measurement begins; the accessor is the panicking half of the Option field"
    )]
    pub(crate) fn session(&self) -> lash::LashSession {
        self.session.as_ref().expect("benchmark session").clone()
    }

    pub(crate) async fn open_child_session(
        &self,
        session_id: SessionId,
    ) -> anyhow::Result<lash::LashSession> {
        let parent_session_id = self.session().session_id();
        self.core
            .open_child_session(session_id, parent_session_id)
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
                    SessionId::from(format!("runtime-perf-{}", scenario.name())),
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
                .open_session(SessionId::from(format!("runtime-perf-{}", scenario.name())))
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
        probe: Arc<dyn lash::runtime::RuntimeTurnPhaseProbe>,
    ) {
        let session = self.session.as_ref().expect("benchmark session");
        if let Some(slot) = &self.process_phase_probes {
            slot.set_for_session(session.session_id(), Arc::clone(&probe));
        }
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
            .durable()
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
        reason = "the benchmark session is taken by set_up before scoped-effect turn work runs"
    )]
    pub(crate) async fn run_turn_with_execution_scope(
        &self,
        input: lash::TurnInput,
        turn_id: &TurnId,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<lash::TurnReport> {
        let session = self.session.as_ref().expect("benchmark session");
        self.turn_entry
            .run_driven(
                session,
                input,
                Some(turn_id),
                cancel,
                TurnDrive::RunWithScope,
            )
            .await
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
        TurnOutcome::AgentFrameSwitch { frame_key, .. } => {
            anyhow::bail!(
                "runtime perf scenario {} turn {} unexpectedly switched to agent frame {}",
                scenario.name(),
                turn_index + 1,
                frame_key.as_str()
            );
        }
        TurnOutcome::Queued { ahead } => {
            anyhow::bail!(
                "runtime perf scenario {} turn {} was queued behind {} inputs",
                scenario.name(),
                turn_index + 1,
                ahead
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

fn rlm_trajectory_errors(turn: &lash::TurnReport) -> Vec<RlmTrajectoryEntry> {
    rlm_trajectory_entries(turn)
        .into_iter()
        .filter(|entry| {
            entry
                .outcome
                .error()
                .is_some_and(|error| !error.trim().is_empty())
        })
        .collect()
}

#[expect(
    clippy::expect_used,
    reason = "a TurnReport always commits into one active runtime frame scope, so its read view resolves; also stated by the message"
)]
fn rlm_trajectory_entries(turn: &lash::TurnReport) -> Vec<RlmTrajectoryEntry> {
    turn.state
        .read_view()
        .expect("runtime frame scope resolves")
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
                preview(entry.outcome.error().map_or("", String::as_str), 900,)
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
                .outcome
                .terminal_value()
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

fn benchmark_rlm_protocol_factory(
    backend: &dyn lash::persistence::LashlangArtifactBackend,
) -> lash_protocol_rlm::RlmProtocolPluginFactory {
    lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        backend,
    )
}

fn benchmark_standard_builder(
    backend: Arc<dyn lash::Backend>,
    provider: ProviderHandle,
) -> lash::LashCoreBuilder {
    lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
        .provider(provider)
        .model(benchmark_model_spec())
}

fn benchmark_rlm_builder(
    backend: Arc<dyn lash::Backend>,
    provider: ProviderHandle,
    factory: lash_protocol_rlm::RlmProtocolPluginFactory,
) -> lash::LashCoreBuilder {
    lash::LashCore::rlm_builder(backend, lash::TurnBudget::Unbounded, factory)
        .provider(provider)
        .model(benchmark_model_spec())
        .turn_budget(lash::TurnBudget::bounded(RUNTIME_PERF_MAX_TURNS))
}

// The benchmark plugin list, in push order. Every conditional reads the
// scenario's `ScenarioWiring` column; the builders only append the result.
fn benchmark_plugin_factories(
    scenario: RuntimePerfScenario,
    effect_host: &Arc<dyn lash_core::EffectHost>,
    settlement_control: Option<&Arc<BenchmarkSettlementControl>>,
    tool_catalog_observer: Option<&Arc<BenchmarkToolCatalogObserver>>,
) -> Vec<Arc<dyn PluginFactory>> {
    let wiring = scenario.wiring();
    let benchmark_tool = settlement_control.map_or_else(
        || BenchmarkEchoTool::new(Arc::clone(effect_host)),
        |control| {
            BenchmarkEchoTool::with_settlement_control(Arc::clone(effect_host), Arc::clone(control))
        },
    );
    let mut factories: Vec<Arc<dyn PluginFactory>> = vec![Arc::new(StaticPluginFactory::new(
        "runtime_perf_tools",
        PluginSpec::new().with_tool_provider(Arc::new(benchmark_tool)),
    ))];
    if wiring.llm_query_plugin {
        factories.push(Arc::new(LlmToolsPluginFactory::default()));
    }
    if wiring.subagents_plugin {
        factories.push(Arc::new(lash_subagents::SubagentsPluginFactory::new(
            Arc::new(lash_subagents::CapabilityRegistry::new().with(Arc::new(
                lash_subagents::StaticCapability::new(
                    "default",
                    lash_core::facade_support::SessionSpec::inherit(),
                ),
            ))),
        )));
    }
    if wiring.oblique_tools_plugin {
        factories.push(Arc::new(StaticPluginFactory::new(
            "runtime_perf_oblique_tools",
            PluginSpec::new().with_tool_provider(Arc::new(BenchmarkObliqueTools)),
        )));
    }
    if wiring.large_tool_catalog_plugin {
        factories.push(Arc::new(StaticPluginFactory::new(
            "runtime_perf_large_tool_catalog",
            PluginSpec::new().with_tool_provider(Arc::new(BenchmarkLargeToolCatalog::default())),
        )));
    }
    if let Some(observer) = tool_catalog_observer {
        let composition_observer = Arc::clone(observer);
        factories.push(Arc::new(StaticPluginFactory::new(
            "runtime_perf_tool_catalog_observer",
            PluginSpec::new().with_tool_catalog_contributor(Arc::new(move |context| {
                composition_observer.observe_session_catalog_composition(&context.session_id)?;
                Ok(Default::default())
            })),
        )));
    }
    if wiring.workbench_trigger_plugin {
        factories.push(Arc::new(BenchmarkWorkbenchTriggerPluginFactory));
    }
    factories
}

/// The in-process lane: lash-restate's engine on a fresh server double,
/// its session catalog behind the perf store decorator.
struct InProcessLane {
    restate: lash_restate_test::RestateTestBackend,
    backend: PerfBackend,
    stores: RuntimePerfStoreFactory,
}

async fn in_process_lane() -> anyhow::Result<InProcessLane> {
    let restate = restate_backend().await?;
    let stores = RuntimePerfStoreFactory::decorating_without_commit_measurement(
        restate.lash_backend().session_store_factory(),
    );
    let backend = PerfBackend::over_restate(&restate).with_catalog(Arc::new(stores.clone()));
    Ok(InProcessLane {
        restate,
        backend,
        stores,
    })
}

/// Serve the lane's process segments with `core`'s durable worker, as a
/// Restate deployment does. The worker reads its turn-phase probes from the
/// returned slot: the deployment's worker is not the session's runtime, so a
/// probe the benchmark installs on a session reaches its processes only
/// through this slot.
fn install_process_worker(
    restate: &lash_restate_test::RestateTestBackend,
    core: &BenchmarkCore,
) -> anyhow::Result<lash::runtime::RuntimeTurnPhaseProbeSlot> {
    let probes = lash::runtime::RuntimeTurnPhaseProbeSlot::default();
    let config = core
        .as_lash_core()
        .durable_process_worker_config()?
        .with_turn_phase_probe_slot(probes.clone());
    restate.install_process_worker(
        lash::durability::DurableProcessWorker::new(config)
            .map_err(|err| anyhow::anyhow!(err.to_string()))?,
    );
    Ok(probes)
}

/// A decorated root session store on a SQLite memory store set of its own,
/// for the store-level scenarios that drive no engine.
pub(crate) async fn memory_perf_store(
    session_id: &SessionId,
) -> anyhow::Result<Arc<RuntimePerfStore>> {
    let factory = RuntimePerfStoreFactory::decorating_without_commit_measurement(
        memory_stores().await?.session_store_factory(),
    );
    Ok(factory.root_store(session_id).await?)
}

pub(crate) async fn build_embed_core(
    scenario: RuntimePerfScenario,
) -> anyhow::Result<(BenchmarkCore, RuntimePerfStoreFactory, TurnEntry)> {
    let InProcessLane {
        restate,
        backend,
        stores,
    } = in_process_lane().await?;
    let backend: Arc<dyn lash::persistence::LashlangArtifactBackend> = Arc::new(backend);
    let effect_host = backend.effect_host();
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
            benchmark_rlm_protocol_factory(backend.as_ref()),
        )
        .with_explicit_ephemeral_facets()
        .tools(Arc::new(BenchmarkEchoTool::new(effect_host)))
        .build(runtime_perf_owner())
        .map(BenchmarkCore::Rlm)
        .map_err(anyhow::Error::from),
    }?;
    install_process_worker(&restate, &core)?;
    Ok((core, stores, TurnEntry::RestateHandler(restate)))
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
        restate,
        backend: perf_backend,
        stores: store_factory,
    } = in_process_lane().await?;
    let backend: Arc<dyn lash::persistence::LashlangArtifactBackend> = Arc::new(perf_backend);
    let effect_host = backend.effect_host();
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
            if !wiring.queued_work {
                // Scenarios without a queued-work lane still use the decorated
                // catalog installed above.
                builder = builder.without_queued_work();
            }
            BenchmarkCore::Standard(builder.build(runtime_perf_owner())?)
        }
        ExecutionMode::Rlm => {
            let mut factory = benchmark_rlm_protocol_factory(backend.as_ref());
            if let Some(path) = trace_config
                .as_ref()
                .and_then(|config| config.lashlang_execution_jsonl_path.clone())
            {
                factory = factory.with_lashlang_execution_jsonl_path(path);
            }
            let mut builder = benchmark_rlm_builder(backend, provider, factory)
                .with_explicit_ephemeral_facets()
                .plugins(plugin_stack);
            if let Some(config) = trace_config {
                if let Some(path) = config.trace_jsonl_path {
                    builder = builder.trace_jsonl_path(path);
                }
                builder = builder.trace_level(config.trace_level);
            }
            if !wiring.queued_work {
                // Scenarios without a queued-work lane still use the decorated
                // catalog installed above.
                builder = builder.without_queued_work();
            }
            BenchmarkCore::Rlm(builder.build(runtime_perf_owner())?)
        }
    };
    let process_phase_probes = install_process_worker(&restate, &core)?;
    let session_id = SessionId::from(format!("runtime-perf-{}", scenario.name()));
    let session = core.open_session(session_id.clone()).await?;
    let store = store_factory
        .session_store(&session_id)
        .ok_or_else(|| anyhow::anyhow!("runtime perf session store was not opened"))?;
    Ok(BenchmarkRuntime {
        store_metrics: store_factory.metrics(),
        turn_entry: TurnEntry::RestateHandler(restate),
        process_phase_probes: Some(process_phase_probes),
        core,
        session: Some(session),
        store: Some(store),
        persistence: None,
        provider_control,
        settlement_control,
        tool_catalog_observer,
        _openai_compat_server: openai_compat_server,
    })
}

struct BenchmarkWorkbenchTriggerPluginFactory;

impl PluginFactory for BenchmarkWorkbenchTriggerPluginFactory {
    fn id(&self) -> &'static str {
        "runtime_perf_workbench_trigger"
    }

    #[expect(
        clippy::expect_used,
        reason = "the extension id and contribution are workspace constants wired by the benchmark itself, so construction cannot fail"
    )]
    fn extension_contributions(&self) -> Vec<PluginExtensionContribution> {
        vec![
            PluginExtensionContribution::new(
                lash::rlm::LASHLANG_SURFACE_EXTENSION_ID,
                lash::rlm::LashlangSurfaceContribution::new(
                    lash::rlm::LashlangAbilities::default().with_sleep(),
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

#[expect(
    clippy::expect_used,
    reason = "the benchmark's own trigger source constructor is added to a freshly built catalog and cannot conflict, per the site's message"
)]
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

#[expect(
    clippy::expect_used,
    reason = "the mail.Received type is built from three Str fields fixed here, so validation passes, per the site's message"
)]
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

pub(crate) fn durable_sqlite_session_store_factory_without_commit_measurement(
    sessions_root: PathBuf,
    process_registry_path: &std::path::Path,
) -> (
    Arc<dyn lash_core::SessionStoreFactory>,
    Arc<RuntimePerfStoreMetrics>,
) {
    let factory = RuntimePerfStoreFactory::decorating_without_commit_measurement(Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new_with_process_registry(
            sessions_root,
            process_registry_path,
        ),
    ));
    let metrics = factory.metrics();
    (Arc::new(factory), metrics)
}

pub(crate) fn durable_postgres_session_store_factory_without_commit_measurement(
    postgres: &lash_postgres_store::PostgresStorage,
) -> (
    Arc<dyn lash_core::SessionStoreFactory>,
    Arc<RuntimePerfStoreMetrics>,
) {
    let factory = RuntimePerfStoreFactory::decorating_without_commit_measurement(Arc::new(
        postgres.session_store_factory_with_shared_process_registry(),
    ));
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
    let sqlite = Arc::new(
        lash_sqlite_store::SqliteBackend::open(&root)
            .await
            .map_err(|err| anyhow::anyhow!(err.to_string()))?,
    );
    let (store_factory, store_metrics): (
        Arc<dyn lash_core::SessionStoreFactory>,
        Arc<RuntimePerfStoreMetrics>,
    ) = if !wiring.measure_commit_bytes {
        // Store-reopen scenarios keep the backend's own catalog: they
        // measure reopen, not decorated durable commits.
        (
            sqlite.session_store_factory(),
            Arc::new(RuntimePerfStoreMetrics::default()),
        )
    } else {
        let factory = RuntimePerfStoreFactory::decorating(sqlite.session_store_factory());
        let metrics = factory.metrics();
        (Arc::new(factory), metrics)
    };
    let backend: Arc<dyn lash::persistence::LashlangArtifactBackend> =
        Arc::new(PerfBackend::over(sqlite).with_catalog(Arc::clone(&store_factory)));
    let effect_host = backend.effect_host();
    for factory in benchmark_plugin_factories(scenario, &effect_host, None, None) {
        plugin_stack.push(factory);
    }
    let core =
        durable_benchmark_core(backend, mode_id, provider, plugin_stack, wiring.queued_work)?;
    let session_id = SessionId::from(format!("runtime-perf-{}", scenario.name()));
    let session = core.open_session(session_id.clone()).await?;
    let persistence = if wiring.session_store_handle {
        Some(
            store_factory
                .open_existing_store_by_id(&session_id)
                .await
                .map_err(anyhow::Error::msg)?
                .ok_or_else(|| {
                    anyhow::anyhow!("runtime perf SQLite session store was not created")
                })?,
        )
    } else {
        None
    };
    Ok(BenchmarkRuntime {
        store_metrics,
        turn_entry: TurnEntry::Host,
        process_phase_probes: None,
        core,
        session: Some(session),
        store: None,
        persistence,
        provider_control: None,
        settlement_control: None,
        tool_catalog_observer: None,
        _openai_compat_server: None,
    })
}

/// A benchmark core on a durable backend, with the lane's plugin stack.
fn durable_benchmark_core(
    backend: Arc<dyn lash::persistence::LashlangArtifactBackend>,
    mode_id: ExecutionMode,
    provider: ProviderHandle,
    plugin_stack: lash::PluginStack,
    queued_work: bool,
) -> anyhow::Result<BenchmarkCore> {
    let builder = match mode_id {
        ExecutionMode::Standard => benchmark_standard_builder(backend, provider),
        ExecutionMode::Rlm => {
            let factory = benchmark_rlm_protocol_factory(backend.as_ref());
            benchmark_rlm_builder(backend, provider, factory)
        }
    };
    let mut builder = builder
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .plugins(plugin_stack);
    if !queued_work {
        builder = builder.without_queued_work();
    }
    let core = builder.build(runtime_perf_owner())?;
    Ok(match mode_id {
        ExecutionMode::Standard => BenchmarkCore::Standard(core),
        ExecutionMode::Rlm => BenchmarkCore::Rlm(core),
    })
}

#[cfg(test)]
mod tests;
