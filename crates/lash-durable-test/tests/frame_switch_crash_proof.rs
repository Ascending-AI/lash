//! An agent frame switch and its follow-on turn on the production session
//! activation (FIG-5232, ADR 0101 §3).
//!
//! A lash core's session holds one sent input. Its turn runs on the
//! standard protocol with the core's own turn services: the scripted model
//! calls the host tool `switch_frame`, whose control switches the agent
//! frame with a task. The turn's `turn.commit` publishes the new frame and
//! mails the session its follow-on, the task, in the same transaction; the
//! session's next turn is that ordinary mail, and it runs the task on the
//! new frame, where the model answers it in prose. The nodes are simulated
//! (A and B) over the production durable store.
//!
//! The matrix cuts the uncut run at every labelled write, under
//! fail-before, ack-hidden, zombie, abort and commit-then-abort, recovers on
//! the other node, and checks:
//!
//! - the switching turn answered with the frame switch, and exactly one
//!   follow-on turn ran after it: two turns admitted, two committed;
//! - the follow-on ran the task on the new frame: the model saw the task
//!   without the first frame's input, and answered it;
//! - no turn is open, no row is bound to an ended run, and no mail is left;
//! - a zombie's writes after its reap are refused.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/dialect.rs"]
mod dialect;
#[path = "support/sim.rs"]
mod sim;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{LlmRequest, LlmResponse, LlmStreamEvent, StreamBlockIdentity};
use lash_core::runtime::durable::session::SessionActivation;
use lash_core::{LlmOutputPart, ToolCall, ToolControl, ToolOutcome};
use lash_core_execution::{Backend, BackendParts, NoProjectionProviders, StoreSet};
use lash_core_store::store::{RunCommittedOutcome, RunTerminalCause, RunTerminalKind};
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableError, DurableStore, LeaseConfig};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire, WriteKind,
};
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{SessionId, TurnId};

use dialect::Dialect;

const SESSION: &str = "frame-switch-session";
const RUN: &str = "frame-switch-turn";
const TOOL: &str = "switch_frame";
const MODEL: &str = "frame-switch-model";
/// The first frame's input.
const ASK: &str = "switch frames, then carry on";
/// The task the switch hands its follow-on.
const TASK: &str = "summarise where the work stands";
/// What the follow-on's answer starts with.
const FINAL: &str = "follow-on answer";

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

fn run() -> TurnId {
    TurnId::try_from(RUN.to_owned()).unwrap()
}

fn actor() -> ActorKey {
    ActorKey::session(SESSION).unwrap()
}

/// `switch_frame`'s body: it switches the agent frame with the task.
struct SwitchFrame;

#[async_trait::async_trait]
impl StaticToolExecute for SwitchFrame {
    async fn execute(&self, _call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        ToolOutcome::ok(serde_json::json!({ "ok": true }))
            .with_control(ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material("frame-switch-law")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some(TASK.to_owned()),
            })
            .into()
    }
}

