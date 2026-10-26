//! L4 (FIG-5174): tool rounds on the durable substrate, ADR 0132 §15's
//! second kill criterion.
//!
//! One session's turn owns one tool round. Its owner admits the round (the
//! `model.done` commit), runs it with the production round runner, presents
//! it (`round.present+model.start`) and ends. Member bodies are catalog tools
//! that write to an [`ExternalWorld`] which survives every node, as the
//! outside world would.
//!
//! The matrix cuts the uncut run at every labelled write under fail-before,
//! ack-hidden, zombie, abort and commit-then-abort, recovers on the other
//! node, and checks:
//!
//! - F2 / NR-1 / NR-2: no `Once` body is entered twice, a `Once` whose body
//!   did not commit its outcome folds to `Interrupted`, and a completed
//!   `Once` reached the outside world exactly once;
//! - NR-3 and retry ownership: a `Repeatable` started without an outcome
//!   reruns at its same ordinal, so a crash never advances the attempt; only
//!   its known failure does;
//! - NR-4: no outcome lookup on behalf of re-running code, no committed
//!   ordinal emitted again;
//! - no body runs for an `x_start` that never committed;
//! - the rows fold without a gap, and the presentation is in declared order;
//! - F1: a zombie's writes after its reap are refused;
//! - Pending: a member that parks takes the key of the completion wait its
//!   admission pinned, a rerun is handed the same key, its body is never
//!   entered again once the park committed, and the host's resolution of
//!   that key settles it; a forged key resolves nothing.

// Test code.
#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/sim.rs"]
mod sim;

#[path = "support/matrix.rs"]
mod matrix;

use matrix::MatrixTestExt as _;

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core_execution::ActorContext;
use lash_core_execution::AdmittedScope;
use lash_core_execution::runtime::actor::round::{
    self, ExecutionDraft, MemberBodies, MemberBody, MemberOutcome, PolicyView, RoundDraft,
    RoundEnd, RoundRunner, RunFold,
};
use lash_core_execution::runtime::actor::waits;
use lash_core_execution::runtime::actor::waits::Resolution;
use lash_core_execution::runtime::actor::waits::ResolveAnswer;
use lash_core_execution::runtime::actor::waits::WaitId;
use lash_core_store::effect_opener::EffectOpener;
use lash_core_store::tool_run::Material;
use lash_core_store::tool_run::SettledOutput;
use lash_core_store::tool_run::{
    CompletionSource, KnownFailureReason, MaterialDigest, MaterialLocation, MaterialOwner,
    MaterialRef, MaterialRole,
};
use lash_durable::domain::WaitKind;
use lash_durable::domain::{AdmittedId, OwnerKey, RunFactKind, RunSeq};
use lash_durable::runner::{Activation, Exit, Owned};
use lash_durable::{
    ActorKey, ActorState, CommitLabel, DurableError, DurableStore, FormatSet, MailTx, Release,
};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire, WriteKind,
};
use lash_execution_contracts::backend::Backend;
use lash_execution_contracts::waits::ParkDeadline;
use lash_execution_contracts::waits::WaitDeadline;
use lash_protocol_types::ExecutionLimit;
use lash_protocol_types::ToolCallId;
use lash_protocol_types::sync::MutexExt as _;
use lash_vocabulary::identity::SessionId;
use lash_vocabulary::identity::TurnId;
use lash_vocabulary::tool_contract::ExecutionPolicy;
use lash_vocabulary::tool_contract::LimitCause;
use lash_vocabulary::tool_contract::ToolId;
use tokio_util::sync::CancellationToken;

const FORMATS: &str = "l4";
const SESSION: &str = "l4-session";
const TURN: &str = "l4-turn";
const RUN: RunSeq = RunSeq(1);

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

fn turn() -> TurnId {
    TurnId::try_from(TURN.to_owned()).unwrap()
}

fn actor() -> ActorKey {
    ActorKey::session(SESSION).unwrap()
}

fn owner() -> OwnerKey {
    OwnerKey::Turn(session(), turn())
}

fn call(name: &str) -> ToolCallId {
    ToolCallId::fixture(name)
}

/// The outside world: every body entry, per call and attempt, and every
/// completion key a parking body handed out. It survives every node, so a
/// write a crash cannot undo is visible to the laws.
#[derive(Debug, Default)]
struct ExternalWorld {
    writes: Mutex<BTreeMap<ToolCallId, Vec<u32>>>,
    keys: Mutex<BTreeMap<ToolCallId, Vec<String>>>,
    timers: Mutex<BTreeMap<ToolCallId, Vec<i64>>>,
}

impl ExternalWorld {
    fn hand_out(&self, call: &ToolCallId, key: &str) {
        self.keys
            .lock_recover()
            .entry(call.clone())
            .or_default()
            .push(key.to_owned());
    }

    fn keys(&self, call: &ToolCallId) -> Vec<String> {
        self.keys
            .lock_recover()
            .get(call)
            .cloned()
            .unwrap_or_default()
    }

    fn write(&self, call: &ToolCallId, attempt: u32) {
        self.writes
            .lock_recover()
            .entry(call.clone())
            .or_default()
            .push(attempt);
    }

    fn writes(&self, call: &ToolCallId) -> Vec<u32> {
        self.writes
            .lock_recover()
            .get(call)
            .cloned()
            .unwrap_or_default()
    }
}

/// A journal-local material reference to `payload`.
fn material(payload: &str) -> MaterialRef {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    payload.hash(&mut hasher);
    MaterialRef {
        owner: MaterialOwner::Run {
            opener: EffectOpener::turn(session(), turn()),
        },
        role: MaterialRole::AttemptOutput,
        location: MaterialLocation::JournalLocal,
        digest: MaterialDigest::parse(&format!("{:064x}", hasher.finish())).unwrap(),
    }
}

/// `payload` as a member's journal-local output, owned by the turn.
fn output(payload: String) -> Material {
    Material::journal_local(
        MaterialOwner::Run {
            opener: EffectOpener::turn(session(), turn()),
        },
        MaterialRole::AttemptOutput,
        payload,
    )
}

/// What attempt `attempt` of the write `call` answers.
fn written(call: &ToolCallId, attempt: u32) -> round::CompletedCall {
    let answer = lash_core_execution::ToolCallOutput::success(
        serde_json::json!({ "wrote": format!("{call}#{attempt}") }),
    );
    round::CompletedCall {
        control_origin: lash_sansio::ToolControlOrigin::OwnCall,
        call_id: call.clone(),
        provider_call_id: None,
        tool_name: "write".to_owned(),
        args: serde_json::json!({}),
        model_return: lash_protocol_types::ModelToolReturn::from_output(
            "write".to_owned(),
            &answer,
        ),
        output: answer,
        intent_outcomes: Vec::new(),
        replay: None,
    }
}

