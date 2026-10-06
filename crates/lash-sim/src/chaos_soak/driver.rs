//! One epoch on one world: the plan's steps against a live deployment, the
//! faults between them, and the ledger of what the host saw admitted.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use lash_core::{ProcessId, ScopeId, SessionId};
use lash_restate_test::{CrashPoint as EngineCut, CrashRule};

use super::host;
use super::plan::{Lane, SessionRef, Step};
use crate::crash_matrix::cases::process;
use crate::crash_matrix::world::{CoreBuild, CrashWorld};

/// How long one step may run in wall time before the epoch calls it hung.
const STEP_WALL_LIMIT: Duration = Duration::from_secs(60);

/// How long a held step waits in wall time for its run to reach the model.
const HELD_WAIT: Duration = Duration::from_secs(20);

/// The ticks a rolling deploy's drain may take before the epoch reports the
/// old generation stuck: five minutes of virtual time, and
/// [`lapsed_claim_ticks`] more for each host death during the drain. The
/// drain takes a tick only once the engine's work has settled
/// (`Driver::settle_work`), so the bound measures time, not the runner.
pub(super) const DRAIN_TICKS: usize = 30;

/// The ticks a claim a dead host held takes to lapse and be retaken: the
/// relay's claim TTL, then the tick that retakes it.
fn lapsed_claim_ticks() -> usize {
    let tick_ms = crate::crash_matrix::TICK.as_millis() as u64;
    let ttl_ms = lash_core::shift::relay::RelayPolicy::default().claim_ttl_ms;
    usize::try_from(ttl_ms.div_ceil(tick_ms) + 1).unwrap_or(usize::MAX)
}

/// The store's refusal of a deletion while a turn cancellation's closure is
/// still pinned (`TurnCancelClosureLifecyclePinned`).
const PINNED_CLOSURE: &str = "pending turn cancellation closure pin(s)";

/// The ticks a deletion refused over a pinned closure waits, one per retry,
/// before the epoch calls the pin stuck.
const DELETE_PINNED_RETRIES: u32 = 10;

/// The creates a host that died inside one runs before it gives up on the
/// session.
const OPEN_ATTEMPTS: u32 = 3;

/// The deletes a host that died inside one sends again before it leaves the
/// delete unanswered.
const DELETE_UNANSWERED_RETRIES: u32 = 2;

pub use crate::invariants::HostOutcome as Admission;
use crate::invariants::{Fact, FaultKind, HostOp, HostRefusalCode};

/// An input the host sent, and whether it saw the acceptance.
#[derive(Clone, Debug)]
pub struct SentInput {
    pub session: SessionId,
    pub run: String,
    pub admission: Admission,
}

/// A held run the host sent and then cancelled or deleted.
#[derive(Clone, Debug)]
pub struct HeldRun {
    pub session: SessionId,
    pub run: String,
    pub admission: Admission,
}

/// A process the host started, with the engine waiter on its terminal.
#[derive(Clone, Debug)]
pub struct StartedProcess {
    pub process: ProcessId,
    pub waiter: String,
}

/// A generation a rolling deploy drained and retired.
#[derive(Clone, Debug)]
pub struct Retired {
    pub generation: lash_core::engine::BuildGeneration,
    pub deployment: lash_restate_test::DeploymentId,
}

#[derive(Clone, Debug)]
pub struct SessionSlot {
    pub id: SessionId,
    pub lane: Lane,
    /// `None` while open; the deletion's admission once the host asked.
    pub deleted: Option<Admission>,
}

/// What the host saw admitted over an epoch: what the end state is checked
/// against.
#[derive(Clone, Debug, Default)]
pub struct Ledger {
    pub sessions: Vec<SessionSlot>,
    pub inputs: Vec<SentInput>,
    pub held: Vec<HeldRun>,
    pub children: Vec<crate::crash_matrix::invariants::ChildOf>,
    pub child_scopes: Vec<ScopeId>,
    pub processes: Vec<StartedProcess>,
    pub retired: Vec<Retired>,
    pub commands: usize,
}

/// What an epoch counted, for its report.
#[derive(Clone, Debug, Default)]
pub struct Counts {
    pub kills: usize,
    pub crashes: usize,
    pub rolls: usize,
    pub lease_losses: usize,
    pub lease_losses_taken: usize,
    pub ticks: usize,
}

/// The deployment every epoch runs: the standard protocol answering from the
/// shared scripted model, a Lashlang process engine and the held child
/// engine, and a recovery lease
/// that competes at the rank of the build it runs. `drain` is how much
/// eligible turn-lane work one run takes.
pub(super) fn soak_core(
    reached: Reached,
    rank: Arc<AtomicI64>,
    drain: lash::DrainMode,
    tool: Arc<super::witness::WitnessTool>,
) -> CoreBuild {
    Arc::new(move |backend, owner| {
        let model = process::llm_profile_spec()?;
        let processes = LashlangProcesses::over(&backend);
        lash::LashCore::standard_builder(backend)
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024).with_drain_mode(drain))
            .recovery_lease(lash::RecoveryLeaseConfig {
                generation_rank: rank.load(Ordering::SeqCst),
                timings: soak_lease_timings(),
            })
            .serve_test_llm_profile(soak_provider(Arc::clone(&reached)), model)
            .tools(tool.clone() as Arc<dyn lash_core::ToolProvider>)
            .plugin(Arc::new(processes))
            .build(owner)
            .map_err(|error| format!("build the lash core: {error}"))
    })
}

/// The held runs that reached their model call, by run: a held step
/// waits for its own run, not for any held run, since a cancelled run a
/// restart replays reaches the model again.
pub(super) type Reached = Arc<tokio::sync::watch::Sender<BTreeSet<String>>>;

/// How long the model takes, in wall time, to answer a `slow-` run.
#[cfg(test)]
const SLOW_ANSWER: Duration = Duration::from_millis(250);

