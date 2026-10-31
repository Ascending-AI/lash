//! Causal engine clocks, host completion, and physical SQLite work for the
//! send-to-completion diagnostic. The observer retains discovery lag only;
//! phase boundaries come from the serving engine's successful commits.

use lash_protocol_types::{llm::types::StreamBlockEvent, llm::types::StreamBlockKind};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use lash_core_ids::perf_witness::latency::Collector;
use lash_core_ids::perf_witness::latency::Snapshot;
use lash_core_ids::perf_witness::latency::Stamp;
use lash_core_ids::perf_witness::latency::Worker;
use serde::Serialize;

use lash_kernel_value::identity::SessionId;
use lash_protocol_types::sync::MutexExt;

use super::FollowerMode;
#[path = "diagnostics.rs"]
mod diagnostics;
use super::loop_spans::{
    HostEvent, LoopClocks, LoopShape, LoopSpans, LoopSummaries, SessionStamps, delta_ordinals,
};
use super::provider::{
    HoldRegistry, LaneHold, LatencyProviderKind, ProviderTiming, latency_provider,
};
use crate::perf_support::memory::process_memory_sample;
use crate::perf_support::metrics::percentile_sorted;
use crate::perf_support::scheduler::process_cpu_ms;
use crate::perf_support::time::round3;
use crate::runtime_perf::openai_compat::OpenAiCompatBenchServer;
use crate::runtime_perf::providers::BenchmarkStreamProfile;
pub(crate) use diagnostics::{CompletedSample, DiagnosticDisposition, Diagnostics};

/// The store poller's interval bounds. A fresh send's admission is imminent, so
/// the interval starts at the floor and a tick that lands a mark or first
/// sees the open row restarts it there; a tick that observes nothing
/// doubles it up to the ceiling, keeping an idle wait's reads sparse enough
/// to leave checkpoint windows under concurrent lanes. These phase markers
/// carry up to one interval of observation error; the send-to-completion
/// measurement uses the direct outcome clock.
const STORE_POLL_FLOOR: Duration = Duration::from_millis(2);
pub(crate) const STORE_POLL_CEILING: Duration = Duration::from_millis(50);
/// A sample's store poller gives up here; its marks stay `None`.
pub(crate) const STORE_POLL_TIMEOUT: Duration = Duration::from_secs(120);
/// The follower poll schedule the `poll_detect` phase simulates.
const POLL_FLOOR_MS: f64 = 25.0;
const POLL_CEILING_MS: f64 = 1_000.0;

/// Where a case's turns execute.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Topology {
    /// The node serves in this process: the same backend submits and
    /// executes, and live replay plus the settled-run mailbox are
    /// process-local.
    SameProcess,
    /// A `lash-perf latency-worker` child process serves the endpoint; the
    /// host only sends. Live replay and the mailbox never reach the host.
    CrossWorker,
}

/// One latency case's fixed shape.
pub(crate) struct CaseSpec {
    pub(crate) name: &'static str,
    pub(crate) topology: Topology,
    pub(crate) provider: LatencyProviderKind,
    pub(crate) follower: FollowerMode,
    pub(crate) samples: usize,
    pub(crate) lanes: usize,
    /// Queue each measured input behind a held sibling run.
    pub(crate) busy: bool,
    /// Open a fresh session for every sample: each send is a cold first turn.
    pub(crate) fresh_sessions: bool,
}

impl CaseSpec {
    /// The chat loop this case's samples run (FIG-6841).
    pub(crate) fn loop_shape(&self) -> LoopShape {
        if self.busy {
            LoopShape::Held
        } else if self.fresh_sessions {
            LoopShape::Cold
        } else {
            match self.provider {
                LatencyProviderKind::Steer => LoopShape::Steer,
                LatencyProviderKind::Tool | LatencyProviderKind::Cell => LoopShape::ToolLoop,
                _ => LoopShape::BackToBack,
            }
        }
    }
}

/// Shared run environment: the store root.
pub(crate) struct LatencyEnv {
    store_dir: PathBuf,
    pub(crate) dhat_out: Option<PathBuf>,
    pub(crate) dhat_frames: Option<usize>,
}

impl LatencyEnv {
    /// The run's environment over `store_dir`.
    pub(crate) async fn open(store_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(store_dir)
            .with_context(|| format!("create {}", store_dir.display()))?;
        Ok(Self {
            store_dir: store_dir.to_path_buf(),
            dhat_out: None,
            dhat_frames: None,
        })
    }

    /// What the report records about the engine under measurement.
    pub(crate) fn describe(&self) -> serde_json::Value {
        let executable_sha256 = std::env::current_exe()
            .ok()
            .and_then(|path| crate::perf_support::git::artifact_sha256(&path));
        serde_json::json!({
            "executable_sha256": executable_sha256,
            "clock": "Linux CLOCK_MONOTONIC for intervals; Unix nanoseconds for load joins",
            "engine_commit_clock": "successful COMMIT start, with terminal COMMIT return retained as the upper bound",
            "engine": "lash-durable",
            "store": "sqlite-file",
            "sqlite_synchronous": "Full",
            "heap_profile_parent": self.dhat_out,
            "heap_profile_children": self.dhat_out.as_ref().map(|path|
                "one sibling <parent-stem>.<case>.child.dhat.json per cross-worker case".to_owned() +
                &format!(" beside {}", path.display())),
        })
    }
}

