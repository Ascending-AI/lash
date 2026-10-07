//! Steering input and process wakes on the production session activation
//! (FIG-5293, FIG-5294, ADR 0101 §5, ADR 0132 §4).
//!
//! A lash core's session holds one sent input. While its turn runs, a host
//! sends the session a steering input addressed to that turn
//! (`TurnInputIngress::ActiveTurn`), or a process files a wake for the
//! session (queued turn work). The nodes are simulated (A and B) over the
//! production durable store, and the core's own turn services run the turns
//! with a scripted model.
//!
//! - **At a work checkpoint:** the steer is sent by a tool body of the
//!   turn's first round. The checkpoint after that round delivers it: the
//!   model's next request holds it, and the turn runs a second round before
//!   it answers. Its admission commits with the next phase, so the steer is
//!   bound to the running run, applied at the checkpoint, and committed as
//!   one user row of its own input id.
//! - **At the terminal checkpoint:** the steer is sent while the turn's last
//!   model call runs. The committed finish is the turn's answer, so the
//!   terminal checkpoint withholds it; it stays session mail and runs as
//!   the session's next run, under its own id (ADR 0101 §3, §5.1).
//! - **A process wake** is delivered on the same path: filed during the
//!   first round, the next work checkpoint delivers it with the steering
//!   input, in ingress order, and its admission commits with the next phase;
//!   filed during the last model call, it stays session mail and is the
//!   session's next run (FIG-5294).
//!
//! The matrix cuts the uncut run at every labelled write, under
//! fail-before, ack-hidden, zombie, abort and commit-then-abort, recovers on
//! the other node, and checks that the steer was delivered exactly once,
//! where its scenario says, never lost or duplicated, and that nothing is
//! left open, bound or mailed.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/dialect.rs"]
mod dialect;
#[path = "support/sim.rs"]
mod sim;

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{LlmRequest, LlmResponse, LlmStreamEvent, StreamBlockIdentity};
use lash_core::runtime::durable::session::SessionActivation;
use lash_core::{LlmOutputPart, ToolCall, ToolOutcome};
use lash_core_execution::facade_support::{
    ProcessWake, ProcessWakeDeliveryRequest, process_wake_batch_draft, process_wake_delivery,
};
use lash_core_execution::{
    Backend, BackendParts, FleetFormat, NoProjectionProviders, ProcessId, StoreSet,
};
use lash_core_store::store::{IngressTerminalCause, RunTerminalKind};
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableError, DurableStore, LeaseConfig};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire, WriteKind,
};
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{InputId, SessionId, TurnId};

use dialect::Dialect;

const SESSION: &str = "steering-session";
const RUN: &str = "steering-turn";
/// The steer's host id: the run it opens when no running turn takes it.
const STEER_RUN: &str = "steering-follow-on";
const TOOL: &str = "steer_round";
const MODEL: &str = "steering-model";
const ASK: &str = "work through two rounds";
const STEER: &str = "and mind the steer";
/// The tool results of the two rounds, by label.
const FIRST_ROUND: &str = "first-round";
const SECOND_ROUND: &str = "second-round";
/// The steered turn's answer.
const FINAL: &str = "steered answer";
/// The first run's answer when the steer is withheld from it.
const ANSWER: &str = "unsteered answer";
/// What the model answers when the work checkpoint never delivered the steer.
const UNSTEERED: &str = "the steer never arrived";
/// The process whose wake the session receives, and the wake's input.
const PROCESS: &str = "p_0192a3b4c5d670008000000000000001";
const WAKE: &str = "the background process settled";

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

fn run() -> TurnId {
    TurnId::try_from(RUN.to_owned()).unwrap()
}

fn steer_run() -> TurnId {
    TurnId::try_from(STEER_RUN.to_owned()).unwrap()
}

/// The steer's input id, keyed by its host id as the facade's send keys it.
fn steer_input() -> InputId {
    lash_core::PendingTurnInputDraft::keyed_input_id(&session(), STEER_RUN)
}

fn actor() -> ActorKey {
    ActorKey::session(SESSION).unwrap()
}

