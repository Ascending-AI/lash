//! Declared reads in a code cell (ADR 0144), on the production turn driver
//! over the durable store.
//!
//! A lash core's session holds one sent input; its turn runs on the
//! production session activation with the RLM protocol, a scripted model
//! and two host tools: `ext_read`, which declares `Read` and whose provider
//! registers a reader, and `ext_write`, declared `Once`. The reader answers
//! each read with the next number of a counter that survives every node,
//! so a read made again answers differently. The nodes are simulated (A
//! and B).
//!
//! - **READ-CRASH-RECOMPUTES:** a node lost before the save that follows a
//!   read makes the read again, and the cell goes on with the new answer.
//! - **READ-SAVED-STATE-EXACT:** once a save holds a read's answer, a node
//!   lost after it never makes the read again: the cell resumes with the
//!   answer the save holds.
//! - **READ-PLAN-CLAIMABLE:** a parked cell that plans reads owes its
//!   provider's reader: a node that does not serve the reader does not
//!   claim it, and one that does claims it requiring the reader.
//! - **READ-CANNOT-LOSE-OR-DUPLICATE-WRITE:** cut at every labelled write
//!   under every fault, the write between two reads reaches the world at
//!   most once, with an answer a read made, and is lost only where a `Once`
//!   call may be interrupted.
//! - **READ-ZERO-CALL-PHASES:** a cell that reads 50 times commits exactly
//!   what a cell that reads once does, and no read runs a tool body.
//!
//! READ-EXACT-CHARGE is a law of the machine across both dialects, in
//! `lash-vm-worker`'s `read_laws`.

// Test code: the matrix reads its optional proof selection from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/sim.rs"]
mod sim;

#[path = "support/matrix.rs"]
mod matrix;

use matrix::MatrixTestExt as _;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{LlmRequest, LlmResponse};
use lash_core::runtime::durable::session::SessionActivation;
use lash_core::{ExecutionPolicy, LlmOutputPart, ToolCall, ToolCallId, ToolOutcome};
use lash_core_execution::runtime::actor::process::ProcessActivation;
use lash_core_execution::runtime::actor::round::{AdmittedExecution, MemberBody};
use lash_core_execution::runtime::process::steps::{ProcessSteps, StepAdmission, StepRefusal};
use lash_core_store::tool_run::Material;
use lash_core_store::tool_run::SettledOutput;
use lash_durable::domain::WaitKind;
use lash_durable::runner::Activation;
use lash_durable::{ActorDispatch, ActorKey, ActorState, CommitLabel, DurableStore};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, Script, SimClock, SimNodes, SimNodesConfig, Tripwire, WriteKind,
};
use lash_execution_contracts::backend::Backend;
use lash_execution_contracts::backend::BackendParts;
use lash_execution_contracts::backend::StoreSet;
use lash_execution_contracts::projection::NoProjectionProviders;
use lash_protocol_types::SessionId;
use lash_protocol_types::sync::MutexExt as _;
use serde_json::{Value, json};

const SESSION: &str = "cell-read-session";
const READ: &str = "ext_read";
const WRITE: &str = "ext_write";
/// The plugin that registers the reading provider.
const READER: &str = "cell-read-reader";
const MODEL: &str = "cell-read-model";
const FINAL: &str = "final answer";
/// How long the claimable cell sleeps after its read, in virtual
/// milliseconds: well past a lease's failover and the idle eviction.
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
    /// A read, a `Once` write of its answer, and another read.
    ReadWriteRead,
    /// `n` reads in a loop.
    Reads(u32),
    /// A read, then a durable sleep of [`SLEEP_MS`].
    ReadThenSleep,
}

impl Cell {
    fn source(self) -> String {
        let body = match self {
            Self::ReadWriteRead => "const a = await tools.extRead({ x: 1 });\n\
                 const w = await tools.extWrite({ x: a.v });\n\
                 const b = await tools.extRead({ x: 2 });\n\
                 console.log([a.v, b.v]);"
                .to_owned(),
            Self::Reads(n) => format!(
                "let s = 0;\n\
                 for (let i = 0; i < {n}; i++) {{ const r = await tools.extRead({{ x: i }}); s = s + r.v; }}\n\
                 console.log(s);"
            ),
            Self::ReadThenSleep => format!(
                "const a = await tools.extRead({{ x: 1 }});\n\
                 await sleep({SLEEP_MS});\n\
                 console.log(a.v);"
            ),
        };
        format!("<typescript>\n{body}\n</typescript>")
    }
}