/// One measured send→outcome round trip. Instants are elapsed milliseconds
/// since the request; `provider_ms` is provider-side call time summed over
/// the sample's model calls.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Sample {
    pub(crate) case: &'static str,
    pub(crate) lane: usize,
    pub(crate) index: usize,
    /// First send on a freshly opened session.
    pub(crate) cold: bool,
    pub(crate) status: String,
    pub(crate) session_id: String,
    pub(crate) input_id: String,
    pub(crate) clocks: SampleClocks,
    pub(crate) admission_to_first_visible_ms: Option<f64>,
    pub(crate) admission_to_first_activity_ms: Option<f64>,
    pub(crate) admission_observation_lag_ms: Option<f64>,
    pub(crate) terminal_observation_lag_ms: Option<f64>,
    pub(crate) request_to_accept_ms: f64,
    pub(crate) accept_to_admission_ms: Option<f64>,
    pub(crate) accept_to_applied_ms: Option<f64>,
    pub(crate) admission_to_first_delta_ms: Option<f64>,
    pub(crate) admission_to_settled_ms: Option<f64>,
    pub(crate) settled_to_complete_ms: Option<f64>,
    pub(crate) send_to_completion_ms: f64,
    pub(crate) provider_ms: Option<f64>,
    /// `send_to_completion − provider`, the number the gate binds.
    pub(crate) overhead_ms: Option<f64>,
    /// Host-observed provider span (ModelRequestStarted→ModelCallRecorded),
    /// the provider-time fallback where provider-side timing is unavailable.
    pub(crate) model_span_ms: Option<f64>,
    /// First instant a pure-poll follower (25 ms→1 s backoff) could see the
    /// settlement — the simulated wake schedule applied to `settled`.
    pub(crate) poll_detect_ms: Option<f64>,
    pub(crate) poller_timed_out: bool,
    pub(crate) diagnostic: DiagnosticDisposition,
    /// Process-wide CPU (every thread, `utime + stime`) and RSS read as the
    /// sample completes. Both are cumulative: consecutive samples' deltas
    /// give per-turn growth on a 1-lane run, not a turn's own cost.
    pub(crate) sqlite_workers_at_request: Vec<Worker>,
    pub(crate) sqlite_workers_at_completion: Vec<Worker>,
    pub(crate) process_cpu_ms_at_end: Option<f64>,
    pub(crate) process_rss_kb_at_end: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    /// The chat-loop clocks and the spans derived from them (FIG-6841).
    pub(crate) loop_clocks: LoopClocks,
    pub(crate) loop_spans: LoopSpans,
}

/// Absolute clocks use a shared host monotonic clock for intervals and Unix
/// nanoseconds for joining the load ledger. The acknowledgement can race
/// admission; it is retained separately from durable acceptance.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SampleClocks {
    pub(crate) request: Stamp,
    accepted: Option<Stamp>,
    acceptance_acknowledged: Stamp,
    admission: Option<Stamp>,
    terminal: Option<Stamp>,
    terminal_commit_returned: Option<Stamp>,
    pub(crate) completion: Stamp,
    first_activity: Option<Stamp>,
    first_visible: Option<Stamp>,
    first_delta: Option<Stamp>,
    admission_observed: Option<Stamp>,
    terminal_observed: Option<Stamp>,
    host_first_activity: Option<Stamp>,
    host_first_visible: Option<Stamp>,
    pub(crate) host_first_delta: Option<Stamp>,
}
#[derive(Default)]
pub(crate) struct PollMarks {
    pub(crate) admission_observed: Option<Stamp>,
    pub(crate) terminal_observed: Option<Stamp>,
    pub(crate) timed_out: bool,
    pub(crate) error: Option<String>,
}

/// The sink `outcome_into` taps: first activity, first visible delta and
/// the host-observed model span, all elapsed ms from the request.
struct TimingSink {
    t_request: Instant,
    state: std::sync::Mutex<TimingSinkState>,
}

#[derive(Default)]
struct TimingSinkState {
    first_activity: Option<Stamp>,
    first_delta: Option<Stamp>,
    first_visible: Option<Stamp>,
    model_started_ms: Option<f64>,
    model_span_ms: f64,
    host_events: Vec<HostEvent>,
}

impl TimingSink {
    fn new(t_request: Instant) -> Self {
        Self {
            t_request,
            state: std::sync::Mutex::new(TimingSinkState::default()),
        }
    }

    fn elapsed_ms(&self) -> f64 {
        self.t_request.elapsed().as_secs_f64() * 1000.0
    }
}

#[async_trait::async_trait]
impl lash::TurnActivitySink for TimingSink {
    async fn emit(&self, activity: lash_core::TurnActivity) {
        let now = self.elapsed_ms();
        let stamp = Stamp::now();
        let mut state = self.state.lock_recover();
        state.first_activity.get_or_insert(stamp);
        match activity.event {
            lash_core::TurnEvent::StreamBlock(StreamBlockEvent::Delta {
                kind: StreamBlockKind::AssistantText,
                text,
                ..
            }) => {
                state.first_delta.get_or_insert(stamp);
                state.first_visible.get_or_insert(stamp);
                let ordinals = delta_ordinals(&text);
                if !ordinals.is_empty() {
                    state.host_events.push(HostEvent::Delta {
                        at: stamp,
                        ordinals,
                    });
                }
            }
            lash_core::TurnEvent::StreamBlock(StreamBlockEvent::Delta {
                kind: StreamBlockKind::Reasoning,
                ..
            }) => {
                state.first_delta.get_or_insert(stamp);
                state.first_visible.get_or_insert(stamp);
            }
            lash_core::TurnEvent::CheckpointRecorded { .. } => {
                state
                    .host_events
                    .push(HostEvent::CheckpointRecorded { at: stamp });
            }
            lash_core::TurnEvent::StreamBlock(StreamBlockEvent::Started { .. })
            | lash_core::TurnEvent::ToolCallStarted { .. }
            | lash_core::TurnEvent::Finished { .. } => {
                state.first_visible.get_or_insert(stamp);
            }
            lash_core::TurnEvent::ModelRequestStarted { .. } => {
                state.model_started_ms = Some(now);
            }
            lash_core::TurnEvent::ModelCallRecorded { .. } => {
                if let Some(started) = state.model_started_ms.take() {
                    state.model_span_ms += now - started;
                }
            }
            _ => {}
        }
    }
}

/// The session handle a lane sends on: a live open session in-process, a
/// durable session over the shared store cross-worker.
pub(crate) enum LaneSession {
    Live(lash::LashSession),
    Durable(lash::DurableSession),
}

impl LaneSession {
    pub(crate) fn send(&self, input: lash::TurnInput) -> lash::SendBuilder {
        match self {
            Self::Live(session) => session.send(input),
            Self::Durable(session) => session.send(input),
        }
    }

    /// Lane teardown: a live session is parked out of the shifts so its last
    /// shift leg stops instead of being orphaned when the endpoint drops.
    /// A durable session runs no runtime — its last shift ends on its own
    /// admission check.
    pub(crate) async fn close(self) -> Result<()> {
        match self {
            Self::Live(session) => session.close().await.map_err(anyhow::Error::from),
            Self::Durable(_) => Ok(()),
        }
    }
}

/// The topology a case's lanes run over.
///
/// `observer` is a second core over its own store set on the same directory:
/// the per-sample durable pollers read through its connections (WAL readers
/// beside the writer) so marker reads never serialize on the
/// connections the shift itself writes through.
pub(crate) struct CaseTopology {
    pub(crate) core: lash::LashCore,
    pub(crate) observer: lash::LashCore,
    worker: Option<tokio::process::Child>,
    pub(crate) diagnostics: Diagnostics,
}

