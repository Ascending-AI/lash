//! A context-pressure frame on the production session activation (FIG-4110,
//! ADR 0101 §3).
//!
//! A lash core's session holds two sent inputs, and its sessions load the
//! standard compaction plugin. The first turn's model call overflows the
//! context window, so the turn stops on it and the plugin's after-turn
//! callback records a pending recovery with the turn's commit. Preparing the
//! second turn runs the plugin's context-pressure hook: it summarizes the
//! history in one direct completion and opens a recovery frame seeded with
//! the summary, which commits as the session actor's own `pressure.frame`
//! before the turn runs in it (FIG-5355). The nodes are simulated (A and B)
//! over the production durable store.
//!
//! The matrix cuts the uncut run at every labelled write, under
//! fail-before, ack-hidden, zombie, abort and commit-then-abort, recovers on
//! the other node, and checks:
//!
//! - the pressure frame opened once: one `pressure.frame` commit landed, the
//!   session's whole ancestry holds two frames, and the recovery frame holds
//!   one summary seed;
//! - the second turn ran on the recovery frame and answered there: no model
//!   call of it saw the first frame's input;
//! - two turns were admitted and two committed, nothing is open, bound or
//!   mailed;
//! - a zombie's writes after its reap are refused;
//! - every send of the summary, the first and each resend after a cut past
//!   `completion.start`, carried one body and was read as one request, the
//!   one admitted with the summary instruction (FIG-5479).
//!
//! The pressure hook reads what the session's last turn committed as its
//! prompt usage. A second matrix (FIG-5352) cuts an RLM turn whose one model
//! call reports a prompt past the compaction threshold at every label, and
//! checks the turn commits that call's prompt and total usage however its
//! owners resumed it (FIG-5352, FIG-5389):
//! a pass that resumes it after its committed `model.done` makes no model
//! call of its own.
//!
//! The RLM legs (FIG-5355) run the protocol with a live interpreter.
//!
//! - **Globals:** the first turn's cell sets a session global, and its model
//!   call reports a prompt usage over the compaction threshold, so preparing
//!   the second turn opens a compaction frame, the plugin's other decision.
//!   The second turn runs in it, and its cell finds the global gone: the
//!   live interpreter restarted from the frame's seed. With no pressure the
//!   same cell finds the global kept, so the law is about the frame.
//! - **Pressure then `continue_as`:** the first turn's model call also
//!   reports a usage over the threshold, so the second turn runs in a
//!   compaction frame. Its cell switches frames with `control.continue_as`,
//!   again over the threshold, and the follow-on runs the task in the third
//!   frame. Cut at every label, the session commits both frames,
//!   in order, each once: the summary seeds the second frame, one
//!   `pressure.frame` commit landed, and the follow-on never compacts again
//!   on the switching turn's usage.

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
/// A prompt usage over the 200 000-token window's compaction threshold.
const OVER_THRESHOLD: i64 = 190_000;
/// The session global the RLM globals leg sets.
const GLOBAL: &str = "pressureLawGlobal";
/// The task the RLM `continue_as` leg switches frames to.
const TASK: &str = "finish the briefing in a fresh frame";

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

/// What a scenario's session runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Script {
    /// The standard protocol: the first turn overflows, and the recovered
    /// turn answers in prose.
    Overflow,
    /// RLM: the first turn's cell sets [`GLOBAL`], over the compaction
    /// threshold when `pressure`, and the second turn's cell finishes with
    /// the global's type.
    Globals { pressure: bool },
    /// RLM: the first turn finishes over the compaction threshold, the
    /// compacted turn's cell calls `control.continue_as` with [`TASK`], also
    /// over it, and the follow-on finishes.
    ContinueAs,
}

impl Script {
    fn rlm(self) -> bool {
        self != Self::Overflow
    }