/// What arrives while the turn runs, and where.
#[derive(Clone, Copy, Debug)]
enum Arrival {
    /// A steer, while the first round's tool body runs: the work checkpoint
    /// after the round delivers it.
    SteerAtWork,
    /// A steer, while the turn's last model call runs: the terminal
    /// checkpoint withholds it.
    SteerAtTerminal,
    /// A process wake, while the first round's tool body runs: the work
    /// checkpoint after the round delivers it.
    WakeAtWork,
    /// A process wake, while the turn's last model call runs: no checkpoint
    /// of the turn is left, so it runs next.
    WakeAfterTheLastCheckpoint,
    /// A steer and then a process wake, both while the first round's tool
    /// body runs: the same work checkpoint delivers both, in ingress order.
    SteerAndWakeAtWork,
}

impl Arrival {
    fn steers(self) -> bool {
        matches!(
            self,
            Self::SteerAtWork | Self::SteerAtTerminal | Self::SteerAndWakeAtWork
        )
    }

    fn wakes(self) -> bool {
        matches!(
            self,
            Self::WakeAtWork | Self::WakeAfterTheLastCheckpoint | Self::SteerAndWakeAtWork
        )
    }

    fn at_work(self) -> bool {
        matches!(
            self,
            Self::SteerAtWork | Self::WakeAtWork | Self::SteerAndWakeAtWork
        )
    }

    /// What it sends, as the turn the session runs sees it.
    fn marker(self) -> &'static str {
        if self.steers() { STEER } else { WAKE }
    }

    /// Send what arrives through `core`.
    async fn send(self, core: &OnceLock<lash::LashCore>) {
        if self.steers() {
            send_steer(core).await;
        }
        if self.wakes() {
            send_wake(core).await;
        }
    }
}

/// Send the steer through `core`, addressed to the running turn. Every send
/// is the same submission under its host id, so a body or call that runs
/// again after a cut accepts it once.
async fn send_steer(core: &OnceLock<lash::LashCore>) {
    let core = core.get().expect("the core is built before its turn runs");
    let session = core
        .session(session())
        .durable()
        .await
        .expect("the session's durable handle");
    session
        .send(lash::TurnInput::text(STEER))
        .id(steer_run())
        .ingress(lash::persistence::TurnInputIngress::active_turn(
            run(),
            lash::persistence::TurnInputCheckpointBoundary::AfterWork,
        ))
        .await
        .expect("the running turn accepts its steer");
}

/// The process's wake for the session, as its event append files it: queued
/// turn work under the wake's source key, so a repeat is the same batch.
fn wake() -> lash::persistence::QueuedWorkBatchDraft {
    process_wake_batch_draft(
        process_wake_delivery(ProcessWakeDeliveryRequest {
            target_session_id: session(),
            process_id: ProcessId::parse(PROCESS).expect("the process id"),
            sequence: 1,
            event_type: "process.settled".to_owned(),
            process_caused_by: None,
            authority: lash::persistence::QueuedWorkAuthority::default(),
            wake: ProcessWake {
                input: WAKE.to_owned(),
            },
            trace_cause: Default::default(),
            occurred_at_ms: 1,
            fleet_format: FleetFormat::current(),
        })
        .expect("the wake's delivery"),
    )
}

/// File the wake through `core`'s store, which wakes the session actor in
/// the same transaction (ADR 0132 §12).
async fn send_wake(core: &OnceLock<lash::LashCore>) {
    let core = core.get().expect("the core is built before its turn runs");
    core.backend()
        .session_store_factory()
        .enqueue_queued_work(wake())
        .await
        .expect("the session accepts the wake");
}

/// `steer_round`'s body: it sends what arrives on the first round, then
/// answers its label.
struct SteerRound(Arc<OnceLock<lash::LashCore>>, Arrival);

#[async_trait::async_trait]
impl StaticToolExecute for SteerRound {
    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let label = call.args["label"].as_str().unwrap_or_default().to_owned();
        if label == FIRST_ROUND && self.1.at_work() {
            self.1.send(&self.0).await;
        }
        ToolOutcome::ok(serde_json::json!({ "answered": label })).into()
    }
}

