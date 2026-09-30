//! The latency case runner: topologies, lane loops and the store poller.
//!
//! One *sample* is a send→outcome round trip on a live Restate server. The
//! host measures the wall spans it can see; a per-sample store poller reads
//! durable evidence so the admission, application and settlement instants do
//! not ride the follower's own wake schedule. Every read is keyed — the
//! input's open-set row, the input's root binding, the root's terminal —
//! and all of them run on the lane's one observer connection, so a poll
//! never rescans session history or opens a connection per tick. The tick
//! backs off from a 2 ms floor toward a 50 ms ceiling while nothing changes
//! and restarts at the floor the moment a mark lands, so clustered marks
//! keep tick-fine precision while a quiet wait stays cheap:
//!
//! * `admission` — the input's pending row first reports `Admitted` (a root took
//!   it), or leaves the open set.
//! * `applied` — the input's durable binding names the root that took it.
//! * `settled` — the root's terminal evidence is readable.
//!
//! The follower tail (`settled→complete`) is what live replay, the settled
//! mailbox, the drive-attach wake and the 25 ms→1 s poll each contribute
//! to; the `poll`/`grace` cases isolate those contributions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;

use super::provider::{
    HoldRegistry, LaneHold, LatencyProviderKind, ProviderTiming, latency_provider,
};
use super::restate::{LocalDeployment, LocalRestate, LocalRestateServer};
use super::work_engine::{AwaitDriveMode, LatencySessionWork};
use crate::perf_support::memory::process_memory_sample;
use crate::perf_support::metrics::percentile_sorted;
use crate::perf_support::scheduler::process_cpu_ms;
use crate::perf_support::time::round3;
use crate::runtime_perf::openai_compat::OpenAiCompatBenchServer;
use crate::runtime_perf::providers::BenchmarkStreamProfile;

/// The store poller's interval bounds. A fresh send's admission is imminent, so
/// the interval starts at the floor and a tick that lands a mark or first
/// sees the open row restarts it there; a tick that observes nothing
/// doubles it up to the ceiling, keeping an idle wait's reads sparse enough
/// to leave checkpoint windows under concurrent lanes. These phase markers
/// carry up to one interval of observation error; the send-to-completion
/// measurement uses the direct outcome clock.
const STORE_POLL_FLOOR: Duration = Duration::from_millis(2);
const STORE_POLL_CEILING: Duration = Duration::from_millis(50);
/// A sample's store poller gives up here; its marks stay `None`.
const STORE_POLL_TIMEOUT: Duration = Duration::from_secs(120);
/// The follower poll schedule the `poll_detect` phase simulates.
const POLL_FLOOR_MS: f64 = 25.0;
const POLL_CEILING_MS: f64 = 1_000.0;
/// How long a cross-worker child has to serve its endpoint.
const WORKER_READY: Duration = Duration::from_secs(120);

/// Where a case's turns execute.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Topology {
    /// The endpoint serves in this process: the same `RestateEngine` submits
    /// and executes, and live replay plus the settled-root mailbox are
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
    pub(crate) await_drive: AwaitDriveMode,
    pub(crate) samples: usize,
    pub(crate) lanes: usize,
    /// Queue each measured input behind a held sibling root.
    pub(crate) busy: bool,
}

/// Shared run environment: the server, its addresses and the store root.
pub(crate) struct LatencyEnv {
    pub(crate) restate: LocalRestate,
    /// The seed the cross-worker child hashes into the same authority id.
    pub(crate) authority_seed: String,
    _server: Option<LocalRestateServer>,
    store_dir: PathBuf,
}

impl LatencyEnv {
    /// The server this run measures: `RESTATE_*` env when a launcher handed
    /// one over, a spawned private server otherwise.
    pub(crate) async fn open(store_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(store_dir)
            .with_context(|| format!("create {}", store_dir.display()))?;
        let authority_seed = std::env::var("LASH_LATENCY_AUTHORITY_SEED")
            .ok()
            .filter(|seed| !seed.is_empty())
            .unwrap_or_else(|| format!("lash-perf-latency-{}", std::process::id()));
        let (restate, server) = LocalRestate::discover(&authority_seed).await?;
        Ok(Self {
            restate,
            authority_seed,
            _server: server,
            store_dir: store_dir.to_path_buf(),
        })
    }