/// The outside world: every read the reader answered, every body entry of
/// every host call. It survives every node.
#[derive(Debug, Default)]
struct ExternalWorld {
    /// Each read's `x` and its answer, in the order they were made.
    reads: Mutex<Vec<(i64, i64)>>,
    /// Every body entry, by call.
    entries: Mutex<BTreeMap<ToolCallId, Vec<(String, Value)>>>,
}

impl ExternalWorld {
    fn reads(&self) -> Vec<(i64, i64)> {
        self.reads.lock_recover().clone()
    }

    /// The answers of the reads of `x`, in order.
    fn answers(&self, x: i64) -> Vec<i64> {
        self.reads()
            .into_iter()
            .filter(|(read, _)| *read == x)
            .map(|(_, answer)| answer)
            .collect()
    }

    /// The `x` of every write body entry.
    fn writes(&self) -> Vec<i64> {
        self.entries
            .lock_recover()
            .values()
            .flatten()
            .filter(|(tool, _)| *tool == format!("tools.{WRITE}"))
            .filter_map(|(_, args)| written_x(args))
            .collect()
    }

    /// Every body entry of a tool other than [`WRITE`].
    fn other_bodies(&self) -> Vec<String> {
        self.entries
            .lock_recover()
            .values()
            .flatten()
            .filter(|(tool, _)| *tool != format!("tools.{WRITE}"))
            .map(|(tool, _)| tool.clone())
            .collect()
    }
}

/// The `x` a call was made with. A cell's numbers are its dialect's: a
/// TypeScript `1` reaches the tool as the number it is, written `1.0`.
fn written_x(args: &Value) -> Option<i64> {
    let x = args["x"].as_f64()?;
    (x.fract() == 0.0).then_some(x as i64)
}

struct ExtTools {
    world: Arc<ExternalWorld>,
}

#[async_trait::async_trait]
impl StaticToolExecute for ExtTools {
    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.world
            .entries
            .lock_recover()
            .entry(call.context.call_id().clone())
            .or_default()
            .push((call.name().to_owned(), call.args.clone()));
        ToolOutcome::ok(json!({ "ran": call.name(), "with": call.args })).into()
    }
}

/// The reader of `ext_read`: each read answers the next number.
struct WorldReads {
    world: Arc<ExternalWorld>,
}

#[async_trait::async_trait]
impl lash::tools::ToolReads for WorldReads {
    async fn read(&self, call: lash::tools::ReadCall<'_>) -> Result<Value, lash::tools::ReadError> {
        assert_eq!(
            call.manifest.tool(),
            format!("tools.{READ}"),
            "the reader answers its own tool"
        );
        let x = written_x(call.args).expect("a read names its x");
        let mut reads = self.world.reads.lock_recover();
        let answer = i64::try_from(reads.len()).unwrap() + 1;
        reads.push((x, answer));
        Ok(json!({ "v": answer }))
    }
}

fn definition(name: &str, policy: ExecutionPolicy) -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        name,
        format!("tools.{name}"),
        "A host call the cell read laws watch.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "x": { "type": "number" } }
        }),
        json!({ "type": "object" }),
    )
    .expect("the tool's schemas")
    .with_execution(Duration::from_secs(120))
    .with_execution_policy(policy)
}

/// The token a node serves when it registers `ext_read`'s reader, and a
/// cell that plans reads of it owes.
fn reader_token() -> String {
    lash_core::ObligationToken::for_callback(
        lash_core::RegistryKind::Session,
        &lash_core::plugin::ToolProviderHandle::new(
            lash_core::plugin::RegistryKind::Session,
            READER,
            lash_core::tool_provider_key!("reads"),
        )
        .reads_identity(),
    )
    .to_string()
}

/// The scripted model: the cell until the transcript holds it, then prose.
fn model(cell: Cell) -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("cell-read-scripted")
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

/// No process runs a step here.
struct NoSteps;

#[async_trait::async_trait]
impl ProcessSteps for NoSteps {
    fn stop_grace(&self) -> Duration {
        Duration::from_secs(2)
    }

