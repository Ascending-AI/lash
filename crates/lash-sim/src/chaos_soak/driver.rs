//! One epoch on one world: the plan's steps against a live deployment, the
//! faults between them, and the ledger of what the host saw admitted.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::{ProcessId, ScopeId, SessionId, TurnId};
use lash_restate_test::{CrashPoint as EngineCut, CrashRule};

use super::host;
use super::plan::{Lane, SessionRef, Step};
use crate::crash_matrix::cases::process;
use crate::crash_matrix::world::{CoreBuild, CrashWorld};

/// How long one step may run in wall time before the epoch calls it hung.
const STEP_WALL_LIMIT: Duration = Duration::from_secs(300);

/// How long a held step waits in wall time for its root to reach the model.
const HELD_WAIT: Duration = Duration::from_secs(20);

/// The ticks a rolling deploy's drain may take before the epoch reports the
/// old generation stuck: five minutes of virtual time.
pub(super) const DRAIN_TICKS: usize = 30;

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

/// Whether the host saw an admission answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// The host saw it accepted: it must take effect exactly once.
    Known,
    /// The host died inside the call: it took effect at most once, and
    /// exactly once if its commit landed.
    Maybe,
}

/// An input the host sent, and whether it saw the acceptance.
#[derive(Clone, Debug)]
pub struct SentInput {
    pub session: SessionId,
    pub root: String,
    pub admission: Admission,
}

