//! A code cell's helper declarations across a node kill (FIG-6481).
//!
//! A cell holds its document under its execution and the declaration of
//! every helper the document lists under the frame its turn was admitted
//! on, which retains them across the frame's cells. A host creates a session
//! on an RLM core and sends it an input; the simulated nodes A and B run its
//! turn, one cell that reaches library helpers, over the production store.
//! The matrix runs the uncut turn, then cuts it at every labelled write
//! under fail-before, ack-hidden, zombie, abort and commit-then-abort,
//! between the cell's publication and its commit among them, recovers on
//! the other node, and checks, once the turn ended and the cleanup relay
//! ran:
//!
//! - no execution holds a kernel-document artifact: the cell's document and
//!   whatever an abandoned attempt held are reclaimed;
//! - every kernel-document pointer is held by an edge and every edge names
//!   a pointer: no row leaked and none dangles;
//! - install pins hold exactly their package definitions, and every other
//!   edge left is the session frame's, retaining the cell's declarations;
//! - the turn committed once, and a zombie's writes after its reap are
//!   refused.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

// The law runs on SQLite in memory and PostgreSQL, not on a SQLite file.
#[allow(dead_code)]
#[path = "support/dialect.rs"]
mod dialect;
#[path = "support/matrix.rs"]
mod matrix;
#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::runtime::durable::session::SessionActivation;
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableError, DurableStore};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire, WriteKind,
};
use lash_execution_contracts::backend::StoreSet;
use lash_protocol_types::sync::MutexExt as _;
use lash_vocabulary::identity::SessionId;
use matrix::MatrixTestExt as _;

use dialect::Dialect;

const SESSION: &str = "cell-declaration-crash";
const INPUT: &str = "cell-declaration-crash-turn";
/// A cell whose document lists library helpers: `map` reaches them.
const CELL: &str =
    "const doubled = [1, 2].map((value) => value * 2); await control.finish(doubled[1]);";

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

fn actor() -> ActorKey {
    ActorKey::session(SESSION).unwrap()
}

/// One kernel-document edge: the artifact and its referrer's kind and id.
type Edge = (String, String, String);

/// The kernel-document rows of a scenario's database.
enum Rows {
    Sqlite(String),
    Postgres(String),
}

impl Rows {
    /// Every kernel-document pointer, and every kernel-document edge.
    async fn read(&self) -> (BTreeSet<String>, BTreeSet<Edge>) {
        const POINTERS: &str =
            "SELECT artifact_ref FROM artifact_refs WHERE namespace = 'kernel_document'";
        const EDGES: &str = "SELECT artifact_ref, referrer_kind, referrer_id
            FROM artifact_referrer_edges WHERE namespace = 'kernel_document'";
        match self {
            Self::Sqlite(uri) => {
                let connection = rusqlite::Connection::open_with_flags(
                    uri,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                        | rusqlite::OpenFlags::SQLITE_OPEN_URI,
                )
                .expect("open the SQLite database for inspection");
                let pointers = connection
                    .prepare(POINTERS)
                    .unwrap()
                    .query_map([], |row| row.get(0))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect();
                let edges = connection
                    .prepare(EDGES)
                    .unwrap()
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect();
                (pointers, edges)
            }
            Self::Postgres(url) => {
                let pool = sqlx::postgres::PgPoolOptions::new()
                    .max_connections(1)
                    .connect(url)
                    .await
                    .expect("open the PostgreSQL database for inspection");
                let pointers = sqlx::query_scalar::<_, String>(
                    "SELECT artifact_ref FROM lash_lash_vm_artifacts WHERE namespace = 'kernel_document'",
                )
                .fetch_all(&pool)
                .await
                .expect("read PostgreSQL pointers")
                .into_iter()
                .collect();
                let edges = sqlx::query_as::<_, Edge>(
                    "SELECT artifact_ref, referrer_kind, referrer_id
                     FROM lash_artifact_referrer_edges WHERE namespace = 'kernel_document'",
                )
                .fetch_all(&pool)
                .await
                .expect("read PostgreSQL edges")
                .into_iter()
                .collect();
                pool.close().await;
                (pointers, edges)
            }
        }
    }
}