/// The crash matrix's scripted model, recording which held run reached it:
/// a held run's call never answers, and every other run is answered from
/// its input. Under test a `slow-` run is answered after [`SLOW_ANSWER`],
/// as every run is on a runner short of CPU; no plan names one.
fn soak_provider(reached: Reached) -> lash_core::facade_support::ProviderHandle {
    use crate::crash_matrix::invariants::{answer_text, input_runs};
    lash_core::testing::TestProvider::builder()
        .kind("chaos-soak")
        .complete(move |request: lash_core::llm::types::LlmRequest| {
            let reached = Arc::clone(&reached);
            async move {
                let (input_at, runs) = request
                    .messages
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(at, message)| {
                        if !matches!(message.role, lash_core::llm::types::LlmRole::User) {
                            return None;
                        }
                        let runs: Vec<_> = message.blocks.iter().flat_map(|block| match block {
                            lash_core::llm::types::LlmContentBlock::Text { text, .. } => input_runs(text),
                            _ => Vec::new(),
                        }).collect();
                        (!runs.is_empty()).then_some((at, runs))
                    })
                    .unwrap_or_default();
                let held: Vec<String> = runs
                    .iter()
                    .filter(|run| run.starts_with("held-"))
                    .cloned()
                    .collect();
                if !held.is_empty() {
                    reached.send_modify(|seen| seen.extend(held));
                    std::future::pending::<()>().await;
                }
                #[cfg(test)]
                if runs.iter().any(|run| run.starts_with("slow-")) {
                    tokio::time::sleep(SLOW_ANSWER).await;
                }
                let tool_done = request
                    .messages
                    .iter()
                    .skip(input_at + 1)
                    .flat_map(|message| message.blocks.iter())
                    .any(|block| matches!(block,
                        lash_core::llm::types::LlmContentBlock::ToolResult { tool_name: Some(name), .. }
                        if name == "soak_witness"
                    ));
                if runs.iter().any(|run| run == "tool-witness") && !tool_done {
                    return Ok(lash_core::llm::types::LlmResponse {
                        parts: vec![lash_core::llm::types::LlmOutputPart::ToolCall {
                            call_id: "witness-call".to_owned(),
                            tool_name: "soak_witness".to_owned(),
                            input_json: "{}".to_owned(),
                            replay: None,
                        }],
                        ..Default::default()
                    });
                }
                Ok::<_, lash_core::llm::transport::LlmTransportError>(
                    lash_core::llm::types::LlmResponse {
                        parts: vec![lash_core::llm::types::LlmOutputPart::Text {
                            text: runs.iter().map(|run| answer_text(run)).collect(),
                            response_meta: None,
                        }],
                        ..lash_core::llm::types::LlmResponse::default()
                    },
                )
            }
        })
        .build()
        .into_handle()
}

/// The lease timings of a soak deployment. The trust window is measured on
/// the store clock, which the world moves a whole tick at a time, so the TTL
/// is long; the attempts run on wall time and are quick, so a deployment
/// sees a lost lease, and takes a free one, within a fraction of a second.
fn soak_lease_timings() -> lash::RecoveryLeaseTimings {
    lash::RecoveryLeaseTimings {
        ttl: Duration::from_secs(24 * 60 * 60),
        renew_every: Duration::from_millis(100),
        renew_timeout: Duration::from_millis(2_500),
        trust_margin: Duration::from_secs(2),
        follower_retry: Duration::from_millis(100),
        follower_jitter: Duration::ZERO,
        min_tenure: Duration::ZERO,
    }
}

/// A plugin that contributes the Lashlang process engine to a standard-protocol
/// core and nothing to its sessions: the soak's processes run beside its
/// standard sessions on one deployment.
struct LashlangProcesses {
    rlm: lash_protocol_rlm::RlmProtocolPluginFactory,
}

impl LashlangProcesses {
    fn over(backend: &lash_core::Backend) -> Self {
        Self {
            rlm: lash_protocol_rlm::RlmProtocolPluginFactory::new(
                lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                    .channel(lash_protocol_rlm::RlmChannel::Cell)
                    .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                    .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                    .build(),
                std::sync::Arc::new(lash_protocol_rlm::TypescriptDialect),
                backend,
            ),
        }
    }
}

struct NoSessionSurface;

impl lash_core::plugin::SessionPlugin for NoSessionSurface {
    fn id(&self) -> &'static str {
        "chaos-soak-lashlang-processes"
    }

    fn register(
        &self,
        _registrar: &mut lash_core::plugin::PluginRegistrar,
    ) -> Result<(), lash_core::PluginError> {
        Ok(())
    }
}

impl lash_core::plugin::PluginFactory for LashlangProcesses {
    fn id(&self) -> &'static str {
        "chaos-soak-lashlang-processes"
    }

    fn bound_backend(&self) -> Option<&str> {
        lash_core::plugin::PluginFactory::bound_backend(&self.rlm)
    }

    fn process_engine_contributions(
        &self,
        ctx: &lash_core::ProcessEngineContributionContext<'_>,
    ) -> Result<Vec<lash_core::ProcessEngineRegistration>, lash_core::PluginError> {
        let mut engines =
            lash_core::plugin::PluginFactory::process_engine_contributions(&self.rlm, ctx)?;
        // The soak's registered children run until their scope's parent-end
        // cancel ends them.
        engines.push(lash_core::ProcessEngineRegistration::accepting(Arc::new(
            lash_core::testing::HeldProcessEngine,
        )));
        Ok(engines)
    }

    fn build(
        &self,
        _ctx: &lash_core::plugin::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError> {
        Ok(Arc::new(NoSessionSurface))
    }
}

impl lash_core::plugin::PluginDefinition for LashlangProcesses {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("chaos-soak-lashlang-processes")
    }
}

/// One epoch's driver: the world, the plan's ledger, and the levers the
/// steps pull.
pub(super) struct Driver {
    pub world: CrashWorld,
    pub ledger: Ledger,
    pub counts: Counts,
    /// Wall-clock epoch milliseconds the live deployment came up at: the
    /// recovery lease's row names its election in the same clock.
    pub live_since_wall_ms: i64,
    /// The held runs that reached their model call.
    reached: Reached,
    rank: Arc<AtomicI64>,
    /// Crashes already answered with a restart.
    handled: usize,
    /// The deployment the current generation's build registered.
    deployment: lash_restate_test::DeploymentId,
    /// Process starts asked for so far: each names its own operation.
    starts: usize,
    /// Start requests already published, by sleep.
    published: std::collections::BTreeMap<u64, lash_core::ProcessStartRequest>,
    /// Where a multi-part step stands, for the report of one that hung.
    phase: String,
    tool: Arc<super::witness::WitnessTool>,
}

impl Driver {
    /// A world under the default drain: every input is its own run.
    pub async fn new(seed: u64) -> Result<Self, String> {
        Self::with_drain(seed, lash::DrainMode::default()).await
    }

    /// A world whose drain takes every eligible input into one run
    /// (`DrainMode::All`), for a law about an input another input's run
    /// admitted: the default drain never composes one (FIG-4457).
    #[cfg(test)]
    pub async fn composing(seed: u64) -> Result<Self, String> {
        Self::with_drain(seed, lash::DrainMode::All).await
    }