impl CaseTopology {
    /// A `lash-perf latency-worker` child serves `stores_dir`; the host core
    /// only sends and follows.
    pub(crate) async fn cross_worker(
        stores_dir: &Path,
        provider: LatencyProviderKind,
        follower: FollowerMode,
        dhat_out: Option<&Path>,
        dhat_frames: Option<usize>,
    ) -> Result<Self> {
        let worker = start_worker(stores_dir, provider, dhat_out, dhat_frames).await?;
        let core = build_observer_with_mode(stores_dir, follower).await?;
        let observer = build_observer(stores_dir).await?;
        Ok(Self {
            core,
            observer,
            worker: Some(worker),
            diagnostics: Diagnostics::default(),
        })
    }

    pub(crate) async fn shutdown(self) -> Result<()> {
        self.core.shutdown().await?;
        self.observer.shutdown().await?;
        if let Some(mut worker) = self.worker {
            // EOF requests a clean node shutdown; the timeout still kills and reaps
            // a child that cannot stop. kill_on_drop covers earlier error exits.
            drop(worker.stdin.take());
            match tokio::time::timeout(Duration::from_secs(10), worker.wait()).await {
                Ok(status) => anyhow::ensure!(status?.success(), "latency worker failed"),
                Err(_) => {
                    worker.kill().await?;
                }
            }
        }
        Ok(())
    }
}

/// Run one case to completion and collect every sample.
pub(crate) async fn run_case(
    spec: &CaseSpec,
    env: &LatencyEnv,
) -> Result<(CaseReport, Vec<Sample>)> {
    let collector =
        Collector::install().map_err(|_| anyhow::anyhow!("latency witness already installed"))?;
    let timing = Arc::new(ProviderTiming::default());
    let holds = (spec.busy || spec.provider == LatencyProviderKind::Steer)
        .then(|| Arc::new(HoldRegistry::default()));
    let compat_server = if spec.provider == LatencyProviderKind::OpenAiCompat {
        Some(OpenAiCompatBenchServer::start(compat_profile()).await?)
    } else {
        None
    };
    let case_dir = env.store_dir.join(spec.name);
    let _ = std::fs::remove_dir_all(&case_dir);
    std::fs::create_dir_all(&case_dir).with_context(|| format!("create {}", case_dir.display()))?;

    let topology = match spec.topology {
        Topology::SameProcess => {
            let stores_dir = case_dir.join("same");
            let stores = lash::sqlite::SqliteStoreSet::open(
                stores_dir.join("lash.db"),
                lash::sqlite::SqliteSynchronous::Full,
            )
            .await
            .map_err(|error| anyhow::anyhow!("open case store set: {error}"))?;
            let backend = durable_backend(stores)?;
            let core = build_core(
                backend,
                spec,
                Arc::clone(&timing),
                holds.clone(),
                &compat_server,
                None,
            )?;
            let observer = build_observer(&stores_dir).await?;
            CaseTopology {
                core,
                observer,
                worker: None,
                diagnostics: Diagnostics::default(),
            }
        }
        Topology::CrossWorker => {
            let child_profile = env.dhat_out.as_ref().map(|path| {
                path.with_file_name(format!(
                    "{}.{}.child.dhat.json",
                    path.file_stem().unwrap_or_default().to_string_lossy(),
                    spec.name
                ))
            });
            CaseTopology::cross_worker(
                &case_dir.join("worker"),
                spec.provider,
                spec.follower,
                child_profile.as_deref(),
                env.dhat_frames,
            )
            .await?
        }
    };

    eprintln!("durability-window {} begin", spec.name);
    let work_start = collector.snapshot();
    let wakes_before = lash_core::perf_witness::wakes::counts();
    let follows_before = lash_core::perf_witness::wakes::follow_counts();
    let started = Instant::now();
    let per_lane = spec.samples.div_ceil(spec.lanes);
    let lane_count = spec.samples.div_ceil(per_lane);
    let mut lanes = Vec::with_capacity(lane_count);
    for lane in 0..lane_count {
        let lane_samples = per_lane.min(spec.samples - lane * per_lane);
        lanes.push(run_lane(
            spec,
            &topology,
            lane,
            lane_samples,
            Arc::clone(&timing),
            holds.clone(),
        ));
    }
    let mut samples = Vec::new();
    let mut errors = Vec::new();
    for outcome in futures_util::future::join_all(lanes).await {
        match outcome {
            Ok(lane_samples) => samples.extend(lane_samples),
            Err(error) => errors.push(format!("lane failed: {error:#}")),
        }
    }
    let wall = started.elapsed();
    let (mut samples, diagnostics) = topology.diagnostics.finish(samples).await?;
    samples.sort_by_key(|sample| (sample.lane, sample.index));
    let work_end = collector.snapshot();
    eprintln!("durability-window {} end", spec.name);
    let mut workers = worker_delta(&work_start, &work_end);
    let wakes = lash_core::perf_witness::wakes::counts().since(wakes_before);
    let follows = lash_core::perf_witness::wakes::follow_counts().since(follows_before);
    topology.shutdown().await?;
    let mut witness = collector.snapshot();
    if spec.topology == Topology::CrossWorker {
        let child: Snapshot = serde_json::from_slice(&std::fs::read(
            case_dir.join("worker/latency-witness.json"),
        )?)?;
        workers.extend(child.workers.values().cloned());
        for (input, child_input) in child.inputs {
            let input = witness.inputs.entry(input).or_default();
            input.run = child_input.run;
        }
        witness.runs.extend(child.runs);
        witness.dropped_records += child.dropped_records;
        let stamps: std::collections::HashMap<String, SessionStamps> = serde_json::from_slice(
            &std::fs::read(case_dir.join("worker").join(LOOP_STAMPS_FILE))?,
        )?;
        timing.stamps.merge(stamps);
    }
    anyhow::ensure!(
        witness.dropped_records == 0,
        "latency witness overflow: {}",
        witness.dropped_records
    );
    for sample in &mut samples {
        fill_engine_phases(sample, &witness)?;
    }
    super::loop_spans::fill(spec.loop_shape(), &mut samples, &timing);
    let mut report = CaseReport::assemble(spec, &samples, errors, wall);
    report.diagnostics = diagnostics;
    report.actual_lanes = lane_count;
    report.sqlite_work = SqliteWork::assemble(workers, samples.len(), spec.busy);
    // A cross-worker case's sessions run in the child: this process saw none.
    if spec.topology == Topology::SameProcess {
        report.session_wakes = Some(wakes);
    }
    report.follow_resolves = Some(follows);
    Ok((report, samples))
}