/// [`written`] as a member's journal-local output, held typed.
fn completion(call: &ToolCallId, attempt: u32) -> Material {
    round::completed_material(
        &EffectOpener::turn(session(), turn()),
        written(call, attempt),
    )
    .unwrap()
}

/// What a member's tool does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tool {
    /// A `Once` write that completes after `millis`.
    Write { millis: u64 },
    /// A `Repeatable` write whose first attempt reports a known failure.
    Flaky,
    /// A `Repeatable` write that always reports a known failure.
    Failing,
    /// A `Repeatable` write that completes.
    Quick,
    /// A write that parks on its completion wait: it hands the wait's key
    /// to the outside world, which resolves it `after` milliseconds later.
    Defer { repeatable: bool },
    /// Ordinary Pending operation; the lifecycle owns the durable timer.
    Timer { after: i64 },
}

impl Tool {
    fn id(self) -> ToolId {
        ToolId::new(match self {
            Self::Write { .. } => "write",
            Self::Flaky => "flaky",
            Self::Failing => "failing",
            Self::Quick => "quick",
            Self::Timer { .. } => "timer",
            Self::Defer { repeatable: false } => "defer",
            Self::Defer { repeatable: true } => "defer-repeatable",
        })
    }

    fn policy(self) -> ExecutionPolicy {
        match self {
            Self::Write { .. } | Self::Defer { repeatable: false } | Self::Timer { .. } => {
                ExecutionPolicy::Once
            }
            Self::Flaky | Self::Failing | Self::Quick | Self::Defer { repeatable: true } => {
                ExecutionPolicy::repeatable(NonZeroU32::new(3).unwrap(), 100, 1_000)
            }
        }
    }

    /// Whether it may park.
    fn defers(self) -> bool {
        matches!(self, Self::Defer { .. } | Self::Timer { .. })
    }
}

/// How long the outside world takes to resolve a key it was handed.
const RESOLVE_AFTER_MS: u64 = 20;

/// The resolution the outside world answers a parked call with.
fn host_answer(call: &ToolCallId) -> serde_json::Value {
    serde_json::json!({ "answered": call.as_str() })
}

/// One member: its call and its tool.
#[derive(Clone, Debug)]
struct Member {
    call: ToolCallId,
    tool: Tool,
}

/// The catalog the round's bodies come from.
struct Catalog {
    world: Arc<ExternalWorld>,
    tools: BTreeMap<ToolCallId, Tool>,
    clock: Arc<SimClock>,
    backend: Backend,
}

impl MemberBodies for Catalog {
    fn stop_grace(&self) -> std::time::Duration {
        std::time::Duration::from_secs(2)
    }

    fn resolved(
        &self,
        execution: &round::AdmittedExecution,
        parked: &Material<CompletionSource>,
        resolution: Resolution,
    ) -> SettledOutput {
        assert_eq!(
            parked.payload(),
            "parked",
            "the park's material rides its row"
        );
        let answer = match resolution {
            Resolution::Ok(value) => value.to_string(),
            other => format!("{other:?}"),
        };
        assert!(execution.draft().pinned_wait().is_some());
        SettledOutput::Completed(output(answer))
    }

    fn body(&self, execution: &round::AdmittedExecution) -> MemberBody {
        let call = execution.call().clone();
        let tool = self.tools[&call];
        if tool.defers() {
            return self.parking_body(execution);
        }
        let world = Arc::clone(&self.world);
        let clock = Arc::clone(&self.clock);
        let attempt = execution.attempt();
        Box::new(move |_token| {
            Box::pin(async move {
                if let Tool::Write { millis } = tool
                    && millis > 0
                {
                    lash_core_ids::clock::Clock::sleep(&*clock, Duration::from_millis(millis))
                        .await;
                }
                world.write(&call, attempt);
                let fails = match tool {
                    Tool::Write { .. } | Tool::Quick | Tool::Defer { .. } | Tool::Timer { .. } => {
                        false
                    }
                    Tool::Flaky => attempt == 1,
                    Tool::Failing => true,
                };
                MemberOutcome::from(if fails {
                    SettledOutput::Failed(
                        output(format!("{call}#{attempt}"))
                            .failure(KnownFailureReason::Reported, None),
                    )
                } else {
                    SettledOutput::Completed(completion(&call, attempt))
                })
            })
        })
    }
}

impl Catalog {
    /// A body that takes the key of the completion wait its admission
    /// pinned, hands it to the outside world, which resolves it a little
    /// later, and parks on it.
    fn parking_body(&self, execution: &round::AdmittedExecution) -> MemberBody {
        let world = Arc::clone(&self.world);
        let clock = Arc::clone(&self.clock);
        let backend = self.backend.clone();
        let call = execution.call().clone();
        let attempt = execution.attempt();
        let pinned = execution
            .draft()
            .pinned_wait()
            .expect("a member that may park has its completion wait pinned");
        let timer = match self.tools[&call] {
            Tool::Timer { after } => Some(lash_durable::DurableInstant(
                execution.draft().admitted_at().0.saturating_add(after),
            )),
            _ => None,
        };
        let key = waits::host_key(&pinned.wait()).expect("a tool completion wait has a host key");
        Box::new(move |_token| {
            Box::pin(async move {
                world.write(&call, attempt);
                world.hand_out(&call, key.as_str());
                if let Some(deadline) = timer {
                    world
                        .timers
                        .lock_recover()
                        .entry(call.clone())
                        .or_default()
                        .push(deadline.0);
                } else {
                    let answer = host_answer(&call);
                    tokio::spawn(async move {
                        lash_core_ids::clock::Clock::sleep(
                            &*clock,
                            Duration::from_millis(RESOLVE_AFTER_MS),
                        )
                        .await;
                        let _ = waits::resolve_host(&backend, key.as_str(), Resolution::Ok(answer))
                            .await;
                    });
                }
                MemberOutcome {
                    timer,
                    ..MemberOutcome::from(SettledOutput::Waiting(
                        output("parked".to_owned()).parked(pinned.id.to_hex()),
                    ))
                }
            })
        })
    }
}