    /// The scenario's answer to the turn request `rendered`.
    fn answer(self, request: &LlmRequest, rendered: &str) -> LlmResponse {
        let over = |mut response: LlmResponse| {
            response.usage.input_tokens = OVER_THRESHOLD;
            response
        };
        match self {
            Self::Overflow if rendered.contains(NEXT) => text(request, FINAL),
            Self::Globals { .. } if rendered.contains(NEXT) => text(
                request,
                &cell(&format!("finish(typeof globalThis.{GLOBAL});")),
            ),
            Self::Globals { pressure } => {
                let set = text(
                    request,
                    &cell(&format!(
                        "globalThis.{GLOBAL} = \"kept\";\nfinish(\"set\");"
                    )),
                );
                if pressure { over(set) } else { set }
            }
            Self::ContinueAs if rendered.contains(TASK) && !rendered.contains(NEXT) => {
                text(request, &cell("finish(\"done\");"))
            }
            Self::ContinueAs if rendered.contains(NEXT) => over(text(
                request,
                &cell(&format!("await control.continue_as({{ task: {TASK:?} }});")),
            )),
            Self::ContinueAs => over(text(request, &cell("finish(\"set\");"))),
            Self::Overflow => LlmResponse {
                terminal_reason: LlmTerminalReason::ContextOverflow,
                terminal_diagnostic: Some("context window exceeded".to_owned()),
                ..LlmResponse::default()
            },
        }
    }
}

/// An RLM cell of `code`.
fn cell(code: &str) -> String {
    format!("<typescript>\n{code}\n</typescript>")
}

/// One send of the summary as the model received it: the exact body, and
/// what the request it was handed asks (its instructions and messages).
#[derive(Clone, Debug, PartialEq, Eq)]
struct SummarySend {
    body: String,
    asked: String,
}

/// What `request` asks: its instructions and messages, rendered.
fn asked(request: &LlmRequest) -> String {
    serde_json::to_string(&(&request.instructions, &request.messages)).expect("a request encodes")
}

/// The scripted model: the summarizer's request gets the summary, and every
/// turn request `script`'s answer. It decides from the request it is handed,
/// as an in-process model does. Every turn request it saw is rendered into
/// `seen`, and every send of the summary into `summaries`.
fn model(
    script: Script,
    seen: Arc<Mutex<Vec<String>>>,
    summaries: Arc<Mutex<Vec<SummarySend>>>,
) -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("pressure-frame-scripted")
        .requires_streaming(true)
        .complete_with_wire(move |request: LlmRequest, body| {
            let seen = Arc::clone(&seen);
            let summaries = Arc::clone(&summaries);
            async move {
                let rendered = serde_json::to_string(&request.messages).expect("a request encodes");
                // A send of the summary is told by the body it carries, so
                // it is recorded however the model then reads its request.
                if body.contains(SUMMARIZER) {
                    summaries.lock_recover().push(SummarySend {
                        body,
                        asked: asked(&request),
                    });
                }
                if rendered.contains(SUMMARIZER) {
                    return Ok(text(&request, SUMMARY));
                }
                seen.lock_recover().push(rendered.clone());
                Ok(script.answer(&request, &rendered))
            }
        })
        .build()
        .into_handle()
}

/// RESEND (FIG-5479): every send of the summary carried the admitted body
/// and was read as the request admitted with the summary instruction. A
/// resend after a cut past `completion.start` is handed the recorded body
/// and no request beside it, so it cannot be read differently from the
/// first attempt. `first` is the uncut run's send, which every cell's sends
/// equal in what they ask.
fn summary_laws(sends: &[SummarySend], first: Option<&SummarySend>) -> Vec<String> {
    let mut violations = Vec::new();
    let Some(admitted) = sends.first() else {
        return vec!["the summary was never sent".to_owned()];
    };
    if !admitted.asked.contains(SUMMARIZER) {
        violations.push(format!(
            "the summary was read without its instruction: {}",
            admitted.asked
        ));
    }
    for (index, send) in sends.iter().enumerate().skip(1) {
        if send.body != admitted.body {
            violations.push(format!(
                "send {} of the summary carried another body than the first: {} vs {}",
                index + 1,
                send.body,
                admitted.body
            ));
        }
        if send.asked != admitted.asked {
            violations.push(format!(
                "send {} of the summary was read as another request than the first: {} vs {}",
                index + 1,
                send.asked,
                admitted.asked
            ));
        }
    }
    if let Some(first) = first
        && first.asked != admitted.asked
    {
        violations.push(format!(
            "the summary was read as another request than the uncut run's first attempt: {} vs {}",
            admitted.asked, first.asked
        ));
    }
    violations
}