/// A held root the host sent and then cancelled or deleted.
#[derive(Clone, Debug)]
pub struct HeldRoot {
    pub session: SessionId,
    pub root: String,
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
    pub held: Vec<HeldRoot>,
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
/// shared scripted model, a Lashlang process engine, and a recovery lease
/// that competes at the rank of the build it runs.
pub(super) fn soak_core(reached: Reached, rank: Arc<AtomicI64>) -> CoreBuild {
    Arc::new(move |backend, owner| {
        let model = process::model_spec()?;
        let processes = LashlangProcesses::over(&backend);
        lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .recovery_lease(lash::RecoveryLeaseConfig {
                generation_rank: rank.load(Ordering::SeqCst),
                timings: soak_lease_timings(),
            })
            .provider(soak_provider(Arc::clone(&reached)))
            .model(model)
            .plugin(Arc::new(processes))
            .build(owner)
            .map_err(|error| format!("build the lash core: {error}"))
    })
}

/// The held roots that reached their model call, by root: a held step
/// waits for its own root, not for any held root, since a cancelled root a
/// restart replays reaches the model again.
pub(super) type Reached = Arc<Mutex<BTreeSet<String>>>;

/// The crash matrix's scripted model, recording which held root reached it:
/// a held root's call never answers, and every other root is answered from
/// its input.
fn soak_provider(reached: Reached) -> lash_core::facade_support::ProviderHandle {
    use crate::crash_matrix::invariants::{answer_text, input_roots};
    lash_core::testing::TestProvider::builder()
        .kind("chaos-soak")
        .complete(move |request: lash_core::llm::types::LlmRequest| {
            let reached = Arc::clone(&reached);
            async move {
                let latest_user = request
                    .messages
                    .iter()
                    .rev()
                    .find(|message| matches!(message.role, lash_core::llm::types::LlmRole::User))
                    .and_then(|message| serde_json::to_string(message).ok())
                    .unwrap_or_default();
                let roots = input_roots(&latest_user);
                let held: Vec<String> = roots
                    .iter()
                    .filter(|root| root.starts_with("held-"))
                    .cloned()
                    .collect();
                if !held.is_empty() {
                    reached
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .extend(held);
                    std::future::pending::<()>().await;
                }
                Ok::<_, lash_core::llm::transport::LlmTransportError>(
                    lash_core::llm::types::LlmResponse {
                        parts: vec![lash_core::llm::types::LlmOutputPart::Text {
                            text: roots.iter().map(|root| answer_text(root)).collect(),
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
        lash_core::plugin::PluginFactory::process_engine_contributions(&self.rlm, ctx)
    }

    fn build(
        &self,
        _ctx: &lash_core::plugin::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError> {
        Ok(Arc::new(NoSessionSurface))
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
    /// The held roots that reached their model call.
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
}

impl Driver {
    pub async fn new(seed: u64) -> Result<Self, String> {
        let reached = Reached::default();
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
        let world = CrashWorld::on_server(
            seed,
            soak_core(Arc::clone(&reached), Arc::clone(&rank)),
            true,
            config,
        )
        .await?;
        let deployment = world
            .double()?
            .server()
            .deployments()
            .into_iter()
            .next()
            .ok_or_else(|| "the world registered no build".to_owned())?;
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
        })
    }

    fn session_id(&self, seed: u64, session: SessionRef) -> SessionId {
        SessionId::from(format!("soak-{seed:016x}-s{session}"))
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
        let fires = self.world.trip().fires();
        if fires <= self.handled {
            return Ok(false);
        }
        self.handled = fires;
        self.counts.crashes += 1;
        self.world.crash_and_restart().await?;
        self.live_since_wall_ms = wall_ms();
        // The restart's own kill fires nothing, but a crash that raced it
        // counts as answered by it.
        self.handled = self.handled.max(self.world.trip().fires());
        Ok(true)
    }

    /// Run `task` as host work, and tell whether a crash fired during it.
    async fn host<F, T>(&mut self, task: F) -> Result<(Option<T>, bool), String>
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let before = self.world.trip().fires();
        let answer = self.world.host_op(task).await;
        let crashed = self.world.trip().fires() > before;
        self.settle_crash().await?;
        Ok((answer, crashed))
    }

    /// One recovery tick, cut short when the deployment dies inside it.
    pub async fn tick(&mut self) -> Result<(), String> {
        self.settle_crash().await?;
        let seen = self.world.trip().fires();
        let ticked = tokio::select! {
            ticked = self.world.tick() => ticked.map(|_| ()),
            () = self.world.trip().fired_beyond(seen) => Ok(()),
        };
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
                "the step ran past {STEP_WALL_LIMIT:?} of wall time: the host or the engine hung; {:?}",
                crate::crash_matrix::invariants::diagnose(&self.world).await
            )),
        }
    }

    async fn run_step(&mut self, seed: u64, step: &Step) -> Result<String, String> {
        self.settle_crash().await?;
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
                // comes back: the create is idempotent, and every later step
                // on the session needs it.
                for attempt in 1..=OPEN_ATTEMPTS {
                    let core = self.world.core()?;
                    let id = id.clone();
                    let parent = parent.clone();
                    let opened = self
                        .host(Box::pin(async move {
                            let builder = core.session(id);
                            let builder = match parent {
                                Some(parent) => builder.parent(parent),
                                None => builder,
                            };
                            builder.create().await.map(|_| ())
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
            Step::Send { session, root } => {
                let id = self.slot(*session)?.id.clone();
                let admission = self.send(&id, vec![root.clone()]).await?;
                Ok(match admission {
                    Some(admission) => {
                        self.ledger.inputs.push(SentInput {
                            session: id,
                            root: root.clone(),
                            admission,
                        });
                        format!("{admission:?}")
                    }
                    None => "refused".to_owned(),
                })
            }
            Step::SendBatch { session, roots } => {
                let id = self.slot(*session)?.id.clone();
                let admission = self.send(&id, roots.clone()).await?;
                Ok(match admission {
                    Some(admission) => {
                        for root in roots {
                            self.ledger.inputs.push(SentInput {
                                session: id.clone(),
                                root: root.clone(),
                                admission,
                            });
                        }
                        format!("{admission:?}")
                    }
                    None => "refused".to_owned(),
                })
            }
            Step::Command { session, key } => {
                let id = self.slot(*session)?.id.clone();
                self.ledger.commands += 1;
                // A command runs on the session's own runtime, which a drive
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
                root,
                child,
            } => {
                let id = self.slot(*session)?.id.clone();
                let admission = self.send_held(&id, root, *child).await?;
                let receipt = self.cancel(&id, root).await?;
                Ok(format!("{admission:?}; cancel: {receipt}"))
            }
            Step::DeleteHeld {
                session,
                root,
                child,
            } => {
                let id = self.slot(*session)?.id.clone();
                let admission = self.send_held(&id, root, *child).await?;
                let deleted = self.delete(*session).await?;
                Ok(format!("{admission:?}; delete: {deleted}"))
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
                self.world.crash_and_restart().await?;
                self.live_since_wall_ms = wall_ms();
                self.handled = self.handled.max(self.world.trip().fires());
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
            Step::LeaseLoss { ticks } => self.lose_lease(*ticks).await,
            Step::Roll => self.roll().await,
        }
    }

    /// Accept `roots` on `session` as one request (one input, or a batch).
    /// `None` when the session refused it; the admission otherwise. A
    /// retryable refusal is retried, as a host retries it.
    async fn send(
        &mut self,
        session: &SessionId,
        roots: Vec<String>,
    ) -> Result<Option<Admission>, String> {
        for _ in 0..20 {
            let core = self.world.core()?;
            let id = session.clone();
            let inputs = roots.clone();
            let sent = self
                .host(async move {
                    let session = core.session(id).durable().await?;
                    if let [root] = inputs.as_slice() {
                        session
                            .send(lash::TurnInput::text(
                                crate::crash_matrix::invariants::input_text(root),
                            ))
                            .id(root.as_str())
                            .await
                            .map(|_| ())
                    } else {
                        session
                            .send_batch(inputs.iter().map(|root| {
                                lash::BatchInput::new(lash::TurnInput::text(
                                    crate::crash_matrix::invariants::input_text(root),
                                ))
                                .id(root.as_str())
                            }))
                            .await
                            .map(|_| ())
                    }
                })
                .await?;
            match sent {
                (Some(Ok(())), false) => return Ok(Some(Admission::Known)),
                // The host saw the acceptance, but a crash raced it: the
                // commit still landed.
                (Some(Ok(())), true) => return Ok(Some(Admission::Known)),
                (None, _) => return Ok(Some(Admission::Maybe)),
                (Some(Err(error)), _) if error.is_retryable() => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                (Some(Err(_)), _) => return Ok(None),
            }
        }
        Err(format!("`{session}` refused a send retryably twenty times"))
    }

    /// Whether the held root `root` reached its model call.
    fn reached(&self, root: &str) -> bool {
        self.reached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(root)
    }

    /// Send the held root `root` on `session` (with a child that lives until
    /// it ends, when `child`) and wait until it reaches its model call.
    async fn send_held(
        &mut self,
        session: &SessionId,
        root: &str,
        child: bool,
    ) -> Result<Admission, String> {
        let root = root.to_owned();
        let admission = self
            .send(session, vec![root.clone()])
            .await?
            .unwrap_or(Admission::Maybe);
        self.ledger.held.push(HeldRoot {
            session: session.clone(),
            root: root.clone(),
            admission,
        });
        let deadline = tokio::time::Instant::now() + HELD_WAIT;
        while !self.reached(&root) {
            if tokio::time::Instant::now() > deadline {
                // The root never reached its model call: a send that never
                // committed, or a drive still waiting for recovery. The
                // cancel or the delete that follows answers for it.
                return Ok(admission);
            }
            let seen = self.world.trip().fires();
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(10)) => {}
                () = self.world.trip().fired_beyond(seen) => {}
            }
            self.settle_crash().await?;
        }
        // The root runs: a child it registers lives until it ends, as a
        // tool call's child process would.
        if child {
            let scope = ScopeId::turn(session.clone(), TurnId::from(root.as_str()));
            let registered = register_child(&self.world, session, &scope).await?;
            self.ledger
                .children
                .push(crate::crash_matrix::invariants::ChildOf {
                    child: registered,
                    parent: scope.clone(),
                });
            self.ledger.child_scopes.push(scope);
        }
        Ok(admission)
    }

    /// Cancel the held root `root` of `session` until the host sees the
    /// cancel answered: a host that died inside it retries after the
    /// restart, since a held root ends only by its cancel.
    async fn cancel(&mut self, session: &SessionId, root: &str) -> Result<String, String> {
        let root = root.to_owned();
        let mut last = String::new();
        for _ in 0..20 {
            let core = self.world.core()?;
            let id = session.clone();
            let target = TurnId::from(root.as_str());
            let cancelled = self
                .host(async move {
                    let session = core.session(id).durable().await?;
                    session.cancel(lash::CancelTarget::Root(target)).await
                })
                .await?;
            match cancelled {
                (Some(Ok(receipt)), _) => {
                    return Ok(match receipt {
                        lash::CancelReceipt::Withdrawn(_) => "withdrawn".to_owned(),
                        lash::CancelReceipt::Requested { .. } => "requested".to_owned(),
                        lash::CancelReceipt::AlreadySettled { .. } => "already settled".to_owned(),
                        lash::CancelReceipt::NotFound => "not found".to_owned(),
                        other => format!("{other:?}"),
                    });
                }
                (Some(Err(error)), _) if !error.is_retryable() => {
                    return Err(format!("the cancel of `{root}` was refused: {error}"));
                }
                (Some(Err(error)), _) => {
                    last = error.to_string();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                (None, _) => last = "the host died inside the cancel".to_owned(),
            }
        }
        Err(format!("the cancel of `{root}` never answered: {last}"))
    }

    /// Delete a session through a handler of the host's own, once. A host
    /// that died inside it does not retry: the handler replays on the next
    /// deployment, and the deletion is owed once its close intent commits.
    async fn delete(&mut self, session: SessionRef) -> Result<String, String> {
        let id = self.slot(session)?.id.clone();
        let mut pinned = 0;
        let mut unanswered = 0;
        let admission = loop {
            let deleted = host::delete_session(&self.world, &id).await;
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
        self.ledger.sessions[session].deleted = Some(admission);
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
            let answer = host::start_process(&self.world, request.clone(), &operation).await;
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
                &lash_restate::RestateProcessAwaitRequest {
                    process_id: process_id.clone(),
                },
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
        let old = self.world.generation();
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
        let core = self.world.core()?;
        core.drain_generation(&old)
            .await
            .map_err(|error| format!("mark `{old}` draining: {error}"))?;
        for tick in 0..=DRAIN_TICKS {
            self.world.quiesce().await;
            self.settle_crash().await?;
            let core = self.world.core()?;
            let status = core
                .generation_drain_status(&old)
                .await
                .map_err(|error| format!("read `{old}`'s drain status: {error}"))?;
            let pinned = pinned_open(&self.world, &old_deployment);
            let holds = status.live_processes + status.parked_processes + status.parked_turns;
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
            if tick == DRAIN_TICKS {
                return Err(format!(
                    "generation `{old}` never drained within {DRAIN_TICKS} ticks: it holds {} live process(es), {} parked process(es), {} parked turn(s), stalled {:?}; open invocations pinned to its build: {pinned:?}",
                    status.live_processes,
                    status.parked_processes,
                    status.parked_turns,
                    status.stalled_obligations
                ));
            }
            self.tick().await?;
        }
        unreachable!("the drain loop returns on its last tick")
    }
}

/// Register an externally-owned child process of `session` that lives until
/// `parent` ends: a row lash never executes, so it pins no build, and only
/// the parent-end cancel of its scope settles it.
async fn register_child(
    world: &CrashWorld,
    session: &SessionId,
    parent: &ScopeId,
) -> Result<ProcessId, String> {
    let mut registration = lash_core::ProcessRegistration::new(
        lash_core::ProcessInput::External {
            metadata: serde_json::json!({ "chaos_soak": "child" }),
        },
        lash_core::RecoveryContract::ExternallyOwned,
        lash_core::ProcessProvenance::session(lash_core::SessionScope::new(session.as_str())),
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

/// The printable text of `invocation`'s input, for a report.
fn invocation_input(double: &lash_restate_test::RestateTestBackend, invocation: &str) -> String {
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