/// The owner of the round: admit, run, present, end.
struct RoundOwner {
    backend: Backend,
    members: Vec<Member>,
    limit_ms: u64,
    /// The turn's cancel, fired this long after the round starts running.
    cancel_after_ms: Option<u64>,
    cancel_on_timeout: Option<CancellationToken>,
    bodies: Arc<Catalog>,
    tripwire: Arc<Tripwire>,
    presentations: Arc<Mutex<Vec<round::Presentation>>>,
}

impl RoundOwner {
    fn policies(&self) -> PolicyView {
        PolicyView::new(
            self.members
                .iter()
                .map(|member| (member.tool.id(), member.tool.policy())),
        )
    }

    /// One pass: admit when nothing is admitted, run to every outcome,
    /// present, release. An error ends the pass; the next pass reads what
    /// committed.
    async fn pass(&self, cx: &ActorContext) -> Result<(), Pass> {
        let rows = cx
            .durable_reads()
            .map_err(Pass::from)?
            .run_facts(&owner())
            .await
            .map_err(Pass::from)?;
        let bodies: Arc<dyn MemberBodies> = Arc::clone(&self.bodies) as _;
        let runner = if rows.rows.iter().any(|row| row.run == RUN) {
            RoundRunner::resumed(cx, owner(), RUN, self.policies(), bodies)
        } else {
            let now = cx.durable_now().await.map_err(Pass::from)?;
            let now = u64::try_from(now.0).unwrap();
            let drafts = self
                .members
                .iter()
                .map(|member| {
                    let limit = ExecutionLimit::starting_at(
                        now,
                        Duration::from_millis(self.limit_ms),
                        Duration::from_millis(self.limit_ms),
                    );
                    ExecutionDraft::new(
                        lash_durable::DurableInstant(i64::try_from(now).unwrap()),
                        member.call.clone(),
                        member.tool.id(),
                        material(member.call.as_str()),
                        member.tool.policy(),
                        limit,
                        member.tool.defers().then(|| {
                            ParkDeadline::At(WaitDeadline::at_instant(
                                lash_durable::DurableInstant(
                                    i64::try_from(limit.expires_at).unwrap(),
                                ),
                            ))
                        }),
                    )
                })
                .collect();
            let mut tx = cx.begin().await.map_err(Pass::from)?;
            let admitted = round::admit_round(
                &mut tx,
                &waits::wait_scope(cx).map_err(Pass::from)?,
                RoundDraft {
                    owner: owner(),
                    run: RUN,
                    members: drafts,
                },
            )
            .map_err(Pass::from)?;
            cx.commit(tx, CommitLabel::MODEL_DONE)
                .await
                .map_err(Pass::from)?;
            RoundRunner::admitted(cx, &admitted, self.policies(), bodies).unwrap()
        };
        let runner = if let Some(cancel) = &self.cancel_on_timeout {
            runner.cancelled_by(cancel.clone())
        } else {
            match self.cancel_after_ms {
                Some(after) => {
                    let cancel = CancellationToken::new();
                    let fire = cancel.clone();
                    let clock = Arc::clone(cx.clock());
                    let deadline = self.bodies.clock.logical_ms() + after;
                    tokio::spawn(async move {
                        clock.sleep(Duration::from_millis(after)).await;
                        fire.cancel();
                    });
                    self.bodies.clock.wait_for_sleep(deadline).await;
                    runner.cancelled_by(cancel)
                }
                None => runner,
            }
        };
        let end = match runner.run().await.map_err(Pass::from)? {
            RoundEnd::Settled(end) => end,
            // Only parked waits and retry dues are left: release as
            // `waiting` until the earliest; the next claim resumes.
            RoundEnd::Suspended { due } => {
                let mut tx = cx.begin().await.map_err(Pass::from)?;
                tx.give_up(Release::Waiting { next_due: due });
                cx.commit(tx, CommitLabel::SESSION_RELEASE)
                    .await
                    .map_err(Pass::from)?;
                return Ok(());
            }
        };
        if end.round().presented().is_none() {
            let mut tx = cx.begin().await.map_err(Pass::from)?;
            let admitted = end.fold().admitted_round(RUN).unwrap();
            let presentation = round::present(&mut tx, &admitted, end.fold());
            self.presentations.lock_recover().push(presentation);
            cx.commit(tx, CommitLabel::ROUND_PRESENT_MODEL_START)
                .await
                .map_err(Pass::from)?;
        }
        let mut tx = cx.begin().await.map_err(Pass::from)?;
        tx.ack_seen().give_up(Release::Terminal);
        cx.commit(tx, CommitLabel::SESSION_RELEASE)
            .await
            .map_err(Pass::from)?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl Activation for RoundOwner {
    async fn activate(&self, owned: Owned) -> Exit {
        let cx = ActorContext::claimed(
            self.backend.clone(),
            &owned,
            AdmittedScope::turn(session(), turn()),
            CancellationToken::new(),
            Arc::clone(&self.tripwire) as _,
        );
        loop {
            match self.pass(&cx).await {
                Ok(()) => return Exit::Released,
                Err(Pass::Lost) => return Exit::Released,
                Err(Pass::Again) => {
                    owned.wait_for_mail().await;
                }
            }
        }
    }
}

/// How a pass ended short.
enum Pass {
    /// The actor is someone else's, or the node is going away.
    Lost,
    /// Something failed that the next pass reads past.
    Again,
}

fn lost(error: &DurableError) -> bool {
    matches!(
        error,
        DurableError::OwnershipLost(_) | DurableError::NodeLeaseLost { .. }
    )
}

impl From<DurableError> for Pass {
    fn from(error: DurableError) -> Self {
        if lost(&error) {
            Self::Lost
        } else {
            Self::Again
        }
    }
}

impl From<round::RoundError> for Pass {
    fn from(error: round::RoundError) -> Self {
        match error {
            round::RoundError::Durable(error) => error.into(),
            round::RoundError::Stopped => Self::Lost,
            round::RoundError::Fold(_)
            | round::RoundError::Settle(_)
            | round::RoundError::StatePublication(_)
            | round::RoundError::NotAdmitted(_) => Self::Again,
        }
    }
}

impl From<round::AdmissionRefusal> for Pass {
    fn from(_: round::AdmissionRefusal) -> Self {
        Self::Again
    }
}

/// One deployment of one round, fresh for every matrix cell.
struct RoundScenario {
    members: Vec<Member>,
    limit_ms: u64,
    cancel_after_ms: Option<u64>,
    cancel_on_timeout: CancellationToken,
    world: Arc<ExternalWorld>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<(Backend, Arc<SimClock>)>>,
    /// The calls whose final outcomes committed, in commit order.
    outcomes: Arc<Mutex<Vec<ToolCallId>>>,
    /// Every final outcome each run of the scenario ended with.
    settled: Arc<Mutex<Vec<SettledOutput>>>,
    /// What the owner actually handed to its consumer.
    presentations: Arc<Mutex<Vec<round::Presentation>>>,
}

impl RoundScenario {
    fn new(members: Vec<Member>) -> Self {
        Self {
            members,
            limit_ms: 60_000,
            cancel_after_ms: None,
            cancel_on_timeout: CancellationToken::new(),
            world: Arc::default(),
            tripwire: Arc::default(),
            backend: Mutex::default(),
            outcomes: Arc::default(),
            settled: Arc::default(),
            presentations: Arc::default(),
        }
    }

    fn limit_ms(mut self, limit_ms: u64) -> Self {
        self.limit_ms = limit_ms;
        self
    }

    fn cancels_on_timeout(&self) -> bool {
        self.cancel_after_ms.is_some_and(|after| {
            self.members.iter().any(|member| {
                matches!(member.tool, Tool::Timer { after: timer } if u64::try_from(timer).ok() == Some(after))
            })
        })
    }
}

#[async_trait::async_trait]
impl Scenario for RoundScenario {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let stores = sim::memory(Arc::clone(&clock)).await;
        let database = Arc::new(stores.durable_store());
        *self.backend.lock_recover() = Some((
            Backend::for_testing(
                Arc::new(stores),
                lash_durable::OwnerRetentionSettings::default(),
            ),
            clock,
        ));
        database
    }

    fn config(&self) -> SimNodesConfig {
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: vec![FormatSet::new(FORMATS)],
            serves: Vec::new(),
            max_active: 4,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        let (backend, clock) = self
            .backend
            .lock_recover()
            .clone()
            .expect("the database is built first");
        Arc::new(RoundOwner {
            backend: backend.clone(),
            members: self.members.clone(),
            limit_ms: self.limit_ms,
            cancel_after_ms: self.cancel_after_ms,
            cancel_on_timeout: self
                .cancels_on_timeout()
                .then(|| self.cancel_on_timeout.clone()),
            bodies: Arc::new(Catalog {
                world: Arc::clone(&self.world),
                tools: self
                    .members
                    .iter()
                    .map(|member| (member.call.clone(), member.tool))
                    .collect(),
                clock,
                backend,
            }),
            tripwire: Arc::clone(&self.tripwire),
            presentations: Arc::clone(&self.presentations),
        })
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        // At an equal deadline, finish the timeout write before signalling
        // cancellation. Signalling before the store call lets the runner drop
        // an in-flight commit; its next outcome can then race that write and
        // need a revision-conflict retry, changing the census's numbering.
        if self.cancels_on_timeout() {
            let cancel = self.cancel_on_timeout.clone();
            nodes.script().observe_actor_replies(move |label, _, _| {
                if label == CommitLabel::WAIT_TIMEOUT {
                    cancel.cancel();
                }
            });
        }
        let mut seed = MailTx::new();
        seed.create_actor(actor(), FormatSet::new(FORMATS));
        nodes
            .database()
            .commit_mail(seed, CommitLabel::MAIL_SESSION)
            .await
            .map_err(|error| error.to_string())?;
        nodes.start("a");
        nodes.start("b");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        vec![actor()]
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        matches!(
            nodes.database().actor(&actor()).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Terminal
        )
    }

    async fn check(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let rows = match nodes.database().run_facts(&owner()).await {
            Ok(rows) => rows,
            Err(error) => return vec![format!("the run facts do not read: {error}")],
        };
        *self.outcomes.lock_recover() = rows
            .rows
            .iter()
            .filter(|row| row.kind == RunFactKind::XOutcome)
            .filter_map(|row| row.call.clone())
            .collect();
        let waits = match round::PinnedWaits::read(nodes.database().as_ref(), &rows).await {
            Ok(waits) => waits,
            Err(error) => return vec![format!("the pinned waits do not read: {error}")],
        };
        let fold = match round::fold(&rows, &PolicyView::default(), &waits) {
            Ok(fold) => fold,
            Err(refusal) => return vec![format!("the run records do not fold: {refusal}")],
        };
        if let Some(view) = fold.round(RUN) {
            self.settled.lock_recover().extend(
                view.members()
                    .iter()
                    .filter_map(|member| member.outcome().cloned()),
            );
        }
        violations.extend(round_laws(
            &self.members,
            self.cancel_after_ms.is_some(),
            &fold,
            &self.world,
            &self.tripwire,
        ));
        let committed_starts: BTreeSet<AdmittedId> = rows
            .rows
            .iter()
            .filter(|row| row.kind == RunFactKind::XStart)
            .map(|row| AdmittedId {
                owner: row.owner.clone(),
                run: row.run,
                ordinal: row.ordinal,
            })
            .collect();
        for (id, entries) in self.tripwire.counts().bodies {
            if entries > 0 && !committed_starts.contains(&id) {
                violations.push(format!(
                    "a body ran for {id:?}, whose x_start never committed"
                ));
            }
        }
        if let Some(cut) = cut {
            violations.extend(zombie_laws(cut, &nodes.script().trace()));
        }
        let backend = self
            .backend
            .lock_recover()
            .clone()
            .map(|(backend, _)| backend);
        if let Some(backend) = backend {
            for member in self.members.iter().filter(|member| member.tool.defers()) {
                for key in self.world.keys(&member.call) {
                    violations.extend(key_laws(&backend, &member.call, &key).await);
                }
            }
        }
        violations
    }
}

/// K: a key a parking body was handed is its call's tool completion wait's
/// id, and a key that is not an issued wait id is refused `Unknown` and
/// resolves nothing.
async fn key_laws(backend: &Backend, call: &ToolCallId, key: &str) -> Vec<String> {
    let mut violations = Vec::new();
    let named = match WaitId::parse_hex(key) {
        Some(id) => backend.durable().wait(&id).await,
        None => Ok(None),
    };
    match named {
        Ok(Some(row)) if row.purpose.kind() == WaitKind::ToolCompletion => {}
        other => violations.push(format!(
            "K: {call}'s key names no tool completion wait: {other:?}"
        )),
    }
    let flipped = |text: &str| {
        let mut chars: Vec<char> = text.chars().collect();
        let last = chars.last_mut().expect("a non-empty key");
        *last = if *last == '0' { '1' } else { '0' };
        chars.into_iter().collect::<String>()
    };
    let forgeries = [
        // Another id, never issued.
        flipped(key),
        // Not an id at all.
        key[..key.len() - 2].to_owned(),
    ];
    for forged in forgeries {
        match waits::resolve_host(
            backend,
            &forged,
            Resolution::Ok(serde_json::json!("forged")),
        )
        .await
        {
            Ok(ResolveAnswer::Unknown) => {}
            other => violations.push(format!(
                "K: a forgery of {call}'s key resolved as {other:?}"
            )),
        }
    }
    violations
}

/// The laws every run of a round must hold after it ends.
fn round_laws(
    members: &[Member],
    cancelled: bool,
    fold: &RunFold,
    world: &ExternalWorld,
    tripwire: &Tripwire,
) -> Vec<String> {
    let mut violations = Vec::new();
    let Some(view) = fold.round(RUN) else {
        return vec!["the round was never admitted".to_owned()];
    };
    let counts = tripwire.counts();
    if !counts.outcome_lookups.is_empty() || !counts.committed_ordinals.is_empty() {
        violations.push(format!(
            "NR-4: outcome lookups {:?}, committed ordinals emitted again {:?}",
            counts.outcome_lookups, counts.committed_ordinals
        ));
    }
    let declared: Vec<ToolCallId> = members.iter().map(|member| member.call.clone()).collect();
    if view.presented() != Some(declared.to_vec()) {
        violations.push(format!(
            "the presentation {:?} is not the declared order {declared:?}",
            view.presented()
        ));
    }
    for (member, view_member) in members.iter().zip(view.members()) {
        let call = &member.call;
        let ids: Vec<AdmittedId> = view_member
            .starts()
            .iter()
            .map(|ordinal| AdmittedId {
                owner: owner(),
                run: RUN,
                ordinal: *ordinal,
            })
            .collect();
        let entries: usize = ids.iter().map(|id| tripwire.bodies(id)).sum();
        let writes = world.writes(call);
        let Some(outcome) = view_member.outcome() else {
            violations.push(format!("{call} has no final outcome"));
            continue;
        };
        match member.tool {
            Tool::Write { .. } => {
                if entries > 1 || writes.len() > 1 {
                    violations.push(format!(
                        "F2: Once {call} was entered {entries} times, wrote {writes:?}"
                    ));
                }
                // A completion recovers typed, exactly as its one write
                // answered it.
                let answered = |attempt: u32| {
                    round::completed_call(outcome).map(|answer| serde_json::to_value(answer).ok())
                        == Some(serde_json::to_value(written(call, attempt)).ok())
                };
                match outcome {
                    SettledOutput::Completed(_) if writes.len() == 1 && answered(writes[0]) => {}
                    SettledOutput::Interrupted => {}
                    SettledOutput::Cancelled { .. } if cancelled => {}
                    other => violations.push(format!(
                        "F2: Once {call} settled {other:?} after writing {writes:?}"
                    )),
                }
            }
            Tool::Flaky => {
                if !matches!(outcome, SettledOutput::Completed(_)) {
                    violations.push(format!("{call} settled {outcome:?}"));
                }
                if view_member.starts().len() != 2 {
                    violations.push(format!(
                        "NR-3: {call} took {} attempts; only its one known failure advances it",
                        view_member.starts().len()
                    ));
                }
                if writes.iter().any(|attempt| *attempt > 2) {
                    violations.push(format!("NR-3: {call} wrote {writes:?}"));
                }
            }
            Tool::Failing => match outcome {
                SettledOutput::Failed(_) => {}
                // A cancel in the backoff ends the call before its next
                // attempt.
                SettledOutput::Cancelled { .. } if cancelled => {}
                other => violations.push(format!("{call} settled {other:?}")),
            },
            Tool::Defer { repeatable } => {
                if !repeatable && (entries > 1 || writes.len() > 1) {
                    violations.push(format!(
                        "F2: Once {call} was entered {entries} times, wrote {writes:?}"
                    ));
                }
                if view_member.starts().len() != 1 {
                    violations.push(format!(
                        "L-B2: a crash advanced parked {call} to {} attempts",
                        view_member.starts().len()
                    ));
                }
                let keys = world.keys(call);
                if keys.windows(2).any(|pair| pair[0] != pair[1]) {
                    violations.push(format!(
                        "L-B2: a rerun of {call} was handed another completion key"
                    ));
                }
                let answered = host_answer(call).to_string();
                match outcome {
                    SettledOutput::Completed(answer)
                        if !writes.is_empty() && answer.payload() == answered => {}
                    SettledOutput::Interrupted if !repeatable => {}
                    SettledOutput::Cancelled { .. } if cancelled => {}
                    other => violations.push(format!(
                        "Pending: {call} settled {other:?} after writing {writes:?}"
                    )),
                }
            }
            Tool::Timer { after } => {
                if entries > 1 || writes.len() > 1 || view_member.starts().len() != 1 {
                    violations.push(format!(
                        "TIMER-COLD: {call} repeated a committed pending body"
                    ));
                }
                let expected = view_member.draft().admitted_at().0.saturating_add(after);
                if world
                    .timers
                    .lock_recover()
                    .get(call)
                    .is_some_and(|deadlines| deadlines.iter().any(|deadline| *deadline != expected))
                {
                    violations.push(format!(
                        "TIMER-COLD: {call} refreshed its admitted deadline"
                    ));
                }
                match outcome {
                    SettledOutput::Completed(answer)
                        if !writes.is_empty() && answer.payload() == "null" => {}
                    SettledOutput::Interrupted => {}
                    SettledOutput::Cancelled { .. } if cancelled => {}
                    other => violations.push(format!("TIMER-COLD: {call} settled {other:?}")),
                }
            }
            Tool::Quick => {
                if view_member.starts().len() != 1 {
                    violations.push(format!(
                        "L-B2: a crash advanced {call} to {} attempts",
                        view_member.starts().len()
                    ));
                }
                match outcome {
                    SettledOutput::Completed(_) if !writes.is_empty() => {}
                    SettledOutput::TimedOut {
                        cause: LimitCause::ExecutionTotal,
                        ..
                    } if entries <= 1 => {}
                    other => violations.push(format!(
                        "L-C1: {call} settled {other:?} after {entries} entries"
                    )),
                }
            }
        }
    }
    violations
}

/// F1: once a zombie's actors moved, every owner write it attempts is
/// refused with `OwnershipLost`.
fn zombie_laws(cut: &Cut, trace: &[lash_durable_test::Write]) -> Vec<String> {
    let mut violations = Vec::new();
    if cut.fault != Fault::Zombie || cut.kind != WriteKind::Actor {
        return violations;
    }
    let Some(at) = trace.iter().position(|write| {
        write.node == cut.node && write.point == cut.point && write.cut == Some(cut.fault)
    }) else {
        return vec!["F1: the zombie's cut write is not in the trace".to_owned()];
    };
    for write in trace[at..]
        .iter()
        .filter(|write| write.node == cut.node && write.kind == WriteKind::Actor)
    {
        match &write.stored {
            Stored::Refused(DurableError::OwnershipLost(_)) => {}
            other => violations.push(format!("F1: zombie write {write} was {other:?}")),
        }
    }
    violations
}

fn matrix() -> Matrix {
    Matrix::new()
        .faults(&[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
}

/// Member 0 finishes last and member 2 first, so their outcomes commit in
/// the opposite of their declared order.
fn mixed_members() -> Vec<Member> {
    vec![
        Member {
            call: call("call-a"),
            tool: Tool::Write { millis: 50 },
        },
        Member {
            call: call("call-b"),
            tool: Tool::Flaky,
        },
        Member {
            call: call("call-c"),
            tool: Tool::Write { millis: 0 },
        },
    ]
}

/// F2, NR-1 to NR-4 and retry ownership over every cut of a three-member
/// round mixing `Once` and `Repeatable` members, on SQLite in memory.
#[tokio::test]
async fn a_mixed_round_holds_once_and_repeatable_rules_across_every_cut() {
    let report = matrix()
        .run_test(|| RoundScenario::new(mixed_members()))
        .await;
    eprintln!("L4 mixed round: {} cells", report.cells.len());
    report.assert_held();
    for label in [
        CommitLabel::MODEL_DONE,
        CommitLabel::ROUND_OUTCOME,
        CommitLabel::ROUND_START,
        CommitLabel::ROUND_PRESENT_MODEL_START,
    ] {
        assert!(
            report.labels().contains(&label),
            "the matrix never cut {label}"
        );
    }
}

/// Pending, F2 and K over every cut of a round whose `Once` member parks on
/// the completion wait its admission pinned beside a `Once` write: the
/// parked body is entered at most once, a crash before its park commits
/// interrupts it, and the host's resolution of the key it handed out
/// settles it; a forged key resolves nothing.
#[tokio::test]
async fn a_pending_member_parks_once_and_its_key_settles_it_across_every_cut() {
    let members = vec![
        Member {
            call: call("call-p"),
            tool: Tool::Defer { repeatable: false },
        },
        Member {
            call: call("call-w"),
            tool: Tool::Write { millis: 0 },
        },
    ];
    let settled: Arc<Mutex<Vec<SettledOutput>>> = Arc::default();
    let shared = Arc::clone(&settled);
    let report = matrix()
        .run_test(move || {
            let mut fresh = RoundScenario::new(members.clone());
            fresh.settled = Arc::clone(&shared);
            fresh
        })
        .await;
    eprintln!("L4 pending round: {} cells", report.cells.len());
    report.assert_held();
    for label in [
        CommitLabel::MODEL_DONE,
        CommitLabel::ROUND_OUTCOME,
        CommitLabel::ROUND_PRESENT_MODEL_START,
    ] {
        assert!(
            report.labels().contains(&label),
            "the matrix never cut {label}"
        );
    }
    assert!(
        settled
            .lock_recover()
            .iter()
            .any(|outcome| matches!(outcome, SettledOutput::Completed(_))),
        "no run settled the parked call from its key"
    );
}

/// L-B2 for a parked `Repeatable` member: a crash before its park commits
/// reruns it at its same ordinal, handed the same completion key, and the
/// host's resolution of that key settles it.
#[tokio::test]
async fn a_repeatable_pending_member_reruns_with_the_same_key_across_every_cut() {
    let members = vec![Member {
        call: call("call-r"),
        tool: Tool::Defer { repeatable: true },
    }];
    let report = matrix()
        .run_test(move || RoundScenario::new(members.clone()))
        .await;
    eprintln!("L4 repeatable pending round: {} cells", report.cells.len());
    report.assert_held();
}

/// Declared order: the fixture commits c,a,b and hands its consumer a,b,c.
#[tokio::test]
async fn outcomes_committed_out_of_order_present_in_declared_order() {
    let outcomes_in_order: Arc<Mutex<Vec<ToolCallId>>> = Arc::default();
    let shared = Arc::clone(&outcomes_in_order);
    let presentations: Arc<Mutex<Vec<round::Presentation>>> = Arc::default();
    let presented = Arc::clone(&presentations);
    let report = Matrix::new()
        .faults(&[])
        .run_test(move || {
            let mut fresh = RoundScenario::new(mixed_members());
            fresh.outcomes = Arc::clone(&shared);
            fresh.presentations = Arc::clone(&presented);
            fresh
        })
        .await;
    report.assert_held();
    assert_eq!(
        *outcomes_in_order.lock_recover(),
        vec![call("call-c"), call("call-a"), call("call-b")],
        "the outcomes committed out of their declared order"
    );
    let presentations = presentations.lock_recover();
    assert_eq!(presentations.len(), 1, "the owner presented once");
    assert_eq!(
        presentations[0].calls(),
        &[call("call-a"), call("call-b"), call("call-c")],
        "the returned Presentation must follow the model's declared order"
    );
}

/// Retry ownership: the attempt cap ends the call with its last known
/// failure.
#[tokio::test]
async fn the_attempt_cap_ends_a_repeatable_call() {
    let members = vec![Member {
        call: call("call-f"),
        tool: Tool::Failing,
    }];
    let world: Arc<ExternalWorld> = Arc::default();
    let shared = Arc::clone(&world);
    let report = Matrix::new()
        .faults(&[])
        .run_test(move || {
            let mut fresh = RoundScenario::new(members.clone());
            fresh.world = Arc::clone(&shared);
            fresh
        })
        .await;
    report.assert_held();
    assert_eq!(world.writes(&call("call-f")), vec![1, 2, 3]);
}

/// Retry ownership: a backoff is spent from the call's one limit; a retry
/// that would start after the limit expires ends the call instead.
#[tokio::test]
async fn a_retry_backoff_consumes_the_total_limit() {
    let members = vec![Member {
        call: call("call-f"),
        tool: Tool::Failing,
    }];
    let world: Arc<ExternalWorld> = Arc::default();
    let shared = Arc::clone(&world);
    let report = Matrix::new()
        .faults(&[])
        .run_test(move || {
            let mut fresh = RoundScenario::new(members.clone()).limit_ms(250);
            fresh.world = Arc::clone(&shared);
            fresh
        })
        .await;
    report.assert_held();
    // Attempt 1 fails at 0 and retries at 100; attempt 2 fails at 100 and
    // its backoff (200) would end past the 250 ms limit.
    assert_eq!(world.writes(&call("call-f")), vec![1, 2]);
}

/// L-B2 and L-C1: a `Repeatable` started without an outcome reruns at its
/// same ordinal with the limit its admission recorded; once that limit has
/// expired by the time another owner resumes it, it settles
/// `TimedOut { ExecutionTotal }` at once, without entering its body again.
#[tokio::test]
async fn an_expired_limit_settles_at_once_on_resume_and_is_never_refreshed() {
    let members = vec![Member {
        call: call("call-q"),
        tool: Tool::Quick,
    }];
    let outcomes: Arc<Mutex<Vec<ToolCallId>>> = Arc::default();
    let settled: Arc<Mutex<Vec<SettledOutput>>> = Arc::default();
    let shared = (Arc::clone(&outcomes), Arc::clone(&settled));
    // The limit is shorter than a failover, so a resumed rerun finds it
    // expired.
    let report = matrix()
        // The pinned 5s limit must expire before recovery; the default
        // 17.25s failover proves that deadline law.
        .lease(lash_durable::LeaseConfig::default())
        .run(move || {
            let mut fresh = RoundScenario::new(members.clone()).limit_ms(5_000);
            fresh.outcomes = Arc::clone(&shared.0);
            fresh.settled = Arc::clone(&shared.1);
            fresh
        })
        .await;
    report.assert_held();
    assert!(
        settled.lock_recover().iter().any(|outcome| matches!(
            outcome,
            SettledOutput::TimedOut {
                cause: LimitCause::ExecutionTotal,
                ..
            }
        )),
        "no cut resumed the call after its limit expired"
    );
}

/// Member cancel: once the turn is cancelled, a member still running gets
/// its token and the stop grace, then records `Cancelled`; a member that
/// already finished keeps its outcome.
#[tokio::test]
async fn a_turn_cancel_ends_unfinished_members_as_cancelled() {
    let members = vec![
        Member {
            call: call("call-slow"),
            tool: Tool::Write { millis: 30_000 },
        },
        Member {
            call: call("call-fast"),
            tool: Tool::Write { millis: 0 },
        },
    ];
    let settled: Arc<Mutex<Vec<SettledOutput>>> = Arc::default();
    let shared = Arc::clone(&settled);
    let report = Matrix::new()
        .faults(&[])
        .run_test(move || {
            let mut fresh = RoundScenario::new(members.clone());
            fresh.cancel_after_ms = Some(1_000);
            fresh.settled = Arc::clone(&shared);
            fresh
        })
        .await;
    report.assert_held();
    let settled = settled.lock_recover().clone();
    assert!(
        matches!(
            settled.as_slice(),
            [SettledOutput::Cancelled { .. }, SettledOutput::Completed(_)]
        ),
        "{settled:?}"
    );
}

/// Member cancel during a backoff: a cancel that lands while a `Repeatable`
/// call waits out its retry starts no next attempt, and records the call
/// `Cancelled` at once rather than at the retry's due time. The failed
/// attempt is the call's last: the cancel settles it on that attempt's own
/// start, and no next ordinal is opened.
#[tokio::test]
async fn a_cancel_during_a_retry_backoff_starts_no_next_attempt() {
    let members = vec![Member {
        call: call("call-f"),
        tool: Tool::Failing,
    }];
    let world: Arc<ExternalWorld> = Arc::default();
    let settled: Arc<Mutex<Vec<SettledOutput>>> = Arc::default();
    let shared = (Arc::clone(&world), Arc::clone(&settled));
    let report = Matrix::new()
        .faults(&[])
        .run_test(move || {
            let mut fresh = RoundScenario::new(members.clone());
            // Attempt 1 fails at 0 and its retry is due at 100.
            fresh.cancel_after_ms = Some(50);
            fresh.world = Arc::clone(&shared.0);
            fresh.settled = Arc::clone(&shared.1);
            fresh
        })
        .await;
    report.assert_held();
    assert_eq!(world.writes(&call("call-f")), vec![1]);
    let settled = settled.lock_recover().clone();
    assert!(
        matches!(settled.as_slice(), [SettledOutput::Cancelled { .. }]),
        "{settled:?}"
    );
    let cancelled_at = report
        .baseline
        .iter()
        .filter(|write| write.committed() && write.point.label == CommitLabel::ROUND_OUTCOME)
        .map(|write| write.at_ms)
        .max();
    assert!(
        cancelled_at.is_some_and(|at| at < 100),
        "the cancel waited for the retry's due time: {cancelled_at:?}"
    );
}

/// SETTLED-PAYLOAD-REUSE: settled native members, their completions held
/// typed, reuse their verified payload image across folds, attempt
/// histories and recovery views; each projects its completion's exact JSON.
#[tokio::test]
async fn settled_payload_reuse() {
    use lash_durable::DurableReads as _;
    let stores = sim::memory(SimClock::new()).await;
    let database = stores.durable_store();
    let mut seed = MailTx::new();
    seed.create_actor(actor(), FormatSet::new(FORMATS));
    database
        .commit_mail(seed, CommitLabel::MAIL_SESSION)
        .await
        .unwrap();
    let lease = database
        .register_node(&lash_durable::NodeSpec {
            node: lash_durable::NodeId::new("payload-reuse"),
            decodes: vec![FormatSet::new(FORMATS)],
            serves: Vec::new(),
            capabilities: Default::default(),
            ttl_millis: 15_000,
        })
        .await
        .unwrap();
    let claimed = database.claim(&lease, 1).await.unwrap();
    assert_eq!(claimed.len(), 1);
    let epoch = claimed[0].epoch;
    let mut tx = database.begin(&actor(), epoch).await.unwrap();
    let drafts = (0..18)
        .map(|index| {
            ExecutionDraft::new(
                tx.opened_at(),
                call(&format!("branch-{index}")),
                ToolId::new("fixture_large"),
                material("request"),
                ExecutionPolicy::Once,
                ExecutionLimit {
                    expires_at: u64::MAX,
                    max_slice: Duration::from_secs(60),
                },
                None,
            )
        })
        .collect();
    let members = round::admit(&mut tx, &owner(), RUN, drafts).unwrap();
    let mut expected = Vec::new();
    for member in &members {
        let answer = lash_core_execution::ToolCallOutput::success(
            serde_json::json!({"text": "x".repeat(3370)}),
        );
        let completed = round::CompletedCall {
            control_origin: lash_sansio::ToolControlOrigin::OwnCall,
            call_id: member.call().clone(),
            provider_call_id: None,
            tool_name: "fixture_large".to_owned(),
            args: serde_json::json!({"bytes": 3370}),
            model_return: lash_protocol_types::ModelToolReturn::from_output(
                "fixture_large".to_owned(),
                &answer,
            ),
            output: answer,
            intent_outcomes: Vec::new(),
            replay: None,
        };
        let payload = serde_json::to_string(&completed).unwrap();
        expected.push(payload);
        round::settle(
            &mut tx,
            member,
            SettledOutput::Completed(
                round::completed_material(&EffectOpener::turn(session(), turn()), completed)
                    .unwrap(),
            ),
            Vec::new(),
            None,
        )
        .unwrap();
    }
    database
        .commit(tx, CommitLabel::ROUND_OUTCOME)
        .await
        .unwrap();
    let facts = database.run_facts(&owner()).await.unwrap();
    let waits = round::PinnedWaits::default();
    let policies = PolicyView::default();
    let mut records = round::RunRecords::new(facts);
    let first = records.fold(&policies, &waits).unwrap();
    let revisited = records.fold(&policies, &waits).unwrap();
    assert_eq!(first, revisited);
    for ((before, after), payload) in first
        .round(RUN)
        .unwrap()
        .members()
        .iter()
        .zip(revisited.round(RUN).unwrap().members())
        .zip(&expected)
    {
        let original = before.outcome().unwrap().payload().unwrap();
        let reused = after.outcome().unwrap().payload().unwrap();
        assert_eq!(reused, payload);
        let output = after.outcome().unwrap();
        let wire = serde_json::json!({"outcome": output.outcome(), "payload": payload});
        assert_eq!(serde_json::to_value(output).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<SettledOutput>(wire).unwrap(),
            *output
        );
        assert_eq!(
            original.as_ptr(),
            reused.as_ptr(),
            "a settled payload is shared on revisit"
        );
        assert_eq!(
            after
                .attempts()
                .next()
                .unwrap()
                .1
                .payload()
                .unwrap()
                .as_ptr(),
            reused.as_ptr()
        );
        let named = match after.outcome().unwrap() {
            SettledOutput::Completed(material) => material.reference(),
            _ => panic!("a completed member"),
        };
        assert!(Material::new(named.clone(), "changed".to_owned()).is_err());
        let mut wrong_owner = named.clone();
        wrong_owner.owner = MaterialOwner::Run {
            opener: EffectOpener::turn("another", "run"),
        };
        assert!(Material::new(wrong_owner, reused.to_owned()).is_err());
    }
    for ((_, recovery), member) in revisited
        .recoveries()
        .iter()
        .zip(revisited.round(RUN).unwrap().members())
    {
        let round::Recovery::Settled(output) = recovery else {
            panic!("a settled recovery")
        };
        assert_eq!(
            output.payload().unwrap().as_ptr(),
            member.outcome().unwrap().payload().unwrap().as_ptr()
        );
    }
    // A new settlement changes only its own image, never earlier members'.
    let mut tx = database.begin(&actor(), epoch).await.unwrap();
    let next = RunSeq(RUN.0 + 1);
    let changed_draft = members[0].draft().clone();
    // A different run may reuse this call's pinned request.
    let changed = round::admit(&mut tx, &owner(), next, vec![changed_draft]).unwrap();
    let changed_output = SettledOutput::Completed(output("changed payload".to_owned()));
    round::settle(
        &mut tx,
        &changed[0],
        changed_output.clone(),
        Vec::new(),
        None,
    )
    .unwrap();
    let appended = lash_durable::domain::RunFacts::appended(tx.domain(), tx.graph(), epoch);
    database
        .commit(tx, CommitLabel::ROUND_OUTCOME)
        .await
        .unwrap();
    records.extend(appended, Vec::new());
    let extended = records.fold(&policies, &waits).unwrap();
    assert_eq!(
        extended.round(next).unwrap().members()[0].outcome(),
        Some(&changed_output)
    );
    for (before, after) in first
        .round(RUN)
        .unwrap()
        .members()
        .iter()
        .zip(extended.round(RUN).unwrap().members())
    {
        assert_eq!(
            before.outcome().unwrap().payload().unwrap().as_ptr(),
            after.outcome().unwrap().payload().unwrap().as_ptr()
        );
    }

    // Replacing an already checked node cannot reuse its old checked image.
    let id = records
        .facts()
        .rows
        .iter()
        .find(|row| row.kind == RunFactKind::XOutcome)
        .unwrap()
        .record;
    let mut bad = records.facts().nodes[&id].clone();
    let mut bytes = bad.bytes.to_vec();
    *bytes.last_mut().unwrap() ^= 1;
    bad.bytes = bytes.into();
    let mut corrupted = lash_durable::domain::RunFacts::default();
    corrupted.nodes.insert(id, bad);
    records.extend(corrupted, Vec::new());
    assert!(
        records.fold(&policies, &waits).is_err(),
        "a conflicting node must be checked again"
    );
}

/// TIMER-COLD: an ordinary pending timer commits its wait with the park;
/// every crash/drop cut recovers the same admission/deadline, and cancellation
/// and timer settlement commit a single final outcome without reentering the body.
#[tokio::test]
async fn an_ordinary_pending_timer_recovers_its_original_deadline_across_every_cut() {
    for cancel_after_ms in [None, Some(10), Some(20)] {
        let settled: Arc<Mutex<Vec<SettledOutput>>> = Arc::default();
        let shared = Arc::clone(&settled);
        let report = matrix()
            .run_test(move || {
                let mut scenario = RoundScenario::new(vec![Member {
                    call: call("ordinary-timer"),
                    tool: Tool::Timer { after: 20 },
                }]);
                scenario.cancel_after_ms = cancel_after_ms;
                scenario.settled = Arc::clone(&shared);
                scenario
            })
            .await;
        report.assert_held();
        assert!(report.labels().contains(&CommitLabel::ROUND_OUTCOME));
        let outcomes = settled.lock_recover();
        if cancel_after_ms.is_none() {
            assert!(
                outcomes
                    .iter()
                    .any(|outcome| matches!(outcome, SettledOutput::Completed(_))),
                "the durable timer never resolved on cold recovery"
            );
        } else {
            assert!(
                outcomes
                    .iter()
                    .any(|outcome| matches!(outcome, SettledOutput::Cancelled { .. })),
                "cancellation never raced the durable timer"
            );
        }
    }
}
