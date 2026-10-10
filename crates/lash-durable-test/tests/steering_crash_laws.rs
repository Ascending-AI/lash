//! Steering input on the production session activation
//! (FIG-5293, FIG-5294, FIG-5800, ADR 0101 §5, ADR 0132 §4).
//!
//! A lash core's session holds one sent input. While its turn runs, a host
//! sends the session a steering input addressed to that turn
//! (`TurnInputIngress::ActiveTurn`). The nodes are simulated (A and B) over the
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
//! - **At a completion candidate:** the turn's first round is a control
//!   call whose body sends the steer before its finish settles. The
//!   completion checkpoint that decides the candidate delivers it: the
//!   candidate is superseded, the turn goes on into a new iteration whose
//!   model reads the steer, and its second finish is the run's one answer
//!   (FIG-5800). The two finishes are calls of two finish tools that
//!   declare different value schemas: the answer is checked against its
//!   own tool's, and the run's committed outcome records that schema, not
//!   the superseded finish's (FIG-5823).
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

use lash_sansio::llm::types::{StreamBlockEvent, StreamBlockKind};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{LlmRequest, LlmResponse, LlmStreamEvent, StreamBlockIdentity};
use lash_core::runtime::durable::session::SessionActivation;
use lash_core::{LlmOutputPart, ToolCall, ToolOutcome};
use lash_core_execution::{Backend, BackendParts, NoProjectionProviders, StoreSet};
use lash_core_store::store::RunTerminalKind;
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
/// The control tool of the completion scenario: it finishes with its label.
const FINISH_TOOL: &str = "finish_round";
/// The control tool the completion scenario's model finishes with once it
/// read the steer.
const FINISH_STEERED: &str = "finish_steered";
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
    Work,
    /// A steer, while the turn's last model call runs: the terminal
    /// checkpoint withholds it.
    Terminal,
    /// A steer, while the first round's control call runs: the completion
    /// checkpoint deciding its candidate delivers it, and the candidate is
    /// superseded.
    Candidate,
}

impl Arrival {
    async fn send(self, core: &OnceLock<lash::LashCore>) {
        let boundary = match self {
            Self::Candidate => lash::persistence::TurnInputCheckpointBoundary::BeforeCompletion,
            Self::Work | Self::Terminal => {
                lash::persistence::TurnInputCheckpointBoundary::AfterWork
            }
        };
        send_steer(core, boundary).await;
    }

    fn at_work(self) -> bool {
        matches!(self, Self::Work)
    }

    /// What it sends, as the turn the session runs sees it.
    fn marker(self) -> &'static str {
        STEER
    }
}

/// Send the steer through `core`, addressed to the running turn. Every send
/// is the same submission under its host id, so a body or call that runs
/// again after a cut accepts it once.
async fn send_steer(
    core: &OnceLock<lash::LashCore>,
    boundary: lash::persistence::TurnInputCheckpointBoundary,
) {
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
            boundary,
        ))
        .await
        .expect("the running turn accepts its steer");
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
    .with_execution(std::time::Duration::from_secs(120))
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

/// `finish_round`'s body: the first round's call sends the steer, then
/// every call finishes the turn with its label.
struct FinishRound(Arc<OnceLock<lash::LashCore>>, Arrival);

#[async_trait::async_trait]
impl StaticToolExecute for FinishRound {
    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let label = call.args["label"].as_str().unwrap_or_default().to_owned();
        if label == FIRST_ROUND {
            self.1.send(&self.0).await;
        }
        ToolOutcome::finish(serde_json::json!(label)).into()
    }
}

/// The value schema of the finish tool `tool`: the one label its call may
/// finish with.
fn finish_value_schema(tool: &str) -> serde_json::Value {
    serde_json::json!({
        "const": if tool == FINISH_TOOL { FIRST_ROUND } else { SECOND_ROUND }
    })
}

fn finish_definition(tool: &str) -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::control(
        tool,
        tool,
        "Finishes the turn with its label; the first round's call sends the steer.",
        serde_json::json!({ "type": "object", "additionalProperties": true }),
        lash_core::TurnControls::finish(
            lash_core::JsonSchema::admit(finish_value_schema(tool))
                .expect("a finish tool's value schema"),
        ),
    )
    .expect("a finish tool's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    // A call a kill interrupted runs again at its ordinal, so the first
    // round's body sends the steer whatever was cut.
    .with_execution_policy(lash_core::ExecutionPolicy::repeatable(
        std::num::NonZeroU32::new(3).expect("a nonzero attempt bound"),
        1,
        1,
    ))
}

