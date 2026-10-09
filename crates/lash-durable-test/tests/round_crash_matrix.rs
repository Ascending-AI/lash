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

use lash_core_execution::runtime::actor::round::{
    self, ExecutionDraft, Material, MemberBodies, MemberBody, MemberResult, PolicyView, RoundDraft,
    RoundEnd, RoundRunner, RunFold, SettledOutput,
};
use lash_core_execution::runtime::actor::waits::{
    self, ParkDeadline, Resolution, ResolveAnswer, WaitDeadline, WaitId, WaitKind,
};
use lash_core_execution::{ActorContext, AdmittedScope, Backend};
use lash_core_store::effect_opener::EffectOpener;
use lash_core_store::tool_run::{
    CompletionSource, KnownFailureReason, MaterialDigest, MaterialLocation, MaterialOwner,
    MaterialRef, MaterialRole,
};
use lash_durable::domain::{AdmittedId, OwnerKey, RunRecordKind, RunSeq};
use lash_durable::runner::{Activation, Exit, Owned};
use lash_durable::{
    ActorKey, ActorState, CommitLabel, DurableError, DurableStore, FormatSet, MailTx, Release,
};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire, WriteKind,
};
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{
    ExecutionLimit, ExecutionPolicy, LimitCause, SessionId, ToolCallId, ToolId, TurnId,
};
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
}

impl Tool {
    fn id(self) -> ToolId {
        ToolId::new(match self {
            Self::Write { .. } => "write",
            Self::Flaky => "flaky",
            Self::Failing => "failing",
            Self::Quick => "quick",
            Self::Defer { repeatable: false } => "defer",
            Self::Defer { repeatable: true } => "defer-repeatable",
        })
    }

    fn policy(self) -> ExecutionPolicy {
        match self {
            Self::Write { .. } | Self::Defer { repeatable: false } => ExecutionPolicy::Once,
            Self::Flaky | Self::Failing | Self::Quick | Self::Defer { repeatable: true } => {
                ExecutionPolicy::repeatable(NonZeroU32::new(3).unwrap(), 100, 1_000)
            }
        }
    }