/// The scenario on one dialect, fresh for every matrix cell.
struct CellDeclarations {
    dialect: Dialect,
    postgres_url: Option<String>,
    scripts: Arc<served::Scripts>,
    tripwire: Arc<Tripwire>,
    clock: Mutex<Option<Arc<SimClock>>>,
    backend: Mutex<Option<lash::Backend>>,
    rows: Mutex<Option<Arc<Rows>>>,
    core: Mutex<Option<lash::LashCore>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl CellDeclarations {
    fn new(dialect: Dialect, postgres_url: Option<String>) -> Self {
        let scripts = Arc::new(served::Scripts::default());
        scripts.register(INPUT, vec![served::cell(CELL)]);
        Self {
            dialect,
            postgres_url,
            scripts,
            tripwire: Arc::default(),
            clock: Mutex::default(),
            backend: Mutex::default(),
            rows: Mutex::default(),
            core: Mutex::default(),
            keep: Mutex::default(),
        }
    }

    fn backend(&self) -> lash::Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }

    fn clock(&self) -> Arc<SimClock> {
        self.clock
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }

    /// The deployment's RLM core over the scenario's backend: it serves no
    /// node of its own, the simulated nodes run its sessions' turns.
    fn core(&self) -> lash::LashCore {
        let backend = self.backend();
        let clock = self.clock();
        self.core
            .lock_recover()
            .get_or_insert_with(|| {
                lash::LashCore::rlm_builder(
                    backend.clone(),
                    served::rlm(&backend, sim::workers(&clock)),
                )
                .serve_sessions(false)
                .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
                .data_retention(lash::DataRetention::standard())
                .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                .execution_budgets(lash::ExecutionBudgets::recommended())
                .delta_coalescing(lash::DeltaCoalescing::recommended())
                .serve_test_llm_profile(
                    served::model(Arc::clone(&self.scripts)),
                    served::metadata(),
                )
                .build(lash::persistence::LeaseOwnerIdentity::opaque(
                    lash::persistence::LeaseOwnerId::new("cell-declaration-deployment"),
                    lash::persistence::LeaseIncarnationId::new("cell-declaration-boot"),
                ))
                .expect("the core builds")
            })
            .clone()
    }

    /// One due pass of the cleanup relay, once every execution that still
    /// holds a kernel document is made due, as its end fact's nudge makes
    /// it (see `sim::relay_ended_executions`).
    async fn relay(&self, edges: &BTreeSet<Edge>) -> Result<(), String> {
        let backend = self.backend();
        let clock = self.clock();
        let ledger = backend.artifact_cleanup();
        let now_ms = lash_core::ClockWallTime::timestamp_ms(clock.as_ref());
        for (_, kind, id) in edges.iter().filter(|(_, kind, _)| kind == "execution") {
            let referrer = lash_core::ArtifactReferrer::decode(kind, id)
                .map_err(|error| format!("decode the referrer `{kind}:{id}`: {error}"))?;
            ledger
                .nudge(&referrer, now_ms)
                .await
                .map_err(|error| format!("make `{referrer}`'s cleanup due: {error}"))?;
        }
        let relay = lash_core::runtime::artifact_cleanup::ArtifactCleanupRelay::over_backend(
            &backend,
            lash_core::ProcessEngineRegistry::default(),
        );
        lash_core::runtime::obligations::relay::relay_due(
            &relay,
            clock.as_ref(),
            std::num::NonZeroUsize::new(256).expect("a page"),
        )
        .await
        .map_err(|error| format!("the cleanup relay's due pass: {error}"))?;
        Ok(())
    }

    /// The declaration laws, once the turn ended.
    async fn declaration_laws(&self) -> Vec<String> {
        let rows = self
            .rows
            .lock_recover()
            .clone()
            .expect("the database is built first");
        let (_, edges) = rows.read().await;
        if let Err(error) = self.relay(&edges).await {
            return vec![error];
        }
        let (pointers, edges) = rows.read().await;
        let mut violations = Vec::new();
        let held: BTreeSet<String> = edges
            .iter()
            .map(|(artifact, _, _)| artifact.clone())
            .collect();
        if let Some(edge) = edges.iter().find(|(_, kind, _)| kind == "execution") {
            violations.push(format!(
                "an execution still holds a kernel document once the turn ended: {edge:?}"
            ));
        }
        let leaked: Vec<&String> = pointers.difference(&held).collect();
        if !leaked.is_empty() {
            violations.push(format!("kernel-document rows no edge holds: {leaked:?}"));
        }
        let dangling: Vec<&String> = held.difference(&pointers).collect();
        if !dangling.is_empty() {
            violations.push(format!("edges to no stored artifact: {dangling:?}"));
        }
        let session_edges: BTreeSet<_> = edges.iter().collect();
        let frame = session_edges
            .iter()
            .filter(|(_, kind, id)| kind == "frame_environment" && id.contains(SESSION))
            .count();
        if frame == 0 || frame != session_edges.len() {
            violations.push(format!(
                "the session's frame holds {frame} declarations of {} kernel-document edges: {edges:?}",
                session_edges.len()
            ));
        }
        violations
    }
}