/// Build the host core for a case: the latency provider, the benchmark echo
/// tool plugin and the harness model spec.
pub(super) fn build_core(
    backend: lash::Backend,
    spec: &CaseSpec,
    timing: Arc<ProviderTiming>,
    holds: Option<Arc<HoldRegistry>>,
    compat_server: &Option<OpenAiCompatBenchServer>,
    transport: Option<Arc<dyn lash_http_transport::HttpTransport>>,
) -> Result<lash::LashCore> {
    let effect_host = backend.clone();
    let provider: lash::provider::ProviderHandle = if let Some(server) = compat_server {
        let mut compat = lash_provider_openai::OpenAiCompatibleProvider::new(
            "latency-gate",
            server.base_url.clone(),
        )
        .with_options(lash::provider::ProviderOptions {
            reliability: lash::provider::ProviderReliability::disabled(),
            ..lash::provider::ProviderOptions::default()
        });
        if let Some(transport) = transport {
            compat = compat.with_transport(transport);
        }
        lash::provider::ProviderHandle::new(compat.into_components())
    } else {
        latency_provider(spec.provider, Arc::clone(&timing), holds).into_handle()
    };
    let mut plugins = lash::PluginStack::new();
    plugins.push(Arc::new(crate::perf_support::tool_plugin(
        "latency_tools",
        lash::tool_provider_key!("tools"),
        Arc::new(super::chat_loop::StampedTools::new(
            crate::runtime_perf::providers::BenchmarkEchoTool::new(effect_host.clone()),
            Arc::clone(&timing),
        )),
    )?));
    let builder = if matches!(
        spec.provider,
        LatencyProviderKind::Cell | LatencyProviderKind::HistoryCell { .. }
    ) {
        let protocol = super::chat_loop::cell_protocol(spec.name == "history")?;
        if spec.name == "history" {
            *timing.vm_pool.lock_recover() = Some(protocol.worker_service().pool()?);
            *timing.history_vm_service.lock_recover() = Some(protocol.worker_service().clone());
        }
        lash::LashCore::rlm_builder(backend, protocol)
    } else {
        lash::LashCore::standard_builder(backend)
    };
    let builder = if spec.name == "history" {
        plugins.push(Arc::new(
            lash_plugin_standard_compaction::StandardCompactionPluginFactory::new(
                lash_plugin_standard_compaction::StandardCompactionConfig {
                    pressure_compaction: false,
                    attachment_pruning: false,
                    keep_recent_tokens: 64,
                    ..lash_plugin_standard_compaction::StandardCompactionConfig::standard()
                },
            ),
        ));
        builder
            .trace_sink(Arc::new(super::history::DiscardTrace))
            .content_policy(lash::tracing::ContentPolicy::omitted())
    } else {
        builder
    };
    builder
        .serve_sessions(spec.topology == Topology::SameProcess && spec.name != "observer")
        .observer_pacing(spec.follower.pacing()?)
        .serve_test_llm_profile(provider, latency_llm_profile_spec()?)
        .plugins(plugins)
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(latency_owner())
        .map_err(anyhow::Error::from)
}

/// A read-only core over a second store set on `stores_dir`. The per-sample
/// durable pollers read through its connections (WAL readers beside the
/// writer) so their cadence never serializes on the shift's own connections.
async fn build_observer(stores_dir: &Path) -> Result<lash::LashCore> {
    build_observer_with_mode(stores_dir, FollowerMode::Standard).await
}

async fn build_observer_with_mode(
    stores_dir: &Path,
    follower: FollowerMode,
) -> Result<lash::LashCore> {
    let stores = lash::sqlite::SqliteStoreSet::open(
        stores_dir.join("lash.db"),
        lash::sqlite::SqliteSynchronous::Full,
    )
    .await
    .map_err(|error| anyhow::anyhow!("open observer store set: {error}"))?;
    let backend = durable_backend(stores)?;
    let spec = CaseSpec {
        name: "observer",
        topology: Topology::SameProcess,
        provider: LatencyProviderKind::Text,
        follower,
        samples: 0,
        lanes: 0,
        busy: false,
        fresh_sessions: false,
    };
    build_core(
        backend,
        &spec,
        Arc::new(ProviderTiming::default()),
        None,
        &None,
        None,
    )
}

/// The durable engine's backend over `stores`.
pub(super) fn durable_backend(stores: lash::sqlite::SqliteStoreSet) -> Result<lash::Backend> {
    lash::durable::DurableBackendBuilder::new(Arc::new(stores))
        .config(lash::durable::DurableSettings::standard())
        .build()
        .map_err(|error| anyhow::anyhow!("build the durable backend: {error}"))
}

pub(super) fn latency_llm_profile_spec() -> Result<lash::LlmProfileMetadata> {
    lash::LlmProfileMetadata::builder("latency-model")
        .cache_retention(lash::provider::CacheRetention::Short)
        .context_window_tokens(200_000)
        .build()
        .map_err(|error| anyhow::anyhow!("latency model spec: {error}"))
}

fn latency_owner() -> lash::persistence::LeaseOwnerIdentity {
    lash::persistence::LeaseOwnerIdentity::opaque(
        lash::persistence::LeaseOwnerId::new("lash-perf-latency"),
        lash::persistence::LeaseIncarnationId::new(format!("{}", std::process::id())),
    )
}

/// The stream profile the OpenAI-compat fixture serves: 32 small chunks.
fn compat_profile() -> BenchmarkStreamProfile {
    let deltas: Vec<String> = (0..32).map(|index| format!("delta-{index:03} ")).collect();
    BenchmarkStreamProfile {
        full_text: deltas.concat(),
        deltas,
        parts: Vec::new(),
    }
}

/// Create a fresh session for a lane: a live open session in-process, a
/// durable handle over the shared store cross-worker.
pub(crate) async fn create_lane_session(
    topology: &CaseTopology,
    kind: Topology,
    session_id: &SessionId,
) -> Result<LaneSession> {
    let durable = topology
        .core
        .session(session_id.clone())
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            latency_llm_profile_spec()?.wire_model,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        )))
        .await
        .map_err(anyhow::Error::from)?;
    Ok(match kind {
        Topology::SameProcess => LaneSession::Live(
            topology
                .core
                .session(session_id.clone())
                .open()
                .await
                .map_err(anyhow::Error::from)?,
        ),
        Topology::CrossWorker => LaneSession::Durable(durable),
    })
}