    /// Whether it may park.
    fn defers(self) -> bool {
        matches!(self, Self::Defer { .. })
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
                let output = output(format!("{call}#{attempt}"));
                let fails = match tool {
                    Tool::Write { .. } | Tool::Quick | Tool::Defer { .. } => false,
                    Tool::Flaky => attempt == 1,
                    Tool::Failing => true,
                };
                MemberResult::from(if fails {
                    SettledOutput::Failed(output.failure(KnownFailureReason::Reported, None))
                } else {
                    SettledOutput::Completed(output)
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
        let key = waits::host_key(&pinned.wait()).expect("a tool completion wait has a host key");
        Box::new(move |_token| {
            Box::pin(async move {
                world.write(&call, attempt);
                world.hand_out(&call, key.as_str());
                let answer = host_answer(&call);
                tokio::spawn(async move {
                    lash_core_ids::clock::Clock::sleep(
                        &*clock,
                        Duration::from_millis(RESOLVE_AFTER_MS),
                    )
                    .await;
                    let _ =
                        waits::resolve_host(&backend, key.as_str(), Resolution::Ok(answer)).await;
                });
                MemberResult::from(SettledOutput::Waiting(
                    output("parked".to_owned()).parked(pinned.id.to_hex()),
                ))
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
    bodies: Arc<Catalog>,
    tripwire: Arc<Tripwire>,
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
            .run_records(&owner())
            .await
            .map_err(Pass::from)?;
        let bodies: Arc<dyn MemberBodies> = Arc::clone(&self.bodies) as _;
        let runner = if rows.iter().any(|row| row.run == RUN) {
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
        let runner = match self.cancel_after_ms {
            Some(after) => {
                let cancel = CancellationToken::new();
                let fire = cancel.clone();
                let clock = Arc::clone(cx.clock());
                tokio::spawn(async move {
                    clock.sleep(Duration::from_millis(after)).await;
                    fire.cancel();
                });
                runner.cancelled_by(cancel)
            }
            None => runner,
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
            round::present(&mut tx, &admitted, end.fold());
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
                Err(Pass::Again) => owned.wait_for_mail().await,
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
    world: Arc<ExternalWorld>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<(Backend, Arc<SimClock>)>>,
    /// The calls whose final outcomes committed, in commit order.
    outcomes: Arc<Mutex<Vec<ToolCallId>>>,
    /// Every final outcome each run of the scenario ended with.
    settled: Arc<Mutex<Vec<SettledOutput>>>,
}

impl RoundScenario {
    fn new(members: Vec<Member>) -> Self {
        Self {
            members,
            limit_ms: 60_000,
            cancel_after_ms: None,
            world: Arc::default(),
            tripwire: Arc::default(),
            backend: Mutex::default(),
            outcomes: Arc::default(),
            settled: Arc::default(),
        }
    }

    fn limit_ms(mut self, limit_ms: u64) -> Self {
        self.limit_ms = limit_ms;
        self
    }
}

#[async_trait::async_trait]
impl Scenario for RoundScenario {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let stores = sim::memory(Arc::clone(&clock)).await;
        let database = Arc::new(stores.durable_store());
        *self.backend.lock_recover() = Some((Backend::for_testing(Arc::new(stores)), clock));
        database
    }

    fn config(&self) -> SimNodesConfig {
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: vec![FormatSet::new(FORMATS)],
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
        })
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
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
        let rows = match nodes.database().run_records(&owner()).await {
            Ok(rows) => rows,
            Err(error) => return vec![format!("the run records do not read: {error}")],
        };
        *self.outcomes.lock_recover() = rows
            .iter()
            .filter(|row| row.kind == RunRecordKind::XOutcome)
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
            .iter()
            .filter(|row| row.kind == RunRecordKind::XStart)
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
    if view.presented() != Some(declared.as_slice()) {
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
                match outcome {
                    SettledOutput::Completed(_) if writes.len() == 1 => {}
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

/// Declared order: two members whose outcomes commit in the opposite order
/// present in declared order.
#[tokio::test]
async fn outcomes_committed_out_of_order_present_in_declared_order() {
    let outcomes_in_order: Arc<Mutex<Vec<ToolCallId>>> = Arc::default();
    let shared = Arc::clone(&outcomes_in_order);
    let report = Matrix::new()
        .faults(&[])
        .run_test(move || {
            let mut fresh = RoundScenario::new(mixed_members());
            fresh.outcomes = Arc::clone(&shared);
            fresh
        })
        .await;
    report.assert_held();
    let outcomes: Vec<CommitLabel> = report
        .baseline
        .iter()
        .filter(|write| write.kind == WriteKind::Actor && write.committed())
        .map(|write| write.point.label)
        .collect();
    assert_eq!(
        outcomes,
        vec![
            CommitLabel::MODEL_DONE,
            // c's outcome with b's first attempt's retry, inside one window
            CommitLabel::ROUND_OUTCOME,
            // a's, once its body ends
            CommitLabel::ROUND_OUTCOME,
            CommitLabel::ROUND_START,
            // b's second attempt
            CommitLabel::ROUND_OUTCOME,
            CommitLabel::ROUND_PRESENT_MODEL_START,
            CommitLabel::SESSION_RELEASE,
        ]
    );
    assert_eq!(
        *outcomes_in_order.lock_recover(),
        vec![call("call-c"), call("call-a"), call("call-b")],
        "the outcomes committed out of their declared order"
    );
}

/// Group commit: members that finish within the window share one outcome
/// transaction.
#[tokio::test]
async fn members_finishing_together_commit_in_one_transaction() {
    let members = ["call-a", "call-b", "call-c"]
        .into_iter()
        .map(|name| Member {
            call: call(name),
            tool: Tool::Write { millis: 0 },
        })
        .collect();
    let report = Matrix::new()
        .faults(&[])
        .run_test(|| RoundScenario::new(Vec::clone(&members)))
        .await;
    report.assert_held();
    let outcomes = report
        .baseline
        .iter()
        .filter(|write| {
            write.kind == WriteKind::Actor
                && write.committed()
                && write.point.label == CommitLabel::ROUND_OUTCOME
        })
        .count();
    assert_eq!(outcomes, 1);
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
