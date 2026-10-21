//! The vertical crash proof, ADR 0132 §15's first kill criterion (V0,
//! FIG-5170), on the production turn driver (L3, FIG-5172).
//!
//! A lash core's session holds one sent input. Its turn runs on the
//! production session activation with the core's own turn services: the
//! session's runtime, its protocol, the scripted model the core serves and
//! a host tool `ext_write`, declared `Once`, whose body writes to an
//! [`ExternalWorld`] that survives every node, as the outside world would.
//! The nodes are simulated (A and B) over the production durable store.
//! Two protocols run the turn:
//!
//! - **Code:** the RLM protocol. The model answers with a TypeScript cell
//!   that calls `tools.ext_write({ x: 7 })` and prints the answer; the cell
//!   runs through the code mode worker path on the durable snapshot store, and
//!   once its printout is in the transcript the model answers in prose.
//! - **Tools:** the standard protocol. The model calls `ext_write`
//!   natively, the turn's tool round runs it, and once its result is in the
//!   transcript the model answers in prose.
//!
//! The matrix cuts the uncut run at every labelled write, under
//! fail-before, ack-hidden, zombie, abort and commit-then-abort, recovers on
//! the other node, and checks the laws:
//!
//! - P1: every call reached the outside world at most once, and every
//!   admitted body was entered at most once;
//! - P2 (K1, abort at `round.outcome`): the operation's outcome is
//!   `Interrupted`, and the model was told so;
//! - P3 (K2, commit-then-abort at `round.outcome`): the saved `Completed`
//!   value reached the model, the body ran once;
//! - P4: no hidden replay: a cell's program is entered fresh only before its
//!   first snapshot commits, a claim restores the turn at most once, no
//!   outcome is looked up for re-running code and no committed ordinal is
//!   emitted again;
//! - P5: a zombie's writes after its reap are refused;
//! - P7: the session head advanced exactly once, and no row stays bound to
//!   the ended run.
//!
//! A resumed turn commits the same protocol history as an uncut one: a node
//! killed after the second model call's `model.start`, whose checkpoint has
//! consumed the cell's protocol records through a progress boundary, leaves
//! a turn whose resumed commit holds every one of them (FIG-5229).
//!
//! A protocol record appended while a tool call is unanswered is committed
//! with the turn, uncut and resumed alike (FIG-5588): the **Noted** protocol
//! is a tool round whose driver appends one beside the model's call.
//!
//! A turn's commit holds on the session every attachment its committed
//! history references, wherever the turn was cut (FIG-5318): the
//! **Archived** protocol is the code turn whose cell prints past the
//! deployment's inline limit, so its prints are one archive that only its
//! committed cell record names.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/dialect.rs"]
mod dialect;
#[path = "support/noted.rs"]
mod noted;
#[path = "support/sim.rs"]
mod sim;

#[path = "support/matrix.rs"]
mod matrix;
#[path = "vertical_crash_proof/run_config.rs"]
mod run_config;

use lash_protocol_types::{llm::types::StreamBlockEvent, llm::types::StreamBlockKind};
use matrix::MatrixTestExt as _;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{LlmRequest, LlmResponse, LlmStreamEvent, StreamBlockIdentity};
use lash_core::runtime::durable::session::SessionActivation;
use lash_core::{ExecutionPolicy, LlmOutputPart, ToolCall, ToolCallId, ToolOutcome};
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableError, DurableStore};
use lash_durable_test::{
    Cut, Fault, Life, Matrix, Scenario, Script, SimClock, SimNodes, SimNodesConfig, Stored,
    Tripwire, Verdict, WriteKind,
};
use lash_execution_contracts::backend::Backend;
use lash_execution_contracts::backend::BackendParts;
use lash_execution_contracts::backend::StoreSet;
use lash_execution_contracts::projection::NoProjectionProviders;
use lash_protocol_types::sync::MutexExt as _;
use lash_protocol_types::{SessionId, TurnId};

use dialect::Dialect;

const SESSION: &str = "v0-session";
const RUN: &str = "v0-turn";
const TOOL: &str = "ext_write";
const TOOL_OPERATION: &str = "tools.ext_write";
const MODEL: &str = "v0-model";

/// The cell the model writes: one `Once` host operation, then a printout of
/// what it answered.
const CELL: &str = "<typescript>\nconst written = await tools.extWrite({ x: 7 });\nconsole.log(written);\n</typescript>";
/// The cell [`Protocol::Archived`]'s model writes: [`CELL`]'s operation and
/// printout, then prints past [`ARCHIVE_RETENTION`]'s inline limit.
const ARCHIVED_CELL: &str = "<typescript>\nconst written = await tools.extWrite({ x: 7 });\nconsole.log(written);\nfor (let i = 0; i < 64; i++) { console.log('archived print ' + i + ' of the cell output'); }\n</typescript>";
/// The output retention [`Protocol::Archived`]'s deployment runs under: its
/// cell's prints exceed the inline limit, and the witness keeps the
/// operation's answer.
const ARCHIVE_RETENTION: lash_core::OutputRetentionPolicy = lash_core::OutputRetentionPolicy {
    shape: lash_core::ValueShapeLimits::standard(),
    inline_limit_bytes: 1024,
    witness_bytes: 512,
};
/// What marks the operation's answer in the transcript.
const WROTE: &str = "wrote";
/// What the final answer starts with.
const FINAL: &str = "final answer from ";

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

fn run() -> TurnId {
    TurnId::try_from(RUN.to_owned()).unwrap()
}

fn actor() -> ActorKey {
    ActorKey::session(SESSION).unwrap()
}

/// Which protocol runs the turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol {
    /// The RLM protocol: the operation runs inside a code cell.
    Code,
    /// The standard protocol: the operation runs in a tool round.
    Tools,
    /// A tool round whose protocol appends a record of its own while the
    /// model's call is unanswered ([`noted`]).
    Noted,
    /// [`Protocol::Noted`] over two tool rounds: two progress boundaries
    /// that each carry a protocol record.
    NotedTwice,
    /// [`Protocol::Code`] whose cell's prints are retained out of history
    /// as one archive.
    Archived,
    /// The session-capture law (FIG-6418 L3): a host-set default global,
    /// a first turn binding it and a second turn's cell reading it. Its
    /// calls are scripted by ordinal, not by [`Protocol::rounds`].
    Defaults,
    /// Two cells in one turn (PC-WORKING): A binds a text and a function
    /// its report never shows, around the operation; after a model wait B
    /// reads them, and ends as [`Second`] says.
    Working(Second),
}

/// How [`Protocol::Working`]'s second cell ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Second {
    /// It binds a total of A's text and function.
    Succeeds,
    /// It binds a partial value, then throws.
    Fails,
}

/// The text cell A binds: longer than any preview of it, and made at run
/// time, so not even A's source holds it whole.
fn carried() -> String {
    "carried ".repeat(250)
}