/// The store `poll_marks` reads `session_id` through. The observer uses a
/// non-creating catalog lookup and keyed reads beside the live writer; it
/// does not acquire execution authority.
pub(crate) async fn observer_poll_store(
    observer: &lash::LashCore,
    session_id: &SessionId,
) -> Result<Arc<dyn lash::persistence::RuntimeStore>> {
    let factory = observer.backend().session_store_factory();
    if !matches!(
        factory.lookup_session(session_id).await?,
        lash_core::SessionLookup::Live(_)
    ) {
        anyhow::bail!("the observer catalog has no store for `{session_id}`");
    }
    Ok(factory)
}

/// One lane: open its session, run `count` measured sends sequentially.
async fn run_lane(
    spec: &CaseSpec,
    topology: &CaseTopology,
    lane: usize,
    count: usize,
    timing: Arc<ProviderTiming>,
    holds: Option<Arc<HoldRegistry>>,
) -> Result<Vec<CompletedSample>> {
    if spec.fresh_sessions {
        return super::chat_loop::cold_lane(spec, topology, lane, count, &timing).await;
    }
    let session_id = SessionId::fixture(format!("latency-{}-{lane}", spec.name));
    let session = create_lane_session(topology, spec.topology, &session_id).await?;
    let poll_store = observer_poll_store(&topology.observer, &session_id).await?;
    let hold = holds.map(|registry| registry.lane(&session_id));
    let ctx = SampleCtx {
        spec,
        session: &session,
        poll_store: &poll_store,
        session_id: &session_id,
        timing: &timing,
        diagnostics: &topology.diagnostics,
    };
    let mut samples = Vec::with_capacity(count);
    for index in 0..count {
        let sample = match (&hold, spec.provider) {
            (Some(hold), LatencyProviderKind::Steer) => {
                super::chat_loop::steer_sample(&ctx, lane, index, hold).await?
            }
            _ => run_sample(&ctx, lane, index, hold.as_deref()).await?,
        };
        samples.push(sample);
    }
    // Teardown only: a close refusal leaves the quiesce wait below to finish
    // the shift; it must not cost the lane its measured samples.
    let _ = session.close().await;
    Ok(samples)
}

/// The immutable context every measured send in a lane shares.
pub(crate) struct SampleCtx<'a> {
    pub(crate) spec: &'a CaseSpec,
    pub(crate) session: &'a LaneSession,
    pub(crate) poll_store: &'a Arc<dyn lash::persistence::RuntimeStore>,
    pub(crate) session_id: &'a SessionId,
    pub(crate) timing: &'a Arc<ProviderTiming>,
    pub(crate) diagnostics: &'a Diagnostics,
}

/// One measured send. `busy` lanes first park a sibling input's provider
/// call so the measured input queues behind a live run; the hold releases
/// the moment the measured input is durably accepted.
async fn run_sample(
    ctx: &SampleCtx<'_>,
    lane: usize,
    index: usize,
    hold: Option<&LaneHold>,
) -> Result<CompletedSample> {
    let held = if let Some(hold) = hold {
        hold.arm();
        let held = ctx
            .session
            .send(lash::TurnInput::text(format!(
                "latency {} held {lane}/{index}",
                ctx.spec.name
            )))
            .into_future()
            .await
            .map_err(anyhow::Error::from)?;
        tokio::time::timeout(STORE_POLL_TIMEOUT, hold.started.notified())
            .await
            .context("the held provider call never started")?;
        Some((held, hold))
    } else {
        None
    };
    let sample = measure_send(ctx, lane, index, None, LoopClocks::default(), || {
        if let Some((_, hold)) = &held {
            hold.release_one();
        }
    })
    .await;
    if let Some((held_handle, _)) = held {
        let _ = held_handle.outcome().await;
    }
    sample
}

/// The measured send itself: acceptance, the store poller, the tapped
/// outcome and the provider-side duration. `after_accept` runs the moment
/// the input is durably accepted — the `busy` case's release point. `id`
/// names the input's run when another send must address it (`steer`).
pub(crate) async fn measure_send(
    ctx: &SampleCtx<'_>,
    lane: usize,
    index: usize,
    id: Option<lash_vocabulary::identity::TurnId>,
    mut loop_clocks: LoopClocks,
    after_accept: impl FnOnce(),
) -> Result<CompletedSample> {
    let sqlite_workers_at_request = lash_core::perf_witness::latency::workers();
    let text = if ctx.spec.provider == LatencyProviderKind::Cell {
        format!("{} {lane}/{index}", super::provider::CELL_SAMPLE_TEXT)
    } else {
        format!("latency {} sample {lane}/{index}", ctx.spec.name)
    };
    let request = Stamp::now();
    let t_request = Instant::now();
    let send = ctx.session.send(lash::TurnInput::text(text));
    let send = match id {
        Some(id) => send.id(id),
        None => send,
    };
    let handle = send.into_future().await.map_err(anyhow::Error::from)?;
    let acceptance_acknowledged = Stamp::now();
    let request_to_accept_ms = acceptance_acknowledged.since(request);
    after_accept();
    let input_id = handle.input_id().clone();
    let pending = ctx.diagnostics.start(
        ctx.session_id,
        &input_id,
        poll_marks(
            Arc::clone(ctx.poll_store),
            ctx.session_id.clone(),
            input_id.clone(),
        ),
    );
    let sink = TimingSink::new(t_request);
    let (status, outcome_error) = match handle.outcome_into(&sink).await {
        Ok(outcome) => (status_name(&outcome.status()), None),
        Err(error) => ("error".to_string(), Some(format!("{error:#}"))),
    };
    let completion = Stamp::now();
    let sqlite_workers_at_completion = lash_core::perf_witness::latency::workers();
    let send_to_completion_ms = completion.since(request);
    let process_cpu_ms_at_end = process_cpu_ms();
    let process_rss_kb_at_end = process_memory_sample().rss_kb;
    let mut sink_state = sink.state.lock_recover();
    let model_span_ms = (sink_state.model_span_ms > 0.0).then_some(sink_state.model_span_ms);
    let host_first_activity = sink_state.first_activity;
    let host_first_visible = sink_state.first_visible;
    let host_first_delta = sink_state.first_delta;
    loop_clocks.host_events = std::mem::take(&mut sink_state.host_events);
    drop(sink_state);
    let provider_ms = ctx.timing.take_ms(ctx.session_id);
    let overhead_ms = provider_ms
        .or(model_span_ms)
        .map(|provider| send_to_completion_ms - provider);

    Ok(CompletedSample {
        pending,
        sample: Sample {
            case: ctx.spec.name,
            lane,
            index,
            cold: index == 0,
            status,
            session_id: ctx.session_id.as_str().to_owned(),
            input_id: input_id.as_str().to_owned(),
            clocks: SampleClocks {
                request,
                acceptance_acknowledged,
                completion,
                accepted: None,
                admission: None,
                terminal: None,
                terminal_commit_returned: None,
                first_activity: None,
                first_visible: None,
                first_delta: None,
                admission_observed: None,
                terminal_observed: None,
                host_first_activity,
                host_first_visible,
                host_first_delta,
            },
            request_to_accept_ms,
            accept_to_admission_ms: None,
            accept_to_applied_ms: None,
            admission_to_first_delta_ms: None,
            admission_to_first_visible_ms: None,
            admission_to_first_activity_ms: None,
            admission_to_settled_ms: None,
            settled_to_complete_ms: None,
            admission_observation_lag_ms: None,
            terminal_observation_lag_ms: None,
            send_to_completion_ms,
            provider_ms,
            overhead_ms,
            model_span_ms,
            poll_detect_ms: None,
            poller_timed_out: false,
            diagnostic: DiagnosticDisposition::Pending,
            sqlite_workers_at_request,
            sqlite_workers_at_completion,
            process_cpu_ms_at_end,
            process_rss_kb_at_end,
            error: outcome_error,
            loop_clocks,
            loop_spans: LoopSpans::default(),
        },
    })
}

