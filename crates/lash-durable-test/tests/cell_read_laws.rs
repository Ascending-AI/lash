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

#[path = "support/packages.rs"]
mod packages;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::LlmOutputPart;
use lash_core::ToolCall;
use lash_core::ToolCallId;
use lash_core::ToolOutcome;
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{LlmRequest, LlmResponse};
use lash_core::runtime::durable::session::SessionActivation;
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
use lash_kernel_value::identity::SessionId;
use lash_kernel_value::tool_contract::ExecutionPolicy;
use lash_protocol_types::sync::MutexExt as _;
use serde_json::{Value, json};

const SESSION: &str = "cell-read-session";
const READ: &str = "ext_read";
const WRITE: &str = "ext_write";
/// The plugin that registers the reading provider.
const READER: &str = "cell-read-reader";
const MODEL: &str = "cell-read-model";
const FINAL: &str = "final answer";
const PRESENTED: &str = "model-read-presented-under-saved-plan\n";
fn oversized_read(answer: i64) -> Value {
    json!({"v":answer, "body":"read body ".repeat(4_000)})
}
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
    /// A loop, a taken branch, an untaken branch, and a failed read.
    ReadActivity,
    Panic,
    InvalidOutput,
    ModelInvalidOutput,
    Pending,
    ModelMixed,
    Bound,
    Hooked,
    HookedModel,
}

impl Cell {
    fn source(self) -> String {
        let body = match self {
            Self::Hooked | Self::HookedModel => "await reads.extRead({ x: 1 });".to_owned(),
            Self::Panic => "try { await reads.extRead({ x: -10 }); } catch (e) { await tools.extWrite({ x: 91 }); } await tools.extWrite({ x: 92 });".to_owned(),
            Self::InvalidOutput | Self::ModelInvalidOutput => "await reads.extRead({ x: -11 });".to_owned(),
            Self::Pending => "await reads.extRead({ x: -12 });".to_owned(),
            Self::ModelMixed => String::new(),
            Self::Bound => "const sibling = tools.extWrite({ x: -20 }); await tools.extWrite({ x: 1 }); while (true) {}".to_owned(),
            Self::ReadWriteRead => "const a = await reads.extRead({ x: 1 });\n\
                 const w = await tools.extWrite({ x: a.v });\n\
                 const b = await reads.extRead({ x: 2 });\n\
                 console.log([a.v, b.v]);"
                .to_owned(),
            Self::Reads(n) => format!(
                "let s = 0;\n\
                 for (let i = 0; i < {n}; i++) {{ const r = await reads.extRead({{ x: i }}); s = s + r.v; }}\n\
                 console.log(s);"
            ),
            Self::ReadActivity => "for (let i = 1; i <= 3; i++) {
                const r = await reads.extRead({ x: i });
                if (i === 1) { await reads.extRead({ x: 10 }); }
                if (i === 9) { await reads.extRead({ x: 99 }); }
                }"
            .to_owned(),
            Self::ReadThenSleep => format!(
                "const a = await reads.extRead({{ x: 1 }});\n\
                 await sleep({SLEEP_MS});\n\
                 console.log(a.v);"
            ),
        };
        body.to_string()
    }
}

/// The outside world: every read the reader answered, every body entry of
/// every host call. It survives every node.
#[derive(Debug, Default)]
struct ExternalWorld {
    pending_read: tokio::sync::Notify,
    presentations: Mutex<BTreeMap<ToolCallId, Vec<String>>>,
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

/// The integral `x` a call was made with, across integer and float JSON operands.
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
        if written_x(call.args) == Some(-20) {
            return lash_core::ToolAttemptOutcome::Pending(lash_core::PendingCompletion::new());
        }
        ToolOutcome::ok(json!({ "ran": call.name(), "with": call.args })).into()
    }
}

/// The reader of `ext_read`: each read answers the next number.
struct WorldReads {
    world: Arc<ExternalWorld>,
    fail_third: bool,
    large: bool,
}