/// Cell A: binds [`carried`] and a function, then calls the operation,
/// whatever its answer.
const WORKING_FIRST_CELL: &str = "<typescript>\nlet carried_text = 'carried '.repeat(250);\nfunction twice_of(n) { return n * 2; }\nlet written = null;\ntry { written = await tools.extWrite({ x: 7 }); } catch (error) { written = String(error); }\nconsole.log(written);\n</typescript>";

/// The session global the host binds before [`Protocol::Defaults`]' turns
/// (FIG-6418 L3).
const HOST_DEFAULT: &str = "fig6418_default";
/// [`Protocol::Defaults`]' first turn's cell: binds the host's default.
const DEFAULTS_FIRST_CELL: &str =
    "<typescript>\nconst bound_first = fig6418_default;\nconsole.log(bound_first);\n</typescript>";
/// Its second turn's cell: writes the default to the outside world and
/// finishes with it.
const DEFAULTS_SECOND_CELL: &str = "<typescript>\nconst seen = fig6418_default;\nawait tools.extWrite({ x: seen });\nawait control.finish(seen);\n</typescript>";
/// The turn [`RUN_TWO`] is sent under.
const RUN_TWO: &str = "v0-turn-2";

/// Cell B, as [`Second`] says.
fn working_second_cell(second: Second) -> &'static str {
    match second {
        Second::Succeeds => {
            "<typescript>\nlet second_total = twice_of(carried_text.length);\nconsole.log('second total ' + second_total);\n</typescript>"
        }
        Second::Fails => {
            "<typescript>\nlet second_partial = twice_of(carried_text.length);\nthrow new Error('the second cell fails at ' + second_partial);\n</typescript>"
        }
    }
}

impl Protocol {
    /// How many times the scripted model calls the operation before it
    /// answers in prose.
    fn rounds(self) -> usize {
        match self {
            Self::Code | Self::Tools | Self::Noted | Self::Archived => 1,
            Self::NotedTwice | Self::Working(_) => 2,
            // Defaults scripts every call by its ordinal itself.
            Self::Defaults => usize::MAX,
        }
    }
}

/// The outside world: what `ext_write` wrote, per call. It survives every
/// node, so a write a crash cannot undo is visible to the laws.
#[derive(Debug, Default)]
struct ExternalWorld {
    writes: Mutex<BTreeMap<ToolCallId, Vec<serde_json::Value>>>,
}

impl ExternalWorld {
    fn write(&self, call: &ToolCallId, value: serde_json::Value) {
        self.writes
            .lock_recover()
            .entry(call.clone())
            .or_default()
            .push(value);
    }

    fn writes(&self) -> BTreeMap<ToolCallId, Vec<serde_json::Value>> {
        self.writes.lock_recover().clone()
    }
}

/// `ext_write`'s body: it writes to the outside world and answers what it
/// wrote.
struct ExtWrite {
    world: Arc<ExternalWorld>,
}

#[async_trait::async_trait]
impl StaticToolExecute for ExtWrite {
    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.world.write(call.context.call_id(), call.args.clone());
        ToolOutcome::ok(serde_json::json!({ "ok": true, WROTE: call.args })).into()
    }
}

fn ext_write(world: Arc<ExternalWorld>) -> Arc<dyn lash_core::ToolProvider> {
    let definition = lash_core::ToolDefinition::raw(
        TOOL,
        TOOL_OPERATION,
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
    .with_execution(std::time::Duration::from_secs(120))
    .with_execution_policy(ExecutionPolicy::Once);
    Arc::new(StaticToolProvider::new(
        vec![definition],
        ExtWrite { world },
    ))
}

/// What the model saw on each call, across every node.
#[derive(Debug, Default)]
struct Seen {
    /// Each request, rendered.
    requests: Vec<String>,
}

/// The scripted model: before the transcript holds the operation's answer
/// it calls the operation (a cell, or a native call); after, it answers in
/// prose that quotes the answer.
fn model(protocol: Protocol, seen: Arc<Mutex<Seen>>) -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("v0-scripted")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| {
            let seen = Arc::clone(&seen);
            async move {
                let rendered = serde_json::to_string(&request.messages).expect("a request encodes");
                seen.lock_recover().requests.push(rendered.clone());
                // Each of the model's own calls is in the transcript once
                // the operation answered it, however it ended.
                let called = request
                    .messages
                    .iter()
                    .filter(|message| message.role == lash_core::llm::types::LlmRole::Assistant)
                    .count();
                let response = match (protocol, called) {
                    // Turn one's cell binds the host's default; the second
                    // send's cell reads it back after the armed cut lands.
                    (Protocol::Defaults, 0) => text(&request, DEFAULTS_FIRST_CELL),
                    (Protocol::Defaults, 2) => text(&request, DEFAULTS_SECOND_CELL),
                    (Protocol::Defaults, _) => text(&request, "the session answered"),
                    _ => match (called >= protocol.rounds(), protocol) {
                        (true, _) => {
                            let told = if rendered.contains(WROTE) {
                                "the write completed"
                            } else {
                                "the write did not complete"
                            };
                            text(&request, &format!("{FINAL}{told}"))
                        }
                        (false, Protocol::Code) => text(&request, CELL),
                        (false, Protocol::Archived) => text(&request, ARCHIVED_CELL),
                        (false, Protocol::Working(second)) => match called {
                            0 => text(&request, WORKING_FIRST_CELL),
                            _ => text(&request, working_second_cell(second)),
                        },
                        (false, Protocol::Tools | Protocol::Noted | Protocol::NotedTwice) => {
                            LlmResponse {
                                parts: vec![LlmOutputPart::ToolCall {
                                    call_id: match called {
                                        0 => "v0-call".to_owned(),
                                        round => format!("v0-call-{round}"),
                                    },
                                    tool_name: TOOL_OPERATION.replace('.', "__"),
                                    input_json: r#"{"x":7}"#.to_owned(),
                                    replay: None,
                                }],
                                ..LlmResponse::default()
                            }
                        }
                        (_, Protocol::Defaults) => {
                            unreachable!("Defaults scripts every call by ordinal")
                        }
                    },
                };
                Ok(response)
            }
        })
        .build()
        .into_handle()
}

/// A text answer, streamed as one delta.
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

fn metadata() -> lash_core::LlmProfileMetadata {
    lash_core::LlmProfileMetadata::builder(MODEL)
        .cache_retention(lash_core::provider::CacheRetention::Short)
        .context_window_tokens(200_000)
        .build()
        .expect("the model's metadata")
}

