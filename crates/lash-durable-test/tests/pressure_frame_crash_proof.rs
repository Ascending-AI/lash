//! A context-pressure frame on the production session activation (FIG-4110,
//! ADR 0101 §3).
//!
//! A lash core's session holds two sent inputs, and its sessions load the
//! standard compaction plugin. The first turn's model call overflows the
//! context window, so the turn stops on it and the plugin's after-turn
//! callback records a pending recovery with the turn's commit. Preparing the
//! second turn runs the plugin's context-pressure hook: it summarizes the
//! history in one direct completion and opens a recovery frame seeded with
//! the summary, which commits on its own before the turn runs in it. The
//! nodes are simulated (A and B) over the production durable store.
//!
//! The matrix cuts the uncut run at every labelled write, under
//! fail-before, ack-hidden, zombie, abort and commit-then-abort, recovers on
//! the other node, and checks:
//!
//! - the pressure frame opened once: the session's whole ancestry holds two
//!   frames, and the recovery frame holds one summary seed;
//! - the second turn ran on the recovery frame and answered there: no model
//!   call of it saw the first frame's input;
//! - two turns were admitted and two committed, nothing is open, bound or
//!   mailed;
//! - a zombie's writes after its reap are refused.
//!
//! The pressure hook reads what the session's last turn committed as its
//! prompt usage. A second matrix (FIG-5352) cuts an RLM turn whose one model
//! call reports a prompt past the compaction threshold at every label, and
//! checks the turn commits that call's usage however its owners resumed it:
//! a pass that resumes it after its committed `model.done` makes no model
//! call of its own.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/dialect.rs"]
mod dialect;
#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

#[path = "support/matrix.rs"]
mod matrix;

use matrix::MatrixTestExt as _;

use std::collections::BTreeSet;
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{LlmRequest, LlmResponse, LlmStreamEvent, StreamBlockIdentity};
use lash_core::runtime::durable::session::SessionActivation;
use lash_core::{LlmOutputPart, LlmTerminalReason};
use lash_core_execution::store::{HistoryAnchor, HistoryBudget};
use lash_core_execution::{Backend, BackendParts, NoProjectionProviders, StoreSet};
use lash_core_store::store::{RunCommittedOutcome, RunTerminalCause, RunTerminalKind};
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableError, DurableStore};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire, WriteKind,
};
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{SessionId, TurnId};

use dialect::Dialect;

const SESSION: &str = "pressure-frame-session";
const FIRST_RUN: &str = "pressure-frame-overflow";
const SECOND_RUN: &str = "pressure-frame-recovered";
const MODEL: &str = "pressure-frame-model";
/// The first frame's input, whose turn overflows.
const ASK: &str = "read the whole repository";
/// The input of the turn the recovery frame runs.
const NEXT: &str = "now answer briefly";
/// The summary the recovery summarizer answers.
const SUMMARY: &str = "the user asked to read the repository";
/// What the second turn answers.
const FINAL: &str = "a brief answer";
/// What marks the compaction summarizer's request.
const SUMMARIZER: &str = "Provide a detailed summary of the conversation above";

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

fn turn(id: &str) -> TurnId {
    TurnId::try_from(id.to_owned()).unwrap()
}

fn actor() -> ActorKey {
    ActorKey::session(SESSION).unwrap()
}

/// A text answer, streamed as one delta when the request streams.
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