fn metadata() -> lash_core::LlmProfileMetadata {
    lash_core::LlmProfileMetadata::builder(MODEL)
        .cache_retention(lash_core::provider::CacheRetention::Short)
        .context_window_tokens(200_000)
        .build()
        .expect("the model's metadata")
}

/// The scenario of one script on one dialect, fresh for every matrix cell.
struct PressureFrame {
    script: Script,
    dialect: Dialect,
    postgres_url: Option<String>,
    seen: Arc<Mutex<Vec<String>>>,
    /// Every send of the summary the model received.
    summaries: Arc<Mutex<Vec<SummarySend>>>,
    /// The uncut run's send of the summary, when the proof compares its
    /// cells with it: the uncut run sets it and every cut cell reads it.
    first_summary: Option<Arc<std::sync::OnceLock<SummarySend>>>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<Backend>>,
    /// The virtual clock the database was built on, which an RLM core's VM
    /// worker calls hold.
    clock: Mutex<Option<Arc<SimClock>>>,
    core: Mutex<Option<lash::LashCore>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl PressureFrame {
    fn of(script: Script, dialect: Dialect, postgres_url: Option<String>) -> Self {
        Self {
            script,
            dialect,
            postgres_url,
            seen: Arc::default(),
            summaries: Arc::default(),
            first_summary: None,
            tripwire: Arc::default(),
            backend: Mutex::default(),
            clock: Mutex::default(),
            core: Mutex::default(),
            keep: Mutex::default(),
        }
    }

    /// This scenario comparing its summary sends with the uncut run's.
    fn against(mut self, first: &Arc<std::sync::OnceLock<SummarySend>>) -> Self {
        self.first_summary = Some(Arc::clone(first));
        self
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
                let builder = if self.script.rlm() {
                    let clock = self
                        .clock
                        .lock_recover()
                        .clone()
                        .expect("the database is built first");
                    lash::LashCore::rlm_builder(
                        backend.clone(),
                        lash::rlm::RlmProtocolPluginFactory::new(
                            lash::rlm::RlmProtocolPluginConfig::builder()
                                .channel(lash::rlm::RlmChannel::Cell)
                                .instruction_limit(lash::rlm::InstructionBound::instructions(
                                    1_000_000,
                                ))
                                .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                                .build(),
                            Arc::new(lash::rlm::TypescriptDialect),
                            &backend,
                        )
                        .with_worker_service(sim::workers(&clock)),
                    )
                } else {
                    lash::LashCore::standard_builder(backend)
                };
                builder
                    .serve_sessions(false)
                    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
                    .data_retention(lash::DataRetention::standard())
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
                    .execution_budgets(lash::ExecutionBudgets::recommended())
                    .delta_coalescing(lash::DeltaCoalescing::recommended())
                    .serve_test_llm_profile(
                        model(
                            self.script,
                            Arc::clone(&self.seen),
                            Arc::clone(&self.summaries),
                        ),
                        metadata(),
                    )
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

    /// The frames of the session's ancestry, root first, and the frame of
    /// each summary seed in it.
    async fn frames(&self) -> Result<(Vec<String>, Vec<String>), String> {
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
        let mut nodes = page.nodes;
        nodes.sort_by_key(|node| node.generation);
        let mut frames = Vec::new();
        for node in &nodes {
            let frame = node.frame_node_id.as_str().to_owned();
            if !frames.contains(&frame) {
                frames.push(frame);
            }
        }
        let summaries = nodes
            .iter()
            .filter(|node| {
                serde_json::to_string(&node.record).is_ok_and(|body| body.contains(SUMMARY))
            })
            .map(|node| node.frame_node_id.as_str().to_owned())
            .collect();
        Ok((frames, summaries))
    }

    /// The session's turns ended, and nothing is left open, bound or
    /// mailed.
    async fn settled(&self, database: &Arc<dyn DurableStore>) -> Vec<String> {
        let mut violations = Vec::new();
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
        violations
    }
}

#[async_trait::async_trait]
impl Scenario for PressureFrame {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        *self.clock.lock_recover() = Some(Arc::clone(&clock));
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
        let database = nodes.database();
        let trace = nodes.script().trace();
        let seen = std::mem::take(&mut *self.seen.lock_recover());
        let committed = |label: CommitLabel| {
            trace
                .iter()
                .filter(|write| write.point.label == label && write.committed())
                .count()
        };
        let mut violations = match self.script {
            Script::Overflow => self.recovered(database, &seen).await,
            Script::Globals { pressure } => self.globals(database, pressure).await,
            Script::ContinueAs => self.continued(database, &seen).await,
        };

        // Every turn was admitted and committed once, and the pressure
        // frame, when one opens, landed in one owner commit.
        let (turns, frames) = match self.script {
            Script::Overflow | Script::Globals { pressure: true } => (2, 1),
            Script::Globals { pressure: false } => (2, 0),
            Script::ContinueAs => (3, 1),
        };
        let admitted = committed(CommitLabel::TURN_ADMIT);
        let commits = committed(CommitLabel::TURN_COMMIT);
        if admitted != turns || commits != turns {
            violations.push(format!(
                "{admitted} turns were admitted and {commits} committed, not {turns}"
            ));
        }
        let opened = committed(CommitLabel::PRESSURE_FRAME);
        if opened != frames {
            violations.push(format!(
                "{opened} pressure.frame commits landed, not {frames}"
            ));
        }
        if frames > 0 {
            let summaries = std::mem::take(&mut *self.summaries.lock_recover());
            let first = self.first_summary.as_ref().and_then(|first| {
                if cut.is_none()
                    && let Some(send) = summaries.first()
                {
                    let _ = first.set(send.clone());
                }
                first.get()
            });
            violations.extend(summary_laws(&summaries, first));
        }
        violations.extend(self.settled(database).await);
        if let Some(cut) = cut {
            violations.extend(zombie_laws(cut, &trace));
        }
        violations
    }
}

impl PressureFrame {
    /// The overflow script's laws: the first turn ended, and the second ran
    /// on the recovery frame, seeded with one summary, and answered there.
    async fn recovered(&self, database: &Arc<dyn DurableStore>, seen: &[String]) -> Vec<String> {
        let mut violations = Vec::new();
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
        match self.frames().await {
            Ok((frames, summaries)) if frames.len() == 2 && summaries.len() == 1 => {}
            other => violations.push(format!(
                "the ancestry does not hold two frames and one summary: {other:?}"
            )),
        }
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
        violations
    }

