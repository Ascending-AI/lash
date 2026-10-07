//! What a code cell restored from its snapshot keeps (FIG-5224; ADR 0132
//! §6–§8), on the production turn driver over the durable store.
//!
//! A lash core's session holds one sent input; its turn runs on the
//! production session activation with the RLM protocol, a scripted model
//! and a host tool `ext_write`, declared `Once`, whose body writes to an
//! [`ExternalWorld`] that survives every node. The nodes are simulated (A
//! and B). Two cells run:
//!
//! - **Identity:** the cell calls `ext_write` twice, operation A then B.
//!   The matrix cuts the uncut run at every labelled write under every
//!   fault and recovers on the other node: wherever it was cut, A and B
//!   never share a `ToolCallId`, so a recipient that deduplicates on it
//!   (ADR 0132 §7) never suppresses B as A's repeat. A cut after A settled
//!   and before B's quiet point restores the cell onto A's outcome, and B
//!   is the next admitted operation, not A again.
//! - **Sleep:** the cell sleeps for `N`, node A is killed at `N / 2`, and
//!   B restores the cell onto its sleep. The sleep keeps the deadline it
//!   was admitted with: the cell wakes at the original deadline, not at
//!   recovery + `N`.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/dialect.rs"]
mod dialect;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash::rlm::Dialect as _;
use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::ToolDefinitionBindingExt as _;
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{LlmRequest, LlmResponse, LlmStreamEvent, StreamBlockIdentity};
use lash_core::runtime::durable::session::SessionActivation;
use lash_core::{ExecutionPolicy, LlmOutputPart, ToolCall, ToolCallId, ToolOutcome};
use lash_core_execution::{
    Backend, BackendParts, DurableSettings, NoProjectionProviders, StoreSet,
};
use lash_durable::domain::WaitKind;
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableStore, LeaseConfig};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, Script, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire,
};
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt as _;

use dialect::Dialect;

const SESSION: &str = "cell-restore-session";
const TOOL: &str = "ext_write";
const MODEL: &str = "cell-restore-model";
/// What the final answer starts with.
const FINAL: &str = "final answer";
/// How long the sleeping cell sleeps, in virtual milliseconds: well past a
/// lease's failover, so a recovery at its half is long before its end.
const SLEEP_MS: u64 = 120_000;

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

fn actor() -> ActorKey {
    ActorKey::session(SESSION).unwrap()
}

/// Which cell the model writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cell {
    /// Two `Once` operations: A writes `x: 1`, then B writes `x: 2`.
    Identity,
    /// One durable sleep of [`SLEEP_MS`].
    Sleep,
}

impl Cell {
    fn source(self) -> String {
        let body = match self {
            Self::Identity => "const a = await tools.ext_write({ x: 1 });\n\
                 const b = await tools.ext_write({ x: 2 });\n\
                 print([a, b]);"
                .to_owned(),
            Self::Sleep => format!("await sleep({SLEEP_MS});\nprint(\"woke\");"),
        };
        format!("<typescript>\n{body}\n</typescript>")
    }
}

/// The outside world: what `ext_write` wrote, per call. It survives every
/// node.
#[derive(Debug, Default)]
struct ExternalWorld {
    writes: Mutex<BTreeMap<ToolCallId, Vec<serde_json::Value>>>,
}

struct ExtWrite {
    world: Arc<ExternalWorld>,
}

#[async_trait::async_trait]
impl StaticToolExecute for ExtWrite {
    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.world
            .writes
            .lock_recover()
            .entry(call.context.call_id().clone())
            .or_default()
            .push(call.args.clone());
        ToolOutcome::ok(serde_json::json!({ "wrote": call.args })).into()
    }
}