/// One deployment's core over `backend`: it serves no node of its own, the
/// scenario's simulated nodes run its sessions' turns.
fn core(
    protocol: Protocol,
    backend: &Backend,
    clock: &Arc<SimClock>,
    world: &Arc<ExternalWorld>,
    seen: &Arc<Mutex<Seen>>,
) -> lash::LashCore {
    let builder = match protocol {
        Protocol::Code | Protocol::Archived | Protocol::Defaults | Protocol::Working(_) => {
            lash::LashCore::rlm_builder(
                backend.clone(),
                lash::rlm::RlmProtocolPluginFactory::new(
                    lash::rlm::RlmProtocolPluginConfig::builder()
                        .channel(lash::rlm::RlmChannel::Cell)
                        .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                        .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                        .build(),
                    lash::rlm::CellDialect::typescript(),
                )
                .with_worker_service(sim::workers(clock)),
            )
        }
        Protocol::Tools => lash::LashCore::standard_builder(backend.clone()),
        Protocol::Noted | Protocol::NotedTwice => lash::LashCore::builder(backend.clone())
            .protocol_plugin(Arc::new(noted::NotedProtocolFactory)),
    };
    builder
        .serve_sessions(false)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(match protocol {
            Protocol::Archived => lash::DataRetention {
                attachments: lash::persistence::AttachmentPolicy {
                    output_retention: ARCHIVE_RETENTION,
                    ..lash::persistence::AttachmentPolicy::standard()
                },
                ..lash::DataRetention::standard()
            },
            _ => lash::DataRetention::standard(),
        })
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(model(protocol, Arc::clone(seen)), metadata())
        .tools(
            lash::tool_provider_key!("tools"),
            ext_write(Arc::clone(world)),
        )
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("v0-deployment"),
            lash::persistence::LeaseIncarnationId::new("v0-boot"),
        ))
        .expect("the core builds")
}

/// The runtime core's backend over `stores`, its actors in the shipped
/// build's format sets: the core's own formats and the VM's.
fn shipped_backend(stores: Arc<dyn StoreSet>) -> Backend {
    Backend::assemble(BackendParts {
        stores,
        settings: sim::settings(),
        engines: Vec::new(),
        providers: Arc::new(NoProjectionProviders),
        formats: lash::formats::actor_state_surfaces(),
    })
    .expect("the V0 backend assembles")
}

/// The scenario on one protocol and one dialect, fresh for every matrix
/// cell.
struct V0 {
    protocol: Protocol,
    dialect: Dialect,
    postgres_url: Option<String>,
    world: Arc<ExternalWorld>,
    seen: Arc<Mutex<Seen>>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<Backend>>,
    /// The virtual clock the database was built on, which the core's VM
    /// worker calls hold.
    clock: Mutex<Option<Arc<SimClock>>>,
    core: Mutex<Option<lash::LashCore>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
    /// Every page a candidate published and every node a commit staged:
    /// where the working-state law reads the committed execution's paged
    /// texts.
    published: Arc<Mutex<Published>>,
}

impl V0 {
    fn new(protocol: Protocol, dialect: Dialect, postgres_url: Option<String>) -> Self {
        Self {
            protocol,
            dialect,
            postgres_url,
            world: Arc::default(),
            seen: Arc::default(),
            tripwire: Arc::default(),
            backend: Mutex::default(),
            clock: Mutex::default(),
            core: Mutex::default(),
            keep: Mutex::default(),
            published: Arc::default(),
        }
    }

    fn backend(&self) -> Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }

    /// The deployment's core, built on first use over the scenario's
    /// backend.
    fn core(&self) -> lash::LashCore {
        let backend = self.backend();
        let clock = self
            .clock
            .lock_recover()
            .clone()
            .expect("the database is built first");
        self.core
            .lock_recover()
            .get_or_insert_with(|| core(self.protocol, &backend, &clock, &self.world, &self.seen))
            .clone()
    }
}

#[async_trait::async_trait]
impl Scenario for V0 {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        *self.clock.lock_recover() = Some(Arc::clone(&clock));
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
        let published = Arc::clone(&self.published);
        nodes
            .script()
            .observe_candidates(Arc::new(move |_, _, pages| {
                published.lock_recover().insert(pages);
            }));
        let published = Arc::clone(&self.published);
        nodes.script().observe_commits(Arc::new(move |_, tx| {
            let staged: Vec<_> = tx.graph().values().cloned().collect();
            published.lock_recover().insert(&staged);
        }));
        self.send().await?;
        // A starts and claims first, B once A is settled: on a database that
        // runs the two claims concurrently, either could win a race, and
        // the matrix cuts the uncut run's writes by node.
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
        self.laws(nodes, cut).await
    }
}

impl V0 {
    /// A new process over this deployment's database: the same outside
    /// world, model and tripwire, nothing else of this deployment.
    fn cold(&self, postgres_url: Option<String>) -> Self {
        Self {
            protocol: self.protocol,
            dialect: self.dialect,
            postgres_url,
            world: Arc::clone(&self.world),
            seen: Arc::clone(&self.seen),
            tripwire: Arc::clone(&self.tripwire),
            backend: Mutex::new(Some(self.backend())),
            clock: Mutex::new(self.clock.lock_recover().clone()),
            core: Mutex::default(),
            keep: Mutex::default(),
            published: Arc::clone(&self.published),
        }
    }

    /// Step `nodes` until the session is idle, within 600 s of virtual time.
    async fn run_until_done(&self, nodes: &SimNodes, clock: &SimClock, what: &str) {
        let horizon = clock.logical_ms() + 600_000;
        while !self.done(nodes).await {
            assert!(
                clock.logical_ms() < horizon,
                "{what} is not done after 600 s of virtual time:\n{}",
                nodes.script().rendered_trace()
            );
            assert!(
                nodes.step().await.is_some(),
                "{what} stalled:\n{}",
                nodes.script().rendered_trace()
            );
        }
        nodes.quiesce().await;
    }

    /// The history the session's committed head holds, messages and protocol
    /// records in commit order.
    async fn committed_history(&self) -> Vec<lash_core::SessionHistoryRecord> {
        let view = self
            .core()
            .session(session())
            .durable()
            .await
            .expect("the durable session resolves")
            .read()
            .await
            .expect("the session's head reads")
            .expect("the session has a head");
        view.active_events().to_vec()
    }

    /// Create the session and send it the turn's input through the core,
    /// uncut: the host is outside the deployment under test.
    async fn send(&self) -> Result<(), String> {
        let core = self.core();
        let session = core
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
            .send(lash::TurnInput::text(
                "write x, then tell me what was written",
            ))
            .id(run())
            .await
            .map(drop)
            .map_err(|error| format!("send the turn's input: {error}"))
    }