fn finish_round(
    core: Arc<OnceLock<lash::LashCore>>,
    arrival: Arrival,
) -> Arc<dyn lash_core::ToolProvider> {
    Arc::new(StaticToolProvider::new(
        vec![
            finish_definition(FINISH_TOOL),
            finish_definition(FINISH_STEERED),
        ],
        FinishRound(core, arrival),
    ))
}

fn text(request: &LlmRequest, text: &str) -> LlmResponse {
    if let Some(stream) = request.stream_events.as_ref() {
        stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
            kind: StreamBlockKind::AssistantText,
            block: StreamBlockIdentity::new("text:0", 0),
            text: text.to_owned(),
        }));
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
    call_tool(TOOL, call, label)
}

fn call_tool(tool: &str, call: &str, label: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::ToolCall {
            call_id: call.to_owned(),
            tool_name: tool.to_owned(),
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
                let delivered = rendered.contains(STEER);
                let response = if matches!(arrival, Arrival::Candidate) {
                    // The finish the steer supersedes, then the one that
                    // answers once the model has read it.
                    if delivered {
                        call_tool(FINISH_STEERED, "finish-call-2", SECOND_ROUND)
                    } else {
                        call_tool(FINISH_TOOL, "finish-call-1", FIRST_ROUND)
                    }
                } else if arrival.at_work() {
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
        .cache_retention(lash_core::provider::CacheRetention::Short)
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
                    .data_retention(lash::DataRetention::standard())
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
                    .execution_budgets(lash::ExecutionBudgets::recommended())
                    .delta_coalescing(lash::DeltaCoalescing::recommended())
                    .serve_test_llm_profile(
                        model(self.arrival, Arc::clone(&self.core), Arc::clone(&self.seen)),
                        metadata(),
                    )
                    .tools(match self.arrival {
                        Arrival::Candidate => finish_round(Arc::clone(&self.core), self.arrival),
                        Arrival::Work | Arrival::Terminal => {
                            steer_round(Arc::clone(&self.core), self.arrival)
                        }
                    })
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(
                        lash::persistence::LeaseOwnerId::new("steering-deployment"),
                        lash::persistence::LeaseIncarnationId::new("steering-boot"),
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
            .create(lash::SessionCreation::root(
                lash::plugins::SessionToolAccess::ambient(),
                lash::SessionSpec::new(
                    MODEL,
                    lash::TurnBudget::Unbounded,
                    lash::MaxToolCalls::new(64),
                )
                .no_progress_budget(lash_core::NoProgressBudget::bounded(12)),
            ))
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
            .into_entries()
            .into_iter()
            .filter(|entry| entry.provenance.input_id.as_ref() == Some(&steer_input()))
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

    fn candidate_laws(
        seen: &[String],
        evidence: &(Option<TurnId>, Vec<lash_core::TurnInputApplication>, usize),
    ) -> Vec<String> {
        let mut violations = Vec::new();
        // The steer reaches the model in the running turn, after the finish
        // it superseded: no request holds it without that first finish.
        if let Some(apart) = seen
            .iter()
            .find(|request| request.contains(STEER) && !request.contains(FIRST_ROUND))
        {
            violations.push(format!(
                "the steer did not reach the turn whose finish it supersedes: {apart}"
            ));
        }
        if !seen.iter().any(|request| request.contains(STEER)) {
            violations.push("the completion checkpoint never delivered the steer".to_owned());
        }
        // The model reads that its finish did not end the turn.
        if let Some(unexplained) = seen
            .iter()
            .find(|request| request.contains(STEER) && !request.contains("was superseded"))
        {
            violations.push(format!(
                "the request after the steer does not say the finish was superseded: {unexplained}"
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
                    && application.checkpoint
                        == Some(lash_core::CheckpointKind::BeforeCompletion)
        ) {
            violations.push(format!(
                "the steer was not applied once, at the running turn's completion checkpoint: {applications:?}"
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
            Arrival::Work | Arrival::Candidate => vec![run()],
            Arrival::Terminal => vec![run(), steer_run()],
        };
        for answered in &runs {
            match database.turn_end(&session(), answered).await {
                Ok(Some(end)) if end.kind() == RunTerminalKind::Answered => {
                    if matches!(self.arrival, Arrival::Candidate) {
                        violations.extend(second_finish_laws(&end.cause));
                    }
                }
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

        {
            match self.steer_evidence().await {
                Ok(evidence) => violations.extend(match self.arrival {
                    Arrival::Work => Self::checkpoint_laws(&seen, &evidence),
                    Arrival::Terminal => Self::terminal_laws(&seen, &evidence),
                    Arrival::Candidate => Self::candidate_laws(&seen, &evidence),
                }),
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

/// The run answers the finish of the iteration the steer opened, under its
/// own tool's value schema: the one it superseded is never its outcome,
/// nor its schema the answer's.
fn second_finish_laws(cause: &lash_core_store::store::RunTerminalCause) -> Vec<String> {
    match cause {
        lash_core_store::store::RunTerminalCause::Committed {
            outcome:
                lash_core_store::store::RunCommittedOutcome::Finished {
                    finish: lash_sansio::TurnFinish::Finished { tool_name, value },
                    value_schema: Some(schema),
                },
            ..
        } if tool_name == FINISH_STEERED
            && *value == serde_json::json!(SECOND_ROUND)
            && schema.as_value() == &finish_value_schema(FINISH_STEERED) =>
        {
            Vec::new()
        }
        other => vec![format!(
            "the run did not answer the finish after the steer: {other:?}"
        )],
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
        // The first finish's round, then the iteration the steer opened:
        // its delivery commits with that iteration's `model.start`.
        Arrival::Candidate => vec![
            CommitLabel::TURN_ADMIT,
            CommitLabel::MODEL_START,
            CommitLabel::MODEL_DONE,
            CommitLabel::ROUND_OUTCOME,
            CommitLabel::ROUND_PRESENT_MODEL_START,
            CommitLabel::MODEL_DONE,
            CommitLabel::ROUND_OUTCOME,
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
    report.assert_baseline_labels(&uncut_labels(arrival));
    for label in uncut_labels(arrival) {
        assert!(
            report.labels().contains(&label),
            "the matrix never cut {label}"
        );
    }
}

/// On SQLite in memory: a steer delivered at a work checkpoint, killed at
/// every label, is delivered exactly once.
#[tokio::test]
async fn a_steer_at_a_work_checkpoint_killed_at_every_label_is_delivered_once_on_sqlite_memory() {
    prove(Arrival::Work, Dialect::SqliteMemory, None).await;
}

/// On a SQLite file.
#[tokio::test]
async fn a_steer_at_a_work_checkpoint_killed_at_every_label_is_delivered_once_on_sqlite_file() {
    prove(Arrival::Work, Dialect::SqliteFile, None).await;
}

/// On PostgreSQL.
#[tokio::test]
async fn a_steer_at_a_work_checkpoint_killed_at_every_label_is_delivered_once_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    prove(Arrival::Work, Dialect::Postgres, Some(url)).await;
}

/// On SQLite in memory: a steer withheld at the terminal checkpoint, killed
/// at every label, runs exactly once, as the next run.
#[tokio::test]
async fn a_withheld_steer_killed_at_every_label_runs_once_next_on_sqlite_memory() {
    prove(Arrival::Terminal, Dialect::SqliteMemory, None).await;
}

/// On a SQLite file.
#[tokio::test]
async fn a_withheld_steer_killed_at_every_label_runs_once_next_on_sqlite_file() {
    prove(Arrival::Terminal, Dialect::SqliteFile, None).await;
}

/// On PostgreSQL.
#[tokio::test]
async fn a_withheld_steer_killed_at_every_label_runs_once_next_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    prove(Arrival::Terminal, Dialect::Postgres, Some(url)).await;
}

/// Uncut, on SQLite in memory: a steer queued while the turn's finish
/// settles supersedes it at the completion checkpoint, and the turn goes on
/// to answer with its next finish (FIG-5800).
#[tokio::test]
async fn a_steer_at_a_completion_candidate_supersedes_it_and_the_turn_goes_on_on_sqlite_memory() {
    let report = Matrix::new()
        .faults(&[])
        .horizon(Duration::from_secs(600))
        .run(|| Steering::new(Arrival::Candidate, Dialect::SqliteMemory, None))
        .await;
    report.assert_held();
    report.assert_baseline_labels(&uncut_labels(Arrival::Candidate));
}

/// On SQLite in memory: the supersession killed at every label, the
/// checkpoint's delivery among them, ends the run once, with the finish
/// after the steer.
#[tokio::test]
async fn a_steer_at_a_completion_candidate_killed_at_every_label_supersedes_once_on_sqlite_memory()
{
    prove(Arrival::Candidate, Dialect::SqliteMemory, None).await;
}
