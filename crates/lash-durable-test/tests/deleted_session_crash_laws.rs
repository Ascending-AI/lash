//! A run its session's deletion meets mid-turn ends once (FIG-4346, ADR 0049;
//! ported by FIG-5311 from the deleted `deleted_session_run_replay.rs`,
//! whose forced-replay legs become kill-and-resume legs here).
//!
//! A deletion is the session's close request, mail to its actor (ADR 0132
//! §12): the actor's next pass drains it before anything else, begins the
//! close and cancels the open turn. A host creates a session on a core and
//! sends it an input; the simulated nodes A and B run its turn with the
//! core's turn services over the production store. The scripted model, on
//! its first call, requests the session's close, as a host deleting the
//! session mid-turn would, and answers in prose. The matrix runs the uncut
//! turn, then cuts it at every labelled write under fail-before,
//! ack-hidden, zombie, abort and commit-then-abort, recovers on the other
//! node, and checks:
//!
//! - **Once:** the run ends exactly once: answered, when its commit landed
//!   before the close began, or cancelled by the close's `cancel` step,
//!   when the node that resumed it drained the close first;
//! - **Never after the close:** no model call starts once the close began;
//! - **Tombstone:** the close ends at its tombstone and the session's actor
//!   is terminal;
//! - a zombie's writes after its reap are refused.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/dialect.rs"]
mod dialect;
#[path = "support/sim.rs"]
mod sim;

#[path = "support/matrix.rs"]
mod matrix;

use matrix::MatrixTestExt as _;

use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use lash_core::LlmOutputPart;
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{LlmRequest, LlmResponse, LlmStreamEvent, StreamBlockIdentity};
use lash_core::runtime::durable::session::SessionActivation;
use lash_core::runtime::durable::session_close::request_session_close;
use lash_core_execution::{Backend, BackendParts, NoProjectionProviders, StoreSet};
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableError, DurableStore};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire, WriteKind,
};
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{SessionId, TurnId};

use dialect::Dialect;

const SESSION: &str = "deleted-session";
const RUN: &str = "deleted-session-turn";
const MODEL: &str = "deleted-session-model";
const ASK: &str = "answer while the session is deleted";

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

fn run() -> TurnId {
    TurnId::try_from(RUN.to_owned()).unwrap()
}

fn actor() -> ActorKey {
    ActorKey::session(SESSION).unwrap()
}

/// The outside world: every model call, and whether the close had begun
/// when it started. No kill undoes it.
#[derive(Default)]
struct World {
    /// Whether each model call started after a `session.close.begin`
    /// committed.
    calls: Mutex<Vec<bool>>,
    backend: OnceLock<Backend>,
    nodes: OnceLock<Weak<SimNodes>>,
}

impl World {
    /// Whether the close has begun by now, read from the trace at once.
    fn close_begun(&self) -> bool {
        self.nodes
            .get()
            .and_then(Weak::upgrade)
            .is_some_and(|nodes| {
                nodes.script().trace().iter().any(|write| {
                    write.point.label == CommitLabel::SESSION_CLOSE_BEGIN && write.committed()
                })
            })
    }
}

/// The scripted model: its call requests the session's close, then answers
/// in prose.
fn model(world: Arc<World>) -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("deleted-session-scripted")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| {
            let world = Arc::clone(&world);
            async move {
                world.calls.lock_recover().push(world.close_begun());
                let backend = world.backend.get().expect("the backend is built first");
                request_session_close(backend, &session())
                    .await
                    .expect("the close request is the session's mail");
                let text = "answered".to_owned();
                if let Some(stream) = request.stream_events.as_ref() {
                    stream.send(LlmStreamEvent::Delta {
                        block: StreamBlockIdentity::new("text:0", 0),
                        text: text.clone(),
                    });
                }
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text,
                        response_meta: None,
                    }],
                    ..LlmResponse::default()
                })
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
struct DeletedSession {
    dialect: Dialect,
    postgres_url: Option<String>,
    world: Arc<World>,
    tripwire: Arc<Tripwire>,
    core: Mutex<Option<lash::LashCore>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl DeletedSession {
    fn new(dialect: Dialect, postgres_url: Option<String>) -> Self {
        Self {
            dialect,
            postgres_url,
            world: Arc::default(),
            tripwire: Arc::default(),
            core: Mutex::default(),
            keep: Mutex::default(),
        }
    }

    fn backend(&self) -> Backend {
        self.world
            .backend
            .get()
            .cloned()
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
                    .data_retention(lash::DataRetention::standard())
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
                    .execution_budgets(lash::ExecutionBudgets::recommended())
                    .delta_coalescing(lash::DeltaCoalescing::recommended())
                    .serve_test_llm_profile(model(Arc::clone(&self.world)), metadata())
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(
                        "deleted-session-deployment",
                        "deleted-session-boot",
                    ))
                    .expect("the core builds")
            })
            .clone()
    }

    /// Create the session and send it its input through the core, uncut:
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
                    lash::MaxToolCalls::new(8),
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
}