    /// The scenario's laws after a run, cut at `cut`.
    async fn laws(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let database = nodes.database();
        let trace = nodes.script().trace();
        let counts = self.tripwire.counts();
        let seen = std::mem::take(&mut self.seen.lock_recover().requests);

        // P1: the outside world saw each call at most once, and no admitted
        // body was entered twice.
        let writes = self.world.writes();
        for (call, entries) in &writes {
            if entries.len() > 1 {
                violations.push(format!(
                    "P1: call {call} reached the world {} times",
                    entries.len()
                ));
            }
        }
        for (id, entered) in &counts.bodies {
            if *entered > 1 {
                violations.push(format!("P1: body {id:?} entered {entered} times"));
            }
        }

        // The turn ended, and nothing stays bound to it.
        match database.turn(&session()).await {
            Ok(None) => {}
            other => violations.push(format!("the turn did not end: {other:?}")),
        }
        match database.session_mailbox(&session()).await {
            Ok(mailbox) if mailbox.bound_run.is_none() => {}
            other => violations.push(format!("the ended run still holds its rows: {other:?}")),
        }

        // What the model was told: the last call saw the operation's
        // answer, completed or not, and the model called it exactly once.
        let calls = seen
            .iter()
            .filter(|request| !request.contains("\"Assistant\""))
            .count();
        let told = seen.last().cloned().unwrap_or_default();
        let completed = told.contains(WROTE);
        if seen.is_empty() || calls == seen.len() {
            let ended = database.turn_end(&session(), &run()).await;
            violations.push(format!(
                "the model never saw the operation end: {seen:?}; the turn ended {ended:?}"
            ));
        }
        if completed && writes.values().map(Vec::len).sum::<usize>() != 1 {
            violations.push(format!(
                "P3: a completed write reached the world {} times",
                writes.len()
            ));
        }

        // P7: the head advanced exactly once.
        let commits = trace
            .iter()
            .filter(|write| write.point.label == CommitLabel::TURN_COMMIT && write.committed())
            .count();
        if commits != 1 {
            violations.push(format!("P7: the turn committed {commits} times"));
        }

        // P4: no hidden replay. A cell's program is entered fresh only by an
        // owner that found no snapshot: once more for each first snapshot
        // that did not commit, and once more when the owner running it was
        // lost before any snapshot committed. The first snapshot commits
        // the model response that asked for the cell (FIG-6000).
        let admits = |label: CommitLabel| {
            label == CommitLabel::CELL_SNAPSHOT_ADMIT
                || label == CommitLabel::MODEL_DONE_CELL_SNAPSHOT_ADMIT
        };
        let unsnapshotted = trace
            .iter()
            .filter(|write| admits(write.point.label) && !write.committed())
            .count();
        let first_snapshot = trace
            .iter()
            .position(|write| admits(write.point.label) && write.committed());
        let lost_before_snapshot = cut
            .filter(|cut| cut.fault.kills() || cut.fault == Fault::Zombie)
            .and_then(|cut| {
                trace
                    .iter()
                    .position(|write| write.node == cut.node && write.point == cut.point)
            })
            .is_some_and(|at| first_snapshot.is_none_or(|first| at < first));
        let programs: usize = counts.vm_programs.values().sum();
        // A working turn's second cell never parks: it runs again under each
        // owner that took the turn over before its end was consumed.
        let second_cell = match self.protocol {
            Protocol::Working(_) => 1 + owners(&trace),
            _ => 0,
        };
        if programs > 1 + second_cell + unsnapshotted + usize::from(lost_before_snapshot) {
            violations.push(format!(
                "P4: the cell's program was entered fresh {programs} times with {unsnapshotted} uncommitted snapshots: {:?}",
                counts.vm_programs
            ));
        }
        if matches!(
            self.protocol,
            Protocol::Code | Protocol::Archived | Protocol::Working(_)
        ) && programs == 0
        {
            violations.push("the turn's cell never ran".to_owned());
        }
        if self.protocol == Protocol::Archived {
            violations.extend(self.archive_laws(completed).await);
        }
        if let Protocol::Working(second) = self.protocol {
            violations.extend(self.working_laws(second, &seen, &told).await);
        }
        if counts.outcome_lookups.values().sum::<usize>() != 0 {
            violations.push("P4: an outcome was looked up for re-running code".to_owned());
        }
        if counts.committed_ordinals.values().sum::<usize>() != 0 {
            violations.push("P4: a committed ordinal was emitted again".to_owned());
        }
        let restores = counts
            .restores
            .get(&(session(), run()))
            .copied()
            .unwrap_or(0);
        // A turn is restored only by an owner taking it over, or by a pass
        // that failed before it committed (a lost acknowledgement, a crashed
        // VM worker) and reloads the rows: at most once per owner.
        let owners = owners(&trace);
        if restores > owners.max(1) {
            violations.push(format!(
                "P4: the turn was restored {restores} times by {owners} owners"
            ));
        }

        if let Some(cut) = cut {
            violations.extend(kill_laws(self.protocol, cut, completed, &writes));
            violations.extend(zombie_laws(cut, &trace));
        }
        violations
    }
}

impl V0 {
    /// FIG-5318: the turn's commit held on the session every print archive
    /// its committed history names. A cell whose operation `completed`
    /// printed past the inline limit, so its record names one; a cell
    /// whose operation was interrupted printed nothing.
    async fn archive_laws(&self, completed: bool) -> Vec<String> {
        let history =
            serde_json::to_value(self.committed_history().await).expect("the history encodes");
        let mut archives = Vec::new();
        retained_prints(&history, &mut archives);
        let mut violations = Vec::new();
        if completed && archives.len() != 1 {
            violations.push(format!(
                "the completed cell's committed record names {} print archives, not one",
                archives.len()
            ));
        }
        for archive in &archives {
            let referrers = self
                .backend()
                .attachment_referrers()
                .attachment_referrers(archive)
                .await
                .expect("read the archive's referrers");
            if !referrers.iter().any(|referrer| {
                matches!(referrer, lash_core::ArtifactReferrer::Session(id) if id.as_str() == SESSION)
            }) {
                violations.push(format!(
                    "the turn's commit did not hold its cell's archive on the session: {referrers:?}"
                ));
            }
        }
        violations
    }
}

/// The owners that took the session: it is the scenario's one actor, so
/// every claim that took it is one.
fn owners(trace: &[lash_durable_test::Write]) -> usize {
    trace
        .iter()
        .filter(|write| {
            write.point.label == CommitLabel::CLAIM
                && matches!(write.stored, Stored::Committed { effective: true })
        })
        .count()
}