fn steer_round(
    core: Arc<OnceLock<lash::LashCore>>,
    arrival: Arrival,
) -> Arc<dyn lash_core::ToolProvider> {
    let definition = lash_core::ToolDefinition::raw(
        TOOL,
        TOOL,
        "Answers its label; the first round's call sends the steer.",
        serde_json::json!({ "type": "object", "additionalProperties": true }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("steer_round's schemas")
    // A call a kill interrupted runs again at its ordinal, so the first
    // round's body sends the steer whatever was cut.
    .with_execution_policy(lash_core::ExecutionPolicy::repeatable(
        std::num::NonZeroU32::new(3).expect("a nonzero attempt bound"),
        1,
        1,
    ));
    Arc::new(StaticToolProvider::new(
        vec![definition],
        SteerRound(core, arrival),
    ))
}

fn text(request: &LlmRequest, text: &str) -> LlmResponse {
    if let Some(stream) = request.stream_events.as_ref() {
        stream.send(LlmStreamEvent::Delta {
            block: StreamBlockIdentity::new("text:0", 0),
            text: text.to_owned(),
        });
    }
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_owned(),
            response_meta: None,
        }],
        ..LlmResponse::default()
    }
}

fn round(call: &str, label: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::ToolCall {
            call_id: call.to_owned(),
            tool_name: TOOL.to_owned(),
            input_json: serde_json::json!({ "label": label }).to_string(),
            replay: None,
        }],
        ..LlmResponse::default()
    }
}

/// The scripted model of `arrival`; every request it saw is rendered into
/// `seen`.
fn model(
    arrival: Arrival,
    core: Arc<OnceLock<lash::LashCore>>,
    seen: Arc<Mutex<Vec<String>>>,
) -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("steering-scripted")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| {
            let seen = Arc::clone(&seen);
            let core = Arc::clone(&core);
            async move {
                let rendered = serde_json::to_string(&request.messages).expect("a request encodes");
                seen.lock_recover().push(rendered.clone());
                // Delivered: everything that arrives reached the request.
                let delivered = (!arrival.steers() || rendered.contains(STEER))
                    && (!arrival.wakes() || rendered.contains(WAKE));
                let response = if arrival.at_work() {
                    if delivered && rendered.contains(SECOND_ROUND) {
                        text(&request, FINAL)
                    } else if delivered {
                        round("steer-call-2", SECOND_ROUND)
                    } else if rendered.contains(FIRST_ROUND) {
                        text(&request, UNSTEERED)
                    } else {
                        round("steer-call-1", FIRST_ROUND)
                    }
                } else if rendered.contains(arrival.marker()) {
                    text(&request, FINAL)
                } else {
                    arrival.send(&core).await;
                    text(&request, ANSWER)
                };
                Ok(response)
            }
        })
        .build()
        .into_handle()
}

fn metadata() -> lash_core::LlmProfileMetadata {
    lash_core::LlmProfileMetadata::builder(MODEL)
        .context_window_tokens(200_000)
        .build()
        .expect("the model's metadata")
}

/// The scenario on one dialect, fresh for every matrix cell.
struct Steering {
    arrival: Arrival,
    dialect: Dialect,
    postgres_url: Option<String>,
    seen: Arc<Mutex<Vec<String>>>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<Backend>>,
    core: Arc<OnceLock<lash::LashCore>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl Steering {
    fn new(arrival: Arrival, dialect: Dialect, postgres_url: Option<String>) -> Self {
        Self {
            arrival,
            dialect,
            postgres_url,
            seen: Arc::default(),
            tripwire: Arc::default(),
            backend: Mutex::default(),
            core: Arc::default(),
            keep: Mutex::default(),
        }
    }

    fn backend(&self) -> Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }

    /// The deployment's core over the scenario's backend: it serves no node
    /// of its own, the simulated nodes run its sessions' turns.
    fn core(&self) -> lash::LashCore {
        self.core
            .get_or_init(|| {
                lash::LashCore::standard_builder(self.backend())
                    .serve_sessions(false)
                    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .serve_test_llm_profile(
                        model(self.arrival, Arc::clone(&self.core), Arc::clone(&self.seen)),
                        metadata(),
                    )
                    .tools(steer_round(Arc::clone(&self.core), self.arrival))
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(
                        "steering-deployment",
                        "steering-boot",
                    ))
                    .expect("the core builds")
            })
            .clone()
    }

    /// Create the session and send it the first input through the core,
    /// uncut: the host is outside the deployment under test.
    async fn send(&self) -> Result<(), String> {
        let session = self
            .core()
            .session(session())
            .create(lash::SessionCreation::root(lash::SessionSpec::new(
                MODEL,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(64),
            )))
            .await
            .map_err(|error| format!("create the session: {error}"))?;
        session
            .send(lash::TurnInput::text(ASK))
            .id(run())
            .await
            .map(drop)
            .map_err(|error| format!("send the turn's input: {error}"))
    }

    /// The run that applied the steer, its application's checkpoint, and how
    /// many committed transcript rows carry its input id.
    async fn steer_evidence(
        &self,
    ) -> Result<(Option<TurnId>, Vec<lash_core::TurnInputApplication>, usize), String> {
        let store = self.backend().session_store_factory();
        let bound = store
            .run_binding(&session(), &steer_input())
            .await
            .map_err(|error| format!("read the steer's binding: {error}"))?;
        let durable = self
            .core()
            .session(session())
            .durable()
            .await
            .map_err(|error| format!("the session's durable handle: {error}"))?;
        let applications = durable
            .turn_input_applications()
            .await
            .map_err(|error| format!("read the applications: {error}"))?
            .into_iter()
            .filter(|application| application.input_id == steer_input())
            .collect();
        let rows = durable
            .transcript()
            .await
            .map_err(|error| format!("read the transcript: {error}"))?
            .into_records()
            .into_iter()
            .filter(|row| row.provenance.input_id.as_ref() == Some(&steer_input()))
            .count();
        Ok((bound, applications, rows))
    }

    fn checkpoint_laws(
        seen: &[String],
        evidence: &(Option<TurnId>, Vec<lash_core::TurnInputApplication>, usize),
    ) -> Vec<String> {
        let mut violations = Vec::new();
        if let Some(unsteered) = seen
            .iter()
            .find(|request| request.contains(FIRST_ROUND) && !request.contains(STEER))
        {
            violations.push(format!(
                "the work checkpoint did not deliver the steer to the next request: {unsteered}"
            ));
        }
        if let Some(twice) = seen
            .iter()
            .find(|request| request.matches(STEER).count() > 1)
        {
            violations.push(format!("a request holds the steer twice: {twice}"));
        }
        let (bound, applications, rows) = evidence;
        if bound.as_ref() != Some(&run()) {
            violations.push(format!(
                "the steer is bound to {bound:?}, not the running run"
            ));
        }
        if !matches!(
            applications.as_slice(),
            [application]
                if application.turn_id == run()
                    && application.checkpoint == Some(lash_core::CheckpointKind::AfterWork)
        ) {
            violations.push(format!(
                "the steer was not applied once, at the running turn's work checkpoint: {applications:?}"
            ));
        }
        if *rows != 1 {
            violations.push(format!("{rows} committed rows carry the steer's input id"));
        }
        violations
    }

    fn terminal_laws(
        seen: &[String],
        evidence: &(Option<TurnId>, Vec<lash_core::TurnInputApplication>, usize),
    ) -> Vec<String> {
        let mut violations = Vec::new();
        // The finish is the first run's answer: no request of that run
        // holds the steer, which only its follow-on run, after the answer,
        // is shown.
        if let Some(extended) = seen
            .iter()
            .find(|request| request.contains(STEER) && !request.contains(ANSWER))
        {
            violations.push(format!(
                "the terminal checkpoint extended its turn with the steer: {extended}"
            ));
        }
        if !seen.iter().any(|request| request.contains(STEER)) {
            violations.push("the withheld steer never ran".to_owned());
        }
        let (bound, applications, rows) = evidence;
        if bound.as_ref() != Some(&steer_run()) {
            violations.push(format!(
                "the withheld steer is bound to {bound:?}, not its own follow-on run"
            ));
        }
        if !matches!(
            applications.as_slice(),
            [application] if application.turn_id == steer_run() && application.checkpoint.is_none()
        ) {
            violations.push(format!(
                "the withheld steer was not applied once, opening its follow-on run: {applications:?}"
            ));
        }
        if *rows != 1 {
            violations.push(format!("{rows} committed rows carry the steer's input id"));
        }
        violations
    }

    /// The wake filed again, after the run: the session answers the batch
    /// it already holds, open or settled, and files nothing new.
    async fn refiled_wake(&self) -> Result<lash::persistence::QueuedWorkEnqueueOutcome, String> {
        self.backend()
            .session_store_factory()
            .enqueue_queued_work_with_outcome(wake())
            .await
            .map_err(|error| format!("file the wake again: {error}"))
    }

    /// The run a wake that no running turn took opens: named by its batch.
    async fn wake_run(&self) -> Option<TurnId> {
        let refiled = self.refiled_wake().await.ok()?;
        TurnId::parse(refiled.batch().batch_id.as_str()).ok()
    }

    /// The wake was filed once and delivered by a committed turn, and no
    /// request of the session saw it twice. `delivered` is where the
    /// scenario says it arrives: a request of the running turn after its
    /// first round, or only a request after the first run's answer.
    fn wake_laws(
        arrival: Arrival,
        seen: &[String],
        refiled: &lash::persistence::QueuedWorkEnqueueOutcome,
    ) -> Vec<String> {
        let mut violations = Vec::new();
        if arrival.at_work() {
            if let Some(unwoken) = seen
                .iter()
                .find(|request| request.contains(FIRST_ROUND) && !request.contains(WAKE))
            {
                violations.push(format!(
                    "the work checkpoint did not deliver the wake to the next request: {unwoken}"
                ));
            }
        } else if let Some(extended) = seen
            .iter()
            .find(|request| request.contains(WAKE) && !request.contains(ANSWER))
        {
            violations.push(format!(
                "a wake after the last checkpoint reached its turn: {extended}"
            ));
        }
        if !seen.iter().any(|request| request.contains(WAKE)) {
            violations.push("the wake never reached a request".to_owned());
        }
        if let Some(twice) = seen
            .iter()
            .find(|request| request.matches(WAKE).count() > 1)
        {
            violations.push(format!("a request holds the wake twice: {twice}"));
        }
        // Delivered with the steer, the wake follows it: the steer was sent
        // first.
        if arrival.steers()
            && arrival.wakes()
            && let Some(reordered) = seen.iter().find(|request| {
                matches!(
                    (request.find(STEER), request.find(WAKE)),
                    (Some(steer), Some(wake)) if wake < steer
                )
            })
        {
            violations.push(format!(
                "the wake was delivered ahead of the earlier steer: {reordered}"
            ));
        }
        match refiled {
            lash::persistence::QueuedWorkEnqueueOutcome::Existing(wake)
                if wake.terminal.as_ref().map(|terminal| terminal.cause)
                    == Some(IngressTerminalCause::Delivered) => {}
            other => violations.push(format!(
                "the wake was not filed once and delivered: {other:?}"
            )),
        }
        violations
    }
}