#[async_trait::async_trait]
impl Scenario for DeletedSession {
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
        assert!(
            self.world.backend.set(backend).is_ok(),
            "one database per scenario"
        );
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
        let _ = self.world.nodes.set(Arc::downgrade(nodes));
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
            Ok(Some(snapshot)) if snapshot.state == ActorState::Terminal
        )
    }

    async fn check(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let database = nodes.database();
        let trace = nodes.script().trace();
        let committed = |label: CommitLabel| {
            trace
                .iter()
                .filter(|write| write.point.label == label && write.committed())
                .count()
        };

        // Once: the run was admitted once and ends once. Its end row goes
        // with the session's storage at the tombstone, so the trace says how
        // it ended: by its own commit, which lands only before the close
        // began, or by the close's `cancel` step, which ends the turn it
        // finds open.
        let position = |label: CommitLabel| {
            trace
                .iter()
                .position(|write| write.point.label == label && write.committed())
        };
        if committed(CommitLabel::TURN_ADMIT) != 1 {
            violations.push(format!(
                "the run was admitted {} times",
                committed(CommitLabel::TURN_ADMIT)
            ));
        }
        match (
            committed(CommitLabel::TURN_COMMIT),
            committed(CommitLabel::TURN_CANCEL),
        ) {
            (1, 0) => {
                if position(CommitLabel::TURN_COMMIT) > position(CommitLabel::SESSION_CLOSE_BEGIN) {
                    violations.push("the run committed after its session's close began".to_owned());
                }
            }
            (0, 0) => {
                if committed(CommitLabel::SESSION_CLOSE_CANCEL) == 0 {
                    violations.push("the run never committed and no close cancelled it".to_owned());
                }
            }
            (commits, cancels) => violations.push(format!(
                "the run ended {commits} times by its commit and {cancels} by a cancel"
            )),
        }
        match database.turn(&session()).await {
            Ok(None) => {}
            other => violations.push(format!("a turn stayed open: {other:?}")),
        }

        // Never after the close: no model call starts once it began.
        let calls = self.world.calls.lock_recover().clone();
        if calls.is_empty() {
            violations.push("the turn never called its model".to_owned());
        }
        if calls.iter().any(|after_close| *after_close) {
            violations.push(format!(
                "a model call started after the close began: {calls:?}"
            ));
        }

        // Tombstone: the close ended and the actor is terminal.
        match database.session_close(&session()).await {
            Ok(Some(close)) if close.is_tombstone() => {}
            other => violations.push(format!("the close did not end at its tombstone: {other:?}")),
        }
        if committed(CommitLabel::SESSION_CLOSE_TOMBSTONE) != 1 {
            violations.push(format!(
                "the tombstone committed {} times",
                committed(CommitLabel::SESSION_CLOSE_TOMBSTONE)
            ));
        }
        match database.actor(&actor()).await {
            Ok(Some(snapshot)) if snapshot.state == ActorState::Terminal => {}
            other => violations.push(format!("the session's actor did not end: {other:?}")),
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
        .run_test(|| DeletedSession::new(dialect, postgres_url.clone()))
        .await;
    let labels: Vec<&str> = report.labels().iter().map(|label| label.as_str()).collect();
    eprintln!(
        "deleted session {dialect:?}: {} cells over {} labels ({})",
        report.cells.len(),
        labels.len(),
        labels.join(", ")
    );
    report.assert_held();
    for label in [
        CommitLabel::TURN_ADMIT,
        CommitLabel::MODEL_START,
        CommitLabel::SESSION_CLOSE_BEGIN,
        CommitLabel::SESSION_CLOSE_TOMBSTONE,
    ] {
        assert!(
            report.labels().contains(&label),
            "the matrix never cut {label}"
        );
    }
}

/// The uncut run: the turn's commit lands first, then the close it
/// requested ends the session at its tombstone.
#[tokio::test]
async fn a_session_deleted_mid_turn_ends_its_run_once_and_closes() {
    let report = Matrix::new()
        .faults(&[])
        .run_test(|| DeletedSession::new(Dialect::SqliteMemory, None))
        .await;
    report.assert_held();
}

/// On SQLite in memory: a run its session's deletion meets, killed at every
/// label, ends once, never calls its model after the close began, and the
/// close ends at its tombstone.
#[tokio::test]
async fn a_run_killed_at_every_label_and_resumed_after_its_sessions_deletion_ends_once_on_sqlite_memory()
 {
    prove(Dialect::SqliteMemory, None).await;
}

/// On a SQLite file.
#[tokio::test]
async fn a_run_killed_at_every_label_and_resumed_after_its_sessions_deletion_ends_once_on_sqlite_file()
 {
    prove(Dialect::SqliteFile, None).await;
}

/// On PostgreSQL.
#[tokio::test]
async fn a_run_killed_at_every_label_and_resumed_after_its_sessions_deletion_ends_once_on_postgres()
{
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    prove(Dialect::Postgres, Some(url)).await;
}