impl V0 {
    /// PC-WORKING (FIG-5887 §6): A's text and function are its working
    /// state, whole whatever its report shows, through the model wait to B
    /// and the turn's commit. The model never saw the text whole, yet B read
    /// it whole; the session's committed execution holds A's bindings and
    /// B's total, or, when B failed, A's bindings and nothing of B.
    async fn working_laws(&self, second: Second, seen: &[String], told: &str) -> Vec<String> {
        let mut violations = Vec::new();
        let carried = carried();
        if seen.iter().any(|request| request.contains(&carried)) {
            violations.push("PC-WORKING: the model was shown A's text whole".to_owned());
        }
        let total = format!("second total {}", 2 * carried.len());
        match second {
            Second::Succeeds if !told.contains(&total) => violations.push(format!(
                "PC-WORKING: B did not read A's text and function whole: the model was told {told}"
            )),
            Second::Fails if !told.contains("the second cell fails at") => violations.push(
                format!("PC-WORKING: B did not fail after reading A's state: {told}"),
            ),
            _ => {}
        }
        let view =
            lash_core::store::SessionStore::new(self.backend().session_store_factory(), session())
                .expect("the session's store");
        let execution = lash_core::store::load_session_window_state(
            &view,
            lash_core::store::WindowSelector::Current,
        )
        .await
        .map(|loaded| loaded.map(|loaded| loaded.state.execution_state_hydration()));
        let Ok(Some(Ok(Some(execution)))) = execution else {
            violations.push(format!(
                "PC-WORKING: the committed head holds no execution state: {execution:?}"
            ));
            return violations;
        };
        // The heap pages its texts out of its root: they are read through
        // the pages the committed nodes reach.
        let descriptor = BTreeMap::from([(execution.root, execution.node())]);
        let pages = self.published.lock_recover().reached(&descriptor);
        let holds = |needle: &str| {
            std::iter::once(&execution.descriptor)
                .chain(&pages)
                .any(|bytes| {
                    bytes
                        .windows(needle.len())
                        .any(|window| window == needle.as_bytes())
                })
        };
        let kept: &[&str] = match second {
            Second::Succeeds => &["carried_text", "twice_of", "second_total"],
            Second::Fails => &["carried_text", "twice_of"],
        };
        let dropped = "second_partial";
        for name in kept {
            if !holds(name) {
                violations.push(format!("PC-WORKING: the committed execution lost `{name}`"));
            }
        }
        if holds(dropped) {
            violations.push(format!(
                "PC-WORKING: the committed execution holds B's `{dropped}`"
            ));
        }
        if !holds(&carried) {
            let published = self.published.lock_recover();
            let anywhere = published
                .0
                .values()
                .filter(|bytes| {
                    bytes
                        .windows(carried.len())
                        .any(|window| window == carried.as_bytes())
                })
                .count();
            violations.push(format!(
                "PC-WORKING: the committed execution lost A's text: its descriptor reaches {} of {} published pages, {anywhere} of which hold it",
                pages.len(),
                published.0.len(),
            ));
        }
        violations
    }
}

/// The attachment of every print archive `value` holds.
fn retained_prints(value: &serde_json::Value, found: &mut Vec<lash_core::AttachmentId>) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                if key == "prints_retained" && !value.is_null() {
                    let retained: lash_core::RetainedValue =
                        serde_json::from_value(value.clone()).expect("a print archive decodes");
                    found.push(retained.output.reference.id);
                } else {
                    retained_prints(value, found);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                retained_prints(value, found);
            }
        }
        _ => {}
    }
}

/// The commit that records the operation's outcome: a cell's VM consumes
/// it from `round.outcome`; a plain final outcome commits with its
/// presentation and the next model start (FIG-5954), whatever the protocol
/// (FIG-6162).
fn outcome_label(protocol: Protocol) -> CommitLabel {
    match protocol {
        Protocol::Tools | Protocol::Noted | Protocol::NotedTwice => {
            CommitLabel::ROUND_OUTCOME_PRESENT_MODEL_START
        }
        _ => CommitLabel::ROUND_OUTCOME,
    }
}

/// K1 and K2: a node killed at the [`outcome_label`] after the body ran.
fn kill_laws(
    protocol: Protocol,
    cut: &Cut,
    completed: bool,
    writes: &BTreeMap<ToolCallId, Vec<serde_json::Value>>,
) -> Vec<String> {
    let mut violations = Vec::new();
    if cut.point.label != outcome_label(protocol) || !cut.fault.kills() {
        return violations;
    }
    let (name, expected) = match cut.fault {
        Fault::Abort => ("K1", !completed),
        _ => ("K2", completed),
    };
    if !expected {
        violations.push(format!(
            "{name}: the model was told the operation {}",
            if completed {
                "completed"
            } else {
                "was interrupted"
            }
        ));
    }
    if writes.values().map(Vec::len).sum::<usize>() != 1 {
        violations.push(format!(
            "{name}: the body did not run exactly once: {writes:?}"
        ));
    }
    violations
}

/// P5: once a zombie's actors moved, every owner write it attempts is
/// refused with `OwnershipLost`.
fn zombie_laws(cut: &Cut, trace: &[lash_durable_test::Write]) -> Vec<String> {
    let mut violations = Vec::new();
    if cut.fault != Fault::Zombie || cut.kind != WriteKind::Actor {
        return violations;
    }
    let Some(at) = trace.iter().position(|write| {
        write.node == cut.node && write.point == cut.point && write.cut == Some(cut.fault)
    }) else {
        return vec!["P5: the zombie's cut write is not in the trace".to_owned()];
    };
    for write in trace[at..]
        .iter()
        .filter(|write| write.node == cut.node && write.kind == WriteKind::Actor)
    {
        match &write.stored {
            Stored::Refused(DurableError::OwnershipLost(_)) => {}
            other => violations.push(format!("P5: zombie write {write} was {other:?}")),
        }
    }
    violations
}

/// The matrix the spec names: every commit label of the uncut run under
/// fail-before, ack-hidden and zombie, plus the kills K1 and K2 come from.
fn matrix() -> Matrix {
    Matrix::new()
        .faults(&[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
}

async fn prove(protocol: Protocol, dialect: Dialect, postgres_url: Option<String>) {
    let report = matrix()
        .run_test(|| V0::new(protocol, dialect, postgres_url.clone()))
        .await;
    let labels: Vec<&str> = report.labels().iter().map(|label| label.as_str()).collect();
    eprintln!(
        "V0 {protocol:?} {dialect:?}: {} cells over {} labels ({}) x {} faults",
        report.cells.len(),
        labels.len(),
        labels.join(", "),
        5
    );
    report.assert_held();
    for fault in [Fault::Abort, Fault::CommitThenAbort] {
        assert!(
            report.cells.iter().any(|cell| {
                cell.point.label == outcome_label(protocol)
                    && cell.fault == fault
                    && cell.verdict == Verdict::Held
            }),
            "{fault:?} at {} was not cut",
            outcome_label(protocol)
        );
    }
    // The matrix discovers and cuts the actual durable boundaries. The
    // counted receipt laws own whether answer and admission need a split;
    // P1-P7 above own crash safety independently of that implementation.
}

/// The code turn on SQLite in memory: every cell of the matrix holds P1 to
/// P7.
#[tokio::test]
async fn a_once_operation_in_a_cell_killed_after_its_work_resumes_without_replay_on_sqlite_memory()
{
    prove(Protocol::Code, Dialect::SqliteMemory, None).await;
}

/// The code turn on a SQLite file.
#[tokio::test]
async fn a_once_operation_in_a_cell_killed_after_its_work_resumes_without_replay_on_sqlite_file() {
    prove(Protocol::Code, Dialect::SqliteFile, None).await;
}

/// The code turn on PostgreSQL.
#[tokio::test]
async fn a_once_operation_in_a_cell_killed_after_its_work_resumes_without_replay_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    prove(Protocol::Code, Dialect::Postgres, Some(url)).await;
}

/// PC-WORKING on SQLite in memory: cell A's working state reaches B across
/// the model wait and the turn's commit, cut at every commit.
#[tokio::test]
async fn a_cells_working_state_reaches_the_next_cell_and_the_session_wherever_its_turn_was_cut() {
    prove(
        Protocol::Working(Second::Succeeds),
        Dialect::SqliteMemory,
        None,
    )
    .await;
}

/// PC-WORKING on SQLite in memory: a second cell that fails leaves A's
/// working state as the session's, cut at every commit.
#[tokio::test]
async fn a_failed_second_cell_leaves_the_first_cells_working_state_wherever_its_turn_was_cut() {
    prove(
        Protocol::Working(Second::Fails),
        Dialect::SqliteMemory,
        None,
    )
    .await;
}

/// The archived code turn on SQLite in memory: every cell of the matrix
/// holds P1 to P7, and the committed cell's print archive is held on the
/// session (FIG-5318).
#[tokio::test]
async fn a_cell_archive_is_held_on_its_session_wherever_its_turn_was_cut_on_sqlite_memory() {
    prove(Protocol::Archived, Dialect::SqliteMemory, None).await;
}

/// The tool turn on SQLite in memory: every cell of the matrix holds P1 to
/// P7.
#[tokio::test]
async fn a_once_tool_call_killed_after_its_work_resumes_without_replay_on_sqlite_memory() {
    prove(Protocol::Tools, Dialect::SqliteMemory, None).await;
}

/// The tool turn on a SQLite file.
#[tokio::test]
async fn a_once_tool_call_killed_after_its_work_resumes_without_replay_on_sqlite_file() {
    prove(Protocol::Tools, Dialect::SqliteFile, None).await;
}

/// The tool turn on PostgreSQL.
#[tokio::test]
async fn a_once_tool_call_killed_after_its_work_resumes_without_replay_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    prove(Protocol::Tools, Dialect::Postgres, Some(url)).await;
}