#[async_trait::async_trait]
impl Scenario for CellDeclarations {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        *self.clock.lock_recover() = Some(Arc::clone(&clock));
        let (stores, database, rows): (Arc<dyn StoreSet>, Arc<dyn DurableStore>, Rows) = match self
            .dialect
        {
            Dialect::SqliteMemory => {
                let stores = sim::memory(clock).await;
                let rows = Rows::Sqlite(stores.database_uri().to_owned());
                let database = Arc::new(stores.durable_store());
                (Arc::new(stores), database, rows)
            }
            dialect => {
                let (stores, database) =
                    dialect::open(dialect, self.postgres_url.as_deref(), clock, &self.keep).await;
                let url = self
                    .keep
                    .lock_recover()
                    .iter()
                    .find_map(|kept| {
                        kept.downcast_ref::<lash_postgres_store::testing::IsolatedDatabase>()
                            .map(|isolated| isolated.url().to_owned())
                    })
                    .expect("the PostgreSQL leg keeps its isolated database");
                (stores, database, Rows::Postgres(url))
            }
        };
        *self.rows.lock_recover() = Some(Arc::new(rows));
        *self.backend.lock_recover() = Some(served::configured_backend(
            stores,
            sim::settings(),
            Vec::new(),
        ));
        database
    }

    fn config(&self) -> SimNodesConfig {
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: self.backend().formats().decodes(),
            serves: lash::testing::node_serves(&self.core()),
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
        // The host is outside the deployment under test: its send is uncut.
        let session = self
            .core()
            .session(session())
            .create(lash::SessionCreation::root(served::spec(8)))
            .await
            .map_err(|error| format!("create the session: {error}"))?;
        session
            .send(lash::TurnInput::text(INPUT))
            .await
            .map(drop)
            .map_err(|error| format!("send the turn's input: {error}"))?;
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
        let trace = nodes.script().trace();
        let commits = trace
            .iter()
            .filter(|write| write.point.label == CommitLabel::TURN_COMMIT && write.committed())
            .count();
        if commits != 1 {
            violations.push(format!("the turn committed {commits} times"));
        }
        match nodes.database().turn(&session()).await {
            Ok(None) => {}
            other => violations.push(format!("the turn did not end: {other:?}")),
        }
        if self.scripts.requests(INPUT).is_empty() {
            violations.push("the model was never asked for the cell".to_owned());
        }
        violations.extend(self.declaration_laws().await);
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
        .run_test(|| CellDeclarations::new(dialect, postgres_url.clone()))
        .await;
    let labels: Vec<&str> = report.labels().iter().map(|label| label.as_str()).collect();
    eprintln!(
        "cell declarations {dialect:?}: {} cells over {} labels ({})",
        report.cells.len(),
        labels.len(),
        labels.join(", ")
    );
    report.assert_held();
    for label in [CommitLabel::TURN_ADMIT, CommitLabel::TURN_COMMIT] {
        assert!(
            report.labels().contains(&label),
            "the matrix never cut {label}"
        );
    }
}

/// On SQLite in memory: a cell killed anywhere, its publication's commit
/// among the cuts, leaves its declarations to its frame alone, and no
/// kernel-document row leaks or dangles.
#[tokio::test]
async fn a_cell_killed_at_every_label_leaves_its_declarations_to_its_frame_on_sqlite_memory() {
    prove(Dialect::SqliteMemory, None).await;
}

/// On PostgreSQL.
#[tokio::test]
async fn a_cell_killed_at_every_label_leaves_its_declarations_to_its_frame_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    prove(Dialect::Postgres, Some(url)).await;
}