    async fn with_drain(seed: u64, drain: lash::DrainMode) -> Result<Self, String> {
        let reached = Arc::new(tokio::sync::watch::channel(BTreeSet::new()).0);
        let rank = Arc::new(AtomicI64::new(0));
        // Time moves only when the soak moves it, and a retry waits out its
        // backoff in virtual time under Restate's default policy: a replay
        // that meets a dead holder's session lease retries past the lease's
        // lapse, as it does against a real server. The auto-advancing double
        // starts every retry at once, so eight turn attempts would spend
        // themselves in one virtual instant.
        let config = lash_restate_test::ServerConfig {
            time: lash_restate_test::TimeMode::Manual,
            ..lash_restate_test::ServerConfig::default()
        };
        let tool = Arc::new(super::witness::WitnessTool::default());
        let world = CrashWorld::on_server(
            seed,
            soak_core(
                Arc::clone(&reached),
                Arc::clone(&rank),
                drain,
                Arc::clone(&tool),
            ),
            config,
        )
        .await?;
        // Held model calls keep ctx.run open intentionally. Waiting for them
        // to settle spends the whole wall-time budget on every recovery tick.
        // Recovery passes and end-state checks retain their virtual bounds.
        world.set_quiesce_budget(Duration::from_millis(50));
        let deployment = world
            .double()?
            .server()
            .deployments()
            .into_iter()
            .next()
            .ok_or_else(|| "the world registered no build".to_owned())?;
        tool.observe(crate::invariants::ToolObserver::new(
            world.history().clone(),
            Some(world.double()?.server().clone()),
        ));
        world.faults().observe(world.history().clone());
        world.replay_host_handlers();
        world.restart().await?;
        Ok(Self {
            world,
            ledger: Ledger::default(),
            counts: Counts::default(),
            live_since_wall_ms: wall_ms(),
            reached,
            rank,
            handled: 0,
            deployment,
            starts: 0,
            published: std::collections::BTreeMap::new(),
            phase: String::new(),
            tool,
        })
    }

    fn session_id(&self, seed: u64, session: SessionRef) -> SessionId {
        SessionId::fixture(format!("soak-{seed:016x}-s{session}"))
    }

    fn slot(&self, session: SessionRef) -> Result<&SessionSlot, String> {
        self.ledger
            .sessions
            .get(session)
            .ok_or_else(|| format!("the plan names session {session} before opening it"))
    }

    /// Answer every crash that fired since the last one answered: the
    /// deployment died there, so kill what is left of it and bring up a
    /// fresh one. `true` when it restarted.
    pub async fn settle_crash(&mut self) -> Result<bool, String> {
        let mut restarted = false;
        loop {
            let fires = self.world.trip().fires();
            if fires <= self.handled {
                return Ok(restarted);
            }
            self.handled = fires;
            self.counts.crashes += 1;
            self.world.crash_and_restart().await?;
            self.live_since_wall_ms = wall_ms();
            // A replay may crash the new deployment during its restart.
            // Only the crashes that initiated this restart were answered.
            restarted = true;
        }
    }

    /// Keep manual-time retries moving while a host waits for their answer.
    /// The next plan tick cannot run until this call returns. Advance to a
    /// retry's actual due time, preserving backoff and the shared store clock.
    fn advance_retry(&self) -> Result<(), String> {
        let server = self.world.double()?.server();
        if let Some(due) = server
            .timers()
            .iter()
            .filter(|timer| timer.kind == "retry")
            .map(|timer| timer.fire_at_ms)
            .min()
        {
            self.world
                .engine()
                .advance(Duration::from_millis(due.saturating_sub(server.now_ms())));
        }
        Ok(())
    }

    async fn host_answer<F, T>(&self, work: F) -> Result<Option<T>, String>
    where
        F: std::future::Future<Output = Option<T>>,
    {
        let mut work = std::pin::pin!(work);
        loop {
            tokio::select! {
                biased;
                () = self.world.trip().fired_beyond(self.handled) => return Ok(None),
                answer = &mut work => return Ok(answer),
                () = tokio::time::sleep(Duration::from_millis(50)) => self.advance_retry()?,
            }
        }
    }