#[async_trait::async_trait]
impl Scenario for Steering {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let (stores, database): (Arc<dyn StoreSet>, Arc<dyn DurableStore>) = dialect::open(
            self.dialect,
            self.postgres_url.as_deref(),
            clock,
            &self.keep,
        )
        .await;
        let backend = Backend::assemble(BackendParts {
            stores,
            settings: sim::settings(),
            engines: Vec::new(),
            providers: Arc::new(NoProjectionProviders),
            formats: lash::formats::actor_state_surfaces(),
        })
        .expect("the backend assembles");
        *self.backend.lock_recover() = Some(backend);
        database
    }

    fn config(&self) -> SimNodesConfig {
        SimNodesConfig {
            lease: LeaseConfig::default(),
            decodes: self.backend().formats().decodes(),
            max_active: 4,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        Arc::new(SessionActivation::new(
            self.backend(),
            lash::testing::session_turn_services(&self.core()),
            Arc::clone(&self.tripwire) as _,
        ))
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        self.send().await?;
        // A starts and claims first, B once A is settled, so the matrix
        // cuts the uncut run's writes by node.
        nodes.start("a");
        nodes.quiesce().await;
        nodes.start("b");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        vec![actor()]
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        matches!(
            nodes.database().actor(&actor()).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Idle
        )
    }

    async fn check(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let database = nodes.database();
        let trace = nodes.script().trace();
        let seen = std::mem::take(&mut *self.seen.lock_recover());
        let committed = |label: CommitLabel| {
            trace
                .iter()
                .filter(|write| write.point.label == label && write.committed())
                .count()
        };

        let runs = match self.arrival {
            arrival if arrival.at_work() => vec![run()],
            Arrival::WakeAfterTheLastCheckpoint => match self.wake_run().await {
                Some(wake_run) => vec![run(), wake_run],
                None => {
                    violations.push("the session holds no wake".to_owned());
                    vec![run()]
                }
            },
            _ => vec![run(), steer_run()],
        };
        for answered in &runs {
            match database.turn_end(&session(), answered).await {
                Ok(Some(end)) if end.kind() == RunTerminalKind::Answered => {}
                other => violations.push(format!("run {answered} did not answer: {other:?}")),
            }
        }
        let admitted = committed(CommitLabel::TURN_ADMIT);
        let commits = committed(CommitLabel::TURN_COMMIT);
        if admitted != runs.len() || commits != runs.len() {
            violations.push(format!(
                "{admitted} turns were admitted and {commits} committed, not {}",
                runs.len()
            ));
        }

        if self.arrival.steers() {
            match self.steer_evidence().await {
                Ok(evidence) if self.arrival.at_work() => {
                    violations.extend(Self::checkpoint_laws(&seen, &evidence));
                }
                Ok(evidence) => violations.extend(Self::terminal_laws(&seen, &evidence)),
                Err(error) => violations.push(error),
            }
        }
        if self.arrival.wakes() {
            match self.refiled_wake().await {
                Ok(refiled) => violations.extend(Self::wake_laws(self.arrival, &seen, &refiled)),
                Err(error) => violations.push(error),
            }
        }

        // Nothing is left open, bound or mailed.
        match database.turn(&session()).await {
            Ok(None) => {}
            other => violations.push(format!("a turn did not end: {other:?}")),
        }
        match database.session_mailbox(&session()).await {
            Ok(mailbox)
                if mailbox.bound_run.is_none()
                    && mailbox.inputs.is_empty()
                    && mailbox.batches.is_empty() => {}
            other => violations.push(format!("the session's mail did not settle: {other:?}")),
        }

        if let Some(cut) = cut {
            violations.extend(zombie_laws(cut, &trace));
        }
        violations
    }
}