    async fn admit(
        &self,
        _runtime: &Arc<lash_core_execution::runtime::process::StepRuntime>,
        _process: &lash_execution_contracts::process::ProcessRecord,
        step: &lash_execution_contracts::process::StepRequest,
        _now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal> {
        Err(StepRefusal::UnknownTool {
            step: step.step().0.clone(),
            tool: step.step().0.clone(),
        })
    }

    fn resolved(
        &self,
        _process: &lash_execution_contracts::process::ProcessRecord,
        _step: &lash_execution_contracts::process::StepRequest,
        _execution: &AdmittedExecution,
        _parked: &Material<lash_core_store::tool_run::CompletionSource>,
        _resolution: lash_core_execution::runtime::actor::waits::Resolution,
    ) -> SettledOutput {
        SettledOutput::Interrupted
    }

    fn body(
        &self,
        _runtime: &Arc<lash_core_execution::runtime::process::StepRuntime>,
        _process: &lash_execution_contracts::process::ProcessRecord,
        step: &lash_execution_contracts::process::StepRequest,
        _execution: &AdmittedExecution,
    ) -> MemberBody {
        unreachable!("no step of `{}` is ever admitted", step.step().0)
    }
}

/// The runtime core's backend over `stores`, in the shipped build's format
/// sets.
fn shipped_backend(stores: Arc<dyn StoreSet>) -> Backend {
    Backend::assemble(BackendParts {
        stores,
        settings: sim::settings(),
        engines: Vec::new(),
        providers: Arc::new(NoProjectionProviders),
        formats: lash::formats::actor_state_surfaces(),
    })
    .expect("the backend assembles")
}

/// Which law a run is held to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Law {
    /// Cut at the save after the first read, before it entered the store.
    Recomputes,
    /// Cut at the save after the first read, once it committed.
    SavedExact,
    /// Cut anywhere.
    Writes,
    /// Uncut.
    Counted,
}

/// The scenario for one cell, fresh for every matrix cell.
struct ReadTurn {
    cell: Cell,
    law: Law,
    world: Arc<ExternalWorld>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<Backend>>,
    clock: Mutex<Option<Arc<SimClock>>>,
    core: Mutex<Option<lash::LashCore>>,
}