fn switch_frame() -> Arc<dyn lash_core::ToolProvider> {
    let definition = lash_core::ToolDefinition::raw(
        TOOL,
        TOOL,
        "Switches the agent frame and hands the new frame a task.",
        serde_json::json!({ "type": "object", "additionalProperties": false, "properties": {} }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("switch_frame's schemas");
    Arc::new(StaticToolProvider::new(vec![definition], SwitchFrame))
}

/// The scripted model: on a request that holds the task it answers in
/// prose; on any other it calls `switch_frame`. Every request it saw is
/// rendered into `seen`.
fn model(seen: Arc<Mutex<Vec<String>>>) -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("frame-switch-scripted")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| {
            let seen = Arc::clone(&seen);
            async move {
                let rendered = serde_json::to_string(&request.messages).expect("a request encodes");
                seen.lock_recover().push(rendered.clone());
                let response = if rendered.contains(TASK) {
                    let text = FINAL.to_owned();
                    if let Some(stream) = request.stream_events.as_ref() {
                        stream.send(LlmStreamEvent::Delta {
                            block: StreamBlockIdentity::new("text:0", 0),
                            text: text.clone(),
                        });
                    }
                    LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text,
                            response_meta: None,
                        }],
                        ..LlmResponse::default()
                    }
                } else {
                    LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "frame-switch-call".to_owned(),
                            tool_name: TOOL.to_owned(),
                            input_json: "{}".to_owned(),
                            replay: None,
                        }],
                        ..LlmResponse::default()
                    }
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
struct FrameSwitch {
    dialect: Dialect,
    postgres_url: Option<String>,
    seen: Arc<Mutex<Vec<String>>>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<Backend>>,
    core: Mutex<Option<lash::LashCore>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl FrameSwitch {
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
                    .tools(switch_frame())
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(
                        "frame-switch-deployment",
                        "frame-switch-boot",
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
}

#[async_trait::async_trait]
impl Scenario for FrameSwitch {
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

        // The switching turn answered with its frame switch.
        match database.turn_end(&session(), &run()).await {
            Ok(Some(end))
                if end.kind() == RunTerminalKind::Answered
                    && matches!(
                        &end.cause,
                        RunTerminalCause::Committed {
                            outcome: RunCommittedOutcome::AgentFrameSwitch { task, .. },
                            ..
                        } if task == TASK
                    ) => {}
            other => violations.push(format!(
                "the switching turn did not answer with its frame switch: {other:?}"
            )),
        }

        // Exactly one follow-on turn ran after it.
        let admitted = committed(CommitLabel::TURN_ADMIT);
        let commits = committed(CommitLabel::TURN_COMMIT);
        if admitted != 2 || commits != 2 {
            violations.push(format!(
                "{admitted} turns were admitted and {commits} committed, not the switch and one follow-on"
            ));
        }

        // The follow-on ran the task on the new frame and answered it.
        let follow_on: Vec<&String> = seen.iter().filter(|seen| seen.contains(TASK)).collect();
        if follow_on.is_empty() {
            violations.push("the model never saw the follow-on's task".to_owned());
        }
        if let Some(request) = follow_on.iter().find(|request| request.contains(ASK)) {
            violations.push(format!(
                "the follow-on ran on the first frame, which holds its input: {request}"
            ));
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

/// The owner commits of the uncut run, in order: the switching turn admits,
/// runs its tool round and commits the switch with its follow-on; the
/// follow-on admits, answers and commits; the actor releases.
fn uncut_labels() -> Vec<CommitLabel> {
    vec![
        CommitLabel::TURN_ADMIT,
        CommitLabel::MODEL_START,
        CommitLabel::MODEL_DONE,
        CommitLabel::ROUND_OUTCOME,
        CommitLabel::TURN_COMMIT,
        CommitLabel::TURN_ADMIT,
        CommitLabel::MODEL_START,
        CommitLabel::TURN_COMMIT,
        CommitLabel::SESSION_RELEASE,
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
        .run(|| FrameSwitch::new(dialect, postgres_url.clone()))
        .await;
    let labels: Vec<&str> = report.labels().iter().map(|label| label.as_str()).collect();
    eprintln!(
        "frame switch {dialect:?}: {} cells over {} labels ({})",
        report.cells.len(),
        labels.len(),
        labels.join(", ")
    );
    report.assert_held();
    for label in uncut_labels() {
        assert!(
            report.labels().contains(&label),
            "the matrix never cut {label}"
        );
    }
}

/// The uncut run: the switch commits with its follow-on, and the follow-on
/// runs as the session's next turn.
#[tokio::test]
async fn a_frame_switch_commits_with_its_follow_on_which_runs_next() {
    let report = Matrix::new()
        .faults(&[])
        .run(|| FrameSwitch::new(Dialect::SqliteMemory, None))
        .await;
    report.assert_held();
    let labels: Vec<CommitLabel> = report
        .baseline
        .iter()
        .filter(|write| write.kind == WriteKind::Actor && write.committed())
        .map(|write| write.point.label)
        .collect();
    assert_eq!(labels, uncut_labels());
}

/// On SQLite in memory: a frame switch killed at every label gives exactly
/// one follow-on turn, on the new frame.
#[tokio::test]
async fn a_frame_switch_killed_at_every_label_runs_one_follow_on_on_sqlite_memory() {
    prove(Dialect::SqliteMemory, None).await;
}

/// On a SQLite file.
#[tokio::test]
async fn a_frame_switch_killed_at_every_label_runs_one_follow_on_on_sqlite_file() {
    prove(Dialect::SqliteFile, None).await;
}

/// On PostgreSQL.
#[tokio::test]
async fn a_frame_switch_killed_at_every_label_runs_one_follow_on_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    prove(Dialect::Postgres, Some(url)).await;
}