#[async_trait::async_trait]
impl lash::tools::ToolReads for WorldReads {
    async fn read(&self, call: lash::tools::ReadCall<'_>) -> Result<Value, lash::tools::ReadError> {
        assert_eq!(
            call.manifest.tool(),
            format!("reads.{READ}"),
            "the reader answers its own tool"
        );
        let x = written_x(call.args).expect("a read names its x");
        if x == -10 {
            panic!("reader invariant failed");
        }
        if x == -11 {
            return Ok(json!(-1));
        }
        if x == -12 {
            self.world.pending_read.notify_one();
            return std::future::pending().await;
        }
        let mut reads = self.world.reads.lock_recover();
        let answer = i64::try_from(reads.len()).unwrap() + 1;
        reads.push((x, answer));
        if self.fail_third && x == 3 {
            return Err(lash::tools::ReadError::new("page unavailable"));
        }
        Ok(if self.large {
            oversized_read(answer)
        } else {
            json!({ "v": answer })
        })
    }
}

fn definition(
    name: &str,
    policy: ExecutionPolicy,
) -> lash_vocabulary::tool_contract::ToolDefinition {
    lash_vocabulary::tool_contract::ToolDefinition::raw(
        name,
        format!("{}.{name}", if name == READ { "reads" } else { "tools" }),
        "A host call the cell read laws watch.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "x": { "type": "number" } }
        }),
        json!({"type":"object"}),
    )
    .expect("the tool's schemas")
    .with_execution(Duration::from_secs(120))
    .with_execution_policy(policy)
}