impl ReadTurn {
    fn new(cell: Cell, law: Law) -> Self {
        Self {
            cell,
            law,
            world: Arc::default(),
            tripwire: Arc::default(),
            backend: Mutex::default(),
            clock: Mutex::default(),
            core: Mutex::default(),
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
        let clock = self
            .clock
            .lock_recover()
            .clone()
            .expect("the database is built first");
        self.core
            .lock_recover()
            .get_or_insert_with(|| {
                let factory = lash::rlm::RlmProtocolPluginFactory::new(
                    lash::rlm::RlmProtocolPluginConfig::builder()
                        .channel(lash::rlm::RlmChannel::Cell)
                        .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                        .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                        .build(),
                    lash::rlm::CellDialect::typescript(),
                )
                .with_worker_service(sim::workers(&clock));
                let reads = Arc::new(StaticToolProvider::new(
                    vec![definition(READ, ExecutionPolicy::Read)],
                    ExtTools {
                        world: Arc::clone(&self.world),
                    },
                ));
                let writes = Arc::new(StaticToolProvider::new(
                    vec![definition(WRITE, ExecutionPolicy::Once)],
                    ExtTools {
                        world: Arc::clone(&self.world),
                    },
                ));
                lash::LashCore::rlm_builder(backend.clone(), factory)
                    .plugin(Arc::new(lash::plugins::StaticPluginFactory::new(
                        lash::plugins::PluginDeclaration::initial(READER),
                        lash::plugins::PluginSpec::new().with_tool_provider_registration(
                            lash::tool_provider_key!("reads"),
                            lash::tools::ToolProviderRegistration::new(reads).with_reads(Arc::new(
                                WorldReads {
                                    world: Arc::clone(&self.world),
                                },
                            )),
                        ),
                    )))
                    .serve_sessions(false)
                    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
                    .data_retention(lash::DataRetention::standard())
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .execution_budgets(lash::ExecutionBudgets::recommended())
                    .delta_coalescing(lash::DeltaCoalescing::recommended())
                    .serve_test_llm_profile(
                        model(self.cell),
                        lash_core::LlmProfileMetadata::builder(MODEL)
                            .cache_retention(lash_core::provider::CacheRetention::Short)
                            .context_window_tokens(200_000)
                            .build()
                            .expect("the model's metadata"),
                    )
                    .tools(lash::tool_provider_key!("tools"), writes)
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(
                        lash::persistence::LeaseOwnerId::new("cell-read-deployment"),
                        lash::persistence::LeaseIncarnationId::new("cell-read-boot"),
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
                lash_core::NoProgressBudget::bounded(12),
            )))
            .await
            .map_err(|error| format!("create the session: {error}"))?;
        session
            .send(lash::TurnInput::text("run the cell"))
            .await
            .map_err(|error| format!("send the turn's input: {error}"))?;
        Ok(())
    }

    /// The laws of [`Cell::ReadWriteRead`] after a run cut at `cut`.
    fn read_write_read_laws(&self, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let first = self.world.answers(1);
        let writes = self.world.writes();
        if writes.len() > 1 {
            violations.push(format!(
                "the write reached the world {} times",
                writes.len()
            ));
        }
        // The write carries the answer of the last first read made before
        // its admission's save: a read made again after a lost save
        // answers anew.
        if let Some(x) = writes.first()
            && !first.contains(x)
        {
            violations.push(format!(
                "the write carries {x}, which no first read answered: {first:?}"
            ));
        }
        if first.is_empty() || first.len() > 2 {
            violations.push(format!("the first read was made {first:?}"));
        }
        let cut_save = cut.filter(|cut| cut.point.label == CommitLabel::CELL_SNAPSHOT_ADMIT);
        match (self.law, cut_save.map(|cut| cut.fault)) {
            (Law::Recomputes, Some(Fault::Abort)) => {
                if first.len() != 2 || writes != [first[1]] {
                    violations.push(format!(
                        "READ-CRASH-RECOMPUTES: the save after the first read was lost, so the \
                         read is made again and the write carries its new answer: reads \
                         {first:?}, writes {writes:?}"
                    ));
                }
            }
            (Law::SavedExact, Some(Fault::CommitThenAbort)) => {
                if first.len() != 1 || writes.iter().any(|x| *x != first[0]) {
                    violations.push(format!(
                        "READ-SAVED-STATE-EXACT: the save after the first read committed, so the \
                         cell resumes with its answer and never reads it again: reads \
                         {first:?}, writes {writes:?}"
                    ));
                }
            }
            (Law::Recomputes | Law::SavedExact, _) | (Law::Writes, None) => {
                if first.len() != 1 || writes != [first[0]] {
                    violations.push(format!(
                        "an unlost run reads once and writes its answer once: reads {first:?}, \
                         writes {writes:?}"
                    ));
                }
            }
            (Law::Writes, Some(_)) => {
                // NR-2: losing a `Once` start may interrupt it before its
                // body runs; nothing else loses the write.
                let interrupted = cut.is_some_and(|cut| {
                    matches!(
                        cut.fault,
                        Fault::Abort | Fault::CommitThenAbort | Fault::AckHidden | Fault::Zombie
                    )
                });
                if writes.is_empty() && !interrupted {
                    violations.push(format!(
                        "READ-CANNOT-LOSE-OR-DUPLICATE-WRITE: the write was lost at {cut:?}"
                    ));
                }
            }
            (Law::Counted, _) => {}
        }
        let others = self.world.other_bodies();
        if !others.is_empty() {
            violations.push(format!("a read ran a tool body: {others:?}"));
        }
        violations
    }
}

#[async_trait::async_trait]
impl Scenario for ReadTurn {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        *self.clock.lock_recover() = Some(Arc::clone(&clock));
        // These laws run on SQLite in memory.
        let stores = sim::memory(clock).await;
        let database = Arc::new(stores.durable_store());
        *self.backend.lock_recover() = Some(shipped_backend(Arc::new(stores)));
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
        Arc::new(ActorDispatch {
            session: Arc::new(SessionActivation::new(
                self.backend(),
                lash::testing::session_turn_services(&self.core()),
                Arc::clone(&self.tripwire) as _,
            )),
            process: Arc::new(ProcessActivation::new(
                self.backend(),
                Arc::new(NoSteps),
                Arc::clone(&self.tripwire) as _,
            )),
        })
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