    /// Run `task` as host work, and tell whether a crash fired during it.
    async fn host<F, T>(&mut self, task: F) -> Result<(Option<T>, bool), String>
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let before = self.handled;
        let answer = self.host_answer(Box::pin(self.world.host_op(task))).await?;
        let crashed = self.world.trip().fires() > before;
        self.settle_crash().await?;
        Ok((answer, crashed))
    }

    /// One recovery tick, cut short when the deployment dies inside it.
    pub async fn tick(&mut self) -> Result<(), String> {
        self.settle_crash().await?;
        let seen = self.handled;
        let ticked = self
            .host_answer(async { Some(self.world.tick().await.map(|_| ())) })
            .await?
            .unwrap_or(Ok(()));
        // A crash that fired between the settle and the tick's look at the
        // deployment took it down: the settle below answers it.
        if ticked.is_err() && self.world.trip().fires() <= seen {
            ticked?;
        }
        self.counts.ticks += 1;
        self.settle_crash().await?;
        Ok(())
    }

    /// Run one step, bounded in wall time. Answers a line for the trace.
    pub async fn step(&mut self, seed: u64, step: &Step) -> Result<String, String> {
        match tokio::time::timeout(STEP_WALL_LIMIT, Box::pin(self.run_step(seed, step))).await {
            Ok(outcome) => outcome,
            Err(_) => Err(format!(
                "the step ran past {STEP_WALL_LIMIT:?} of wall time: the host or the engine hung at `{}`; crashes {}/{} handled; timers {:?}; {:?}; working: {:?}; every open invocation: {:?}",
                self.phase,
                self.handled,
                self.world.trip().fires(),
                self.world.double()?.server().timers(),
                crate::crash_matrix::invariants::diagnose(&self.world).await,
                self.world
                    .double()?
                    .server()
                    .working()
                    .into_iter()
                    .map(|view| view.target)
                    .collect::<Vec<_>>(),
                open_invocations(&self.world)
            )),
        }
    }

    async fn run_step(&mut self, seed: u64, step: &Step) -> Result<String, String> {
        self.phase.clear();
        self.settle_crash().await?;
        let kind = match step {
            Step::Kill => Some(FaultKind::Kill),
            Step::ArmEngineCut { .. } => Some(FaultKind::EngineCut),
            Step::ArmHostCrash { .. } => Some(FaultKind::HostCrash),
            Step::ArmHostRefusal { .. } => Some(FaultKind::Refusal),
            Step::LeaseLoss { .. } => Some(FaultKind::LeaseLoss),
            Step::Roll => Some(FaultKind::Roll),
            _ => None,
        };
        if let Some(kind) = kind {
            self.world.history().record(Fact::Fault {
                kind,
                detail: format!("step {step:?}"),
            });
        }
        match step {
            Step::Open {
                session,
                lane,
                parent,
            } => {
                let id = self.session_id(seed, *session);
                let parent = match parent {
                    Some(parent) => Some(self.slot(*parent)?.id.clone()),
                    None => None,
                };
                self.ledger.sessions.push(SessionSlot {
                    id: id.clone(),
                    lane: *lane,
                    deleted: None,
                });
                // A host that died inside the create creates again when it
                // comes back, and every later step on the session needs it.
                // A create never adopts an existing id (FIG-4112), so a retry
                // that finds the session its lost attempt created treats
                // `SessionAlreadyExists` as present: create-or-use.
                for attempt in 1..=OPEN_ATTEMPTS {
                    let core = self.world.core()?;
                    let id = id.clone();
                    let parent = parent.clone();
                    let opened = self
                        .host(Box::pin(async move {
                            match core
                                .session(id)
                                .create(lash::SessionCreation {
                                    spec: lash::SessionSpec::new(
                                        process::MODEL,
                                        lash::TurnBudget::Unbounded,
                                        lash::MaxToolCalls::new(1024),
                                    ),
                                    parent,
                                })
                                .await
                            {
                                Ok(_) | Err(lash::EmbedError::SessionAlreadyExists { .. }) => {
                                    Ok(())
                                }
                                Err(error) => Err(error),
                            }
                        }))
                        .await?;
                    match opened {
                        (Some(Ok(())), _) if attempt == 1 => return Ok("opened".to_owned()),
                        (Some(Ok(())), _) => {
                            return Ok(format!("opened on attempt {attempt}"));
                        }
                        (Some(Err(error)), _) => return Ok(format!("refused: {error}")),
                        (None, _) => {}
                    }
                }
                Ok(format!("the host died inside all {OPEN_ATTEMPTS} opens"))
            }
            Step::Send { session, run } => {
                let id = self.slot(*session)?.id.clone();
                let admission = self.send(&id, vec![run.clone()]).await?;
                self.ledger.inputs.push(SentInput {
                    session: id,
                    run: run.clone(),
                    admission: admission.clone(),
                });
                Ok(format!("{admission:?}"))
            }
            Step::SendBatch { session, runs } => {
                let id = self.slot(*session)?.id.clone();
                let admission = self.send(&id, runs.clone()).await?;
                for run in runs {
                    self.ledger.inputs.push(SentInput {
                        session: id.clone(),
                        run: run.clone(),
                        admission: admission.clone(),
                    });
                }
                Ok(format!("{admission:?}"))
            }
            Step::Command { session, key } => {
                let id = self.slot(*session)?.id.clone();
                self.ledger.commands += 1;
                // A command runs on the session's own runtime, which a shift
                // the engine is running holds: a contended open is retried,
                // as a host retries it.
                for _ in 0..100 {
                    let core = self.world.core()?;
                    let id = id.clone();
                    let key = key.clone();
                    let submitted = self
                        .host(Box::pin(async move {
                            let session = core.session(id).open().await?;
                            session
                                .admin()
                                .commands()
                                .refresh_tool_catalog("chaos soak", key)
                                .await
                        }))
                        .await?;
                    match submitted {
                        (Some(Ok(_)), _) => return Ok("submitted".to_owned()),
                        (Some(Err(error)), _) if error.is_retryable() => {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                        (Some(Err(error)), _) => return Ok(format!("refused: {error}")),
                        (None, _) => return Ok("the host died inside the submit".to_owned()),
                    }
                }
                Ok("the session stayed contended".to_owned())
            }
            Step::CancelHeld {
                session,
                run,
                child,
            } => {
                let id = self.slot(*session)?.id.clone();
                let admission = self.send_held(&id, run, *child).await?;
                let receipt = self.cancel(&id, run).await?;
                Ok(format!("{admission:?}; cancel: {receipt}"))
            }
            Step::DeleteHeld {
                session,
                run,
                child,
            } => {
                let id = self.slot(*session)?.id.clone();
                let admission = self.send_held(&id, run, *child).await?;
                let deleted = self.delete(*session).await?;
                Ok(format!("{admission:?}; delete: {deleted}"))
            }
            Step::ParkAndRedrive { session, run } => {
                let id = self.slot(*session)?.id.clone();
                let redriven = self.park_and_redrive(&id, run).await?;
                self.ledger.inputs.push(SentInput {
                    session: id,
                    run: run.clone(),
                    admission: redriven.admission.clone(),
                });
                Ok(match redriven.park {
                    Some((parked, redrive)) => format!(
                        "{:?}; parked `{parked}`; redrive: {redrive:?}",
                        redriven.admission
                    ),
                    None => format!("{:?}; nothing parked", redriven.admission),
                })
            }
            Step::Delete { session } => self.delete(*session).await,
            Step::StartProcess { sleep_ms } => self.start_process(*sleep_ms).await,
            Step::Tick { count } => {
                for _ in 0..*count {
                    self.tick().await?;
                }
                Ok(String::new())
            }
            Step::Quiesce => {
                self.world.quiesce().await;
                Ok(String::new())
            }
            Step::Kill => {
                self.counts.kills += 1;
                self.handled = self.world.trip().fires();
                self.world.crash_and_restart().await?;
                self.live_since_wall_ms = wall_ms();
                self.settle_crash().await?;
                Ok(String::new())
            }
            Step::ArmEngineCut { service, index } => {
                let rule = CrashRule::new(EngineCut::BeforeCommand { index: *index })
                    .service(*service)
                    .within_attempts(1);
                let rule = if *service == process::PROCESS_WORKFLOW {
                    rule.handler("run")
                } else {
                    rule
                };
                self.world.crash_on(rule);
                Ok(String::new())
            }
            Step::ArmHostCrash { site } => {
                self.world.faults().crash_once(*site);
                Ok(String::new())
            }
            Step::ArmHostRefusal { site } => {
                self.world.faults().refuse_once(*site);
                Ok(String::new())
            }
            Step::LeaseLoss { ticks } => self.lose_lease(*ticks).await,
            Step::Roll => self.roll().await,
        }
    }

    /// Accept `runs` on `session` as one request (one input, or a batch).
    /// Keyed retry attempts share one final host fact, including refusals.
    async fn send(&mut self, session: &SessionId, runs: Vec<String>) -> Result<Admission, String> {
        let op = if runs.len() == 1 {
            HostOp::Send
        } else {
            HostOp::SendBatch
        };
        for _ in 0..20 {
            let core = self.world.core()?;
            let id = session.clone();
            let inputs = runs.clone();
            let sent = self
                .host(async move {
                    let session = core.session(id).durable().await?;
                    if let [run] = inputs.as_slice() {
                        session
                            .send(lash::TurnInput::text(
                                crate::crash_matrix::invariants::input_text(run),
                            ))
                            .id(lash_core::TurnId::fixture(run.clone()))
                            .await
                            .map(|_| ())
                    } else {
                        session
                            .send_batch(inputs.iter().map(|run| {
                                lash::BatchInput::new(lash::TurnInput::text(
                                    crate::crash_matrix::invariants::input_text(run),
                                ))
                                .id(lash_core::TurnId::fixture(run.clone()))
                            }))
                            .await
                            .map(|_| ())
                    }
                })
                .await?;
            let outcome = match sent {
                (Some(Ok(())), _) => Admission::Known,
                (None, _) => Admission::Maybe,
                (Some(Err(error)), _) if error.is_retryable() => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                (Some(Err(error)), _) => Admission::Refused {
                    code: refusal_code(&error)?,
                },
            };
            self.record_host(op, session, runs, outcome.clone());
            return Ok(outcome);
        }
        self.record_host(op, session, runs, Admission::Maybe);
        Err(format!("`{session}` refused a send retryably twenty times"))
    }

    pub(super) fn record_host(
        &self,
        op: HostOp,
        session: &SessionId,
        runs: Vec<String>,
        outcome: Admission,
    ) {
        self.world.history().record(Fact::HostOp {
            op,
            session: session.to_string(),
            runs,
            outcome,
        });
    }

    pub(super) async fn send_probe(&mut self, session: &SessionId) -> Result<(), String> {
        let run = format!("probe-{}", self.world.seed() % 100_000);
        self.send(session, vec![run]).await.map(|_| ())
    }

    /// One real deferred tool round trip gives the existing tool checkers facts.
    pub(super) async fn witness(&mut self) -> Result<(), String> {
        let id = SessionId::fixture(format!("soak-{:016x}-witness", self.world.seed()));
        self.world
            .core()?
            .session(id.clone())
            .create(lash::SessionCreation::root(lash::SessionSpec::new(
                process::MODEL,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )))
            .await
            .map_err(|error| error.to_string())?;
        self.send(&id, vec!["tool-witness".to_owned()]).await?;
        let key = tokio::time::timeout(HELD_WAIT, async {
            loop {
                if let Some(key) = self.tool.take_key() {
                    break key;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| "the witness tool never registered its completion".to_owned())?;
        let payload = serde_json::json!({"soak_witness": true});
        self.world.history().record(Fact::CompletionResolved {
            key: crate::invariants::completion_key_label(&key),
            session: id.to_string(),
            result_digest: crate::invariants::result_digest(
                &crate::content_oracle::ToolResultContent::from_tool_value(
                    "",
                    "soak_witness",
                    &payload,
                )
                .content,
            ),
        });
        self.world
            .core()?
            .completions()
            .resolve(key, lash_core::Resolution::Ok(payload))
            .await
            .map_err(|error| error.to_string())?;
        let expected = crate::crash_matrix::invariants::Expected {
            inputs: vec![crate::crash_matrix::invariants::AcceptedInput {
                session: id,
                run: lash_core::TurnId::from("tool-witness"),
            }],
            ..Default::default()
        };
        for _ in 0..6 {
            self.world.quiesce().await;
            if crate::crash_matrix::invariants::check(&self.world, &expected)
                .await
                .is_empty()
            {
                return Ok(());
            }
            self.tick().await?;
        }
        Err("the deferred witness did not finish".to_owned())
    }

    /// Whether the held run `run` reached its model call.
    fn reached(&self, run: &str) -> bool {
        self.reached.borrow().contains(run)
    }

    /// Wait for the model to observe an input, without advancing the engine.
    #[cfg(test)]
    pub(super) async fn wait_reached(&self, run: &str) -> bool {
        let mut observed = self.reached.subscribe();
        let deadline = tokio::time::Instant::now() + HELD_WAIT;
        loop {
            if observed.borrow_and_update().contains(run) {
                return true;
            }
            if tokio::time::timeout_at(deadline, observed.changed())
                .await
                .is_err()
            {
                return false;
            }
        }
    }

    /// Send the held run `run` on `session` (with a child that lives until
    /// it ends, when `child`) and wait until it reaches its model call.
    pub(super) async fn send_held(
        &mut self,
        session: &SessionId,
        run: &str,
        child: bool,
    ) -> Result<Admission, String> {
        let run = run.to_owned();
        let admission = self.send(session, vec![run.clone()]).await?;
        if matches!(admission, Admission::Refused { .. }) {
            return Ok(admission);
        }
        self.ledger.held.push(HeldRun {
            session: session.clone(),
            run: run.clone(),
            admission: admission.clone(),
        });
        let deadline = tokio::time::Instant::now() + HELD_WAIT;
        while !self.reached(&run) {
            if tokio::time::Instant::now() > deadline {
                // The run never reached its model call: a send that never
                // committed, or a shift still waiting for recovery. The
                // cancel or the delete that follows answers for it.
                return Ok(admission);
            }
            let seen = self.handled;
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(10)) => {}
                () = self.world.trip().fired_beyond(seen) => {}
            }
            self.settle_crash().await?;
            self.advance_retry()?;
        }
        // The run executes: a child it registers lives until it ends, as a
        // tool call's child process would.
        if child {
            self.register_held_child(session, &run).await?;
        }
        Ok(admission)
    }

    /// Register a child under the run that actually admitted `input`.
    pub(super) async fn register_held_child(
        &mut self,
        session: &SessionId,
        input: &str,
    ) -> Result<ScopeId, String> {
        let store = self.world.backend().session_store_factory();
        if !matches!(
            store
                .lookup_session(session)
                .await
                .map_err(|error| format!("open `{session}` to resolve `{input}`: {error}"))?,
            lash_core::SessionLookup::Live(_)
        ) {
            return Err(format!(
                "`{session}` disappeared before `{input}` registered a child"
            ));
        }
        let owner = store
            .run_of_input(session, &input_id(session, input))
            .await
            .map_err(|error| format!("resolve run of `{input}`: {error}"))?
            .ok_or_else(|| format!("`{input}` reached the model without a durable run"))?;
        let scope = ScopeId::turn(session.clone(), owner);
        let registered = register_child(&self.world, session, &scope).await?;
        self.ledger
            .children
            .push(crate::crash_matrix::invariants::ChildOf {
                child: registered,
                parent: scope.clone(),
            });
        self.ledger.child_scopes.push(scope.clone());
        Ok(scope)
    }

    /// Cancel the held run `run` of `session` until the host sees the
    /// cancel answered: a host that died inside it retries after the
    /// restart, since a held run ends only by its cancel.
    pub(super) async fn cancel(
        &mut self,
        session: &SessionId,
        run: &str,
    ) -> Result<String, String> {
        let run = run.to_owned();
        let store = self.world.backend().session_store_factory();
        if !matches!(
            store
                .lookup_session(session)
                .await
                .map_err(|error| format!("open `{session}` to cancel `{run}`: {error}"))?,
            lash_core::SessionLookup::Live(_)
        ) {
            return Err(format!("`{session}` disappeared before cancelling `{run}`"));
        }
        let target = store
            .run_of_input(session, &input_id(session, &run))
            .await
            .map_err(|error| format!("resolve run to cancel `{run}`: {error}"))?
            .unwrap_or_else(|| lash_core::TurnId::fixture(run.as_str()));
        let mut last = String::new();
        for _ in 0..20 {
            let core = self.world.core()?;
            let id = session.clone();
            let target = target.clone();
            let cancelled = self
                .host(async move {
                    let session = core.session(id).durable().await?;
                    session.cancel(lash::CancelTarget::Run(target)).await
                })
                .await?;
            match cancelled {
                (Some(Ok(receipt)), _) => {
                    self.record_host(HostOp::Cancel, session, vec![run.clone()], Admission::Known);
                    return Ok(match receipt {
                        lash::CancelReceipt::Withdrawn(_) => "withdrawn".to_owned(),
                        lash::CancelReceipt::Requested { .. } => "requested".to_owned(),
                        lash::CancelReceipt::AlreadySettled { .. } => "already settled".to_owned(),
                        lash::CancelReceipt::NotFound => "not found".to_owned(),
                        other => format!("{other:?}"),
                    });
                }
                (Some(Err(error)), _) if !error.is_retryable() => {
                    self.record_host(
                        HostOp::Cancel,
                        session,
                        vec![run.clone()],
                        Admission::Refused {
                            code: refusal_code(&error)?,
                        },
                    );
                    return Err(format!("the cancel of `{run}` was refused: {error}"));
                }
                (Some(Err(error)), _) => {
                    last = error.to_string();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                (None, _) => last = "the host died inside the cancel".to_owned(),
            }
        }
        self.record_host(HostOp::Cancel, session, vec![run.clone()], Admission::Maybe);
        Err(format!("the cancel of `{run}` never answered: {last}"))
    }

    /// Delete a session through a handler of the host's own, once. A host
    /// that died inside it does not retry: the handler replays on the next
    /// deployment, and the deletion is owed once its close intent commits.
    pub(super) async fn delete(&mut self, session: SessionRef) -> Result<String, String> {
        let id = self.slot(session)?.id.clone();
        let mut pinned = 0;
        let mut unanswered = 0;
        let admission = loop {
            let deleted = self
                .host_answer(host::delete_session(&self.world, &id))
                .await?;
            self.settle_crash().await?;
            match deleted {
                Some(Ok(())) => break Admission::Known,
                // A host that died inside the delete deletes again when it
                // comes back, as a host holding a session it meant to delete
                // does; the last unanswered attempt leaves the delete Maybe.
                None if unanswered < DELETE_UNANSWERED_RETRIES => unanswered += 1,
                None => break Admission::Maybe,
                // The store's typed refusal while a turn cancellation's
                // closure is still pinned: the session's lane drains it, and
                // the host deletes again, as the refusal tells it to.
                Some(Err(error))
                    if error.contains(PINNED_CLOSURE) && pinned < DELETE_PINNED_RETRIES =>
                {
                    pinned += 1;
                    self.tick().await?;
                }
                Some(Err(error)) => {
                    return Err(format!(
                        "the deletion of `{id}` failed after {pinned} pinned-closure refusal(s): {error}"
                    ));
                }
            }
        };
        self.record_host(HostOp::Delete, &id, Vec::new(), admission.clone());
        self.ledger.sessions[session].deleted = Some(admission.clone());
        let mut outcome = format!("{admission:?}");
        if unanswered > 0 {
            outcome.push_str(&format!(" after {unanswered} unanswered attempt(s)"));
        }
        if pinned > 0 {
            outcome.push_str(&format!(" after {pinned} pinned-closure refusal(s)"));
        }
        Ok(outcome)
    }

    /// Start a process that sleeps `sleep_ms` and finishes, and arm an
    /// engine waiter on its terminal.
    async fn start_process(&mut self, sleep_ms: u64) -> Result<String, String> {
        let request = match self.published.get(&sleep_ms) {
            Some(request) => request.clone(),
            None => {
                let request =
                    process::publish_process(&self.world, &format!("{sleep_ms}ms")).await?;
                self.published.insert(sleep_ms, request.clone());
                request
            }
        };
        // A host whose start died with it retries the same start: the
        // start's idempotency is its operation's, so a retry of a start that
        // did register answers the same process.
        let operation = format!("chaos-soak-start-{}", self.starts);
        self.starts += 1;
        let mut started = None;
        for _ in 0..5 {
            let answer = self
                .host_answer(host::start_process(
                    &self.world,
                    request.clone(),
                    &operation,
                ))
                .await?;
            self.settle_crash().await?;
            match answer {
                Some(Ok(process_id)) => {
                    started = Some(process_id);
                    break;
                }
                None => {}
                Some(Err(error)) => return Err(format!("the process start failed: {error}")),
            }
        }
        let Some(process_id) = started else {
            return Err("the process start died with the host five times running".to_owned());
        };
        let waiter = self
            .world
            .engine()
            .ingress()
            .send_workflow_json(
                process::PROCESS_WORKFLOW,
                process_id.as_str(),
                "await_terminal",
                &lash_restate::Call::new(lash_restate::RestateProcessAwaitRequest {
                    process_id: process_id.clone(),
                }),
            )
            .await
            .map_err(|error| format!("arm the engine waiter: {error}"))?
            .into_string();
        self.ledger.processes.push(StartedProcess {
            process: process_id.clone(),
            waiter,
        });
        Ok(process_id.to_string())
    }

    /// Another holder takes the recovery leader lease — a deployment the
    /// network partitioned, say — for `ticks` ticks, then gives it up.
    async fn lose_lease(&mut self, ticks: u32) -> Result<String, String> {
        self.counts.lease_losses += 1;
        let backend = self.world.backend();
        let name = lash_core::store::LeaseName::new(format!(
            "recovery:{}",
            backend.effect_host().turn_control_binding_id()
        ));
        let rival = lash_core::runtime::recovery_lease::RecoveryLease::new(
            backend.recovery_leader(),
            name,
            i64::MAX,
            lash::RecoveryLeaseTimings {
                ttl: Duration::from_secs(10 * 60),
                ..soak_lease_timings()
            },
            backend.clock(),
            lash_core::operational_metrics::StoreObserver::default(),
        );
        let taken = matches!(
            rival.step().await,
            lash_core::runtime::recovery_lease::Standing::Leader { .. }
        );
        if taken {
            self.counts.lease_losses_taken += 1;
            // The deployment's own attempts run on wall time: let one see
            // the lease gone before the ticks.
            tokio::time::sleep(soak_lease_timings().renew_every * 3).await;
        }
        let mut ticked = Ok(());
        for _ in 0..ticks {
            ticked = self.tick().await;
            if ticked.is_err() {
                break;
            }
        }
        // The rival resigns whatever the ticks did, or it would hold the
        // lease past the step.
        rival.resign().await;
        ticked.map(|()| format!("taken={taken}"))
    }

    /// A rolling deploy: register build N+1, bring the deployment up on it,
    /// mark N draining from it, tick until N holds nothing and no invocation
    /// is left pinned to N's build, and remove that build.
    async fn roll(&mut self) -> Result<String, String> {
        self.counts.rolls += 1;
        let old = self.world.generation()?;
        let old_deployment = self.deployment.clone();
        let index = self.counts.rolls;
        let next = {
            let [.., a, b, c, d] = (index as u64).to_be_bytes();
            lash_core::engine::BuildGeneration::from_digest([0x50, 0xa6, a, b, c, d])
        };
        self.deployment = self
            .world
            .add_generation(next.clone(), format!("build-g{index}"))
            .await?;
        self.rank
            .store(i64::try_from(index).unwrap_or(i64::MAX), Ordering::SeqCst);
        // The old deployment hands the host over to the new one: no crash.
        self.settle_crash().await?;
        self.world.restart().await?;
        self.live_since_wall_ms = wall_ms();
        // No binding here holds a deployment's core across a tick: a
        // deployment a crash replaces mid-drain must go with it, as a dead
        // process does, or its recovery lease outlives it and the deployment
        // that replaced it never leads.
        let marked = {
            let core = self.world.core()?;
            self.host_answer(async { Some(core.drain_generation(&old).await) })
                .await?
        };
        if let Some(result) = marked {
            result.map_err(|error| format!("mark `{old}` draining: {error}"))?;
        } else {
            // A death can interrupt the mark or its immediate handover.
            // Retain the requested mark in either case; recovery owns the
            // delivery. Never keep a dead core while awaiting its endpoint.
            let backend = self.world.backend();
            backend
                .generation_drain()
                .mark_draining(&old, backend.clock().timestamp_ms())
                .await
                .map_err(|error| format!("retain `{old}`'s drain mark: {error}"))?;
        }
        self.settle_crash().await?;
        // Each host death during the drain may leave a claim that only its
        // lapse frees (ADR 0109 §1.8: `claimed_at + claim_ttl + T`), so the
        // drain's bound grows by that much per death it saw.
        let crashes_before = self.counts.crashes;
        let mut tick = 0;
        loop {
            self.phase = format!("drain tick {tick}: settling work");
            self.settle_work().await?;
            self.phase = format!("drain tick {tick}: reading the drain status");
            let status = self
                .world
                .core()?
                .generation_drain_status(&old)
                .await
                .map_err(|error| format!("read `{old}`'s drain status: {error}"))?;
            let pinned = pinned_open(&self.world, &old_deployment);
            let holds = status.live_processes
                + status.parked_processes
                + status.parked_turns
                + status.closing_sessions;
            if holds == 0 && pinned.is_empty() {
                self.world
                    .double()?
                    .server()
                    .remove_deployment(&old_deployment, false)
                    .map_err(|error| {
                        format!("`{old}` reads drained but its build cannot be removed: {error:?}")
                    })?;
                self.ledger.retired.push(Retired {
                    generation: old.clone(),
                    deployment: old_deployment,
                });
                return Ok(format!(
                    "`{old}` drained after {tick} tick(s); now `{next}`"
                ));
            }
            let crashes = self.counts.crashes - crashes_before;
            let bound = DRAIN_TICKS + crashes * lapsed_claim_ticks();
            if tick >= bound {
                return Err(format!(
                    "generation `{old}` never drained within {bound} ticks ({DRAIN_TICKS} and {crashes} host death(s) during the drain): it holds {} live process(es), {} parked process(es), {} parked turn(s), {} closing session(s), stalled {:?}; open invocations pinned to its build: {pinned:?}; closing: {:?}; {:?}; every open invocation: {:?}",
                    status.live_processes,
                    status.parked_processes,
                    status.parked_turns,
                    status.closing_sessions,
                    status.stalled_obligations,
                    self.closing_sessions().await,
                    super::checks::diagnose_recovery(&self.world, self.live_since_wall_ms).await,
                    open_invocations(&self.world)
                ));
            }
            self.phase = format!("drain tick {tick}: ticking");
            self.tick().await?;
            tick += 1;
        }
    }

    /// Wait until the engine has run everything it can run without time
    /// moving, and answer every crash that work fired.
    ///
    /// [`CrashWorld::quiesce`] gives up after a wall-time budget, because a
    /// held run's model call never answers. A caller that counts ticks
    /// against a bound must not take that budget for the end of the work:
    /// a shift on a starved runner is still calling its runs when it runs
    /// out, and each tick taken then is charged to work that needs no time
    /// to pass. So this waits past the budget for as long as an attempt other
    /// than a held run's execution is working. An attempt that never stops working
    /// runs the step into [`STEP_WALL_LIMIT`].
    async fn settle_work(&mut self) -> Result<(), String> {
        loop {
            self.world.quiesce().await;
            if self.settle_crash().await? {
                continue;
            }
            if !self.works()? {
                return Ok(());
            }
        }
    }

    /// Whether an invocation that ends by itself is working right now: any
    /// the engine is running but the execution of a held run, which stays in its
    /// model call until the run is cancelled or its session deleted.
    fn works(&self) -> Result<bool, String> {
        let server = self.world.double()?.server();
        let held: Vec<String> = self
            .ledger
            .held
            .iter()
            .flat_map(|held| {
                server
                    .turn_invocations(&held.session, &lash_core::TurnId::fixture(&held.run))
                    .into_iter()
                    .map(|view| view.id)
            })
            .collect();
        Ok(server.working().iter().any(|view| !held.contains(&view.id)))
    }

    /// Every session the host asked to delete that is still closing: its
    /// delete obligation's standing and the cleanup it waits on.
    async fn closing_sessions(&self) -> Vec<String> {
        let backend = self.world.backend();
        let deletes = backend.session_delete_ledger();
        let obligations =
            backend.obligation_ledger(lash_core::store::ObligationKind::SessionDelete);
        let intents = backend.obligation_ledger(lash_core::store::ObligationKind::ControlIntent);
        let mut lines = Vec::new();
        if let Ok(all) = backend
            .session_store_factory()
            .list_control_intents(None, NonZeroUsize::MIN.saturating_add(255))
            .await
        {
            for intent in all {
                let deleted = self
                    .ledger
                    .sessions
                    .iter()
                    .any(|slot| slot.id == intent.session_id && slot.deleted.is_some());
                if !deleted {
                    continue;
                }
                let standing = match intent.obligation_id() {
                    Some(id) => format!("{:?}", intents.standing(id).await),
                    None => "no obligation".to_owned(),
                };
                lines.push(format!(
                    "intent {} of `{}` {:?} obligation {standing}",
                    intent.id, intent.session_id, intent.state
                ));
            }
        }
        for slot in &self.ledger.sessions {
            if slot.deleted.is_none() {
                continue;
            }
            let line = match deletes.delete_obligation(&slot.id).await {
                Ok(None) => continue,
                Ok(Some(obligation)) => {
                    let standing = obligations.standing(&obligation.id).await;
                    let cleanup = deletes.undelivered_cleanup(&slot.id).await;
                    format!(
                        "`{}` delete {} {standing:?}, cleanup {cleanup:?}",
                        slot.id, obligation.id
                    )
                }
                Err(error) => format!("`{}` delete obligation unreadable: {error}", slot.id),
            };
            lines.push(line);
        }
        lines
    }
}

/// Register a held child process of `session` that lives until `parent` ends:
/// it runs until it is cancelled, so only the parent-end cancel of its scope
/// settles it.
pub(super) async fn register_child(
    world: &CrashWorld,
    session: &SessionId,
    parent: &ScopeId,
) -> Result<ProcessId, String> {
    // The environment the held child captures.
    lash_core::testing::process_execution_env_fixture(world.backend().process_env_store().as_ref())
        .await;
    let mut registration = lash_core::testing::held_engine_registration(
        serde_json::json!({ "chaos_soak": "child" }),
        lash_core::ProcessProvenance::session(lash_core::SessionScope::new(session.clone())),
        lash_core::Lifetime::Detached,
    );
    registration.ancestry = lash_core::Ancestry::from_scopes([parent.clone()]);
    registration.lifetime = lash_core::LifetimeDecision::Until {
        scope: parent.clone(),
        grant: lash_core::ScopeGrant::Ancestor,
    };
    world
        .backend()
        .process_registry()
        .register_process(registration)
        .await
        .map(|registered| registered.id)
        .map_err(|error| format!("register a child of `{parent}`: {error}"))
}

fn refusal_code(error: &lash::EmbedError) -> Result<HostRefusalCode, String> {
    let code = match error {
        lash::EmbedError::Runtime(error)
        | lash::EmbedError::Plugin(lash_core::PluginError::Runtime(error)) => error.code.clone(),
        lash::EmbedError::Store(error) => error.runtime_code(),
        lash::EmbedError::UnknownSession { .. } => return Ok(HostRefusalCode::UnknownSession),
        lash::EmbedError::SessionCreationUnrecorded { .. } => {
            lash_core::RuntimeErrorCode::SessionCreationUnrecorded
        }
        _ => return Err(format!("unclassified host refusal: {error:?}")),
    };
    Ok(HostRefusalCode::Runtime(code))
}

fn input_id(session: &SessionId, key: &str) -> lash_core::InputId {
    lash_core::PendingTurnInputDraft::keyed_input_id(session, key)
}

fn wall_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |now| i64::try_from(now.as_millis()).unwrap_or(i64::MAX))
}