fn ext_write(world: Arc<ExternalWorld>) -> Arc<dyn lash_core::ToolProvider> {
    let definition = lash_core::ToolDefinition::raw(
        TOOL,
        TOOL,
        "Writes x to the outside world, once.",
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "x": { "type": "number" } },
            "required": ["x"]
        }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("ext_write's schemas")
    .with_execution_policy(ExecutionPolicy::Once)
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], TOOL));
    Arc::new(StaticToolProvider::new(
        vec![definition],
        ExtWrite { world },
    ))
}

/// The scripted model: the cell until the transcript holds it, then prose.
fn model(cell: Cell) -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("cell-restore-scripted")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| async move {
            let answered = request
                .messages
                .iter()
                .any(|message| message.role == lash_core::llm::types::LlmRole::Assistant);
            let answer = if answered {
                FINAL.to_owned()
            } else {
                cell.source()
            };
            if let Some(stream) = request.stream_events.as_ref() {
                stream.send(LlmStreamEvent::Delta {
                    block: StreamBlockIdentity::new("text:0", 0),
                    text: answer.clone(),
                });
            }
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: answer,
                    response_meta: None,
                }],
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle()
}

/// The dialect's worker service with its run deadlines off the clock.
fn untimed_workers() -> lash::rlm::WorkerService {
    const OFF_THE_CLOCK: Duration = Duration::from_secs(365 * 24 * 60 * 60);
    let mut config = lash::rlm::TypescriptDialect
        .worker_service()
        .config()
        .clone();
    config.deadlines.compute = OFF_THE_CLOCK;
    config.deadlines.serialization = OFF_THE_CLOCK;
    config.deadlines.cumulative_cpu = OFF_THE_CLOCK;
    lash::rlm::WorkerService::new(config)
}