/// The token a node serves when it registers `ext_read`'s reader, and a
/// cell that plans reads of it owes.
fn reader_token() -> String {
    lash_vocabulary::claims::ObligationToken::for_callback(
        lash_vocabulary::claims::RegistryKind::Session,
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
            if cell == Cell::ModelInvalidOutput && !answered {
                return Ok(LlmResponse {
                    parts: vec![LlmOutputPart::ToolCall {
                        call_id: "r1".into(),
                        tool_name: "reads__ext_read".into(),
                        input_json: json!({"x":-11}).to_string(),
                        replay: None,
                    }],
                    ..Default::default()
                });
            }
            if matches!(cell, Cell::ModelMixed | Cell::HookedModel) && !answered {
                return Ok(LlmResponse {
                    parts: [
                        ("r1", "reads__ext_read", json!({"x":1})),
                        ("w", "tools__ext_write", json!({"x":91})),
                        ("r2", "reads__ext_read", json!({"x":2})),
                    ]
                    .into_iter()
                    .map(|(id, tool, args)| LlmOutputPart::ToolCall {
                        call_id: id.into(),
                        tool_name: tool.into(),
                        input_json: args.to_string(),
                        replay: None,
                    })
                    .collect(),
                    ..Default::default()
                });
            }
            Ok(LlmResponse {
                parts: vec![if answered {
                    LlmOutputPart::Text {
                        text: answer,
                        response_meta: None,
                    }
                } else {
                    lash::testing::execute_code(answer)
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
        _target: Option<&lash_core_execution::runtime::process::steps::InvocationTarget>,
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
                        .instruction_limit(lash::rlm::InstructionBound::instructions(
                            if self.cell == Cell::Bound {
                                100
                            } else {
                                1_000_000
                            },
                        ))
                        .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                        .build(),
                    lash::rlm::CellDialect::typescript(),
                )
                .with_worker_service(sim::workers(&clock));
                let reads = Arc::new(StaticToolProvider::new(
                    vec![
                        if matches!(self.cell, Cell::InvalidOutput | Cell::ModelInvalidOutput) {
                            lash_vocabulary::tool_contract::ToolDefinition::raw(
                                READ,
                                "reads.ext_read",
                                "",
                                json!({"type":"object"}),
                                json!({"type":"integer","minimum":0}),
                            )
                            .unwrap()
                            .with_execution(Duration::from_secs(120))
                            .with_execution_policy(ExecutionPolicy::Read)
                        } else {
                            definition(READ, ExecutionPolicy::Read).with_execution(
                                if self.cell == Cell::Pending {
                                    Duration::from_millis(1_137)
                                } else {
                                    Duration::from_secs(120)
                                },
                            )
                        },
                    ],
                    ExtTools {
                        world: Arc::clone(&self.world),
                    },
                ));
                let writes = Arc::new(StaticToolProvider::new(
                    vec![if self.cell == Cell::Bound {
                        definition(WRITE, ExecutionPolicy::Once).with_declaration(
                            lash_vocabulary::tool_declaration::ToolDeclaration::deferring(),
                            Some(lash_vocabulary::tool_contract::ParkBound::Within(Duration::from_secs(120))),
                        ).unwrap()
                    } else { definition(WRITE, ExecutionPolicy::Once) }],
                    ExtTools {
                        world: Arc::clone(&self.world),
                    },
                ));
                let builder = if matches!(
                    self.cell,
                    Cell::ModelMixed | Cell::HookedModel | Cell::ModelInvalidOutput
                ) {
                    lash::LashCore::standard_builder(backend.clone())
                } else {
                    lash::LashCore::rlm_builder(backend.clone(), factory)
                };
                builder
                    .plugin(Arc::new(lash::plugins::StaticPluginFactory::new(
                        lash::plugins::PluginDeclaration::initial(READER),
                        {
                            let spec = lash::plugins::PluginSpec::new();
                            let spec = if matches!(self.cell, Cell::Hooked | Cell::HookedModel) {
                                spec.with_tool_args_check(
                                    lash::hook_key!("deny-read"),
                                    Arc::new(|_| {
                                        Box::pin(async {
                                            Ok(lash_core::plugin::BeforeToolDecision::Deny(
                                                lash_core::ToolFailure::tool(
                                                    lash_core::ToolFailureClass::PermissionDenied,
                                                    "read_denied",
                                                    "read denied",
                                                ),
                                            ))
                                        })
                                    }),
                                )
                            } else {
                                spec
                            };
                            let spec = if self.cell == Cell::ModelMixed {
                                let world = Arc::clone(&self.world);
                                spec.with_presentation_step(lash::hook_key!("read-presentation"), Arc::new(move |input| {
                                    let world = Arc::clone(&world);
                                    Box::pin(async move {
                                        let mut result = input.previous;
                                        if input.context.tool_name == format!("reads.{READ}") {
                                            world.presentations.lock_recover().entry(input.context.call_id.clone()).or_default().push(serde_json::to_string(&result.parts).unwrap());
                                            result.parts.insert(0, lash_core::facade_support::ModelToolReturnPart::text(PRESENTED));
                                            result.parts.push(lash_core::facade_support::ModelToolReturnPart::text("p".repeat(70_000)));
                                        }
                                        Ok(result)
                                    })
                                }))
                            } else { spec };
                            spec.with_tool_provider_registration(
                                lash::tool_provider_key!("reads"),
                                lash::tools::ToolProviderRegistration::new(reads.clone())
                                    .with_reads(Arc::new(WorldReads {
                                        fail_third: self.cell == Cell::ReadActivity,
                                        large: self.cell == Cell::ModelMixed,
                                        world: Arc::clone(&self.world),
                                    })),
                            )
                            .with_package_provider(
                                lash::tool_provider_key!("reads"),
                                packages::operation_provider(
                                    READER,
                                    lash::tool_provider_key!("reads"),
                                    reads,
                                ),
                            )
                        },
                    )))
                    .serve_sessions(false)
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
                    .plugin(packages::tools(lash::tool_provider_key!("tools"), writes))
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
        if self.cell == Cell::Pending {
            tokio::time::timeout(Duration::from_secs(30), self.world.pending_read.notified())
                .await
                .map_err(|_| "the pending read was never entered")?;
            let clock = self
                .clock
                .lock_recover()
                .clone()
                .expect("the database is open");
            let deadline = clock.logical_ms() + 1_137;
            tokio::time::timeout(Duration::from_secs(2), clock.wait_for_sleep(deadline))
                .await
                .map_err(|_| "the tool deadline was not armed on the injected clock")?;
            clock.advance_to(deadline + 1).await;
        }
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
            Cell::ReadThenSleep
            | Cell::ReadActivity
            | Cell::Panic
            | Cell::InvalidOutput
            | Cell::ModelInvalidOutput
            | Cell::Pending
            | Cell::ModelMixed
            | Cell::Bound
            | Cell::Hooked
            | Cell::HookedModel => Vec::new(),
        };
        violations.extend(turn_ended(nodes).await);
        if self.cell == Cell::Bound {
            let checked = async {
                let durable = self
                    .core()
                    .session(session())
                    .durable()
                    .await
                    .map_err(|e| e.to_string())?;
                let transcript = durable.transcript().await.map_err(|e| e.to_string())?;
                let cell = transcript
                    .entries()
                    .iter()
                    .find_map(|entry| match &entry.item {
                        lash::transcript::TranscriptItem::Cell(cell) => Some(cell),
                        _ => None,
                    })
                    .ok_or("the bound cell is retained")?;
                let lash_core::CellOutcome::Failed(failure) = &cell.result else {
                    return Err(format!("the bound cell succeeded: {:?}", cell.result));
                };
                let fact = failure
                    .bound
                    .as_ref()
                    .ok_or("the cell lost structured bound evidence")?;
                if fact.resource != lash_core::CellResource::Charges
                    || fact.limit != 100
                    || fact.observed != Some(101)
                    || fact.consumed != Some(100)
                {
                    return Err(format!("KERNEL-BOUND-FACT: {fact:?}"));
                }
                Ok::<(), String>(())
            }
            .await;
            if let Err(error) = checked {
                violations.push(error);
            }
        }

        if matches!(self.cell, Cell::Hooked | Cell::HookedModel) {
            if !self.world.reads().is_empty()
                || !self.world.other_bodies().is_empty()
                || !self.world.writes().is_empty()
            {
                violations.push("READ-HOOK-BINDING: a denying hook entered a provider".into());
            }
            let checked = async {
                let durable = self.core().session(session()).durable().await.map_err(|e| e.to_string())?;
                let transcript = durable.transcript().await.map_err(|e| e.to_string())?;
                let turn = transcript.entries().iter().find_map(|entry| entry.provenance.turn_id.clone()).ok_or("the turn is retained")?;
                let calls = durable.read_turn_tool_calls(&turn).await.map_err(|e| e.to_string())?;
                if calls.is_empty() || calls.iter().any(|call| !matches!(&call.output.outcome, lash_core::ToolCallOutcome::Failure(failure) if failure.code == "read_denied")) {
                    return Err(format!("READ-HOOK-BINDING: {calls:?}"));
                }
                Ok::<(),String>(())
            }.await;
            if let Err(error) = checked {
                violations.push(error);
            }
        }

        if matches!(
            self.cell,
            Cell::Panic
                | Cell::InvalidOutput
                | Cell::ModelInvalidOutput
                | Cell::Pending
                | Cell::ModelMixed
        ) {
            let checked = async {
                let durable = self.core().session(session()).durable().await.map_err(|e| e.to_string())?;
                let transcript = durable.transcript().await.map_err(|e| e.to_string())?;
                if self.cell == Cell::ModelInvalidOutput {
                    let turn = transcript.entries().iter().find_map(|entry| entry.provenance.turn_id.clone()).ok_or("the model turn is retained")?;
                    let calls = durable.read_turn_tool_calls(&turn).await.map_err(|e|e.to_string())?;
                    let [call] = calls.as_slice() else { return Err(format!("one model read result: {calls:?}")); };
                    if !matches!(&call.output.outcome, lash_core::ToolCallOutcome::Failure(failure) if matches!(failure.cause.as_deref(),Some(lash_core::ToolFailureCause::Declaration{..}))) { return Err(format!("READ-OUTPUT-ADMISSION: {call:?}")); }
                    return Ok(());
                }
                if self.cell == Cell::ModelMixed {
                    let turn = transcript.entries().iter().find_map(|entry| entry.provenance.turn_id.clone()).ok_or("a model turn is retained")?;
                    let records = durable.read_turn_tool_calls(&turn).await.map_err(|e| e.to_string())?;
                    let blocks: Vec<_> = transcript.entries().iter().filter_map(|entry| match &entry.item {
                        lash::transcript::TranscriptItem::Message(message) => Some(message.blocks.as_slice()),
                        _ => None,
                    }).flatten().collect();
                    let requested: Vec<_> = blocks.iter().filter_map(|block| match block {
                        lash::transcript::TranscriptBlock::ToolCall { call_id, arguments, .. } => Some((call_id.clone(), arguments.clone())),
                        _ => None,
                    }).collect();
                    let returned: Vec<_> = blocks.iter().filter_map(|block| match block {
                        lash::transcript::TranscriptBlock::ToolResult { call_id, .. } => Some(call_id.clone()),
                        _ => None,
                    }).collect();
                    if requested.iter().map(|(id,_)| id).collect::<Vec<_>>() != returned.iter().collect::<Vec<_>>() || returned.len() != 3 {
                        return Err(format!("MODEL-READ-PAIRED-ORDER: requests {requested:?}, results {returned:?}"));
                    }
                    let args = requested.iter().map(|(_,args)| serde_json::from_str::<Value>(args)).collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?;
                    if args != [json!({"x":1}), json!({"x":91}), json!({"x":2})] { return Err(format!("MODEL-READ-SAVED-ORDER: {args:?}")); }
                    for (slot,x) in [(0,1),(2,2)] {
                        let record = records.iter().find(|record| record.call_id == returned[slot]).ok_or("a saved read slot is not hydrated")?;
                        let answer = self.world.answers(x);
                        if record.args != json!({"x":x}) || record.output.value_for_projection() != oversized_read(*answer.last().ok_or("the read ran")?) {
                            return Err(format!("MODEL-READ-SAVED-ORDER: {record:?}"));
                        }
                        if cut.is_none_or(|cut| cut.fault == Fault::CommitThenAbort) && answer.len() != 1 {
                            return Err(format!("a committed model result was read again: {answer:?}"));
                        }
                    }
                    for slot in [0, 2] {
                        let id = &returned[slot];
                        let presentations = self.world.presentations.lock_recover().get(id).cloned().unwrap_or_default();
                        let x = if slot == 0 { 1 } else { 2 };
                        if presentations.len() != self.world.answers(x).len() || presentations.iter().any(|text| text.len() >= 20_000) {
                            return Err(format!("MODEL-READ-PRESENTATION: saved read {id} bypassed Standard's renderer or repeated presentation: {} presentations", presentations.len()));
                        }
                        let content = blocks.iter().find_map(|block| match block {
                            lash::transcript::TranscriptBlock::ToolResult { call_id, content, .. } if call_id == id => Some(content),
                            _ => None,
                        }).ok_or("a read has its presented result")?;
                        let [lash::transcript::ToolResultBlock::Retained { output }, lash::transcript::ToolResultBlock::Retained { output: rendered }] = content.as_slice() else {
                            return Err(format!("MODEL-READ-RETENTION: missing retained presented output for {id}: {content:?}"));
                        };
                        if !output.witness.starts_with(PRESENTED) || output.witness.len() > 4_096 {
                            return Err("MODEL-READ-RETENTION: the saved witness lost its presentation or bound".into());
                        }
                        if rendered.witness.chars().count() > 16_000 || !rendered.witness.contains("[output cut:") {
                            return Err("MODEL-READ-PRESENTATION: Standard's saved render policy was bypassed".into());
                        }
                        let full = self.backend().attachment_store().get(&rendered.reference.id, 200_000).await.map_err(|e| e.to_string())?;
                        let value: Value = serde_json::from_slice(&full.bytes).map_err(|e| e.to_string())?;
                        if value != oversized_read(*self.world.answers(x).last().ok_or("the read ran")?) {
                            return Err("MODEL-READ-PRESENTATION: Standard's full read artifact was lost".into());
                        }
                        let retained = self.backend().attachment_store().get(&output.reference.id, 200_000).await.map_err(|e| e.to_string())?;
                        let text = String::from_utf8(retained.bytes).map_err(|e| e.to_string())?;
                        if !text.starts_with(PRESENTED) || !text.ends_with(&"p".repeat(70_000)) {
                            return Err("MODEL-READ-RETENTION: the full presented artifact was lost".into());
                        }
                    }
                    let archived: Vec<_> = records.iter().map(|record| &record.call_id).collect();
                    let expected: Vec<_> = returned.iter().filter(|id| records.iter().any(|record| &record.call_id == *id)).collect();
                    if archived != expected { return Err(format!("MODEL-READ-SAVED-ORDER: {records:?}")); }
                    if !self.world.other_bodies().is_empty() || self.world.writes().len() > 1 {
                        return Err("reads entered recorded bodies or a recorded sibling ran twice".into());
                    }
                    return Ok(());
                }
                let cell = transcript.entries().iter().find_map(|entry| match &entry.item { lash::transcript::TranscriptItem::Cell(cell) => Some(cell), _ => None }).ok_or("the failed cell is retained")?;
                let [fact] = cell.read_facts.as_slice() else { return Err(format!("one failed read fact is retained: {:?}", cell.read_facts)); };
                let lash_core::ToolCallOutcome::Failure(failure) = &fact.outcome else { return Err("the refused read succeeded".into()); };
                match self.cell {
                    Cell::Panic => {
                        if !matches!(failure.cause.as_deref(), Some(lash_core::ToolFailureCause::ReadPanicked { panic }) if panic.message == "reader invariant failed") { return Err(format!("READ-PANIC-TYPED: {failure:?}")); }
                        if !self.world.writes().is_empty() { return Err("a guest continued after a reader panic".into()); }
                        if !matches!(&cell.result, lash_core::CellOutcome::Failed(failure) if failure.read_panic.is_some()) { return Err("the owner lost terminal reader panic evidence".into()); }
                    }
                    Cell::InvalidOutput => {
                        if !matches!(failure.cause.as_deref(), Some(lash_core::ToolFailureCause::Declaration { .. })) { return Err(format!("READ-OUTPUT-ADMISSION: {failure:?}")); }
                    }
                    Cell::Pending => {
                        if failure.code != "read_timed_out" { return Err(format!("READ-INJECTED-CLOCK: {failure:?}")); }
                    }
                    _ => unreachable!(),
                }
                Ok::<(), String>(())
            }.await;
            if let Err(error) = checked {
                violations.push(error);
            }
        }

        if self.cell == Cell::ReadWriteRead {
            let checked = async {
                let durable = self
                    .core()
                    .session(session())
                    .durable()
                    .await
                    .map_err(|error| error.to_string())?;
                let transcript = durable
                    .transcript()
                    .await
                    .map_err(|error| error.to_string())?;
                let facts: Vec<_> = transcript
                    .entries()
                    .iter()
                    .filter_map(|entry| match &entry.item {
                        lash::transcript::TranscriptItem::Cell(cell) => {
                            Some(cell.read_facts.as_slice())
                        }
                        _ => None,
                    })
                    .flatten()
                    .cloned()
                    .collect();
                let actual: Vec<_> = facts
                    .iter()
                    .map(|fact| {
                        (
                            fact.ordinal,
                            written_x(&fact.args).unwrap(),
                            fact.record().output.value_for_projection(),
                        )
                    })
                    .collect();
                let first = self
                    .world
                    .answers(1)
                    .last()
                    .copied()
                    .ok_or("the first read ran")?;
                let second = self.world.answers(2).last().copied();
                let completed = transcript.entries().iter().any(|entry| matches!(&entry.item, lash::transcript::TranscriptItem::Cell(cell) if matches!(cell.result, lash_core::CellOutcome::Completed)));
                if completed && second.is_none() { return Err("a successful cell omitted its second read".into()); }
                let mut expected = vec![(1, 1, json!({"v":first}))];
                if let Some(second) = second { expected.push((2, 2, json!({"v":second}))); }
                if actual != expected {
                    return Err(format!(
                        "READ-SAVED-FACT-EXACT: {actual:?}, expected {expected:?}"
                    ));
                }
                let entry = transcript
                    .entries()
                    .iter()
                    .find(|entry| matches!(entry.item, lash::transcript::TranscriptItem::Cell(_)))
                    .ok_or("a cell is retained")?;
                let turn_id = entry
                    .provenance
                    .turn_id
                    .clone()
                    .ok_or("the cell retains its turn")?;
                for fact in facts {
                    let call = lash::TurnToolCallRef {
                        turn_id: turn_id.clone(),
                        call_id: fact.call_id.clone(),
                    };
                    match durable
                        .read_tool_call(&call)
                        .await
                        .map_err(|error| error.to_string())?
                    {
                        lash::ToolCallRead::Settled { record } if *record == fact.record() => {}
                        actual => {
                            return Err(format!(
                                "a caller-saved read fact is not hydrated: {actual:?}"
                            ));
                        }
                    }
                }
                Ok::<_, String>(())
            }
            .await;
            if let Err(error) = checked {
                violations.push(error);
            }
        }
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
async fn a_saved_read_and_its_fact_survive_reload_exactly_once_on_sqlite_memory() {
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

/// What a probe node serving `serves` and offering `capabilities` reads as the session's claim
/// requirements when it claims it; `None` when it cannot claim it. A
/// claimed session is released ready, untouched.
async fn probe(
    database: &Arc<dyn DurableStore>,
    decodes: Vec<lash_durable::FormatSet>,
    serves: Vec<String>,
    capabilities: lash_durable::NodeCapabilities,
) -> Option<Vec<String>> {
    use lash_durable::{NodeId, NodeSpec, Release};
    let node = database
        .register_node(&NodeSpec {
            node: NodeId::new("cell-read-probe"),
            decodes,
            serves,
            capabilities,
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
    let capabilities = lash::testing::node_capabilities(&turn.core());
    let deadline = u64::try_from(due.0).unwrap() - SimClock::timestamp_ms_at(0);
    clock.advance_to(deadline + 1_000).await;
    let without: Vec<String> = serves
        .iter()
        .filter(|served| **served != token)
        .cloned()
        .collect();
    let mut without_reader = capabilities.clone();
    without_reader.callbacks.retain(|required| {
        token
            != lash_vocabulary::claims::ObligationToken::for_callback(
                required.registry,
                &required.callback,
            )
    });
    assert_eq!(
        probe(&database, decodes.clone(), without, without_reader).await,
        None,
        "a node that does not serve the reader does not claim the cell that plans reads"
    );
    let requires = probe(&database, decodes, serves, capabilities)
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

/// READ-LIVE-ACTUAL: a loop and its taken branch emit the four settled
/// reads, including the failing third loop read. An untaken branch emits none.
#[tokio::test]
async fn actual_reads_stream_their_caller_arguments_and_outcomes_on_sqlite_memory() {
    let turn = ReadTurn::new(Cell::ReadActivity, Law::Counted);
    let clock = SimClock::new();
    let database = turn.database(Arc::clone(&clock)).await;
    let nodes = SimNodes::new(
        database,
        clock,
        Script::new(),
        turn.config(),
        turn.activation(),
    );
    let session = turn
        .core()
        .session(session())
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            MODEL,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(64),
            lash_core::NoProgressBudget::bounded(12),
        )))
        .await
        .unwrap();
    let handle = session
        .send(lash::TurnInput::text("run the cell"))
        .await
        .unwrap();
    nodes.start("a");
    let (output, ()) = tokio::join!(handle.output(), async {
        while !turn.done(&nodes).await {
            assert!(nodes.step().await.is_some(), "the turn must finish");
        }
        nodes.quiesce().await;
    });
    let output = output.unwrap();
    let events: Vec<Value> = output
        .activities
        .iter()
        .map(|activity| serde_json::to_value(&activity.event).unwrap())
        .collect();
    let caller = events
        .iter()
        .find(|event| event["type"] == "code_block_started")
        .expect("the code block started")["graph_key"]
        .clone();
    assert!(
        caller.is_string(),
        "the enclosing code block has a graph key"
    );
    let reads: Vec<_> = events
        .iter()
        .filter(|event| event["execution"] == "read")
        .map(|event| {
            json!({
                "type": event["type"], "graph_key": event["graph_key"],
                "caller": event["read"]["caller"], "ordinal": event["read"]["ordinal"],
                "name": event["name"], "provider": event["read"]["provider"], "args": event["args"],
                "outcome": event["output"]["outcome"]["status"],
            })
        })
        .collect();
    let mut expected = Vec::new();
    for (ordinal, x, answered) in [(1, 1, true), (2, 10, true), (3, 2, true), (4, 3, false)] {
        for (kind, outcome) in [
            ("tool_call_started", Value::Null),
            (
                "tool_call_returned",
                json!(if answered { "success" } else { "failure" }),
            ),
        ] {
            expected.push(json!({
                "type": kind, "graph_key": caller,
                "caller": {"kind":"code_block", "graph_key":caller},
                "ordinal":ordinal, "name":READ,
                "provider":{"registry":"session", "owner":READER, "key":"reads"},
                "args":{"x":x}, "outcome":outcome,
            }));
        }
    }
    assert_eq!(
        reads, expected,
        "actual reads use the ordinary start/return family"
    );
    let mut ids = std::collections::BTreeSet::new();
    let read_events: Vec<_> = events
        .iter()
        .filter(|event| event["execution"] == "read")
        .collect();
    for pair in read_events.as_chunks::<2>().0 {
        let id = pair[0]["call_id"].as_str().unwrap();
        assert_eq!(pair[0]["call_id"], pair[1]["call_id"]);
        assert!(ids.insert(id));
        assert!(
            !events
                .iter()
                .any(|event| event["type"] == "tool_call_completed" && event["call_id"] == id)
        );
        lash::ToolCallId::parse(id).expect("a read has a valid ToolCallId");
    }
    let returned: Vec<_> = read_events.iter().skip(1).step_by(2).collect();
    assert_eq!(
        returned
            .iter()
            .map(|event| event["read"]["args_bytes"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [7, 8, 7, 7]
    );
    assert_eq!(
        returned
            .iter()
            .map(|event| event["read"]["answer_bytes"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [7, 7, 7, 0]
    );
    assert_eq!(returned[0]["read"]["site"], returned[2]["read"]["site"]);
    assert_eq!(returned[0]["read"]["site"], returned[3]["read"]["site"]);
    assert_ne!(returned[0]["read"]["site"], returned[1]["read"]["site"]);
    let last = read_events.last().unwrap();
    assert_eq!(
        last["output"]["outcome"]["payload"]["message"],
        "page unavailable"
    );
    assert_eq!(turn.world.reads(), [(1, 1), (10, 2), (2, 3), (3, 4)]);
}

/// READ-PANIC-TERMINAL: catch cannot continue a cell after its reader panics.
#[tokio::test]
async fn a_reader_panic_stops_the_cell_before_guest_catch_on_sqlite_memory() {
    Matrix::new()
        .faults(&[])
        .run_test(|| ReadTurn::new(Cell::Panic, Law::Counted))
        .await
        .assert_held();
}

/// READ-OUTPUT-ADMISSION: schema-only constraints have the recorded declaration cause.
#[tokio::test]
async fn a_read_refuses_an_undeclared_negative_output_on_sqlite_memory() {
    for cell in [Cell::InvalidOutput, Cell::ModelInvalidOutput] {
        Matrix::new()
            .faults(&[])
            .run_test(|| ReadTurn::new(cell, Law::Counted))
            .await
            .assert_held();
    }
}

/// READ-INJECTED-CLOCK: advancing the injected tool deadline ends a pending read.
#[tokio::test]
async fn a_pending_cell_read_times_out_on_the_injected_clock_on_sqlite_memory() {
    Matrix::new()
        .faults(&[])
        .horizon(Duration::from_secs(600))
        .run_test(|| ReadTurn::new(Cell::Pending, Law::Counted))
        .await
        .assert_held();
}

/// MODEL-READ-SAVED-ORDER: a mixed batch recovers reads and recorded siblings under one saved plan.
#[tokio::test]
async fn a_mixed_model_round_restores_saved_read_slots_on_sqlite_memory() {
    Matrix::new()
        .faults(&[Fault::Abort, Fault::CommitThenAbort])
        .labels(&[
            CommitLabel::ROUND_OUTCOME,
            CommitLabel::ROUND_PRESENT_MODEL_START,
            CommitLabel::ROUND_OUTCOME_PRESENT_MODEL_START,
        ])
        .horizon(Duration::from_secs(600))
        .run_test(|| ReadTurn::new(Cell::ModelMixed, Law::Counted))
        .await
        .assert_held();
}

/// READ-HOOK-BINDING: hooks deny the same Read declaration in a model round and cell.
#[tokio::test]
async fn a_denying_hook_prevents_cell_and_model_reads_on_sqlite_memory() {
    for cell in [Cell::Hooked, Cell::HookedModel] {
        Matrix::new()
            .faults(&[])
            .run_test(|| ReadTurn::new(cell, Law::Counted))
            .await
            .assert_held();
    }
}

/// KERNEL-BOUND-FACT: cell evidence retains the receipt and caps charged consumption.
#[tokio::test]
async fn a_cell_charge_bound_retains_limit_observation_and_consumption_on_sqlite_memory() {
    let report = Matrix::new()
        .faults(&[Fault::CommitThenAbort])
        .labels(&[CommitLabel::CELL_SNAPSHOT])
        .horizon(Duration::from_secs(600))
        .run_test(|| ReadTurn::new(Cell::Bound, Law::Counted))
        .await;
    assert!(
        report
            .cells
            .iter()
            .any(|cell| cell.point.label == CommitLabel::CELL_SNAPSHOT
                && cell.fault == Fault::CommitThenAbort),
        "the saved end must be taken over"
    );
    report.assert_held();
}