/// Cold resume: the whole deployment dies right after the commit that
/// consumes the cell's answer, the second model call's `model.start`, which
/// commits the cell's end with it (FIG-5999), and a fresh deployment over
/// the same database, with a core and an activation that hold nothing of
/// the first one, takes the turn over. A restore loads state and re-runs no
/// code: the turn is restored once, the cell's program and its operation
/// are never entered again, and the only writes left are the `model.start`
/// that sends the pinned model call again as its next attempt,
/// `turn.commit` and the release.
async fn cold_resume(dialect: Dialect, postgres_url: Option<String>) {
    let first = V0::new(Protocol::Code, dialect, postgres_url.clone());
    let clock = SimClock::new();
    let database = first.database(Arc::clone(&clock)).await;
    let script = Script::new();
    script.cut_on("a", CommitLabel::MODEL_START, 2, Fault::CommitThenAbort);
    let before = SimNodes::new(
        Arc::clone(&database),
        Arc::clone(&clock),
        script,
        first.config(),
        first.activation(),
    );
    first.send().await.expect("the turn is sent");
    before.start("a");
    before.quiesce().await;
    while before.script().cuts().is_empty() {
        assert!(
            before.step().await.is_some(),
            "the first deployment stalled before the cell's answer was consumed:\n{}",
            before.script().rendered_trace()
        );
    }
    before.kill("a");
    before.quiesce().await;
    assert_eq!(before.life("a"), Life::Dead);
    assert!(
        !first.done(&before).await,
        "the turn finished before the deployment died"
    );

    let cold = first.cold(postgres_url);
    let after = SimNodes::new(
        Arc::clone(&database),
        Arc::clone(&clock),
        Script::new(),
        cold.config(),
        cold.activation(),
    );
    after.start("c");
    cold.run_until_done(&after, &clock, "the cold deployment")
        .await;

    let mut violations = cold.laws(&after, None).await;
    let counts = first.tripwire.counts();
    let restores = counts
        .restores
        .get(&(session(), run()))
        .copied()
        .unwrap_or(0);
    if restores != 1 {
        violations.push(format!(
            "the cold owner restored the turn {restores} times, not once"
        ));
    }
    let programs: usize = counts.vm_programs.values().sum();
    if programs != 1 {
        violations.push(format!(
            "the cell's program was entered {programs} times; only the first deployment enters it"
        ));
    }
    let labels: Vec<CommitLabel> = after
        .script()
        .trace()
        .iter()
        .filter(|write| write.kind == WriteKind::Actor && write.committed())
        .map(|write| write.point.label)
        .collect();
    if labels
        != [
            CommitLabel::MODEL_START,
            CommitLabel::MODEL_ANSWER,
            CommitLabel::TURN_COMMIT,
            CommitLabel::SESSION_RELEASE,
        ]
    {
        violations.push(format!(
            "the cold owner committed {labels:?}, not the second model call's next attempt, its receipt, the turn's commit and the release"
        ));
    }
    assert!(
        violations.is_empty(),
        "cold resume on {dialect:?}:\n  {}\n{}",
        violations.join("\n  "),
        after.script().rendered_trace()
    );
}

#[tokio::test]
async fn a_cold_restart_restores_from_state_and_reruns_no_code_on_sqlite_memory() {
    cold_resume(Dialect::SqliteMemory, None).await;
}

#[tokio::test]
async fn a_cold_restart_restores_from_state_and_reruns_no_code_on_sqlite_file() {
    cold_resume(Dialect::SqliteFile, None).await;
}

#[tokio::test]
async fn a_cold_restart_restores_from_state_and_reruns_no_code_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    cold_resume(Dialect::Postgres, Some(url)).await;
}

/// A12: the code turn, once uncut and once with node A killed right after
/// the second model call's `model.start` committed. That checkpoint's
/// progress cursor is past the cell's protocol records, which reached the
/// turn's draft only through the progress boundary on A; the cold owner that
/// resumes it must commit them as the uncut owner did.
async fn resumed_history(dialect: Dialect, postgres_url: Option<String>) {
    resumed_as_uncut(
        Protocol::Code,
        (CommitLabel::MODEL_START, 2),
        dialect,
        postgres_url,
    )
    .await;
}

/// The protocol records of a committed history, encoded.
fn protocol_records(history: &[lash_core::SessionHistoryRecord]) -> Vec<serde_json::Value> {
    history
        .iter()
        .filter(|record| matches!(record, lash_core::SessionHistoryRecord::Protocol(_)))
        .map(|record| serde_json::to_value(record).expect("a record encodes"))
        .collect()
}

/// `protocol`'s turn run uncut on one node: the history it commits, and its
/// owner commits in order.
async fn uncut_history(
    protocol: Protocol,
    dialect: Dialect,
    postgres_url: Option<String>,
) -> (Vec<lash_core::SessionHistoryRecord>, Vec<CommitLabel>) {
    let uncut = V0::new(protocol, dialect, postgres_url);
    let clock = SimClock::new();
    let database = uncut.database(Arc::clone(&clock)).await;
    let nodes = SimNodes::new(
        Arc::clone(&database),
        Arc::clone(&clock),
        Script::new(),
        uncut.config(),
        uncut.activation(),
    );
    uncut.send().await.expect("the turn is sent");
    nodes.start("a");
    uncut.run_until_done(&nodes, &clock, "the uncut turn").await;
    let labels = nodes
        .script()
        .trace()
        .iter()
        .filter(|write| write.kind == WriteKind::Actor && write.committed())
        .map(|write| write.point.label)
        .collect();
    (uncut.committed_history().await, labels)
}