/// The scenario for one cell on one dialect, fresh for every matrix cell.
struct CellTurn {
    cell: Cell,
    dialect: Dialect,
    postgres_url: Option<String>,
    world: Arc<ExternalWorld>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<Backend>>,
    core: Mutex<Option<lash::LashCore>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl CellTurn {
    fn new(cell: Cell, dialect: Dialect, postgres_url: Option<String>) -> Self {
        Self {
            cell,
            dialect,
            postgres_url,
            world: Arc::default(),
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

    fn core(&self) -> lash::LashCore {
        let backend = self.backend();
        self.core
            .lock_recover()
            .get_or_insert_with(|| {
                lash::LashCore::rlm_builder(
                    backend.clone(),
                    lash::rlm::RlmProtocolPluginFactory::new(
                        lash::rlm::RlmProtocolPluginConfig::builder()
                            .channel(lash::rlm::RlmChannel::Cell)
                            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                            .build(),
                        Arc::new(lash::rlm::TypescriptDialect),
                        &backend,
                    )
                    .with_worker_service(untimed_workers()),
                )
                .serve_sessions(false)
                .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
                .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                .serve_test_llm_profile(
                    model(self.cell),
                    lash_core::LlmProfileMetadata::builder(MODEL)
                        .context_window_tokens(200_000)
                        .build()
                        .expect("the model's metadata"),
                )
                .tools(ext_write(Arc::clone(&self.world)))
                .build(lash::persistence::LeaseOwnerIdentity::opaque(
                    "cell-restore-deployment",
                    "cell-restore-boot",
                ))
                .expect("the core builds")
            })
            .clone()
    }

    /// Create the session and send it the turn's input, uncut.
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
            .send(lash::TurnInput::text("run the cell"))
            .await
            .map(drop)
            .map_err(|error| format!("send the turn's input: {error}"))
    }

    /// The identity laws after a run.
    async fn identity_laws(&self, nodes: &SimNodes) -> Vec<String> {
        let mut violations = Vec::new();
        let writes = self.world.writes.lock_recover().clone();
        let mut ids: BTreeMap<i64, Vec<&ToolCallId>> = BTreeMap::new();
        for (call, entries) in &writes {
            if entries.len() > 1 {
                violations.push(format!(
                    "call {call} reached the world {} times: {entries:?}",
                    entries.len()
                ));
            }
            for entry in entries {
                let x = entry["x"].as_i64().expect("a write names its x");
                ids.entry(x).or_default().push(call);
            }
        }
        if let (Some(a), Some(b)) = (ids.get(&1), ids.get(&2))
            && a.iter().any(|call| b.contains(call))
        {
            violations.push(format!(
                "operation B took operation A's ToolCallId: {writes:?}"
            ));
        }
        violations.extend(turn_ended(nodes).await);
        violations
    }
}

/// The turn ended, and nothing stays bound to it.
async fn turn_ended(nodes: &SimNodes) -> Vec<String> {
    let mut violations = Vec::new();
    match nodes.database().turn(&session()).await {
        Ok(None) => {}
        other => violations.push(format!("the turn did not end: {other:?}")),
    }
    match nodes.database().session_mailbox(&session()).await {
        Ok(mailbox) if mailbox.bound_run.is_none() => {}
        other => violations.push(format!("the ended run still holds its rows: {other:?}")),
    }
    violations
}

/// The runtime core's backend over `stores`, in the shipped build's format
/// sets.
fn shipped_backend(stores: Arc<dyn StoreSet>) -> Backend {
    Backend::assemble(BackendParts {
        stores,
        settings: DurableSettings::default(),
        engines: Vec::new(),
        providers: Arc::new(NoProjectionProviders),
        formats: lash::formats::actor_state_surfaces(),
    })
    .expect("the backend assembles")
}

#[async_trait::async_trait]
impl Scenario for CellTurn {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let (stores, database) = dialect::open(
            self.dialect,
            self.postgres_url.as_deref(),
            clock,
            &self.keep,
        )
        .await;
        *self.backend.lock_recover() = Some(shipped_backend(stores));
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

    async fn check(&self, nodes: &SimNodes, _cut: Option<&Cut>) -> Vec<String> {
        self.identity_laws(nodes).await
    }
}

/// Identity: cut at every labelled write under every fault, A's and B's
/// `ToolCallId`s differ.
async fn identity(dialect: Dialect, postgres_url: Option<String>) {
    let report = Matrix::new()
        .faults(&[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
        .run(|| CellTurn::new(Cell::Identity, dialect, postgres_url.clone()))
        .await;
    eprintln!(
        "identity on {dialect:?}: {} cells over {} labels",
        report.cells.len(),
        report.labels().len()
    );
    report.assert_held();
}

/// Sleep: the cell sleeps for `SLEEP_MS`, node A is killed at its half, and
/// node B, recovering it, wakes it at the deadline it was admitted with.
async fn sleep_across_a_crash(dialect: Dialect, postgres_url: Option<String>) {
    let turn = CellTurn::new(Cell::Sleep, dialect, postgres_url);
    let clock = SimClock::new();
    let database = turn.database(Arc::clone(&clock)).await;
    let nodes = SimNodes::new(
        Arc::clone(&database),
        Arc::clone(&clock),
        Script::new(),
        turn.config(),
        turn.activation(),
    );
    turn.send().await.expect("the turn is sent");
    nodes.start("a");
    // Run A until the cell sleeps on its pinned timer.
    let timer = loop {
        let timers: Vec<_> = database
            .pending_waits(&actor())
            .await
            .expect("the session's waits read")
            .into_iter()
            .filter(|wait| wait.kind == WaitKind::Timer)
            .collect();
        if let [timer] = timers.as_slice() {
            break timer.clone();
        }
        assert!(
            nodes.step().await.is_some() && clock.logical_ms() < SLEEP_MS,
            "the cell never slept on one timer: {timers:?}\n{}",
            nodes.script().rendered_trace()
        );
    };
    let deadline = timer.deadline.expect("a timer has a deadline").0;
    let deadline = u64::try_from(deadline).unwrap() - SimClock::timestamp_ms_at(0);
    let slept_at = clock.logical_ms();
    assert!(
        deadline >= slept_at + SLEEP_MS - 1_000 && deadline <= slept_at + SLEEP_MS,
        "the timer is due at {deadline}, not {SLEEP_MS} ms after the cell slept at {slept_at}"
    );
    // Kill A halfway through the sleep, and let B take the cell over.
    let half = slept_at + SLEEP_MS / 2;
    while clock.logical_ms() < half {
        assert!(nodes.step().await.is_some(), "A stalled while sleeping");
    }
    nodes.kill("a");
    nodes.quiesce().await;
    nodes.start("b");
    let horizon = deadline + 2 * SLEEP_MS;
    while !turn.done(&nodes).await {
        assert!(
            clock.logical_ms() < horizon,
            "the turn is not done {} ms after its sleep's deadline:\n{}",
            clock.logical_ms() - deadline,
            nodes.script().rendered_trace()
        );
        assert!(
            nodes.step().await.is_some(),
            "B stalled:\n{}",
            nodes.script().rendered_trace()
        );
    }
    nodes.quiesce().await;
    let mut violations = turn_ended(&nodes).await;
    // The cell woke when B committed its end: at its admitted deadline,
    // within a claim poll of the timer's resolution. A sleep run again from
    // its recovery would end `SLEEP_MS` after B took the cell over.
    let woke = nodes
        .script()
        .trace()
        .iter()
        .find(|write| {
            &*write.node == "b"
                && write.point.label == CommitLabel::CELL_SNAPSHOT
                && write.committed()
        })
        .map(|write| write.at_ms);
    let recovered = nodes
        .script()
        .trace()
        .iter()
        .find(|write| {
            &*write.node == "b"
                && write.point.label == CommitLabel::CLAIM
                && matches!(write.stored, Stored::Committed { effective: true })
        })
        .map(|write| write.at_ms);
    let poll = LeaseConfig::default().settings().claim_poll.as_millis() as u64;
    match (woke, recovered) {
        (Some(woke), Some(recovered))
            if recovered < deadline && woke >= deadline && woke <= deadline + poll + 5_000 => {}
        _ => violations.push(format!(
            "the cell slept at {slept_at}, due at {deadline}, was killed at {half}, \
             recovered at {recovered:?} and woke at {woke:?}"
        )),
    }
    assert!(
        violations.is_empty(),
        "sleep across a crash on {dialect:?}:\n  {}\n{}",
        violations.join("\n  "),
        nodes.script().rendered_trace()
    );
}

/// A restored cell never reuses a `ToolCallId`, on SQLite in memory.
#[tokio::test]
async fn a_restored_cell_never_reuses_a_tool_call_id_on_sqlite_memory() {
    identity(Dialect::SqliteMemory, None).await;
}

/// The same law on a SQLite file.
#[tokio::test]
async fn a_restored_cell_never_reuses_a_tool_call_id_on_sqlite_file() {
    identity(Dialect::SqliteFile, None).await;
}

/// The same law on PostgreSQL.
#[tokio::test]
async fn a_restored_cell_never_reuses_a_tool_call_id_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    identity(Dialect::Postgres, Some(url)).await;
}

/// A cell's sleep keeps its admitted deadline across a crash, on SQLite in
/// memory.
#[tokio::test]
async fn a_cells_sleep_keeps_its_deadline_across_a_crash_on_sqlite_memory() {
    sleep_across_a_crash(Dialect::SqliteMemory, None).await;
}

/// The same law on a SQLite file.
#[tokio::test]
async fn a_cells_sleep_keeps_its_deadline_across_a_crash_on_sqlite_file() {
    sleep_across_a_crash(Dialect::SqliteFile, None).await;
}

/// The same law on PostgreSQL.
#[tokio::test]
async fn a_cells_sleep_keeps_its_deadline_across_a_crash_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    sleep_across_a_crash(Dialect::Postgres, Some(url)).await;
}