/// The `TurnStatus` name a sample records.
pub(crate) fn status_name(status: &lash::TurnStatus) -> String {
    match status {
        lash::TurnStatus::Answered => "answered".to_string(),
        lash::TurnStatus::Failed { .. } => "failed".to_string(),
        lash::TurnStatus::Cancelled => "cancelled".to_string(),
        lash::TurnStatus::Parked(_) => "parked".to_string(),
        other => format!("{other:?}"),
    }
}

/// Keyed evidence discovery remains a diagnostic clock, never a phase
/// boundary. Read completion is stamped after the asynchronous read.
pub(crate) async fn poll_marks(
    store: Arc<dyn lash::persistence::RuntimeStore>,
    session_id: SessionId,
    input_id: lash_vocabulary::identity::InputId,
) -> PollMarks {
    let mut marks = PollMarks::default();
    let mut run = None;
    let mut interval = STORE_POLL_FLOOR;
    let discovery = async {
        loop {
            if run.is_none() {
                match store.run_of_input(&session_id, &input_id).await {
                    Ok(Some(bound)) => {
                        marks.admission_observed = Some(Stamp::now());
                        run = Some(bound);
                        interval = STORE_POLL_FLOOR;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        marks.error = Some(error.to_string());
                        return;
                    }
                }
            }
            if let Some(run) = &run {
                match store.run_terminal(&session_id, run).await {
                    Ok(Some(_)) => {
                        marks.terminal_observed = Some(Stamp::now());
                        return;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        marks.error = Some(error.to_string());
                        return;
                    }
                }
            }
            tokio::time::sleep(interval).await;
            interval = (interval * 2).min(STORE_POLL_CEILING);
        }
    };
    if tokio::time::timeout(STORE_POLL_TIMEOUT, discovery)
        .await
        .is_err()
    {
        marks.timed_out = true;
    }
    marks
}

fn fill_engine_phases(sample: &mut Sample, witness: &Snapshot) -> Result<()> {
    let input = witness
        .inputs
        .get(&sample.input_id)
        .context("missing acceptance witness")?;
    let accepted = input.accepted.context("missing durable acceptance clock")?;
    let run = input
        .run
        .as_ref()
        .and_then(|run| witness.runs.get(run))
        .context("missing engine run witness")?;
    let admission = run.admission.context("missing engine admission clock")?;
    let terminal = run.terminal.context("missing engine terminal clock")?;
    let clocks = &mut sample.clocks;
    clocks.accepted = Some(accepted);
    clocks.admission = Some(admission);
    clocks.terminal = Some(terminal);
    clocks.terminal_commit_returned = run.terminal_commit_returned;
    clocks.first_activity = run.first_activity;
    clocks.first_visible = run.first_visible;
    clocks.first_delta = run.first_delta;
    sample.request_to_accept_ms = accepted.since(clocks.request);
    sample.accept_to_admission_ms = Some(admission.since(accepted));
    sample.accept_to_applied_ms = sample.accept_to_admission_ms;
    sample.admission_to_first_activity_ms = run.first_activity.map(|stamp| stamp.since(admission));
    sample.admission_to_first_visible_ms = run.first_visible.map(|stamp| stamp.since(admission));
    sample.admission_to_first_delta_ms = run.first_delta.map(|stamp| stamp.since(admission));
    sample.admission_to_settled_ms = Some(terminal.since(admission));
    sample.settled_to_complete_ms = Some(clocks.completion.since(terminal));
    sample.admission_observation_lag_ms = clocks
        .admission_observed
        .map(|stamp| stamp.since(admission));
    sample.terminal_observation_lag_ms =
        clocks.terminal_observed.map(|stamp| stamp.since(terminal));
    sample.provider_ms = run.provider_ms;
    sample.model_span_ms = (run.model_span_ms > 0.0).then_some(run.model_span_ms);
    sample.overhead_ms = sample
        .provider_ms
        .or(sample.model_span_ms)
        .map(|ms| sample.send_to_completion_ms - ms);
    sample.poll_detect_ms = Some(simulated_poll_detect_ms(terminal.since(clocks.request)));
    Ok(())
}