/// The history `protocol`'s turn commits when node A is killed right after
/// its `nth` `label` write committed and a cold owner resumes it.
async fn resumed_history_after(
    protocol: Protocol,
    (label, nth): (CommitLabel, usize),
    dialect: Dialect,
    postgres_url: Option<String>,
) -> Vec<lash_core::SessionHistoryRecord> {
    let first = V0::new(protocol, dialect, postgres_url.clone());
    let clock = SimClock::new();
    let database = first.database(Arc::clone(&clock)).await;
    let script = Script::new();
    script.cut_on("a", label, nth, Fault::CommitThenAbort);
    let before = SimNodes::new(
        Arc::clone(&database),
        Arc::clone(&clock),
        script,
        first.config(),
        first.activation(),
    );
    first.send().await.expect("the turn is sent");
    before.start("a");
    before.quiesce().await;
    while before.script().cuts().is_empty() {
        // The cut kills A, which leaves no timer armed: the step that cuts
        // can be the last.
        let stepped = before.step().await;
        assert!(
            stepped.is_some() || !before.script().cuts().is_empty(),
            "the turn stalled before its {label} write {nth}:\n{}",
            before.script().rendered_trace()
        );
    }
    before.kill("a");
    before.quiesce().await;
    let cold = first.cold(postgres_url);
    let after = SimNodes::new(
        Arc::clone(&database),
        Arc::clone(&clock),
        Script::new(),
        cold.config(),
        cold.activation(),
    );
    after.start("c");
    cold.run_until_done(&after, &clock, "the resumed turn")
        .await;
    cold.committed_history().await
}

/// The protocol records `protocol`'s turn commits uncut, which the turn
/// commits too when node A is killed right after its `cut` write committed
/// and a cold owner resumes it.
async fn resumed_as_uncut(
    protocol: Protocol,
    cut: (CommitLabel, usize),
    dialect: Dialect,
    postgres_url: Option<String>,
) -> Vec<serde_json::Value> {
    let (uncut, _) = uncut_history(protocol, dialect, postgres_url.clone()).await;
    let expected = protocol_records(&uncut);
    assert!(
        !expected.is_empty(),
        "the uncut {protocol:?} turn committed no protocol record"
    );
    let resumed = resumed_history_after(protocol, cut, dialect, postgres_url).await;
    assert_eq!(
        protocol_records(&resumed),
        expected,
        "the {protocol:?} turn resumed after {} on {dialect:?} committed other protocol history than the uncut one",
        cut.0
    );
    expected
}

/// A resumed turn commits the uncut turn's protocol history, on SQLite in
/// memory.
#[tokio::test]
async fn a_turn_resumed_after_a_progress_boundary_commits_the_uncut_history_on_sqlite_memory() {
    resumed_history(Dialect::SqliteMemory, None).await;
}

/// On a SQLite file.
#[tokio::test]
async fn a_turn_resumed_after_a_progress_boundary_commits_the_uncut_history_on_sqlite_file() {
    resumed_history(Dialect::SqliteFile, None).await;
}

/// On PostgreSQL.
#[tokio::test]
async fn a_turn_resumed_after_a_progress_boundary_commits_the_uncut_history_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    resumed_history(Dialect::Postgres, Some(url)).await;
}

/// FIG-5588: the Noted turn's driver appends a protocol record beside the
/// model's call, before the call's result. The turn's commit holds it once,
/// uncut; and so does the commit of a turn whose owner died while the call
/// was unanswered (`model.done`, the round's checkpoint) or after the
/// round's results made the transcript resume-safe again
/// (`round.outcome+present+model.start`).
#[tokio::test]
async fn a_protocol_record_appended_while_a_call_is_unanswered_is_committed_uncut_and_resumed() {
    let note = serde_json::to_value(lash_core::SessionHistoryRecord::Protocol(
        noted::mid_call_note(),
    ))
    .expect("a record encodes");
    for cut in [
        (CommitLabel::MODEL_DONE, 1),
        (CommitLabel::ROUND_OUTCOME_PRESENT_MODEL_START, 1),
    ] {
        let committed = resumed_as_uncut(Protocol::Noted, cut, Dialect::SqliteMemory, None).await;
        assert_eq!(
            committed,
            vec![note.clone()],
            "the turn's commit holds the record appended mid-call once"
        );
    }
}

/// One line per committed record, in commit order: a message by its role and
/// id, a protocol record by its content.
fn interleaving(history: &[lash_core::SessionHistoryRecord]) -> Vec<String> {
    history
        .iter()
        .map(|record| match record {
            lash_core::SessionHistoryRecord::Conversation(record) => {
                let message = record.to_message();
                format!("message {:?} {}", message.role, message.id)
            }
            lash_core::SessionHistoryRecord::Protocol(event) => {
                format!("protocol {} {}", event.plugin_id, event.payload)
            }
        })
        .collect()
}

/// FIG-5593: replay equivalence covers the order across the two streams of
/// a turn's history. The twice-Noted turn passes two progress boundaries
/// that each carry a protocol record; the history it commits, messages and
/// protocol records in commit order, is the uncut one whichever of its
/// owner commits its first owner died after.
#[tokio::test]
async fn a_turn_resumed_at_any_checkpoint_commits_messages_and_protocol_records_in_the_uncut_order()
{
    let protocol = Protocol::NotedTwice;
    let (uncut, labels) = uncut_history(protocol, Dialect::SqliteMemory, None).await;
    let expected = interleaving(&uncut);
    let notes: Vec<usize> = uncut
        .iter()
        .enumerate()
        .filter(|(_, record)| matches!(record, lash_core::SessionHistoryRecord::Protocol(_)))
        .map(|(at, _)| at)
        .collect();
    assert!(
        matches!(notes[..], [first, second] if first + 1 < second && second + 1 < uncut.len()),
        "the uncut turn commits two protocol records with messages between and after them: {expected:#?}"
    );
    let mut seen = BTreeMap::<&str, usize>::new();
    for label in &labels {
        let nth = seen.entry(label.as_str()).or_default();
        *nth += 1;
        let cut = (*label, *nth);
        let resumed = resumed_history_after(protocol, cut, Dialect::SqliteMemory, None).await;
        assert_eq!(
            interleaving(&resumed),
            expected,
            "the turn resumed after its {label} write {nth} committed its history in another order than the uncut one"
        );
    }
}

/// The heap pages candidates published, by identity: where a law reads
/// back a text the heap paged out of its root.
#[derive(Clone, Debug, Default)]
struct Published(BTreeMap<lash_core_store::checkpoint_graph::NodeId, Arc<[u8]>>);

