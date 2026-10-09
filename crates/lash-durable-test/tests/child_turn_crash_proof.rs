//! A `SessionTurn` process's child turn on its child session's actor
//! (FIG-5208).
//!
//! A host registers one detached `SessionTurn` process. Its actor pins a
//! `child_session` wait, creates the child session and mails it the turn's
//! input; the child's session actor runs the turn with the core's own turn
//! services and a scripted model, and the transaction that ends the turn
//! resolves the wait. The process then commits its terminal from the child
//! turn's committed end. The nodes are simulated (A and B) over the
//! production SQLite store, each running both activations.
//!
//! The matrix cuts the uncut run at every labelled write, the child turn's
//! and the process's, under fail-before, ack-hidden, zombie, abort and
//! commit-then-abort, recovers on the other node, and checks:
//!
//! - the child turn committed exactly once, and nothing stays bound to it;
//! - the process committed exactly one terminal, `Completed` with the child
//!   turn's answer: the parent sees one end, whoever ran it;
//! - a zombie's writes after its reap are refused.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/sim.rs"]
mod sim;

#[path = "support/matrix.rs"]
mod matrix;

use lash_sansio::llm::types::{StreamBlockEvent, StreamBlockKind};
use matrix::MatrixTestExt as _;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::LlmOutputPart;
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{LlmRequest, LlmResponse, LlmStreamEvent, StreamBlockIdentity};
use lash_core_execution::{
    Backend, BackendParts, HostArtifactPin, LifetimeDecision, NoProjectionProviders, ProcessId,
    ProcessInput, ProcessProvenance, ProcessRegistration, SessionCreateRequest, SessionStartPoint,
    SessionTurnOutcome, StoreSet,
};
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableError, DurableStore};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire, WriteKind,
};
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt as _;

const CHILD: &str = "child-turn-session";
const MODEL: &str = "child-turn-model";
const ANSWER: &str = "the child's answer";

fn child() -> SessionId {
    SessionId::try_from(CHILD.to_owned()).unwrap()
}

fn process_actor(process: &ProcessId) -> ActorKey {
    ActorKey::process(process.as_str()).expect("a process actor key")
}

fn session_actor() -> ActorKey {
    ActorKey::session(CHILD).expect("a session actor key")
}

/// The scripted model: it answers every call in prose.
fn model() -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("child-turn-scripted")
        .requires_streaming(true)
        .complete(|request: LlmRequest| async move {
            if let Some(stream) = request.stream_events.as_ref() {
                stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                    kind: StreamBlockKind::AssistantText,
                    block: StreamBlockIdentity::new("text:0", 0),
                    text: ANSWER.to_owned(),
                }));
            }
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: ANSWER.to_owned(),
                    response_meta: None,
                }],
                ..LlmResponse::default()
            })
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

fn spec() -> lash::SessionSpec {
    lash::SessionSpec::new(
        MODEL,
        lash::TurnBudget::Unbounded,
        lash::MaxToolCalls::new(8),
    )
    .no_progress_budget(lash_core::NoProgressBudget::bounded(12))
}

struct ChildTurn {
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<Backend>>,
    core: Mutex<Option<lash::LashCore>>,
    process: Mutex<Option<ProcessId>>,
}

impl ChildTurn {
    fn new() -> Self {
        Self {
            tripwire: Arc::default(),
            backend: Mutex::default(),
            core: Mutex::default(),
            process: Mutex::default(),
        }
    }

    fn backend(&self) -> Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }

    /// The deployment's core over the scenario's backend: it serves no node
    /// of its own, the simulated nodes run its actors.
    fn core(&self) -> lash::LashCore {
        let backend = self.backend();
        self.core
            .lock_recover()
            .get_or_insert_with(|| {
                lash::LashCore::standard_builder(backend)
                    .serve_sessions(false)
                    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
                    .data_retention(lash::DataRetention::standard())
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
                    .execution_budgets(lash::ExecutionBudgets::recommended())
                    .delta_coalescing(lash::DeltaCoalescing::recommended())
                    .serve_test_llm_profile(model(), metadata())
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(
                        lash::persistence::LeaseOwnerId::new("child-turn-deployment"),
                        lash::persistence::LeaseIncarnationId::new("child-turn-boot"),
                    ))
                    .expect("the core builds")
            })
            .clone()
    }

    fn process(&self) -> Option<ProcessId> {
        self.process.lock_recover().clone()
    }

    /// Register the process as a host would, uncut: its execution
    /// environment published under a host pin, its child session named.
    async fn register(&self) -> Result<ProcessId, String> {
        let request = SessionCreateRequest::root(
            lash::plugins::SessionToolAccess::ambient(),
            SessionStartPoint::Empty,
            Default::default(),
        )
        .with_session_id(child())
        .with_spec(&spec())
        .map_err(|error| format!("state the child's spec: {error}"))?;
        let policy = request
            .policy
            .clone()
            .expect("a stated spec states a policy");
        let env_ref = self
            .core()
            .host_artifacts()
            .publish_process_env(
                &HostArtifactPin::mint(),
                &lash_core_execution::ProcessExecutionEnvSpec::new(
                    lash_core_execution::AdmittedPluginConfig::default(),
                    policy,
                    lash_core_execution::SessionToolAccess::ambient(),
                ),
            )
            .await
            .map_err(|error| format!("publish the process's environment: {error}"))?;
        let registration = ProcessRegistration::new(
            ProcessInput::SessionTurn {
                definition_key: "child-turn".to_owned(),
                create_request: Box::new(request),
                turn_input: Box::new(lash_core::TurnInput::text("answer, please")),
                result: SessionTurnOutcome::Turn,
            },
            ProcessProvenance::host(),
            LifetimeDecision::Detached,
        )
        .with_execution_env_ref(Some(env_ref));
        self.backend()
            .process_registry()
            .register_process(registration)
            .await
            .map(|record| record.id)
            .map_err(|error| format!("register the process: {error}"))
    }
}