fn worker_delta(start: &Snapshot, end: &Snapshot) -> Vec<Worker> {
    end.workers
        .iter()
        .map(|(key, worker)| {
            let mut worker = worker.clone();
            if let Some(before) = start.workers.get(key) {
                worker.busy_ns -= before.busy_ns;
                worker.write_transactions -= before.write_transactions;
                for (total, old) in worker
                    .write_queue_depths
                    .iter_mut()
                    .zip(before.write_queue_depths)
                {
                    *total -= old;
                }
                for (total, old) in worker.write_cohorts.iter_mut().zip(before.write_cohorts) {
                    *total -= old;
                }
            }
            worker
        })
        .collect()
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct SqliteWork {
    pub(crate) write_transactions_per_send: f64,
    pub(crate) writer_busy_ms_per_send: f64,
    pub(crate) reader_busy_ms_per_send: f64,
    /// Each physical thread keeps its own normalized cost; the reader total
    /// overlaps in time across readers and is not a latency phase.
    pub(crate) workers: Vec<WorkerWork>,
    includes_held_predecessors: bool,
}
#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkerWork {
    pub(crate) pid: u32,
    pub(crate) thread: String,
    pub(crate) role: &'static str,
    pub(crate) busy_ns: u64,
    pub(crate) write_transactions: u64,
    pub(crate) busy_ms_per_send: f64,
    pub(crate) write_queue_depths: [u64; 32],
    pub(crate) write_cohorts: [u64; 32],
}
impl SqliteWork {
    fn assemble(workers: Vec<Worker>, sends: usize, held: bool) -> Self {
        let denominator = sends.max(1) as f64;
        Self {
            write_transactions_per_send: workers.iter().map(|w| w.write_transactions).sum::<u64>()
                as f64
                / denominator,
            writer_busy_ms_per_send: workers
                .iter()
                .filter(|w| !w.reader)
                .map(|w| w.busy_ns)
                .sum::<u64>() as f64
                / 1_000_000.0
                / denominator,
            reader_busy_ms_per_send: workers
                .iter()
                .filter(|w| w.reader)
                .map(|w| w.busy_ns)
                .sum::<u64>() as f64
                / 1_000_000.0
                / denominator,
            workers: workers
                .into_iter()
                .map(|w| WorkerWork {
                    pid: w.pid,
                    thread: w.thread,
                    role: if w.reader { "reader" } else { "writer" },
                    busy_ns: w.busy_ns,
                    write_transactions: w.write_transactions,
                    busy_ms_per_send: w.busy_ns as f64 / 1_000_000.0 / denominator,
                    write_queue_depths: w.write_queue_depths,
                    write_cohorts: w.write_cohorts,
                })
                .collect(),
            includes_held_predecessors: held,
        }
    }
}

/// The first instant a poll-only follower's wake schedule lands after
/// `settled_ms`: wakes at 25 ms, then doubling to the 1 s ceiling.
fn simulated_poll_detect_ms(settled_ms: f64) -> f64 {
    let mut at = 0.0;
    let mut interval = POLL_FLOOR_MS;
    loop {
        at += interval;
        if at >= settled_ms {
            return at;
        }
        interval = (interval * 2.0).min(POLL_CEILING_MS);
    }
}

// ---------------------------------------------------------------------------
// Reports.
// ---------------------------------------------------------------------------

/// Percentile columns the gate reports per phase.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct LatencySummary {
    pub(crate) n: usize,
    pub(crate) min: f64,
    pub(crate) p50: f64,
    pub(crate) p90: f64,
    pub(crate) p95: f64,
    pub(crate) p99: f64,
    pub(crate) max: f64,
    pub(crate) mean: f64,
}

pub(crate) fn summarize(values: impl Iterator<Item = f64>) -> Option<LatencySummary> {
    let mut values: Vec<f64> = values.collect();
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    Some(LatencySummary {
        n: values.len(),
        min: round3(values[0]),
        p50: round3(percentile_sorted(&values, 0.50)),
        p90: round3(percentile_sorted(&values, 0.90)),
        p95: round3(percentile_sorted(&values, 0.95)),
        p99: round3(percentile_sorted(&values, 0.99)),
        max: round3(values[values.len() - 1]),
        mean: round3(values.iter().sum::<f64>() / values.len() as f64),
    })
}

/// Every phase span one case reports, in milliseconds.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PhaseSummaries {
    pub(crate) request_to_accept: Option<LatencySummary>,
    pub(crate) accept_to_shift_admission: Option<LatencySummary>,
    pub(crate) accept_to_applied: Option<LatencySummary>,
    pub(crate) shift_admission_to_first_delta: Option<LatencySummary>,
    pub(crate) shift_admission_to_first_visible: Option<LatencySummary>,
    pub(crate) shift_admission_to_first_activity: Option<LatencySummary>,
    pub(crate) admission_observation_lag: Option<LatencySummary>,
    pub(crate) terminal_observation_lag: Option<LatencySummary>,
    pub(crate) shift_admission_to_run_settled: Option<LatencySummary>,
    pub(crate) root_settled_to_completion: Option<LatencySummary>,
    pub(crate) send_to_completion: Option<LatencySummary>,
    pub(crate) provider: Option<LatencySummary>,
    pub(crate) overhead: Option<LatencySummary>,
    pub(crate) model_span: Option<LatencySummary>,
    pub(crate) poll_detect: Option<LatencySummary>,
}

fn phases_of(samples: &[&Sample]) -> PhaseSummaries {
    let column = |pick: &dyn Fn(&Sample) -> Option<f64>| {
        summarize(samples.iter().filter_map(|sample| pick(sample)))
    };
    PhaseSummaries {
        request_to_accept: column(&|sample| Some(sample.request_to_accept_ms)),
        accept_to_shift_admission: column(&|sample| sample.accept_to_admission_ms),
        accept_to_applied: column(&|sample| sample.accept_to_applied_ms),
        shift_admission_to_first_delta: column(&|sample| sample.admission_to_first_delta_ms),
        shift_admission_to_first_visible: column(&|sample| sample.admission_to_first_visible_ms),
        shift_admission_to_first_activity: column(&|sample| sample.admission_to_first_activity_ms),
        admission_observation_lag: column(&|sample| sample.admission_observation_lag_ms),
        terminal_observation_lag: column(&|sample| sample.terminal_observation_lag_ms),
        shift_admission_to_run_settled: column(&|sample| sample.admission_to_settled_ms),
        root_settled_to_completion: column(&|sample| sample.settled_to_complete_ms),
        send_to_completion: column(&|sample| Some(sample.send_to_completion_ms)),
        provider: column(&|sample| sample.provider_ms),
        overhead: column(&|sample| sample.overhead_ms),
        model_span: column(&|sample| sample.model_span_ms),
        poll_detect: column(&|sample| sample.poll_detect_ms),
    }
}