impl lash_core_store::checkpoint_graph::NodeSource for Published {
    fn fetch(
        &self,
        id: &lash_core_store::checkpoint_graph::NodeId,
    ) -> Option<lash_core_store::checkpoint_graph::StoredNode<'_>> {
        self.0
            .get(id)
            .map(|bytes| lash_core_store::checkpoint_graph::StoredNode {
                encoding: lash_core_store::checkpoint_graph::BlobEncoding::Graph,
                bytes,
            })
    }
}

impl Published {
    fn insert(&mut self, nodes: &[lash_core_store::checkpoint_graph::EncodedNode]) {
        for node in nodes {
            self.0.insert(node.id, Arc::clone(&node.bytes));
        }
    }

    /// The published pages `nodes` reach, through pages: a hydrated node
    /// that is a heap page is read as its published page.
    fn reached(
        &self,
        nodes: &BTreeMap<
            lash_core_store::checkpoint_graph::NodeId,
            lash_core_store::checkpoint_graph::EncodedNode,
        >,
    ) -> Vec<Arc<[u8]>> {
        use lash_core_store::checkpoint_graph::parse_envelope;
        let mut seen = std::collections::BTreeSet::new();
        let mut next: Vec<_> = nodes
            .values()
            .filter_map(|node| parse_envelope(&node.bytes, Some(node.id)).ok())
            .flat_map(|envelope| envelope.children.iter().collect::<Vec<_>>())
            .chain(nodes.keys().copied())
            .collect();
        let mut reached = Vec::new();
        while let Some(id) = next.pop() {
            if !seen.insert(id) {
                continue;
            }
            if let Some(bytes) = self.0.get(&id) {
                if let Ok(envelope) = parse_envelope(bytes, Some(id)) {
                    next.extend(envelope.children.iter());
                }
                reached.push(Arc::clone(bytes));
            }
        }
        reached
    }
}

#[path = "vertical_crash_proof/session_capture.rs"]
mod session_capture;

#[path = "vertical_crash_proof/refuse.rs"]
mod refuse;

/// LO-RECOVERY exercises the existing durable fault matrix and the two
/// memory-only cuts on every reachable Tools transition. Missing hook
/// occurrences fail rather than silently reducing the recovery matrix.
async fn lo_recovery(dialect: Dialect, postgres_url: Option<String>) {
    use lash_durable::OwnerCut;
    use lash_durable_test::OwnerFault;
    prove(Protocol::Tools, dialect, postgres_url.clone()).await;
    let (expected, labels) = uncut_history(Protocol::Tools, dialect, postgres_url.clone()).await;
    let same_history = |actual: &[lash_core::SessionHistoryRecord]| {
        assert_eq!(actual.len(), expected.len());
        let mut ids = BTreeMap::new();
        let mut reverse = BTreeMap::new();
        for (actual, expected) in actual.iter().zip(&expected) {
            let mut actual = actual.clone();
            if let (
                lash_core::SessionHistoryRecord::Conversation(actual),
                lash_core::SessionHistoryRecord::Conversation(expected),
            ) = (&mut actual, expected)
            {
                if let Some(prior) = ids.insert(actual.id.clone(), expected.id.clone()) {
                    assert_eq!(prior, expected.id);
                }
                if let Some(prior) = reverse.insert(expected.id.clone(), actual.id.clone()) {
                    assert_eq!(prior, actual.id);
                }
                actual.id.clone_from(&expected.id);
            }
            assert!(
                lash_protocol_types::session_model::same_history_record(&actual, expected),
                "warm/cold history differs: {actual:?} vs {expected:?}"
            );
        }
    };
    let mut reachable = Vec::new();
    for label in labels {
        if !reachable.contains(&label) && label != CommitLabel::SESSION_RELEASE {
            reachable.push(label);
        }
    }
    assert!(reachable.contains(&CommitLabel::TURN_COMMIT));
    for label in reachable {
        for cut in [OwnerCut::Acknowledged, OwnerCut::Adopted] {
            for cold in [false, true] {
                let first = V0::new(Protocol::Tools, dialect, postgres_url.clone());
                let clock = SimClock::new();
                let database = first.database(Arc::clone(&clock)).await;
                let script = Script::new();
                script.cut_owner(label, 1, cut, OwnerFault::Fail);
                first.tripwire.observe_owner_cuts(&script);
                let nodes = SimNodes::new(
                    Arc::clone(&database),
                    Arc::clone(&clock),
                    script,
                    first.config(),
                    first.activation(),
                );
                first.send().await.expect("accepted input");
                nodes.start("a");
                if cold {
                    let horizon = clock.logical_ms() + 600_000;
                    while !nodes
                        .script()
                        .owner_cuts()
                        .iter()
                        .any(|receipt| receipt.fault.is_some())
                    {
                        assert!(clock.logical_ms() < horizon, "unreached {label:?}/{cut:?}");
                        assert!(nodes.step().await.is_some(), "unreached {label:?}/{cut:?}");
                    }
                    nodes.kill("a");
                    nodes.quiesce().await;
                    let next = first.cold(postgres_url.clone());
                    let after = SimNodes::new(
                        Arc::clone(&database),
                        Arc::clone(&clock),
                        Script::new(),
                        next.config(),
                        next.activation(),
                    );
                    next.tripwire.observe_owner_cuts(after.script());
                    after.start("cold");
                    next.run_until_done(&after, &clock, "cold owner cut").await;
                    same_history(&next.committed_history().await);
                } else {
                    first.run_until_done(&nodes, &clock, "warm owner cut").await;
                    let violations = first.laws(&nodes, None).await;
                    assert!(violations.is_empty(), "{violations:?}");
                    same_history(&first.committed_history().await);
                }
                let cuts = nodes.script().owner_cuts();
                assert_eq!(
                    cuts.iter()
                        .filter(|receipt| receipt.fault.is_some())
                        .count(),
                    1,
                    "{label:?}/{cut:?}: every selected hook must fire exactly once"
                );
                assert!(database.turn(&session()).await.unwrap().is_none());
                assert!(
                    database
                        .session_mailbox(&session())
                        .await
                        .unwrap()
                        .bound_run
                        .is_none()
                );
                assert_eq!(
                    database
                        .turn_end(&session(), &run())
                        .await
                        .unwrap()
                        .unwrap()
                        .head_revision,
                    Some(1)
                );
                assert_eq!(
                    first.world.writes().values().map(Vec::len).sum::<usize>(),
                    1
                );
                assert!(
                    first
                        .tripwire
                        .counts()
                        .bodies
                        .values()
                        .all(|count| *count == 1)
                );
            }
        }
    }
}

#[tokio::test]
async fn lo_recovery_on_sqlite_memory() {
    lo_recovery(Dialect::SqliteMemory, None).await;
}

#[tokio::test]
async fn lo_recovery_on_sqlite_file() {
    lo_recovery(Dialect::SqliteFile, None).await;
}

#[tokio::test]
#[ignore = "requires a private PostgreSQL service"]
async fn lo_recovery_on_postgres_when_configured() {
    let url = lash_postgres_store::testing::required_database_url();
    lo_recovery(Dialect::Postgres, Some(url)).await;
}