/// The scripted model: the summarizer's request gets the summary, a request
/// holding the second input its answer, and any other request overflows.
/// Every turn request it saw is rendered into `seen`.
fn model(seen: Arc<Mutex<Vec<String>>>) -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("pressure-frame-scripted")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| {
            let seen = Arc::clone(&seen);
            async move {
                let rendered = serde_json::to_string(&request.messages).expect("a request encodes");
                if rendered.contains(SUMMARIZER) {
                    return Ok(text(&request, SUMMARY));
                }
                seen.lock_recover().push(rendered.clone());
                if rendered.contains(NEXT) {
                    return Ok(text(&request, FINAL));
                }
                Ok(LlmResponse {
                    terminal_reason: LlmTerminalReason::ContextOverflow,
                    terminal_diagnostic: Some("context window exceeded".to_owned()),
                    ..LlmResponse::default()
                })
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
struct PressureFrame {
    dialect: Dialect,
    postgres_url: Option<String>,
    seen: Arc<Mutex<Vec<String>>>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<Backend>>,
    core: Mutex<Option<lash::LashCore>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl PressureFrame {
    fn new(dialect: Dialect, postgres_url: Option<String>) -> Self {
        Self {
            dialect,
            postgres_url,
            seen: Arc::default(),
            tripwire: Arc::default(),
            backend: Mutex::default(),
            core: Mutex::default(),
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
        let backend = self.backend();
        self.core
            .lock_recover()
            .get_or_insert_with(|| {
                lash::LashCore::standard_builder(backend)
                    .serve_sessions(false)
                    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .serve_test_llm_profile(model(Arc::clone(&self.seen)), metadata())
                    .plugin(Arc::new(
                        lash_plugin_standard_compaction::StandardCompactionPluginFactory::default(),
                    ))
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(
                        "pressure-frame-deployment",
                        "pressure-frame-boot",
                    ))
                    .expect("the core builds")
            })
            .clone()
    }

    /// Create the session and send it both inputs through the core, uncut:
    /// the host is outside the deployment under test.
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
        for (input, run) in [(ASK, FIRST_RUN), (NEXT, SECOND_RUN)] {
            session
                .send(lash::TurnInput::text(input))
                .id(turn(run))
                .await
                .map(drop)
                .map_err(|error| format!("send {input}: {error}"))?;
        }
        Ok(())
    }

    /// The frame of every node in the session's ancestry, and the recovery
    /// frame's summary seeds.
    async fn frames(&self) -> Result<(BTreeSet<String>, usize), String> {
        let store = self.backend().session_store_factory();
        let page = store
            .load_ancestors(
                &session(),
                HistoryAnchor::Head,
                HistoryBudget {
                    max_nodes: NonZeroU32::new(1024).unwrap(),
                    max_bytes: NonZeroU64::new(u64::MAX).unwrap(),
                },
            )
            .await
            .map_err(|error| format!("read the session's ancestry: {error}"))?;
        if page.next.is_some() {
            return Err("the session's ancestry did not fit one page".to_owned());
        }
        let frames = page
            .nodes
            .iter()
            .map(|node| node.frame_node_id.as_str().to_owned())
            .collect();
        let summaries = page
            .nodes
            .iter()
            .filter(|node| {
                serde_json::to_string(&node.record).is_ok_and(|body| body.contains(SUMMARY))
            })
            .count();
        Ok((frames, summaries))
    }
}

#[async_trait::async_trait]
impl Scenario for PressureFrame {
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
            lease: Matrix::test_lease(),
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

        // The first turn ended; the second answered in prose.
        match database.turn_end(&session(), &turn(FIRST_RUN)).await {
            Ok(Some(_)) => {}
            other => violations.push(format!("the overflowing turn did not end: {other:?}")),
        }
        match database.turn_end(&session(), &turn(SECOND_RUN)).await {
            Ok(Some(end))
                if end.kind() == RunTerminalKind::Answered
                    && matches!(
                        &end.cause,
                        RunTerminalCause::Committed {
                            outcome: RunCommittedOutcome::Finished(_),
                            ..
                        }
                    ) => {}
            other => violations.push(format!(
                "the recovered turn did not answer in prose: {other:?}"
            )),
        }
        let admitted = committed(CommitLabel::TURN_ADMIT);
        let commits = committed(CommitLabel::TURN_COMMIT);
        if admitted != 2 || commits != 2 {
            violations.push(format!(
                "{admitted} turns were admitted and {commits} committed, not two"
            ));
        }

        // The pressure frame opened once, seeded with one summary.
        match self.frames().await {
            Ok((frames, 1)) if frames.len() == 2 => {}
            other => violations.push(format!(
                "the ancestry does not hold two frames and one summary: {other:?}"
            )),
        }

        // The second turn ran on the recovery frame.
        let recovered: Vec<&String> = seen.iter().filter(|seen| seen.contains(NEXT)).collect();
        if recovered.is_empty() {
            violations.push("the model never saw the second input".to_owned());
        }
        if let Some(request) = recovered.iter().find(|request| request.contains(ASK)) {
            violations.push(format!(
                "the second turn ran on the first frame, which holds its input: {request}"
            ));
        }
        if !recovered.iter().all(|request| request.contains(SUMMARY)) {
            violations.push("the second turn ran without the recovery frame's seed".to_owned());
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

/// The owner commits the uncut run makes, in this order among others: the
/// overflowing turn admits, calls the model and commits; the recovered turn
/// admits, summarizes in a direct completion, calls the model on the
/// recovery frame and commits.
fn ordered_labels() -> Vec<CommitLabel> {
    vec![
        CommitLabel::TURN_ADMIT,
        CommitLabel::MODEL_START,
        CommitLabel::TURN_COMMIT,
        CommitLabel::TURN_ADMIT,
        CommitLabel::COMPLETION_START,
        CommitLabel::MODEL_START,
        CommitLabel::TURN_COMMIT,
    ]
}

async fn prove(dialect: Dialect, postgres_url: Option<String>) {
    let report = Matrix::new()
        .faults(&[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
        .run_test(|| PressureFrame::new(dialect, postgres_url.clone()))
        .await;
    let labels: Vec<&str> = report.labels().iter().map(|label| label.as_str()).collect();
    eprintln!(
        "pressure frame {dialect:?}: {} cells over {} labels ({})",
        report.cells.len(),
        labels.len(),
        labels.join(", ")
    );
    report.assert_held();
    for label in ordered_labels() {
        assert!(
            report.labels().contains(&label),
            "the matrix never cut {label}"
        );
    }
}

/// The uncut run: the overflowing turn commits its pending recovery, and
/// the next turn's preparation opens the recovery frame it runs in.
#[tokio::test]
#[ignore = "FIG-5355: a context-pressure frame's commit is refused while its run owns the head"]
async fn a_pressure_frame_opens_before_the_turn_that_runs_in_it() {
    let report = Matrix::new()
        .faults(&[])
        .run_test(|| PressureFrame::new(Dialect::SqliteMemory, None))
        .await;
    report.assert_held();
    let labels: Vec<CommitLabel> = report
        .baseline
        .iter()
        .filter(|write| write.kind == WriteKind::Actor && write.committed())
        .map(|write| write.point.label)
        .collect();
    let mut expected = ordered_labels().into_iter().peekable();
    for label in labels {
        if expected.peek() == Some(&label) {
            expected.next();
        }
    }
    assert_eq!(
        expected.collect::<Vec<_>>(),
        Vec::<CommitLabel>::new(),
        "the uncut run made every expected owner commit, in order"
    );
}

/// On SQLite in memory: a pressure frame killed at every label opens once,
/// and the turn runs in it.
#[tokio::test]
#[ignore = "FIG-5355: a context-pressure frame's commit is refused while its run owns the head"]
async fn a_pressure_frame_killed_at_every_label_opens_once_on_sqlite_memory() {
    prove(Dialect::SqliteMemory, None).await;
}

/// On a SQLite file.
#[tokio::test]
#[ignore = "FIG-5355: a context-pressure frame's commit is refused while its run owns the head"]
async fn a_pressure_frame_killed_at_every_label_opens_once_on_sqlite_file() {
    prove(Dialect::SqliteFile, None).await;
}

/// On PostgreSQL.
#[tokio::test]
#[ignore = "FIG-5355: a context-pressure frame's commit is refused while its run owns the head"]
async fn a_pressure_frame_killed_at_every_label_opens_once_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    prove(Dialect::Postgres, Some(url)).await;
}

const USAGE_SESSION: &str = "prompt-usage-session";
const USAGE_RUN: &str = "prompt-usage-turn";
/// The prompt the usage turn's one model call reports: past the 180,000
/// tokens of the 200,000-token window at which the next turn compacts.
const PROMPT_TOKENS: i64 = 190_000;

/// FIG-5352: an RLM turn whose one model call reports [`PROMPT_TOKENS`] and
/// answers a cell that finishes the turn, fresh for every matrix cell.
struct PromptUsage {
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<Backend>>,
    core: Mutex<Option<lash::LashCore>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl PromptUsage {
    fn new() -> Self {
        Self {
            tripwire: Arc::default(),
            backend: Mutex::default(),
            core: Mutex::default(),
            keep: Mutex::default(),
        }
    }

    fn backend(&self) -> Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }

    /// The deployment's RLM core; the simulated nodes run its turns.
    fn core(&self) -> lash::LashCore {
        let backend = self.backend();
        self.core
            .lock_recover()
            .get_or_insert_with(|| {
                let model = lash_core::testing::TestProvider::builder()
                    .kind("prompt-usage-scripted")
                    .complete(|_request: LlmRequest| async {
                        let mut cell = served::cell("finish(\"done\");");
                        cell.usage = lash_core::llm::types::LlmUsage {
                            input_tokens: PROMPT_TOKENS,
                            ..Default::default()
                        };
                        Ok(cell)
                    })
                    .build()
                    .into_handle();
                lash::LashCore::rlm_builder(
                    backend.clone(),
                    served::rlm(&backend, None, sim::untimed_workers()),
                )
                .serve_sessions(false)
                .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
                .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                .serve_test_llm_profile(model, metadata())
                .build(lash::persistence::LeaseOwnerIdentity::opaque(
                    "prompt-usage-deployment",
                    "prompt-usage-boot",
                ))
                .expect("the core builds")
            })
            .clone()
    }
}

#[async_trait::async_trait]
impl Scenario for PromptUsage {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let (stores, database): (Arc<dyn StoreSet>, Arc<dyn DurableStore>) =
            dialect::open(Dialect::SqliteMemory, None, clock, &self.keep).await;
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
            lease: Matrix::test_lease(),
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
        let session = self
            .core()
            .session(usage_session())
            .create(lash::SessionCreation::root(lash::SessionSpec::new(
                MODEL,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(64),
            )))
            .await
            .map_err(|error| format!("create the session: {error}"))?;
        session
            .send(lash::TurnInput::text(ASK))
            .id(turn(USAGE_RUN))
            .await
            .map(drop)
            .map_err(|error| format!("send the turn: {error}"))?;
        nodes.start("a");
        nodes.quiesce().await;
        nodes.start("b");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        vec![ActorKey::session(USAGE_SESSION).unwrap()]
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        matches!(
            nodes.database().actor(&ActorKey::session(USAGE_SESSION).unwrap()).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Idle
        )
    }

    async fn check(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        match nodes
            .database()
            .turn_end(&usage_session(), &turn(USAGE_RUN))
            .await
        {
            Ok(Some(end)) if end.cause.kind() == RunTerminalKind::Answered => {}
            other => violations.push(format!("the turn did not complete: {other:?}")),
        }
        let committed = match self.core().session(usage_session()).open().await {
            Ok(session) => session.admin().state().export().await.last_prompt_usage,
            Err(error) => {
                violations.push(format!("the session does not open: {error}"));
                return violations;
            }
        };
        if committed.as_ref().map(|usage| usage.input_tokens) != Some(PROMPT_TOKENS) {
            violations.push(format!(
                "the turn committed prompt usage {committed:?}, not its call's {PROMPT_TOKENS} tokens"
            ));
        }
        if let Some(cut) = cut {
            violations.extend(zombie_laws(cut, &nodes.script().trace()));
        }
        violations
    }
}

fn usage_session() -> SessionId {
    SessionId::try_from(USAGE_SESSION.to_owned()).unwrap()
}

/// FIG-5352: an RLM turn whose model call reports a prompt past the
/// compaction threshold, cut at every label, commits that call's prompt
/// usage, so the next turn's pressure hook compacts: a pass that resumes the
/// turn after its committed `model.done` keeps the usage its checkpoint
/// carries.
#[tokio::test]
async fn a_turn_killed_at_every_label_commits_its_last_calls_prompt_usage() {
    let report = Matrix::new()
        .faults(&[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
        .run_test(PromptUsage::new)
        .await;
    report.assert_held();
    for label in [CommitLabel::MODEL_DONE, CommitLabel::TURN_COMMIT] {
        assert!(
            report.labels().contains(&label),
            "the matrix never cut {label}"
        );
    }
}