/// Once a zombie's actors moved, every owner write it attempts is refused
/// with `OwnershipLost`.
fn zombie_laws(cut: &Cut, trace: &[lash_durable_test::Write]) -> Vec<String> {
    let mut violations = Vec::new();
    if cut.fault != Fault::Zombie || cut.kind != WriteKind::Actor {
        return violations;
    }
    let Some(at) = trace.iter().position(|write| {
        write.node == cut.node && write.point == cut.point && write.cut == Some(cut.fault)
    }) else {
        return vec!["the zombie's cut write is not in the trace".to_owned()];
    };
    for write in trace[at..]
        .iter()
        .filter(|write| write.node == cut.node && write.kind == WriteKind::Actor)
    {
        match &write.stored {
            Stored::Refused(DurableError::OwnershipLost(_)) => {}
            other => violations.push(format!("zombie write {write} was {other:?}")),
        }
    }
    violations
}

/// The owner commits of the uncut run, in order.
fn uncut_labels(arrival: Arrival) -> Vec<CommitLabel> {
    match arrival {
        // Two rounds: the admission of what arrived commits with the
        // `round.present+model.start` after the first round.
        arrival if arrival.at_work() => vec![
            CommitLabel::TURN_ADMIT,
            CommitLabel::MODEL_START,
            CommitLabel::MODEL_DONE,
            CommitLabel::ROUND_OUTCOME,
            CommitLabel::ROUND_PRESENT_MODEL_START,
            CommitLabel::MODEL_DONE,
            CommitLabel::ROUND_OUTCOME,
            CommitLabel::ROUND_PRESENT_MODEL_START,
            CommitLabel::TURN_COMMIT,
            CommitLabel::SESSION_RELEASE,
        ],
        // The first run answers; what arrived is the next run.
        _ => vec![
            CommitLabel::TURN_ADMIT,
            CommitLabel::MODEL_START,
            CommitLabel::TURN_COMMIT,
            CommitLabel::TURN_ADMIT,
            CommitLabel::MODEL_START,
            CommitLabel::TURN_COMMIT,
            CommitLabel::SESSION_RELEASE,
        ],
    }
}

async fn uncut(arrival: Arrival) {
    let report = Matrix::new()
        .faults(&[])
        .run(|| Steering::new(arrival, Dialect::SqliteMemory, None))
        .await;
    report.assert_held();
    let labels: Vec<CommitLabel> = report
        .baseline
        .iter()
        .filter(|write| write.kind == WriteKind::Actor && write.committed())
        .map(|write| write.point.label)
        .collect();
    assert_eq!(labels, uncut_labels(arrival));
}