    async fn check(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = match self.cell {
            Cell::ReadWriteRead => self.read_write_read_laws(cut),
            Cell::Reads(n) => {
                let reads = self.world.reads().len();
                let mut violations = Vec::new();
                if reads != usize::try_from(n).unwrap() {
                    violations.push(format!("the cell made {reads} reads, not {n}"));
                }
                let bodies = self.world.other_bodies();
                if !bodies.is_empty() {
                    violations.push(format!("a read ran a tool body: {bodies:?}"));
                }
                violations
            }
            Cell::ReadThenSleep => Vec::new(),
        };
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

const FAULTS: [Fault; 5] = [
    Fault::FailBefore,
    Fault::AckHidden,
    Fault::Zombie,
    Fault::Abort,
    Fault::CommitThenAbort,
];

/// READ-CRASH-RECOMPUTES: the save that admits the write, the first after
/// the first read, is lost before it enters the store. The node that takes
/// the cell over resumes it from its start's save and reads again, and the
/// write carries the new answer.
#[tokio::test]
async fn a_read_lost_with_its_unsaved_stretch_is_made_again_on_sqlite_memory() {
    let report = Matrix::new()
        .labels(&[CommitLabel::CELL_SNAPSHOT_ADMIT])
        .faults(&[Fault::Abort])
        .horizon(Duration::from_secs(600))
        .run_test(|| ReadTurn::new(Cell::ReadWriteRead, Law::Recomputes))
        .await;
    report.assert_held();
    assert!(
        !report.cells.is_empty(),
        "the matrix cut no save after the read"
    );
}

/// READ-SAVED-STATE-EXACT: the save that admits the write commits and its
/// node dies before it sees the reply. The node that takes the cell over
/// resumes it with the answer that save holds, and makes no read again.
#[tokio::test]
async fn a_saved_read_is_never_made_again_on_sqlite_memory() {
    let report = Matrix::new()
        .labels(&[CommitLabel::CELL_SNAPSHOT_ADMIT])
        .faults(&[Fault::CommitThenAbort])
        .horizon(Duration::from_secs(600))
        .run_test(|| ReadTurn::new(Cell::ReadWriteRead, Law::SavedExact))
        .await;
    report.assert_held();
    assert!(
        !report.cells.is_empty(),
        "the matrix cut no save after the read"
    );
}

/// READ-CANNOT-LOSE-OR-DUPLICATE-WRITE: cut at every labelled write under
/// every fault, the recorded write between two reads reaches the world at
/// most once, carrying an answer a read made.
#[tokio::test]
async fn reads_around_a_write_neither_lose_nor_duplicate_it_on_sqlite_memory() {
    let report = Matrix::new()
        .faults(&FAULTS)
        .horizon(Duration::from_secs(600))
        .run_test(|| ReadTurn::new(Cell::ReadWriteRead, Law::Writes))
        .await;
    eprintln!(
        "reads around a write: {} cells over {} labels",
        report.cells.len(),
        report.labels().len()
    );
    report.assert_held();
}

/// The owner commits of an uncut run of `cell`, through the turn's answer.
async fn owner_commits(cell: Cell) -> Vec<CommitLabel> {
    let report = Matrix::new()
        .faults(&[])
        .run_test(|| ReadTurn::new(cell, Law::Counted))
        .await;
    report.assert_held();
    let all: Vec<_> = report
        .baseline
        .iter()
        .filter(|write| write.kind == WriteKind::Actor && write.committed())
        .map(|write| write.point.label)
        .collect();
    let answer = all
        .iter()
        .position(|label| *label == CommitLabel::TURN_COMMIT)
        .expect("the turn committed its host answer");
    all[..=answer].to_vec()
}

/// READ-ZERO-CALL-PHASES: 50 reads commit nothing a single read does not.
/// No read is admitted, waited on, started, settled or traced, and none
/// runs a tool body: the cell saves its start once, with the model's
/// response, and its end with the commit that consumes its answer.
#[tokio::test]
async fn fifty_reads_commit_what_one_read_does_on_sqlite_memory() {
    let one = owner_commits(Cell::Reads(1)).await;
    let fifty = owner_commits(Cell::Reads(50)).await;
    eprintln!("one read: {one:?}\nfifty reads: {fifty:?}");
    assert_eq!(fifty, one, "a read adds no commit");
    for per_call in [
        CommitLabel::ROUND_START,
        CommitLabel::ROUND_OUTCOME,
        CommitLabel::ROUND_RETRY,
        CommitLabel::CELL_SNAPSHOT_ADMIT,
        CommitLabel::CELL_INJECT,
        CommitLabel::WAIT_MINT,
        CommitLabel::TOOL_EFFECT,
    ] {
        assert!(
            !fifty.contains(&per_call),
            "a read commits no {per_call}: {fifty:?}"
        );
    }
    assert_eq!(
        fifty
            .iter()
            .filter(|label| **label == CommitLabel::MODEL_DONE_CELL_SNAPSHOT_ADMIT)
            .count(),
        1,
        "the cell saves its start once, with the model's response: {fifty:?}"
    );
}

/// What a probe node serving `serves` and holding `holding` reads as the session's claim
/// requirements when it claims it; `None` when it cannot claim it. A
/// claimed session is released ready, untouched.
async fn probe(
    database: &Arc<dyn DurableStore>,
    decodes: Vec<lash_durable::FormatSet>,
    serves: Vec<String>,
    holding: lash_durable::NodeHolding,
) -> Option<Vec<String>> {
    use lash_durable::{NodeId, NodeSpec, Release};
    let node = database
        .register_node(&NodeSpec {
            node: NodeId::new("cell-read-probe"),
            decodes,
            serves,
            held: holding.held,
            capabilities: holding.capabilities,
            ttl_millis: 15_000,
        })
        .await
        .expect("register the probe node");
    let mut requires = None;
    for claim in database.claim(&node, 64).await.expect("claim") {
        if claim.actor == actor() {
            requires = Some(claim.requires.clone());
        }
        let mut tx = database
            .begin(&claim.actor, claim.epoch)
            .await
            .expect("open the probe's transaction");
        tx.give_up(Release::Ready);
        database
            .commit(tx, CommitLabel::new("law.probe_requires"))
            .await
            .expect("release the probed actor");
    }
    database
        .release_node(&node)
        .await
        .expect("release the probe node");
    requires
}

/// READ-PLAN-CLAIMABLE: a cell that read, then sleeps past its session's
/// release, owes its provider's reader. Once its sleep is due, a node that
/// serves everything but the reader does not claim it; one that serves the
/// reader claims it, requiring it, and the cell then ends on another node
/// with the answer its save holds, read once.
#[tokio::test]
async fn a_cell_that_reads_is_claimed_only_where_its_reader_is_served_on_sqlite_memory() {
    let turn = ReadTurn::new(Cell::ReadThenSleep, Law::Counted);
    let clock = SimClock::new();
    let database = turn.database(Arc::clone(&clock)).await;
    let config = turn.config();
    let (decodes, serves) = (config.decodes.clone(), config.serves.clone());
    let nodes = SimNodes::new(
        Arc::clone(&database),
        Arc::clone(&clock),
        Script::new(),
        config,
        turn.activation(),
    );
    let token = reader_token();
    assert!(
        serves.contains(&token),
        "a node that registers the reader serves it: {serves:?}"
    );
    turn.send().await.expect("the turn is sent");
    nodes.start("a");
    // Run A until the cell sleeps on its timer and its session releases.
    let due = loop {
        let session = database.actor(&actor()).await.expect("read the session");
        let sleeping = database
            .pending_waits(&actor())
            .await
            .expect("the session's waits read")
            .into_iter()
            .any(|wait| wait.purpose.kind() == WaitKind::Timer);
        if let Some(session) = &session
            && sleeping
            && session.state == ActorState::Waiting
            && session.owner.is_none()
        {
            break session
                .next_due
                .expect("a sleeping session is due at its deadline");
        }
        assert!(
            nodes.step().await.is_some() && clock.logical_ms() < SLEEP_MS / 2,
            "the cell never slept released: {session:?}\n{}",
            nodes.script().rendered_trace()
        );
    };
    nodes.kill("a");
    nodes.quiesce().await;
    let backend = turn.backend();
    let holding = backend
        .packages()
        .expect("the executing nodes' installed packages")
        .hold(&backend)
        .await
        .expect("the probe holds the executing nodes' packages");
    let deadline = u64::try_from(due.0).unwrap() - SimClock::timestamp_ms_at(0);
    clock.advance_to(deadline + 1_000).await;
    let without: Vec<String> = serves
        .iter()
        .filter(|served| **served != token)
        .cloned()
        .collect();
    assert_eq!(
        probe(&database, decodes.clone(), without, holding.clone()).await,
        None,
        "a node that does not serve the reader does not claim the cell that plans reads"
    );
    let requires = probe(&database, decodes, serves, holding)
        .await
        .expect("a node that serves the reader claims the cell");
    assert!(
        requires.contains(&token),
        "the parked cell requires its reader: {requires:?}"
    );
    nodes.start("b");
    let horizon = clock.logical_ms() + 2 * SLEEP_MS;
    while !turn.done(&nodes).await {
        assert!(
            clock.logical_ms() < horizon && nodes.step().await.is_some(),
            "B did not end the turn:\n{}",
            nodes.script().rendered_trace()
        );
    }
    nodes.quiesce().await;
    assert_eq!(turn_ended(&nodes).await, Vec::<String>::new());
    assert_eq!(
        turn.world.reads(),
        [(1, 1)],
        "the saved read is not made again on the node that takes the cell over"
    );
}