    /// What the report records about the engine under measurement.
    pub(crate) fn describe(&self) -> serde_json::Value {
        serde_json::json!({
            "engine": "restate-server",
            "server_source": self.restate.source,
            "ingress_url": self.restate.ingress_url,
            "admin_url": self.restate.admin_url,
            "store": "sqlite-file",
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
    /// Process-wide CPU (every thread, `utime + stime`) and RSS read as the
    /// sample completes. Both are cumulative: consecutive samples' deltas
    /// give per-turn growth on a 1-lane run, not a turn's own cost.
    pub(crate) process_cpu_ms_at_end: Option<f64>,
    pub(crate) process_rss_kb_at_end: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

/// The durable markers the per-sample store poller collects.
#[derive(Default)]
struct PollMarks {
    admission_ms: Option<f64>,
    applied_ms: Option<f64>,
    settled_ms: Option<f64>,
    timed_out: bool,
}

/// The sink `outcome_into` taps: first activity, first visible delta and
/// the host-observed model span, all elapsed ms from the request.
struct TimingSink {
    t_request: Instant,
    state: std::sync::Mutex<TimingSinkState>,
}

#[derive(Default)]
struct TimingSinkState {
    first_activity_ms: Option<f64>,
    first_delta_ms: Option<f64>,
    model_started_ms: Option<f64>,
    model_span_ms: f64,
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
        let mut state = self.state.lock_recover();
        state.first_activity_ms.get_or_insert(now);
        match activity.event {
            lash_core::TurnEvent::AssistantProseDelta { .. }
            | lash_core::TurnEvent::ReasoningDelta { .. }
            | lash_core::TurnEvent::StreamBlockStarted { .. }
            | lash_core::TurnEvent::ToolCallStarted { .. }
            | lash_core::TurnEvent::FinalValue { .. } => {
                state.first_delta_ms.get_or_insert(now);
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

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

/// The session handle a lane sends on: a live open session in-process, a
/// durable session over the shared store cross-worker.
enum LaneSession {
    Live(lash::LashSession),
    Durable(lash::DurableSession),
}

impl LaneSession {
    fn send(&self, input: lash::TurnInput) -> lash::SendBuilder {
        match self {
            Self::Live(session) => session.send(input),
            Self::Durable(session) => session.send(input),
        }
    }

    /// Lane teardown: a live session is parked out of the drives so its last
    /// drive leg stops instead of being orphaned when the endpoint drops.
    /// A durable session runs no runtime — its last drive ends on its own
    /// admission check.
    async fn close(self) -> Result<()> {
        match self {
            Self::Live(session) => session.close().await.map_err(anyhow::Error::from),
            Self::Durable(_) => Ok(()),
        }
    }
}

/// The topology a case's lanes run over, holding the deployment or the
/// worker child alive for the case's duration.
///
/// `observer` is a second core over its own store set on the same directory:
/// the per-sample durable pollers read through its connections (WAL readers
/// beside the writer) so marker reads never serialize on the
/// connections the drive itself writes through.
struct CaseTopology {
    core: lash::LashCore,
    observer: lash::LashCore,
    _deployment: Option<LocalDeployment>,
    _worker: Option<LatencyWorkerProcess>,
}

/// Run one case to completion and collect every sample.
pub(crate) async fn run_case(
    spec: &CaseSpec,
    env: &LatencyEnv,
) -> Result<(CaseReport, Vec<Sample>)> {
    let timing = Arc::new(ProviderTiming::default());
    let holds = spec.busy.then(|| Arc::new(HoldRegistry::default()));
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
            let stores = lash::sqlite::SqliteStoreSet::open(&stores_dir)
                .await
                .map_err(|error| anyhow::anyhow!("open case store set: {error}"))?;
            let engine = env.restate.engine(Arc::new(stores));
            let backend = lash::Backend::new(engine.clone());
            let core = build_core(
                backend,
                spec,
                Arc::clone(&timing),
                holds.clone(),
                &compat_server,
            )?;
            let worker = lash::durability::DurableProcessWorker::new(
                core.durable_process_worker_config()
                    .context("latency process worker config")?,
            )
            .map_err(|error| anyhow::anyhow!("build the latency process worker: {error}"))?;
            let deployment = env
                .restate
                .serve(&engine, engine.endpoint_builder(worker).build())
                .await?;
            let observer = build_observer(&env.restate, &stores_dir).await?;
            CaseTopology {
                core,
                observer,
                _deployment: Some(deployment),
                _worker: None,
            }
        }
        Topology::CrossWorker => {
            let worker_dir = case_dir.join("worker");
            let worker = LatencyWorkerProcess::spawn(&worker_dir, env).await?;
            let stores = lash::sqlite::SqliteStoreSet::open(&worker_dir)
                .await
                .map_err(|error| anyhow::anyhow!("open host store set: {error}"))?;
            let engine = env.restate.engine(Arc::new(stores));
            let backend = lash::Backend::new(engine);
            let backend = match spec.await_drive {
                AwaitDriveMode::Real => backend,
                mode => {
                    let wrapped = LatencySessionWork::wrap(&backend, mode);
                    lash_core::testing::runtime_helpers::LayeredBackend::over(backend)
                        .with_session_work(wrapped)
                        .into_backend()
                }
            };
            let core = build_core(backend, spec, Arc::clone(&timing), None, &compat_server)?;
            let observer = build_observer(&env.restate, &worker_dir).await?;
            CaseTopology {
                core,
                observer,
                _deployment: None,
                _worker: Some(worker),
            }
        }
    };

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
    samples.sort_by_key(|sample| (sample.lane, sample.index));
    quiesce_drives(&env.restate, spec).await;
    let wall = started.elapsed();
    let report = CaseReport::assemble(spec, &samples, errors, wall);
    Ok((report, samples))
}

/// Build the host core for a case: the latency provider, the benchmark echo
/// tool plugin and the harness model spec.
fn build_core(
    backend: lash::Backend,
    spec: &CaseSpec,
    timing: Arc<ProviderTiming>,
    holds: Option<Arc<HoldRegistry>>,
    compat_server: &Option<OpenAiCompatBenchServer>,
) -> Result<lash::LashCore> {
    let effect_host = backend.effect_host();
    let provider: lash::provider::ProviderHandle = if let Some(server) = compat_server {
        lash::provider::ProviderHandle::new(
            lash_provider_openai::OpenAiCompatibleProvider::new(
                "latency-gate",
                server.base_url.clone(),
            )
            .with_options(lash::provider::ProviderOptions {
                reliability: lash::provider::ProviderReliability::disabled(),
                ..lash::provider::ProviderOptions::default()
            })
            .into_components(),
        )
    } else {
        latency_provider(spec.provider, timing, holds).into_handle()
    };
    let mut plugins = lash::PluginStack::new();
    plugins.push(Arc::new(lash::plugins::StaticPluginFactory::new(
        "latency_tools",
        lash::plugins::PluginSpec::new().with_tool_provider(Arc::new(
            crate::runtime_perf::providers::BenchmarkEchoTool::new(Arc::clone(&effect_host)),
        )),
    )));
    lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
        .provider(provider)
        .model(latency_model_spec()?)
        .plugins(plugins)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(latency_owner())
        .map_err(anyhow::Error::from)
}

/// A read-only core over a second store set on `stores_dir`. The per-sample
/// durable pollers read through its connections (WAL readers beside the
/// writer) so their cadence never serializes on the drive's own connections;
/// it never serves an endpoint, so it can never drive.
async fn build_observer(restate: &LocalRestate, stores_dir: &Path) -> Result<lash::LashCore> {
    let stores = lash::sqlite::SqliteStoreSet::open(stores_dir)
        .await
        .map_err(|error| anyhow::anyhow!("open observer store set: {error}"))?;
    let backend = lash::Backend::new(restate.engine(Arc::new(stores)));
    let spec = CaseSpec {
        name: "observer",
        topology: Topology::SameProcess,
        provider: LatencyProviderKind::Text,
        await_drive: AwaitDriveMode::Real,
        samples: 0,
        lanes: 0,
        busy: false,
    };
    build_core(
        backend,
        &spec,
        Arc::new(ProviderTiming::default()),
        None,
        &None,
    )
}

fn latency_model_spec() -> Result<lash::ModelSpec> {
    lash::ModelSpec::builder("latency-model")
        .context_window_tokens(200_000)
        .build()
        .map_err(|error| anyhow::anyhow!("latency model spec: {error}"))
}

fn latency_owner() -> lash::persistence::LeaseOwnerIdentity {
    lash::persistence::LeaseOwnerIdentity::opaque(
        "lash-perf-latency",
        format!("{}", std::process::id()),
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

/// Let the case's last drive legs finish before the endpoint drops. A drive
/// still running when its deployment goes away is retried against the dead
/// address for the rest of the run — dead retry traffic that would inflate
/// the next case's numbers. The server's own invocation table names every
/// lash invocation of the case's sessions — a `LashSession` drive keyed by
/// the session, a root's run keyed by a turn workflow key that carries it —
/// so it sees worker-process drives too.
async fn quiesce_drives(restate: &LocalRestate, spec: &CaseSpec) {
    let admin = lash_restate::RestateAdminClient::new(restate.admin_url.clone());
    let query = format!(
        "SELECT target_service_key FROM sys_invocation \
         WHERE status IN ('pending', 'scheduled', 'running', 'backing-off', 'suspended', 'paused') \
         AND target_service_name LIKE '%Lash%' \
         AND target_service_key LIKE '%latency-{}-%'",
        spec.name
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        // An admin read that fails counts as open work: the deadline ends
        // the wait either way.
        let open = match admin.query_json::<serde::de::IgnoredAny>(&query).await {
            Ok(rows) => !rows.is_empty(),
            Err(_) => true,
        };
        if !open || Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// One lane: open its session, run `count` measured sends sequentially.
async fn run_lane(
    spec: &CaseSpec,
    topology: &CaseTopology,
    lane: usize,
    count: usize,
    timing: Arc<ProviderTiming>,
    holds: Option<Arc<HoldRegistry>>,
) -> Result<Vec<Sample>> {
    let session_id = SessionId::from(format!("latency-{}-{lane}", spec.name));
    // Each lane names a fresh session.
    let durable = topology
        .core
        .session(session_id.clone())
        .create(lash::SessionCreation::default())
        .await
        .map_err(anyhow::Error::from)?;
    let session = match spec.topology {
        Topology::SameProcess => LaneSession::Live(
            topology
                .core
                .session(session_id.clone())
                .open()
                .await
                .map_err(anyhow::Error::from)?,
        ),
        Topology::CrossWorker => LaneSession::Durable(durable),
    };
    // The observer uses a non-creating catalog lookup and keyed reads beside
    // the live writer; it does not acquire execution authority.
    let factory = topology.observer.backend().session_store_factory();
    if !matches!(
        factory.lookup_session(&session_id).await?,
        lash_core::SessionLookup::Live(_)
    ) {
        anyhow::bail!("the observer catalog has no store for `{session_id}`");
    }
    let poll_store: Arc<dyn lash::persistence::RuntimeStore> = factory;
    let hold = holds.map(|registry| registry.lane(&session_id));
    let ctx = SampleCtx {
        spec,
        session: &session,
        poll_store: &poll_store,
        session_id: &session_id,
        timing: &timing,
    };
    let mut samples = Vec::with_capacity(count);
    for index in 0..count {
        samples.push(run_sample(&ctx, lane, index, hold.as_deref()).await?);
    }
    // Teardown only: a close refusal leaves the quiesce wait below to finish
    // the drive; it must not cost the lane its measured samples.
    let _ = session.close().await;
    Ok(samples)
}

/// The immutable context every measured send in a lane shares.
struct SampleCtx<'a> {
    spec: &'a CaseSpec,
    session: &'a LaneSession,
    poll_store: &'a Arc<dyn lash::persistence::RuntimeStore>,
    session_id: &'a SessionId,
    timing: &'a Arc<ProviderTiming>,
}

/// One measured send. `busy` lanes first park a sibling input's provider
/// call so the measured input queues behind a live root; the hold releases
/// the moment the measured input is durably accepted.
async fn run_sample(
    ctx: &SampleCtx<'_>,
    lane: usize,
    index: usize,
    hold: Option<&LaneHold>,
) -> Result<Sample> {
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
    let sample = measure_send(ctx, lane, index, || {
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
/// the input is durably accepted — the `busy` case's release point.
async fn measure_send(
    ctx: &SampleCtx<'_>,
    lane: usize,
    index: usize,
    after_accept: impl FnOnce(),
) -> Result<Sample> {
    let t_request = Instant::now();
    let handle = ctx
        .session
        .send(lash::TurnInput::text(format!(
            "latency {} sample {lane}/{index}",
            ctx.spec.name
        )))
        .into_future()
        .await
        .map_err(anyhow::Error::from)?;
    let request_to_accept_ms = elapsed_ms(t_request);
    after_accept();
    let input_id = handle.input_id().clone();
    let poller = tokio::spawn(poll_marks(
        Arc::clone(ctx.poll_store),
        ctx.session_id.clone(),
        input_id.clone(),
        t_request,
    ));
    let sink = TimingSink::new(t_request);
    let (status, outcome_error) = match handle.outcome_into(&sink).await {
        Ok(outcome) => (status_name(&outcome.status), None),
        Err(error) => ("error".to_string(), Some(format!("{error:#}"))),
    };
    let send_to_completion_ms = elapsed_ms(t_request);
    let process_cpu_ms_at_end = process_cpu_ms();
    let process_rss_kb_at_end = process_memory_sample().rss_kb;
    let marks = poller.await.unwrap_or_default();
    let sink_state = sink.state.lock_recover();
    let first_delta_ms = sink_state.first_delta_ms.or(sink_state.first_activity_ms);
    let model_span_ms = (sink_state.model_span_ms > 0.0).then_some(sink_state.model_span_ms);
    drop(sink_state);
    let provider_ms = ctx.timing.take_ms(ctx.session_id);
    let overhead_ms = provider_ms
        .or(model_span_ms)
        .map(|provider| send_to_completion_ms - provider);
    let poll_detect_ms = marks.settled_ms.map(simulated_poll_detect_ms);
    Ok(Sample {
        case: ctx.spec.name,
        lane,
        index,
        cold: index == 0,
        status,
        request_to_accept_ms,
        accept_to_admission_ms: marks
            .admission_ms
            .map(|admission| admission - request_to_accept_ms),
        accept_to_applied_ms: marks
            .applied_ms
            .map(|applied| applied - request_to_accept_ms),
        admission_to_first_delta_ms: marks
            .admission_ms
            .zip(first_delta_ms)
            .map(|(admission, delta)| delta - admission),
        admission_to_settled_ms: marks
            .admission_ms
            .zip(marks.settled_ms)
            .map(|(admission, settled)| settled - admission),
        settled_to_complete_ms: marks
            .settled_ms
            .map(|settled| send_to_completion_ms - settled),
        send_to_completion_ms,
        provider_ms,
        overhead_ms,
        model_span_ms,
        poll_detect_ms,
        poller_timed_out: marks.timed_out,
        process_cpu_ms_at_end,
        process_rss_kb_at_end,
        error: outcome_error,
    })
}

/// The `TurnStatus` name a sample records.
fn status_name(status: &lash::TurnStatus) -> String {
    match status {
        lash::TurnStatus::Answered => "answered".to_string(),
        lash::TurnStatus::Failed => "failed".to_string(),
        lash::TurnStatus::Cancelled => "cancelled".to_string(),
        lash::TurnStatus::Parked(_) => "parked".to_string(),
        lash::TurnStatus::Stalled(_) => "stalled".to_string(),
        other => format!("{other:?}"),
    }
}

/// The durable-evidence poller for one send: admission, application and
/// settlement instants read from the store itself, not the follower's wake
/// schedule. Every read is keyed — the input's open-set row, the input's
/// root binding, the root's terminal — on `store`'s one connection
/// (FIG-3974, FIG-4061): `pending_turn_input` is the admission read the open-set
/// listing used to answer, narrowed to the one tracked row, so a poll never
/// rescans the session's pending inputs or decodes its commit history.
///
/// The interval starts at `STORE_POLL_FLOOR` — a fresh send's admission is
/// imminent — and doubles each tick that observes nothing, up to
/// `STORE_POLL_CEILING`. A tick that lands a mark or first sees the row
/// restarts it at the floor: admission binds the row and names its root in
/// one transaction, so the next mark is imminent whenever one just landed,
/// and only a quiet wait pays the backoff.
async fn poll_marks(
    store: Arc<dyn lash::persistence::RuntimeStore>,
    session_id: SessionId,
    input_id: lash_core::InputId,
    t_request: Instant,
) -> PollMarks {
    let mut marks = PollMarks::default();
    let mut root: Option<lash_core::TurnId> = None;
    let mut row_seen = false;
    let mut interval = STORE_POLL_FLOOR;
    let deadline = Instant::now() + STORE_POLL_TIMEOUT;
    loop {
        let now = elapsed_ms(t_request);
        let mut changed = false;
        if marks.admission_ms.is_none()
            && let Ok(row) = store.pending_turn_input(&session_id, &input_id).await
        {
            match row {
                Some(row) => {
                    changed |= !row_seen;
                    row_seen = true;
                    if matches!(
                        row.status,
                        lash::PendingTurnInputReadStatus::Admitted { .. }
                    ) {
                        marks.admission_ms = Some(now);
                        changed = true;
                    }
                }
                // The row left the open set: the drive consumed it.
                None if row_seen => {
                    marks.admission_ms = Some(now);
                    changed = true;
                }
                None => {}
            }
        }
        // The admission's transaction binds the input to its root, so the keyed
        // `root_of_input` read is the applied mark and the settled mark's
        // key in one.
        if root.is_none()
            && let Ok(Some(bound)) = store.root_of_input(&session_id, &input_id).await
        {
            marks.applied_ms = Some(now);
            root = Some(bound);
            changed = true;
        }
        if let Some(root) = &root {
            if let Ok(Some(_)) = store.root_terminal(&session_id, root).await {
                marks.settled_ms = Some(now);
                break;
            }
        } else if marks.admission_ms.is_some() && marks.applied_ms.is_some() {
            // Nothing left to learn without a root.
            break;
        }
        if Instant::now() >= deadline {
            marks.timed_out = true;
            break;
        }
        interval = if changed {
            STORE_POLL_FLOOR
        } else {
            (interval * 2).min(STORE_POLL_CEILING)
        };
        tokio::time::sleep(interval).await;
    }
    marks
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

/// The `latency-worker` child a cross-worker case drives: serves lash's
/// Restate services over the shared store directory.
struct LatencyWorkerProcess {
    child: std::process::Child,
}

impl LatencyWorkerProcess {
    async fn spawn(store_dir: &Path, env: &LatencyEnv) -> Result<Self> {
        std::fs::create_dir_all(store_dir)
            .with_context(|| format!("create {}", store_dir.display()))?;
        let ready = store_dir.join("worker-ready");
        let _ = std::fs::remove_file(&ready);
        let binary = std::env::current_exe().context("resolve the lash-perf binary")?;
        let endpoint_bind = std::env::var("LASH_LATENCY_WORKER_BIND")
            .ok()
            .and_then(|value| value.parse::<std::net::SocketAddr>().ok())
            .unwrap_or_else(|| std::net::SocketAddr::from(([127, 0, 0, 1], 0)));
        let mut child = std::process::Command::new(binary)
            .arg("latency-worker")
            .arg("--store-dir")
            .arg(store_dir)
            .arg("--ready-file")
            .arg(&ready)
            .arg("--endpoint-bind")
            .arg(endpoint_bind.to_string())
            .env("RESTATE_INGRESS_URL", &env.restate.ingress_url)
            .env("RESTATE_ADMIN_URL", &env.restate.admin_url)
            .env("LASH_LATENCY_AUTHORITY_SEED", &env.authority_seed)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .context("spawn the latency worker")?;
        let deadline = Instant::now() + WORKER_READY;
        loop {
            if ready.is_file() {
                break;
            }
            if let Some(status) = child.try_wait()? {
                bail!("latency worker exited early with {status}");
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                bail!("latency worker did not report ready within {WORKER_READY:?}");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Ok(Self { child })
    }
}

impl Drop for LatencyWorkerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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

fn summarize(values: impl Iterator<Item = f64>) -> Option<LatencySummary> {
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
    pub(crate) accept_to_drive_admission: Option<LatencySummary>,
    pub(crate) accept_to_applied: Option<LatencySummary>,
    pub(crate) drive_admission_to_first_delta: Option<LatencySummary>,
    pub(crate) drive_admission_to_root_settled: Option<LatencySummary>,
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
        accept_to_drive_admission: column(&|sample| sample.accept_to_admission_ms),
        accept_to_applied: column(&|sample| sample.accept_to_applied_ms),
        drive_admission_to_first_delta: column(&|sample| sample.admission_to_first_delta_ms),
        drive_admission_to_root_settled: column(&|sample| sample.admission_to_settled_ms),
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
    pub(crate) topology: Topology,
    pub(crate) provider: LatencyProviderKind,
    pub(crate) await_drive: AwaitDriveMode,
    pub(crate) busy: bool,
    pub(crate) samples: usize,
    pub(crate) lanes: usize,
    pub(crate) wall_seconds: f64,
    pub(crate) statuses: BTreeMap<String, usize>,
    /// Samples that did not answer — failed and errored sends stay in the
    /// ledger rather than being dropped.
    pub(crate) non_answered: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) errors: Vec<String>,
    pub(crate) phases_ms: PhaseSummaries,
    /// The cold/warm split on every phase.
    pub(crate) cold: Option<PhaseSummaries>,
    pub(crate) warm: Option<PhaseSummaries>,
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
            topology: spec.topology,
            provider: spec.provider,
            await_drive: spec.await_drive,
            busy: spec.busy,
            samples: samples.len(),
            lanes: spec.lanes,
            wall_seconds: round3(wall.as_secs_f64()),
            statuses,
            non_answered: samples
                .iter()
                .filter(|sample| sample.status != "answered")
                .count(),
            errors,
            phases_ms: phases_of(&all),
            cold: (!cold.is_empty()).then(|| phases_of(&cold)),
            warm: (!warm.is_empty()).then(|| phases_of(&warm)),
        }
    }
}

/// The verdict the gate exits on.
#[derive(Debug, Serialize)]
pub(crate) struct GateVerdict {
    pub(crate) gate: &'static str,
    pub(crate) budget: serde_json::Value,
    pub(crate) pass: bool,
    pub(crate) violations: Vec<String>,
}

/// The run report: config, environment, every case and the verdict.
#[derive(Debug, Serialize)]
pub(crate) struct LatencyReport {
    pub(crate) kind: &'static str,
    pub(crate) schema: u32,
    pub(crate) crate_version: &'static str,
    pub(crate) environment: serde_json::Value,
    pub(crate) cases: Vec<CaseReport>,
    pub(crate) verdict: GateVerdict,
}

/// Assemble the run report and evaluate the gate.
pub(crate) fn build_report(
    environment: serde_json::Value,
    cases: Vec<CaseReport>,
) -> LatencyReport {
    let mut violations = Vec::new();
    let gate = cases.iter().find(|case| case.name == super::GATE_CASE);
    match gate {
        None => violations.push(format!("gated case `{}` did not run", super::GATE_CASE)),
        Some(case) => {
            if case.samples < super::GATE_MIN_SAMPLES {
                violations.push(format!(
                    "gated case `{}` ran {} samples (< {})",
                    super::GATE_CASE,
                    case.samples,
                    super::GATE_MIN_SAMPLES
                ));
            }
            match &case.phases_ms.overhead {
                None => violations.push(format!(
                    "gated case `{}` produced no overhead samples",
                    super::GATE_CASE
                )),
                Some(overhead) => {
                    if overhead.p50 >= super::GATE_OVERHEAD_P50_MS {
                        violations.push(format!(
                            "overhead p50 {:.1} ms >= {:.1} ms budget",
                            overhead.p50,
                            super::GATE_OVERHEAD_P50_MS
                        ));
                    }
                    if overhead.p99 >= super::GATE_OVERHEAD_P99_MS {
                        violations.push(format!(
                            "overhead p99 {:.1} ms >= {:.1} ms budget",
                            overhead.p99,
                            super::GATE_OVERHEAD_P99_MS
                        ));
                    }
                }
            }
        }
    }
    LatencyReport {
        kind: "lash.send-latency",
        schema: 1,
        crate_version: env!("CARGO_PKG_VERSION"),
        environment,
        cases,
        verdict: GateVerdict {
            gate: "latency-gate",
            budget: serde_json::json!({
                "case": super::GATE_CASE,
                "overhead_p50_ms_lt": super::GATE_OVERHEAD_P50_MS,
                "overhead_p99_ms_lt": super::GATE_OVERHEAD_P99_MS,
                "min_samples": super::GATE_MIN_SAMPLES,
            }),
            pass: violations.is_empty(),
            violations,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use lash_core::SessionCatalogStore as _;

    use super::*;

    /// The store half of `poll_marks`' keyed-read contract (FIG-3974,
    /// FIG-4061): the admission, applied and settled marks come from point
    /// reads — the pending row, the input's root binding, the root's
    /// terminal — on the one store the lane's pollers share.
    /// `list_pending_turn_inputs` and `list_turn_input_applications`, the
    /// open-set scan and the full-history receipt decode the poller issued
    /// every tick before, are armed to panic: reaching either is the
    /// regression.
    struct PollProbeStore {
        inner: Arc<dyn lash::persistence::RuntimeStore>,
        row: lash::PendingTurnInput,
        root: lash_core::TurnId,
        terminal: lash::persistence::RootTerminal,
        pending_input_calls: AtomicUsize,
        applications_calls: AtomicUsize,
        root_of_input_calls: AtomicUsize,
        root_terminal_calls: AtomicUsize,
    }

    impl PollProbeStore {
        fn over(
            inner: Arc<dyn lash::persistence::RuntimeStore>,
            session_id: &SessionId,
            input_id: &lash_core::InputId,
            root: &lash_core::TurnId,
        ) -> Self {
            Self {
                inner,
                row: lash::PendingTurnInput {
                    input_id: input_id.clone(),
                    session_id: session_id.clone(),
                    enqueue_seq: 1,
                    source_key: None,
                    state: lash_core::TurnInputState::open(lash_core::TurnInputIngress::next_turn()),
                    enqueued_at_ms: 0,
                    input: lash::TurnInput::empty(),
                    run_spec: None,
                },
                root: root.clone(),
                terminal: lash::persistence::RootTerminal {
                    session_id: session_id.clone(),
                    root: root.clone(),
                    kind: lash::persistence::RootTerminalKind::Answered,
                    cause: lash::persistence::RootTerminalCause::Committed {
                        commit: lash::persistence::TurnCommitId::new(root.clone(), 0),
                        turn: root.clone(),
                        stop: None,
                    },
                    head_revision: None,
                    at_ms: 1,
                },
                pending_input_calls: AtomicUsize::new(0),
                applications_calls: AtomicUsize::new(0),
                root_of_input_calls: AtomicUsize::new(0),
                root_terminal_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl lash::persistence::RuntimeStoreDecorator for PollProbeStore {
        type Inner = dyn lash::persistence::RuntimeStore;

        fn inner(&self) -> &Self::Inner {
            self.inner.as_ref()
        }

        /// The whole open-set listing the poller used to issue every tick
        /// (FIG-4061): armed to panic, so reaching it at all is the
        /// regression.
        async fn list_pending_turn_inputs(
            &self,
            _session_id: &SessionId,
        ) -> Result<Vec<lash::PendingTurnInputRead>, lash::persistence::StoreError> {
            panic!("the store poller must never list the open set");
        }

        /// The row reports `Open` on the first keyed read and `Admitted`
        /// after, a drive's root admission landing between two ticks.
        async fn pending_turn_input(
            &self,
            _session_id: &SessionId,
            _input_id: &lash_core::InputId,
        ) -> Result<Option<lash::PendingTurnInputRead>, lash::persistence::StoreError> {
            let call = self.pending_input_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Some(if call == 0 {
                lash::PendingTurnInputRead::open(self.row.clone())
            } else {
                lash::PendingTurnInputRead::admitted(
                    self.row.clone(),
                    lash_core::TurnId::from("poll-probe-root"),
                )
            }))
        }

        async fn list_turn_input_applications(
            &self,
            _session_id: &SessionId,
        ) -> Result<Vec<lash_core::TurnInputApplication>, lash::persistence::StoreError> {
            self.applications_calls.fetch_add(1, Ordering::SeqCst);
            panic!("the store poller must never rescan turn-input applications");
        }

        /// The input's root binding: absent until the admission transaction
        /// records it, keyed on `input` only.
        async fn root_of_input(
            &self,
            _session_id: &SessionId,
            _input: &lash_core::InputId,
        ) -> Result<Option<lash_core::TurnId>, lash::persistence::StoreError> {
            let call = self.root_of_input_calls.fetch_add(1, Ordering::SeqCst);
            Ok((call >= 2).then(|| self.root.clone()))
        }

        /// The root's terminal evidence: absent on the read before it lands,
        /// present after.
        async fn root_terminal(
            &self,
            _session_id: &SessionId,
            _root: &lash_core::TurnId,
        ) -> Result<Option<lash::persistence::RootTerminal>, lash::persistence::StoreError>
        {
            let call = self.root_terminal_calls.fetch_add(1, Ordering::SeqCst);
            Ok((call >= 1).then(|| self.terminal.clone()))
        }
    }

    /// The poller's marks land in admission, applied, settled order — each from
    /// a point read on the probe — and the applications scan never runs.
    #[tokio::test]
    async fn the_store_poller_marks_keyed_reads_without_an_applications_scan() {
        let session_id = SessionId::from("latency-probe");
        let input_id = lash_core::InputId::from("latency-probe-input");
        let root = lash_core::TurnId::from("latency-probe-root");
        let stores = lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open the in-memory store set");
        let factory = stores.session_store_factory();
        factory
            .admit_session(&lash_core::SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: session_id.clone(),
                relation: lash_core::SessionRelation::Root,
                config: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded).into(),
                head: lash_core::SessionCreationHead::CommittedByCreator,
            })
            .await
            .expect("create the probe's inner store");
        let inner: Arc<dyn lash::persistence::RuntimeStore> = factory;
        let probe = Arc::new(PollProbeStore::over(inner, &session_id, &input_id, &root));
        let store: Arc<dyn lash::persistence::RuntimeStore> = probe.clone();

        let marks = poll_marks(store, session_id, input_id, Instant::now()).await;

        assert!(!marks.timed_out, "the probe answers every mark");
        let admission = marks.admission_ms.expect("the held row's admission mark");
        let applied = marks.applied_ms.expect("the binding's applied mark");
        let settled = marks.settled_ms.expect("the terminal's settled mark");
        assert!(
            admission < applied && applied < settled,
            "admission {admission}, applied {applied}, settled {settled}"
        );
        assert_eq!(probe.applications_calls.load(Ordering::SeqCst), 0);
        assert_eq!(probe.pending_input_calls.load(Ordering::SeqCst), 2);
        assert_eq!(probe.root_of_input_calls.load(Ordering::SeqCst), 3);
        assert_eq!(probe.root_terminal_calls.load(Ordering::SeqCst), 2);
    }
}