/// Every invocation pinned to `deployment` that has not completed.
pub(super) fn pinned_open(
    world: &CrashWorld,
    deployment: &lash_restate_test::DeploymentId,
) -> Vec<String> {
    let Ok(double) = world.double() else {
        return vec!["no server double to read pins from".to_owned()];
    };
    double
        .server()
        .invocations()
        .into_iter()
        .filter(|view| {
            view.pinned_deployment_id == deployment.as_str() && view.status != "completed"
        })
        .map(|view| {
            let line = match &view.last_failure {
                Some((code, failure)) => format!(
                    "{} {} after {} attempt(s), last failure {code}: {failure}",
                    view.target, view.status, view.attempts
                ),
                None => format!("{} {}", view.target, view.status),
            };
            // A durable wait's key says what it waits for: its input names it.
            if view.target.starts_with("LashDurableWaitWorkflow/") {
                format!("{line}; waits for {}", invocation_input(double, &view.id))
            } else {
                line
            }
        })
        .collect()
}

/// Every invocation that has not completed, on any build, with where its
/// journal stands: what a drain that never ends is waiting behind.
fn open_invocations(world: &CrashWorld) -> Vec<String> {
    let Ok(double) = world.double() else {
        return Vec::new();
    };
    let server = double.server();
    server
        .invocations()
        .into_iter()
        .filter(|view| view.status != "completed")
        .map(|view| {
            let journal = server.journal(&view.id).unwrap_or_default();
            let tail: Vec<String> = journal
                .iter()
                .skip(journal.len().saturating_sub(4))
                .map(|entry| match &entry.name {
                    Some(name) => format!("{:?} `{name}`", entry.ty),
                    None => format!("{:?}", entry.ty),
                })
                .collect();
            format!(
                "{} {} on {}: attempt {}, {} suspension(s), blocked on the server {:?}, last failure {:?}, journal of {} ending {tail:?}",
                view.target,
                view.status,
                view.pinned_deployment_id,
                view.attempts,
                view.suspensions,
                view.blocked_on_server,
                view.last_failure,
                view.journal_len,
            )
        })
        .collect()
}

/// The printable text of `invocation`'s input, for a report.
fn invocation_input(
    double: &lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
    invocation: &str,
) -> String {
    double
        .server()
        .journal(invocation)
        .and_then(|journal| journal.into_iter().next())
        .map(|entry| {
            String::from_utf8_lossy(&entry.payload)
                .chars()
                .filter(|c| !c.is_control())
                .take(400)
                .collect()
        })
        .unwrap_or_default()
}

mod park;
pub use park::Redriven;

#[cfg(test)]
mod tests;