/// One case's measured totals.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CaseReport {
    pub(crate) name: &'static str,
    /// Lanes are closed loops: a lane sends its next sample when the last
    /// one returns, and every span starts at the send.
    pub(crate) load_model: crate::offered_load::LoadModel,
    pub(crate) topology: Topology,
    pub(crate) provider: LatencyProviderKind,
    pub(crate) follower: FollowerMode,
    pub(crate) busy: bool,
    pub(crate) diagnostics: diagnostics::Receipt,
    pub(crate) samples: usize,
    pub(crate) lanes: usize,
    pub(crate) actual_lanes: usize,
    pub(crate) sqlite_work: SqliteWork,
    pub(crate) baseline: serde_json::Value,
    pub(crate) clock_window: serde_json::Value,
    pub(crate) wall_seconds: f64,
    pub(crate) statuses: BTreeMap<String, usize>,
    /// Samples that did not answer — failed and errored sends stay in the
    /// ledger rather than being dropped.
    pub(crate) non_answered: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) errors: Vec<String>,
    pub(crate) phases_ms: PhaseSummaries,
    /// The chat-loop spans (FIG-6841); rows the case does not exercise are null.
    pub(crate) loop_ms: LoopSummaries,
    pub(crate) population_refusals: Vec<GateRefusal>,
    /// The cold/warm split on every phase.
    pub(crate) cold: Option<PhaseSummaries>,
    pub(crate) warm: Option<PhaseSummaries>,
    /// What brought the case's session owners to their work (FIG-5932):
    /// `work_on_poll` counts passes that found mail only the claim poll
    /// reached. Same-process cases only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) session_wakes: Option<lash_core::perf_witness::wakes::Counts>,
    /// What started this process's send followers' resolutions (FIG-7055):
    /// the first read of each follow, a committed-change hint, a live
    /// replay event or a timer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) follow_resolves: Option<lash_core::perf_witness::wakes::FollowCounts>,
}

impl CaseReport {
    fn assemble(spec: &CaseSpec, samples: &[Sample], errors: Vec<String>, wall: Duration) -> Self {
        let mut statuses: BTreeMap<String, usize> = BTreeMap::new();
        for sample in samples {
            *statuses.entry(sample.status.clone()).or_default() += 1;
        }
        let all: Vec<&Sample> = samples.iter().collect();
        let cold: Vec<&Sample> = all.iter().copied().filter(|sample| sample.cold).collect();
        let warm: Vec<&Sample> = all.iter().copied().filter(|sample| !sample.cold).collect();
        Self {
            name: spec.name,
            load_model: crate::offered_load::LoadModel::ServiceDiagnostic,
            topology: spec.topology,
            provider: spec.provider,
            follower: spec.follower,
            busy: spec.busy,
            diagnostics: diagnostics::Receipt::default(),
            samples: samples.len(),
            lanes: spec.lanes,
            actual_lanes: 0,
            sqlite_work: SqliteWork::default(),
            baseline: serde_json::json!({
                "kind": if spec.provider == LatencyProviderKind::OpenAiCompat { "same-run-engine-model-span" } else { "same-run-provider-duration" },
                "dirty": crate::perf_support::git::git_dirty(),
                "sha": std::env::var("GITHUB_SHA").ok().or_else(crate::perf_support::git::head_commit).unwrap_or_else(|| "unavailable".to_owned()),
            }),
            clock_window: serde_json::json!({
                "first_request": samples.iter().min_by_key(|s| s.clocks.request.monotonic_ns).map(|s| s.clocks.request),
                "last_completion": samples.iter().max_by_key(|s| s.clocks.completion.monotonic_ns).map(|s| s.clocks.completion),
            }),
            wall_seconds: round3(wall.as_secs_f64()),
            statuses,
            non_answered: samples
                .iter()
                .filter(|sample| sample.status != "answered")
                .count(),
            errors,
            phases_ms: phases_of(&all),
            loop_ms: LoopSummaries::of(all.iter().map(|sample| &sample.loop_spans)),
            population_refusals: certificate::population_refusals(samples),
            cold: (!cold.is_empty()).then(|| phases_of(&cold)),
            warm: (!warm.is_empty()).then(|| phases_of(&warm)),
            session_wakes: None,
            follow_resolves: None,
        }
    }
}

#[path = "certificate.rs"]
mod certificate;
pub(crate) use certificate::{GateRefusal, LatencyReport, build_report};

async fn start_worker(
    stores_dir: &Path,
    provider: LatencyProviderKind,
    dhat_out: Option<&Path>,
    dhat_frames: Option<usize>,
) -> Result<tokio::process::Child> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut command = tokio::process::Command::new(std::env::current_exe()?);
    command
        .arg("latency-worker")
        .arg("--store-dir")
        .arg(stores_dir)
        .arg("--provider")
        .arg(provider.cli_name());
    if let Some(path) = dhat_out {
        command.arg("--dhat-out").arg(path);
        if let Some(frames) = dhat_frames {
            command.arg("--dhat-frames").arg(frames.to_string());
        }
    }
    let mut child = command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("launch latency worker")?;
    let stdout = child.stdout.take().context("worker stdout")?;
    let mut lines = BufReader::new(stdout).lines();
    let ready = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
        .await
        .context("latency worker startup timed out")??;
    anyhow::ensure!(
        ready.as_deref() == Some(READY_MARKER),
        "latency worker did not become ready: {ready:?}"
    );
    Ok(child)
}

pub(super) const READY_MARKER: &str = "latency worker ready";
pub(super) fn announce_ready() -> Result<()> {
    use std::io::Write;
    println!("{READY_MARKER}");
    std::io::stdout().flush()?;
    Ok(())
}

/// The child's fixture stamps, beside its witness, for the parent's spans.
const LOOP_STAMPS_FILE: &str = "latency-loop-stamps.json";

pub(super) async fn run_worker(stores_dir: &Path, provider: LatencyProviderKind) -> Result<()> {
    std::fs::create_dir_all(stores_dir)?;
    let stores = lash::sqlite::SqliteStoreSet::open(
        stores_dir.join("lash.db"),
        lash::sqlite::SqliteSynchronous::Full,
    )
    .await?;
    let spec = CaseSpec {
        name: "worker",
        topology: Topology::SameProcess,
        provider,
        follower: FollowerMode::Standard,
        samples: 0,
        lanes: 0,
        busy: false,
        fresh_sessions: false,
    };
    let timing = Arc::new(ProviderTiming::default());
    let core = build_core(
        durable_backend(stores)?,
        &spec,
        Arc::clone(&timing),
        None,
        &None,
        None,
    )?;
    let collector =
        Collector::install().map_err(|_| anyhow::anyhow!("latency witness already installed"))?;
    announce_ready()?;
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)
    })
    .await??;
    // Remote completion can observe the terminal rows while the serving
    // transaction is still publishing its clocks. Let activations finish
    // their acknowledged commits before stopping them or exporting evidence.
    core.drain().await?;
    core.shutdown().await?;
    drop(core);
    std::fs::write(
        stores_dir.join("latency-witness.json"),
        serde_json::to_vec(&collector.snapshot())?,
    )?;
    std::fs::write(
        stores_dir.join(LOOP_STAMPS_FILE),
        serde_json::to_vec(&timing.stamps.export())?,
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "runner_tests.rs"]
mod tests;