#[async_trait::async_trait]
impl Scenario for ChildTurn {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let stores = sim::memory(clock).await;
        let database: Arc<dyn DurableStore> = Arc::new(stores.durable_store());
        let stores: Arc<dyn StoreSet> = Arc::new(stores);
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
        // What the core's own node runs: its sessions, and its processes
        // on its durable process worker.
        lash::testing::node_activation(&self.core(), Arc::clone(&self.tripwire) as _)
            .expect("the core's node activation")
            .1
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        let process = self.register().await?;
        *self.process.lock_recover() = Some(process);
        // A starts and claims first, B once A is settled, so the matrix
        // cuts the uncut run's writes by node.
        nodes.start("a");
        nodes.quiesce().await;
        nodes.start("b");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        let mut actors = vec![session_actor()];
        actors.extend(self.process().as_ref().map(process_actor));
        actors
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        let Some(process) = self.process() else {
            return false;
        };
        let ended = matches!(
            nodes.database().actor(&process_actor(&process)).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Terminal
        );
        let idle = matches!(
            nodes.database().actor(&session_actor()).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Idle
        );
        ended && idle
    }

    async fn check(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let Some(process) = self.process() else {
            return vec!["nothing was registered".to_owned()];
        };
        let database = nodes.database();
        let trace = nodes.script().trace();
        let committed = |label: CommitLabel| {
            trace
                .iter()
                .filter(|write| write.point.label == label && write.committed())
                .count()
        };

        // The child turn ran on its session's actor and committed once.
        let commits = committed(CommitLabel::TURN_COMMIT);
        if commits != 1 {
            violations.push(format!("the child turn committed {commits} times"));
        }
        match database.turn(&child()).await {
            Ok(None) => {}
            other => violations.push(format!("the child turn did not end: {other:?}")),
        }
        match database.session_mailbox(&child()).await {
            Ok(mailbox) if mailbox.bound_run.is_none() => {}
            other => violations.push(format!("the ended run still holds its rows: {other:?}")),
        }

        // The parent sees exactly one end: one committed terminal, the
        // child turn's answer.
        let terminals = committed(CommitLabel::PROCESS_TERMINAL);
        if terminals != 1 {
            violations.push(format!("the process committed {terminals} terminals"));
        }
        let record = self
            .backend()
            .process_registry()
            .get_process(&process)
            .await;
        match record.as_ref().map(|record| {
            record
                .as_ref()
                .and_then(|record| record.terminal())
                .map(|terminal| serde_json::to_value(terminal.clone().into_await_output()))
        }) {
            Ok(Some(Ok(answer))) if answer.to_string().contains(ANSWER) => {}
            other => violations.push(format!(
                "the process did not answer the child turn's end: {other:?}"
            )),
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

/// The owner commits of the uncut run, in order: the process pins its wait,
/// mails the turn and parks; the child's actor admits and runs it, and its
/// commit wakes the process, which ends while the child's actor releases.
fn uncut_labels() -> Vec<CommitLabel> {
    vec![
        CommitLabel::PROCESS_ADVANCE,
        CommitLabel::PROCESS_ADVANCE,
        CommitLabel::PROCESS_ADVANCE,
        CommitLabel::TURN_ADMIT,
        CommitLabel::MODEL_START,
        CommitLabel::TURN_COMMIT,
        CommitLabel::PROCESS_TERMINAL,
        CommitLabel::CASCADE_BATCH,
        CommitLabel::SESSION_RELEASE,
    ]
}

/// The uncut run: the child turn runs on its session's actor, and the
/// process ends with its answer.
#[tokio::test]
async fn a_child_session_turn_runs_on_its_session_actor() {
    let report = Matrix::new().faults(&[]).run_test(ChildTurn::new).await;
    let labels: Vec<CommitLabel> = report
        .baseline
        .iter()
        .filter(|write| write.kind == WriteKind::Actor && write.committed())
        .map(|write| write.point.label)
        .collect();
    assert_eq!(labels, uncut_labels());
}

/// A child turn killed at every label resumes on another owner, and its
/// process commits exactly one terminal.
#[tokio::test]
async fn a_child_session_turn_killed_at_every_label_resumes_and_ends_its_process_once() {
    let report = Matrix::new()
        .faults(&[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ])
        // Which node claims the child session turns on the mail's store
        // write, which the fault stores do not count.
        .across_nodes()
        .horizon(Duration::from_secs(600))
        .run_test(ChildTurn::new)
        .await;
    let labels: Vec<&str> = report.labels().iter().map(|label| label.as_str()).collect();
    eprintln!(
        "child turn: {} cells over {} labels ({})",
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