    /// The globals script's laws: the second turn's cell found the global
    /// gone behind a compaction frame, and kept with none.
    async fn globals(&self, database: &Arc<dyn DurableStore>, pressure: bool) -> Vec<String> {
        let mut violations = Vec::new();
        let expected = if pressure { "undefined" } else { "string" };
        match database.turn_end(&session(), &turn(SECOND_RUN)).await {
            Ok(Some(end))
                if matches!(
                    &end.cause,
                    RunTerminalCause::Committed {
                        outcome: RunCommittedOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue {
                            value,
                        }),
                        ..
                    } if value.as_str() == Some(expected)
                ) => {}
            other => violations.push(format!(
                "the second turn's cell did not find the global's type {expected}: {other:?}"
            )),
        }
        let frames = usize::from(pressure) + 1;
        match self.frames().await {
            Ok((ancestry, summaries))
                if ancestry.len() == frames && summaries.len() == frames - 1 => {}
            other => violations.push(format!(
                "the ancestry does not hold {frames} frames and {} summaries: {other:?}",
                frames - 1
            )),
        }
        violations
    }

    /// The `continue_as` script's laws: the summary seeds the second frame,
    /// the switch opens the third after it, and the follow-on ran the task
    /// there.
    async fn continued(&self, database: &Arc<dyn DurableStore>, seen: &[String]) -> Vec<String> {
        let mut violations = Vec::new();
        match database.turn_end(&session(), &turn(SECOND_RUN)).await {
            Ok(Some(end))
                if matches!(
                    &end.cause,
                    RunTerminalCause::Committed {
                        outcome: RunCommittedOutcome::AgentFrameSwitch { task, .. },
                        ..
                    } if task == TASK
                ) => {}
            other => violations.push(format!(
                "the compacted turn did not switch frames to the task: {other:?}"
            )),
        }
        match self.frames().await {
            Ok((frames, summaries)) if frames.len() == 3 && summaries == [frames[1].clone()] => {}
            other => violations.push(format!(
                "the ancestry does not hold three frames with the summary seeding the second: \
                 {other:?}"
            )),
        }
        let follow_on: Vec<&String> = seen
            .iter()
            .filter(|seen| seen.contains(TASK) && !seen.contains(NEXT))
            .collect();
        if follow_on.is_empty() {
            violations.push("the follow-on never ran the task".to_owned());
        }
        if let Some(request) = follow_on
            .iter()
            .find(|request| request.contains(ASK) || request.contains(SUMMARY))
        {
            violations.push(format!("the follow-on ran on an earlier frame: {request}"));
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

/// The owner commits `script`'s uncut run makes, in this order among
/// others. The overflow script: the overflowing turn admits, calls the model
/// and commits; the recovered turn admits, summarizes in a direct
/// completion, opens the recovery frame, calls the model on it and commits.
/// The RLM scripts' second turn does the same, and the `continue_as`
/// script's follow-on admits, calls the model and commits after it.
fn ordered_labels(script: Script) -> Vec<CommitLabel> {
    let mut labels = vec![
        CommitLabel::TURN_ADMIT,
        CommitLabel::MODEL_START,
        CommitLabel::TURN_COMMIT,
        CommitLabel::TURN_ADMIT,
    ];
    if script != (Script::Globals { pressure: false }) {
        labels.extend([CommitLabel::COMPLETION_START, CommitLabel::PRESSURE_FRAME]);
    }
    labels.extend([CommitLabel::MODEL_START, CommitLabel::TURN_COMMIT]);
    if script == Script::ContinueAs {
        labels.extend([
            CommitLabel::TURN_ADMIT,
            CommitLabel::MODEL_START,
            CommitLabel::TURN_COMMIT,
        ]);
    }
    labels
}

/// `script`'s uncut run on SQLite in memory holds its laws and makes every
/// expected owner commit, in order.
async fn uncut(script: Script) {
    let report = Matrix::new()
        .faults(&[])
        .run_test(|| PressureFrame::of(script, Dialect::SqliteMemory, None))
        .await;
    report.assert_held();
    let labels: Vec<CommitLabel> = report
        .baseline
        .iter()
        .filter(|write| write.kind == WriteKind::Actor && write.committed())
        .map(|write| write.point.label)
        .collect();
    let mut expected = ordered_labels(script).into_iter().peekable();
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

/// `script` cut at every label of its uncut run on `dialect`.
async fn prove(script: Script, dialect: Dialect, postgres_url: Option<String>) {
    let report = Matrix::new()
        .faults(&[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
        .run_test(|| PressureFrame::of(script, dialect, postgres_url.clone()))
        .await;
    let labels: Vec<&str> = report.labels().iter().map(|label| label.as_str()).collect();
    eprintln!(
        "pressure frame {script:?} {dialect:?}: {} cells over {} labels ({})",
        report.cells.len(),
        labels.len(),
        labels.join(", ")
    );
    report.assert_held();
    for label in ordered_labels(script) {
        assert!(
            report.labels().contains(&label),
            "the matrix never cut {label}"
        );
    }
}

/// RESEND (FIG-5479): the summary's owned call cut at `completion.start`
/// under every fault on `dialect`. Its redrive resends the admitted body,
/// and the model reads it as the uncut run's first attempt was read.
async fn prove_resend(dialect: Dialect, postgres_url: Option<String>) {
    let first = Arc::new(std::sync::OnceLock::new());
    let report = Matrix::new()
        .faults(&[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ])
        .labels(&[CommitLabel::COMPLETION_START])
        .horizon(Duration::from_secs(600))
        .run_test(|| {
            PressureFrame::of(Script::Overflow, dialect, postgres_url.clone()).against(&first)
        })
        .await;
    report.assert_held();
    assert!(
        first.get().is_some(),
        "the uncut run sent the summary the cells are compared with"
    );
    assert!(
        report.labels().contains(&CommitLabel::COMPLETION_START) && !report.cells.is_empty(),
        "the matrix never cut completion.start"
    );
}

/// On SQLite in memory: a summary resent after `completion.start` sends the
/// admitted body and is read as its first attempt.
#[tokio::test]
async fn a_summary_resent_after_completion_start_is_read_as_its_first_attempt_on_sqlite_memory() {
    prove_resend(Dialect::SqliteMemory, None).await;
}

/// On a SQLite file.
#[tokio::test]
async fn a_summary_resent_after_completion_start_is_read_as_its_first_attempt_on_sqlite_file() {
    prove_resend(Dialect::SqliteFile, None).await;
}

/// On PostgreSQL.
#[tokio::test]
async fn a_summary_resent_after_completion_start_is_read_as_its_first_attempt_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    prove_resend(Dialect::Postgres, Some(url)).await;
}

/// The uncut run: the overflowing turn commits its pending recovery, and
/// the next turn's preparation opens the recovery frame it runs in.
#[tokio::test]
async fn a_pressure_frame_opens_before_the_turn_that_runs_in_it() {
    uncut(Script::Overflow).await;
}

/// On SQLite in memory: a pressure frame killed at every label opens once,
/// and the turn runs in it.
#[tokio::test]
async fn a_pressure_frame_killed_at_every_label_opens_once_on_sqlite_memory() {
    prove(Script::Overflow, Dialect::SqliteMemory, None).await;
}

/// On a SQLite file.
#[tokio::test]
async fn a_pressure_frame_killed_at_every_label_opens_once_on_sqlite_file() {
    prove(Script::Overflow, Dialect::SqliteFile, None).await;
}

/// On PostgreSQL.
#[tokio::test]
async fn a_pressure_frame_killed_at_every_label_opens_once_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    prove(Script::Overflow, Dialect::Postgres, Some(url)).await;
}

/// An RLM session's live execution state restarts from a pressure frame's
/// seed: the global the first turn's cell set is gone from the interpreter
/// the compacted turn's cell runs in, and kept when no frame opens.
#[tokio::test]
async fn an_rlm_pressure_frame_restarts_the_session_s_globals_from_its_seed() {
    uncut(Script::Globals { pressure: false }).await;
    uncut(Script::Globals { pressure: true }).await;
}

/// On SQLite in memory: an RLM pressure frame followed by a `continue_as`
/// in the next turn, killed at every label, commits both frames in order,
/// each once.
#[tokio::test]
async fn an_rlm_pressure_frame_then_continue_as_killed_at_every_label_commits_both_frames_once_on_sqlite_memory()
 {
    uncut(Script::ContinueAs).await;
    prove(Script::ContinueAs, Dialect::SqliteMemory, None).await;
}

/// On a SQLite file.
#[tokio::test]
async fn an_rlm_pressure_frame_then_continue_as_killed_at_every_label_commits_both_frames_once_on_sqlite_file()
 {
    prove(Script::ContinueAs, Dialect::SqliteFile, None).await;
}

/// On PostgreSQL.
#[tokio::test]
async fn an_rlm_pressure_frame_then_continue_as_killed_at_every_label_commits_both_frames_once_on_postgres()
 {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    prove(Script::ContinueAs, Dialect::Postgres, Some(url)).await;
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
                .data_retention(lash::DataRetention::standard())
                .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
                .execution_budgets(lash::ExecutionBudgets::recommended())
                .delta_coalescing(lash::DeltaCoalescing::recommended())
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
            Ok(session) => session.admin().state().export().await,
            Err(error) => {
                violations.push(format!("the session does not open: {error}"));
                return violations;
            }
        };
        if committed
            .last_prompt_usage
            .as_ref()
            .map(|usage| usage.input_tokens)
            != Some(PROMPT_TOKENS)
        {
            violations.push(format!(
                "the turn committed prompt usage {:?}, not its call's {PROMPT_TOKENS} tokens",
                committed.last_prompt_usage
            ));
        }
        let expected = lash_core::LlmUsage {
            input_tokens: PROMPT_TOKENS,
            ..Default::default()
        };
        if committed.token_usage != expected {
            violations.push(format!(
                "the turn committed total usage {:?}, not its call's {expected:?}",
                committed.token_usage
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

/// FIG-5352 and FIG-5389: an RLM turn whose model call reports a prompt past
/// the compaction threshold, cut at every label, commits that call's prompt
/// and total usage. The next turn's pressure hook compacts, and a pass that
/// resumes the turn after its committed `model.done` keeps the usage its
/// checkpoint carries.
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