async fn prove(arrival: Arrival, dialect: Dialect, postgres_url: Option<String>) {
    let report = Matrix::new()
        .faults(&[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
        .run(|| Steering::new(arrival, dialect, postgres_url.clone()))
        .await;
    let labels: Vec<&str> = report.labels().iter().map(|label| label.as_str()).collect();
    eprintln!(
        "checkpoint mail {arrival:?} {dialect:?}: {} cells over {} labels ({})",
        report.cells.len(),
        labels.len(),
        labels.join(", ")
    );
    report.assert_held();
    for label in uncut_labels(arrival) {
        assert!(
            report.labels().contains(&label),
            "the matrix never cut {label}"
        );
    }
}

/// The uncut run: a steer sent during the first round reaches the model's
/// next request, is bound to the running run with the phase after the
/// checkpoint that delivered it, and commits as its own user row.
#[tokio::test]
async fn a_steer_sent_during_a_round_is_delivered_at_the_next_work_checkpoint() {
    uncut(Arrival::SteerAtWork).await;
}

/// The uncut run: a steer that arrives during the turn's last model call is
/// withheld from its finish and runs as the session's next run.
#[tokio::test]
async fn a_steer_at_the_terminal_checkpoint_is_withheld_and_runs_next() {
    uncut(Arrival::SteerAtTerminal).await;
}

/// On SQLite in memory: a steer delivered at a work checkpoint, killed at
/// every label, is delivered exactly once.
#[tokio::test]
async fn a_steer_at_a_work_checkpoint_killed_at_every_label_is_delivered_once_on_sqlite_memory() {
    prove(Arrival::SteerAtWork, Dialect::SqliteMemory, None).await;
}

/// On a SQLite file.
#[tokio::test]
async fn a_steer_at_a_work_checkpoint_killed_at_every_label_is_delivered_once_on_sqlite_file() {
    prove(Arrival::SteerAtWork, Dialect::SqliteFile, None).await;
}

/// On PostgreSQL.
#[tokio::test]
async fn a_steer_at_a_work_checkpoint_killed_at_every_label_is_delivered_once_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    prove(Arrival::SteerAtWork, Dialect::Postgres, Some(url)).await;
}

/// On SQLite in memory: a steer withheld at the terminal checkpoint, killed
/// at every label, runs exactly once, as the next run.
#[tokio::test]
async fn a_withheld_steer_killed_at_every_label_runs_once_next_on_sqlite_memory() {
    prove(Arrival::SteerAtTerminal, Dialect::SqliteMemory, None).await;
}

/// On a SQLite file.
#[tokio::test]
async fn a_withheld_steer_killed_at_every_label_runs_once_next_on_sqlite_file() {
    prove(Arrival::SteerAtTerminal, Dialect::SqliteFile, None).await;
}

/// On PostgreSQL.
#[tokio::test]
async fn a_withheld_steer_killed_at_every_label_runs_once_next_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    prove(Arrival::SteerAtTerminal, Dialect::Postgres, Some(url)).await;
}

/// The uncut run: a process wake filed during the first round reaches the
/// model's next request, delivered by the running turn: no run of its own
/// follows (FIG-5294).
#[tokio::test]
async fn a_process_wake_filed_during_a_round_is_delivered_at_the_next_work_checkpoint() {
    uncut(Arrival::WakeAtWork).await;
}

/// The uncut run: a process wake filed during the turn's last model call
/// finds no checkpoint left; it stays session mail and is the next run.
#[tokio::test]
async fn a_process_wake_after_the_last_work_checkpoint_runs_next() {
    uncut(Arrival::WakeAfterTheLastCheckpoint).await;
}

/// The uncut run: a steer and then a process wake, both sent during the
/// first round, reach the model's next request together, in ingress order.
#[tokio::test]
async fn a_steer_and_a_process_wake_are_delivered_at_one_work_checkpoint_in_ingress_order() {
    uncut(Arrival::SteerAndWakeAtWork).await;
}

/// On SQLite in memory: a process wake delivered at a work checkpoint,
/// killed at every label, is delivered exactly once.
#[tokio::test]
async fn a_wake_at_a_work_checkpoint_killed_at_every_label_is_delivered_once_on_sqlite_memory() {
    prove(Arrival::WakeAtWork, Dialect::SqliteMemory, None).await;
}

/// On a SQLite file.
#[tokio::test]
async fn a_wake_at_a_work_checkpoint_killed_at_every_label_is_delivered_once_on_sqlite_file() {
    prove(Arrival::WakeAtWork, Dialect::SqliteFile, None).await;
}

/// On PostgreSQL.
#[tokio::test]
async fn a_wake_at_a_work_checkpoint_killed_at_every_label_is_delivered_once_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    prove(Arrival::WakeAtWork, Dialect::Postgres, Some(url)).await;
}
