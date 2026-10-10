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
//!   runs through the RLM worker path on the durable snapshot store, and
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

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/dialect.rs"]
mod dialect;
#[path = "support/images.rs"]
mod images;
#[path = "support/noted.rs"]
mod noted;
#[path = "support/sim.rs"]
mod sim;

#[path = "support/matrix.rs"]
mod matrix;

use lash_sansio::llm::types::{StreamBlockEvent, StreamBlockKind};
use matrix::MatrixTestExt as _;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::ToolDefinitionBindingExt as _;
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{LlmRequest, LlmResponse, LlmStreamEvent, StreamBlockIdentity};
use lash_core::runtime::durable::session::SessionActivation;
use lash_core::{ExecutionPolicy, LlmOutputPart, ToolCall, ToolCallId, ToolOutcome};
use lash_core_execution::{Backend, BackendParts, NoProjectionProviders, StoreSet};
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableError, DurableStore};
use lash_durable_test::{
    Cut, Fault, Life, Matrix, Scenario, Script, SimClock, SimNodes, SimNodesConfig, Stored,
    Tripwire, Verdict, WriteKind,
};
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{SessionId, TurnId};

use dialect::Dialect;

const SESSION: &str = "v0-session";
const RUN: &str = "v0-turn";
const TOOL: &str = "ext_write";
const MODEL: &str = "v0-model";

/// The cell the model writes: one `Once` host operation, then a printout of
/// what it answered.
const CELL: &str = "<typescript>\nconst written = await tools.ext_write({ x: 7 });\nconsole.log(written);\n</typescript>";
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
}

impl Protocol {
    /// How many times the scripted model calls the operation before it
    /// answers in prose.
    fn rounds(self) -> usize {
        match self {
            Self::Code | Self::Tools | Self::Noted => 1,
            Self::NotedTwice => 2,
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
    .with_execution(std::time::Duration::from_secs(120))
    .with_execution_policy(ExecutionPolicy::Once)
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], TOOL));
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
                let response = match (called >= protocol.rounds(), protocol) {
                    (true, _) => {
                        let told = if rendered.contains(WROTE) {
                            "the write completed"
                        } else {
                            "the write did not complete"
                        };
                        text(&request, &format!("{FINAL}{told}"))
                    }
                    (false, Protocol::Code) => text(&request, CELL),
                    (false, Protocol::Tools | Protocol::Noted | Protocol::NotedTwice) => {
                        LlmResponse {
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: match called {
                                    0 => "v0-call".to_owned(),
                                    round => format!("v0-call-{round}"),
                                },
                                tool_name: TOOL.to_owned(),
                                input_json: r#"{"x":7}"#.to_owned(),
                                replay: None,
                            }],
                            ..LlmResponse::default()
                        }
                    }
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
        Protocol::Code => lash::LashCore::rlm_builder(
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
        ),
        Protocol::Tools => lash::LashCore::standard_builder(backend.clone()),
        Protocol::Noted | Protocol::NotedTwice => lash::LashCore::builder(backend.clone())
            .protocol_plugin(Arc::new(noted::NotedProtocolFactory)),
    };
    builder
        .serve_sessions(false)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(model(protocol, Arc::clone(seen)), metadata())
        .tools(ext_write(Arc::clone(world)))
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
    /// A committed store image the scenario opens instead of a fresh
    /// database: a decode-and-resume fixture.
    image: Option<&'static images::Image>,
    /// The SQLite file a fixture is recorded from, instead of a fresh
    /// database.
    record_at: Option<std::path::PathBuf>,
    world: Arc<ExternalWorld>,
    seen: Arc<Mutex<Seen>>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<Backend>>,
    /// The virtual clock the database was built on, which the core's VM
    /// worker calls hold.
    clock: Mutex<Option<Arc<SimClock>>>,
    core: Mutex<Option<lash::LashCore>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl V0 {
    fn new(protocol: Protocol, dialect: Dialect, postgres_url: Option<String>) -> Self {
        Self {
            protocol,
            dialect,
            postgres_url,
            image: None,
            record_at: None,
            world: Arc::default(),
            seen: Arc::default(),
            tripwire: Arc::default(),
            backend: Mutex::default(),
            clock: Mutex::default(),
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
        let (stores, database): (Arc<dyn StoreSet>, Arc<dyn DurableStore>) =
            match (self.image, &self.record_at) {
                (Some(image), _) => {
                    let (stores, dir) = images::open(image, clock).await;
                    self.keep.lock_recover().push(Box::new(dir));
                    let database = Arc::new(stores.durable_store());
                    (Arc::new(stores), database)
                }
                (None, Some(path)) => {
                    let stores = sim::file(path, clock).await;
                    let database = Arc::new(stores.durable_store());
                    (Arc::new(stores), database)
                }
                (None, None) => {
                    dialect::open(
                        self.dialect,
                        self.postgres_url.as_deref(),
                        clock,
                        &self.keep,
                    )
                    .await
                }
            };
        *self.backend.lock_recover() = Some(shipped_backend(stores));
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
            image: None,
            record_at: None,
            world: Arc::clone(&self.world),
            seen: Arc::clone(&self.seen),
            tripwire: Arc::clone(&self.tripwire),
            backend: Mutex::new(Some(self.backend())),
            clock: Mutex::new(self.clock.lock_recover().clone()),
            core: Mutex::default(),
            keep: Mutex::default(),
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
        // lost before any snapshot committed.
        let unsnapshotted = trace
            .iter()
            .filter(|write| {
                write.point.label == CommitLabel::CELL_SNAPSHOT_ADMIT && !write.committed()
            })
            .count();
        let first_snapshot = trace.iter().position(|write| {
            write.point.label == CommitLabel::CELL_SNAPSHOT_ADMIT && write.committed()
        });
        let lost_before_snapshot = cut
            .filter(|cut| cut.fault.kills() || cut.fault == Fault::Zombie)
            .and_then(|cut| {
                trace
                    .iter()
                    .position(|write| write.node == cut.node && write.point == cut.point)
            })
            .is_some_and(|at| first_snapshot.is_none_or(|first| at < first));
        let programs: usize = counts.vm_programs.values().sum();
        if programs > 1 + unsnapshotted + usize::from(lost_before_snapshot) {
            violations.push(format!(
                "P4: the cell's program was entered fresh {programs} times with {unsnapshotted} uncommitted snapshots: {:?}",
                counts.vm_programs
            ));
        }
        if self.protocol == Protocol::Code && programs == 0 {
            violations.push("the turn's cell never ran".to_owned());
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
        // VM worker) and reloads the rows: at most once per owner. The
        // session is the scenario's one actor, so every claim that took it
        // is an owner.
        let owners = trace
            .iter()
            .filter(|write| {
                write.point.label == CommitLabel::CLAIM
                    && matches!(write.stored, Stored::Committed { effective: true })
            })
            .count();
        if restores > owners.max(1) {
            violations.push(format!(
                "P4: the turn was restored {restores} times by {owners} owners"
            ));
        }

        if let Some(cut) = cut {
            violations.extend(kill_laws(cut, completed, &writes));
            violations.extend(zombie_laws(cut, &trace));
        }
        violations
    }
}

/// K1 and K2: a node killed at `round.outcome` after the body ran.
fn kill_laws(
    cut: &Cut,
    completed: bool,
    writes: &BTreeMap<ToolCallId, Vec<serde_json::Value>>,
) -> Vec<String> {
    let mut violations = Vec::new();
    if cut.point.label != CommitLabel::ROUND_OUTCOME || !cut.fault.kills() {
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
                cell.point.label == CommitLabel::ROUND_OUTCOME
                    && cell.fault == fault
                    && cell.verdict == Verdict::Held
            }),
            "{fault:?} at round.outcome was not cut"
        );
    }
    report.assert_baseline_labels(&uncut_labels(protocol));
    for label in uncut_labels(protocol) {
        assert!(
            report.labels().contains(&label),
            "the matrix never cut {label}"
        );
    }
}

/// The owner commits of the uncut turn, in order.
fn uncut_labels(protocol: Protocol) -> Vec<CommitLabel> {
    match protocol {
        Protocol::Code => vec![
            CommitLabel::TURN_ADMIT,
            CommitLabel::MODEL_START,
            CommitLabel::MODEL_DONE,
            CommitLabel::CELL_SNAPSHOT_ADMIT,
            CommitLabel::ROUND_OUTCOME,
            CommitLabel::CELL_SNAPSHOT,
            CommitLabel::MODEL_START,
            CommitLabel::TURN_COMMIT,
            CommitLabel::SESSION_RELEASE,
        ],
        Protocol::Tools | Protocol::Noted => vec![
            CommitLabel::TURN_ADMIT,
            CommitLabel::MODEL_START,
            CommitLabel::MODEL_DONE,
            CommitLabel::ROUND_OUTCOME,
            CommitLabel::ROUND_PRESENT_MODEL_START,
            CommitLabel::TURN_COMMIT,
            CommitLabel::SESSION_RELEASE,
        ],
        Protocol::NotedTwice => vec![
            CommitLabel::TURN_ADMIT,
            CommitLabel::MODEL_START,
            CommitLabel::MODEL_DONE,
            CommitLabel::ROUND_OUTCOME,
            CommitLabel::ROUND_PRESENT_MODEL_START,
            CommitLabel::MODEL_DONE,
            CommitLabel::ROUND_OUTCOME,
            CommitLabel::ROUND_PRESENT_MODEL_START,
            CommitLabel::TURN_COMMIT,
            CommitLabel::SESSION_RELEASE,
        ],
    }
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

/// Cold resume: the whole deployment dies right after the cell's end
/// commits, and a fresh deployment over the same database, with a core and
/// an activation that hold nothing of the first one, takes the turn over. A
/// restore loads state and re-runs no code: the turn is restored once, the
/// cell's end is read from its snapshot without entering its program or
/// its operation again, and the only writes left are the second model
/// call's `model.start`, `turn.commit` and the release.
async fn cold_resume(dialect: Dialect, postgres_url: Option<String>) {
    let first = V0::new(Protocol::Code, dialect, postgres_url.clone());
    let clock = SimClock::new();
    let database = first.database(Arc::clone(&clock)).await;
    let script = Script::new();
    script.cut_on("a", CommitLabel::CELL_SNAPSHOT, 1, Fault::CommitThenAbort);
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
            "the first deployment stalled before the cell's end committed:\n{}",
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
            CommitLabel::TURN_COMMIT,
            CommitLabel::SESSION_RELEASE,
        ]
    {
        violations.push(format!(
            "the cold owner committed {labels:?}, not the second model call's start, the turn's commit and the release"
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
/// (`round.present_model_start`).
#[tokio::test]
async fn a_protocol_record_appended_while_a_call_is_unanswered_is_committed_uncut_and_resumed() {
    let note = serde_json::to_value(lash_core::SessionHistoryRecord::Protocol(
        noted::mid_call_note(),
    ))
    .expect("a record encodes");
    for cut in [
        (CommitLabel::MODEL_DONE, 1),
        (CommitLabel::ROUND_PRESENT_MODEL_START, 1),
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
    assert_eq!(labels, uncut_labels(protocol));
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

/// Re-record the session image (`support/images.rs`): node A runs the turn
/// until its cell's `Once` operation's outcome commits, and dies; the store
/// it leaves is the fixture. It holds the turn checkpoint, the cell's
/// snapshot parked on the operation, the operation's run records and its
/// outcome's material.
#[tokio::test]
#[ignore = "regenerates crates/lash-durable-test/tests/fixtures/formats/session"]
async fn regenerate_session_format_fixture() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = images::database_path(dir.path());
    let mut first = V0::new(Protocol::Code, Dialect::SqliteFile, None);
    first.record_at = Some(path.clone());
    let clock = SimClock::new();
    let database = first.database(Arc::clone(&clock)).await;
    let script = Script::new();
    script.cut_on("a", CommitLabel::ROUND_OUTCOME, 1, Fault::CommitThenAbort);
    let nodes = SimNodes::new(
        Arc::clone(&database),
        Arc::clone(&clock),
        script,
        first.config(),
        first.activation(),
    );
    first.send().await.expect("the turn is sent");
    nodes.start("a");
    nodes.quiesce().await;
    while nodes.script().cuts().is_empty() {
        // The cut kills A, which leaves no timer armed: the step that cuts
        // can be the last.
        let stepped = nodes.step().await;
        assert!(
            stepped.is_some() || !nodes.script().cuts().is_empty(),
            "the turn stalled before its operation's outcome committed:\n{}",
            nodes.script().rendered_trace()
        );
        assert!(
            nodes.clock().logical_ms() < 600_000,
            "the operation's outcome did not commit within ten minutes of virtual time:\n{}",
            nodes.script().rendered_trace()
        );
    }
    nodes.kill("a");
    nodes.quiesce().await;
    assert_eq!(
        first.world.writes().len(),
        1,
        "the operation's body did not run once before the cut"
    );
    images::regenerate(&path, images::SESSION.name);
}

/// The session image resumes on a fresh node of this build: it reaps the
/// dead owner, claims the session in the build's session set, restores the
/// turn from its checkpoint and the cell from its snapshot, injects the
/// operation's committed outcome and commits the turn, without running the
/// operation's body or entering the cell's program again.
#[tokio::test]
async fn a_session_decodes_and_resumes_from_its_1_0_image() {
    let mut resumed = V0::new(Protocol::Code, Dialect::SqliteFile, None);
    resumed.image = Some(&images::SESSION);
    let clock = SimClock::new();
    let database = resumed.database(Arc::clone(&clock)).await;
    let snapshot = database
        .actor(&actor())
        .await
        .expect("the image reads")
        .expect("the image holds the session");
    assert_eq!(
        &snapshot.formats,
        resumed.backend().formats().session(),
        "the image is in another build's set"
    );
    let nodes = SimNodes::new(
        Arc::clone(&database),
        Arc::clone(&clock),
        Script::new(),
        resumed.config(),
        resumed.activation(),
    );
    nodes.start("b");
    let horizon = clock.logical_ms() + 600_000;
    while !resumed.done(&nodes).await {
        assert!(
            clock.logical_ms() < horizon,
            "the resumed session is not done after 600 s of virtual time:\n{}",
            nodes.script().rendered_trace()
        );
        assert!(
            nodes.step().await.is_some(),
            "the resumed session stalled:\n{}",
            nodes.script().rendered_trace()
        );
    }
    nodes.quiesce().await;

    let mut violations = Vec::new();
    let labels: Vec<CommitLabel> = nodes
        .script()
        .trace()
        .iter()
        .filter(|write| write.kind == WriteKind::Actor && write.committed())
        .map(|write| write.point.label)
        .collect();
    if labels
        != [
            CommitLabel::CELL_SNAPSHOT,
            CommitLabel::MODEL_START,
            CommitLabel::TURN_COMMIT,
            CommitLabel::SESSION_RELEASE,
        ]
    {
        violations.push(format!(
            "the resumed session committed {labels:?}, not the cell's end, the second model call's start, the turn's commit and the release"
        ));
    }
    let writes = resumed.world.writes();
    if !writes.is_empty() {
        violations.push(format!(
            "the operation's body ran again on resume: {writes:?}"
        ));
    }
    let counts = resumed.tripwire.counts();
    let programs: usize = counts.vm_programs.values().sum();
    if programs != 0 {
        violations.push(format!(
            "the cell's program was entered {programs} times; it resumes from its snapshot"
        ));
    }
    let restores = counts
        .restores
        .get(&(session(), run()))
        .copied()
        .unwrap_or(0);
    if restores != 1 {
        violations.push(format!("the turn was restored {restores} times, not once"));
    }
    assert!(
        violations.is_empty(),
        "the session image's resume:\n  {}\n{}",
        violations.join("\n  "),
        nodes.script().rendered_trace()
    );
}

/// A session activation whose first claim rewrites the unfinished turn's
/// checkpoint as another build left it, with `stale`.
struct StaleCheckpoint {
    rewritten: std::sync::atomic::AtomicBool,
    database: Arc<dyn DurableStore>,
    session: Arc<dyn Activation>,
    stale: fn(&str) -> String,
}

/// Add an unknown field to the first parked driver state under `value`.
fn stale_driver_state(value: &mut serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(state) = map
                .get_mut("driver_state")
                .and_then(|state| state.pointer_mut("/payload/state"))
                .and_then(serde_json::Value::as_object_mut)
            {
                state.insert("left_by_another_build".to_owned(), true.into());
                return true;
            }
            map.values_mut().any(stale_driver_state)
        }
        serde_json::Value::Array(values) => values.iter_mut().any(stale_driver_state),
        _ => false,
    }
}

/// `checkpoint` with the driver state its protocol parked while the cell ran
/// carrying a field this build does not know.
fn with_a_stale_driver_state(checkpoint: &str) -> String {
    let mut stale: serde_json::Value =
        serde_json::from_str(checkpoint).expect("the image's checkpoint is JSON");
    assert!(
        stale_driver_state(&mut stale),
        "the image's checkpoint parks no driver state"
    );
    stale.to_string()
}

#[async_trait::async_trait]
impl Activation for StaleCheckpoint {
    async fn activate(&self, owned: lash_durable::runner::Owned) -> lash_durable::runner::Exit {
        use lash_durable::DomainWrite;
        use lash_durable::domain::{TurnWrite, UnfinishedPhase};
        if !self
            .rewritten
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let row = self
                .database
                .turn(&session())
                .await
                .expect("the turn reads")
                .expect("the image holds an unfinished turn");
            let UnfinishedPhase::Tools { run, checkpoint } = &row.phase else {
                panic!(
                    "the image's turn is not parked on its cell: {:?}",
                    row.phase
                );
            };
            let mut tx = owned
                .begin()
                .await
                .expect("the first claim reads its actor");
            tx.write(DomainWrite::Turn(TurnWrite::Advance {
                session: session(),
                run: row.run.clone(),
                phase: UnfinishedPhase::Tools {
                    run: *run,
                    checkpoint: (self.stale)(checkpoint),
                },
                iteration: row.iteration,
            }));
            owned
                .commit(tx, CommitLabel::MODEL_DONE)
                .await
                .expect("the stale checkpoint commits");
        }
        self.session.activate(owned).await
    }
}

/// The 1.0 session image, its turn's checkpoint rewritten with `stale` on
/// the first claim, resumed on one node until its session parks or goes
/// idle: the scenario, its store, its nodes and its clock.
async fn resumed_with_a_stale_checkpoint(
    stale: fn(&str) -> String,
) -> (V0, Arc<dyn DurableStore>, SimNodes, Arc<SimClock>) {
    resumed_with_stale_state(|database, session| {
        Arc::new(StaleCheckpoint {
            rewritten: std::sync::atomic::AtomicBool::new(false),
            database,
            session,
            stale,
        })
    })
    .await
}

/// The 1.0 session image resumed on one node until its session parks or
/// goes idle, under the activation `stale` builds over its store and its
/// session's own activation: the scenario, its store, its nodes and its
/// clock.
async fn resumed_with_stale_state(
    stale: impl FnOnce(Arc<dyn DurableStore>, Arc<dyn Activation>) -> Arc<dyn Activation>,
) -> (V0, Arc<dyn DurableStore>, SimNodes, Arc<SimClock>) {
    let mut resumed = V0::new(Protocol::Code, Dialect::SqliteFile, None);
    resumed.image = Some(&images::SESSION);
    let clock = SimClock::new();
    let database = resumed.database(Arc::clone(&clock)).await;
    let nodes = SimNodes::new(
        Arc::clone(&database),
        Arc::clone(&clock),
        Script::new(),
        resumed.config(),
        stale(Arc::clone(&database), resumed.activation()),
    );
    nodes.start("b");
    let horizon = clock.logical_ms() + 600_000;
    loop {
        let snapshot = database
            .actor(&actor())
            .await
            .expect("the actor reads")
            .expect("the image holds the session");
        if matches!(snapshot.state, ActorState::Parked | ActorState::Idle) {
            break;
        }
        assert!(
            clock.logical_ms() < horizon,
            "the resumed session neither parked nor ended in 600 s of virtual time:\n{}",
            nodes.script().rendered_trace()
        );
        assert!(
            nodes.step().await.is_some(),
            "the resumed session stalled:\n{}",
            nodes.script().rendered_trace()
        );
    }
    (resumed, database, nodes, clock)
}

/// The session's park, as a host reads it through the facade.
async fn facade_park(resumed: &V0) -> Option<lash::SessionParkReason> {
    resumed
        .core()
        .session(session())
        .durable()
        .await
        .expect("the durable session resolves")
        .park_reason()
        .await
        .expect("the session's park reads")
}

/// D-PARKREFUSE (FIG-5592, FIG-5601): a session resumed from an image whose
/// parked driver state this build does not decode (it carries an unknown
/// field) is refused when its turn is restored, before anything of the turn
/// is re-delivered: the cell does not run again, the model is not called,
/// and nothing of the turn commits. The session parks with the typed
/// refusal, which a host reads through the facade, the turn stays open and
/// the run has no end.
#[tokio::test]
async fn a_parked_driver_state_this_build_cannot_decode_is_refused_at_restore_before_any_redelivery()
 {
    let (resumed, database, nodes, _clock) =
        resumed_with_a_stale_checkpoint(with_a_stale_driver_state).await;
    nodes.quiesce().await;

    let mut violations = Vec::new();
    let labels: Vec<CommitLabel> = nodes
        .script()
        .trace()
        .iter()
        .filter(|write| write.kind == WriteKind::Actor && write.committed())
        .map(|write| write.point.label)
        .collect();
    // The first claim's rewrite of the checkpoint, then the park.
    if labels != [CommitLabel::MODEL_DONE, CommitLabel::SESSION_RELEASE] {
        violations.push(format!(
            "the refused session committed {labels:?}, not its park alone: its cell ran again or its turn committed"
        ));
    }
    let programs: usize = resumed.tripwire.counts().vm_programs.values().sum();
    if programs != 0 {
        violations.push(format!("the cell's program was entered {programs} times"));
    }
    let writes = resumed.world.writes();
    if !writes.is_empty() {
        violations.push(format!("the cell's operation ran again: {writes:?}"));
    }
    let requests = resumed.seen.lock_recover().requests.len();
    if requests != 0 {
        violations.push(format!("the model was called {requests} times"));
    }
    match database.actor(&actor()).await {
        Ok(Some(snapshot)) if snapshot.state == ActorState::Parked => {}
        other => violations.push(format!("the session is not parked: {other:?}")),
    }
    match facade_park(&resumed).await {
        Some(lash::SessionParkReason::UndecodableState {
            state: lash::ParkedTurnState::DriverState { driver },
            message,
        }) if driver == lash::rlm::RLM_PROTOCOL_PLUGIN_ID
            && message.contains("left_by_another_build") => {}
        other => violations.push(format!(
            "the facade reads the park {other:?}, not the refused driver state"
        )),
    }
    if !matches!(database.turn(&session()).await, Ok(Some(row)) if row.run == run()) {
        violations.push("the refused turn is no longer open".to_owned());
    }
    match database.turn_end(&session(), &run()).await {
        Ok(None) => {}
        other => violations.push(format!("the run ended: {other:?}")),
    }
    assert!(
        violations.is_empty(),
        "the stale driver state's resume:\n  {}\n{}",
        violations.join("\n  "),
        nodes.script().rendered_trace()
    );
}

/// A session activation whose first claim rewrites the snapshot its
/// unfinished turn's cell resumes from, leaving the parked kernel run
/// inside it as a build of another kernel version sealed it.
struct StaleCellContinuation {
    rewritten: std::sync::atomic::AtomicBool,
    /// What the first claim makes of the cell's stored snapshot.
    stale: fn(&str) -> String,
    database: Arc<dyn DurableStore>,
    session: Arc<dyn Activation>,
}

/// The execution of the cell the 1.0 session image stopped in.
fn image_cell() -> lash_durable::domain::ExecKey {
    lash_durable::domain::ExecKey::Cell(
        session(),
        run(),
        lash_durable::domain::CellId::new("v0-session:v0-turn:1:0:exec_code:3"),
    )
}

/// `snapshot`, a cell's stored snapshot, with its parked kernel run sealed
/// under a kernel version no build interprets, as a build past this one's
/// window would have written it. Everything else the parent checks still
/// holds: the checkpoint and the cell's envelope decode, and the state is
/// sealed again over the same bytes, under the same owner and document.
fn with_a_stale_continuation(snapshot: &str) -> String {
    let mut stale: serde_json::Value =
        serde_json::from_str(snapshot).expect("the image's cell snapshot is JSON");
    let parked: lash_vm_protocol::OpaqueVmState =
        serde_json::from_value(stale["phase"]["parked"]["state"].clone())
            .expect("the image's cell stopped on a parked kernel run");
    stale["phase"]["parked"]["state"] =
        serde_json::to_value(lash_vm_protocol::OpaqueVmState::seal(
            parked.owner().clone(),
            u32::MAX,
            parked.document().to_owned(),
            parked.bytes().to_vec(),
        ))
        .expect("the stale parked run encodes");
    stale.to_string()
}

/// `snapshot`, a cell's stored snapshot, with the bytes of its parked
/// kernel run stating another kernel version than the one they are sealed
/// under. The seal is intact: the same owner, kernel version and document,
/// and a hash that matches the bytes.
fn with_bytes_of_another_kernel_version(snapshot: &str) -> String {
    let mut stale: serde_json::Value =
        serde_json::from_str(snapshot).expect("the image's cell snapshot is JSON");
    let parked: lash_vm_protocol::OpaqueVmState =
        serde_json::from_value(stale["phase"]["parked"]["state"].clone())
            .expect("the image's cell stopped on a parked kernel run");
    let mut run: serde_json::Value =
        serde_json::from_slice(parked.bytes()).expect("the parked run is JSON");
    run["run"]["kernel"] = (parked.kernel() + 1).into();
    stale["phase"]["parked"]["state"] =
        serde_json::to_value(lash_vm_protocol::OpaqueVmState::seal(
            parked.owner().clone(),
            parked.kernel(),
            parked.document().to_owned(),
            serde_json::to_vec(&run).expect("the parked run encodes"),
        ))
        .expect("the stale parked run encodes");
    stale.to_string()
}

#[async_trait::async_trait]
impl Activation for StaleCellContinuation {
    async fn activate(&self, owned: lash_durable::runner::Owned) -> lash_durable::runner::Exit {
        use lash_durable::DomainWrite;
        use lash_durable::domain::SnapshotWrite;
        if !self
            .rewritten
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let row = self
                .database
                .snapshot(&image_cell())
                .await
                .expect("the cell's snapshot reads")
                .expect("the image holds its cell's snapshot");
            let mut tx = owned
                .begin()
                .await
                .expect("the first claim reads its actor");
            tx.write(DomainWrite::Snapshot(SnapshotWrite::Put {
                exec: row.exec.clone(),
                expected: Some(row.rev),
                snapshot_ref: (self.stale)(&row.snapshot_ref),
                executable_identity: row.executable_identity.clone(),
                format_version: row.format_version,
            }));
            owned
                .commit(tx, CommitLabel::CELL_SNAPSHOT)
                .await
                .expect("the stale cell snapshot commits");
        }
        self.session.activate(owned).await
    }
}

/// D-PARKREFUSE (FIG-5613): a session whose stopped cell holds a parked
/// kernel run sealed under a kernel version this build does not read is
/// refused when its turn is restored, by the check of the seal, before the
/// cell is re-delivered.
/// The cell's refusal never reaches the model as a failed cell: no model
/// call is made, nothing of the turn commits, and the session parks with the
/// typed refusal of its cell snapshot, its turn open.
#[tokio::test]
async fn a_parked_cell_continuation_this_build_cannot_decode_is_refused_at_restore_before_any_model_call()
 {
    a_stale_cell_snapshot_is_refused(with_a_stale_continuation, "outside read range").await;
}

/// FIG-5716: the seal is where the parent reads a parked run's kernel
/// version from, so bytes that state another version under an intact seal
/// would be resumed as a version they were not parked under. The restore
/// reads the bytes' own version and refuses the mismatch as it refuses a
/// stale seal: typed, before the cell is re-delivered, and never as a
/// failed cell.
#[tokio::test]
async fn a_parked_run_whose_bytes_state_another_kernel_version_than_its_seal_is_refused_at_restore()
{
    a_stale_cell_snapshot_is_refused(
        with_bytes_of_another_kernel_version,
        "its bytes state kernel version",
    )
    .await;
}

/// The 1.0 session image resumed with `stale` made of its cell's snapshot:
/// no model call is made, nothing of the turn commits, and the session
/// parks with the typed refusal of its cell snapshot, which says `account`.
async fn a_stale_cell_snapshot_is_refused(stale: fn(&str) -> String, account: &str) {
    let (resumed, database, nodes, _clock) = resumed_with_stale_state(move |database, session| {
        Arc::new(StaleCellContinuation {
            rewritten: std::sync::atomic::AtomicBool::new(false),
            stale,
            database,
            session,
        })
    })
    .await;
    nodes.quiesce().await;

    let mut violations = Vec::new();
    let labels: Vec<CommitLabel> = nodes
        .script()
        .trace()
        .iter()
        .filter(|write| write.kind == WriteKind::Actor && write.committed())
        .map(|write| write.point.label)
        .collect();
    // The first claim's rewrite of the cell's snapshot, then the park.
    if labels != [CommitLabel::CELL_SNAPSHOT, CommitLabel::SESSION_RELEASE] {
        violations.push(format!(
            "the refused session committed {labels:?}, not its park alone: its model was called or its turn committed"
        ));
    }
    let requests = resumed.seen.lock_recover().requests.len();
    if requests != 0 {
        violations.push(format!("the model was called {requests} times"));
    }
    let programs: usize = resumed.tripwire.counts().vm_programs.values().sum();
    if programs != 0 {
        violations.push(format!("the cell's program was entered {programs} times"));
    }
    let writes = resumed.world.writes();
    if !writes.is_empty() {
        violations.push(format!("the cell's operation ran again: {writes:?}"));
    }
    match database.actor(&actor()).await {
        Ok(Some(snapshot)) if snapshot.state == ActorState::Parked => {}
        other => violations.push(format!("the session is not parked: {other:?}")),
    }
    match facade_park(&resumed).await {
        Some(lash::SessionParkReason::UndecodableState {
            state: lash::ParkedTurnState::CellSnapshot,
            message,
        }) if message.contains(account) => {}
        other => violations.push(format!(
            "the facade reads the park {other:?}, not the refused cell snapshot"
        )),
    }
    if !matches!(database.turn(&session()).await, Ok(Some(row)) if row.run == run()) {
        violations.push("the refused turn is no longer open".to_owned());
    }
    match database.turn_end(&session(), &run()).await {
        Ok(None) => {}
        other => violations.push(format!("the run ended: {other:?}")),
    }
    assert!(
        violations.is_empty(),
        "the stale cell continuation's resume:\n  {}\n{}",
        violations.join("\n  "),
        nodes.script().rendered_trace()
    );
}

/// D-PARKREFUSE (FIG-5601): a turn whose phase checkpoint this build does
/// not decode parks its session, and a cancel of that turn still ends it.
/// The cancel reads none of the refused state: the turn ends `Cancelled`,
/// the session leaves its park, and neither the cell nor the model runs.
#[tokio::test]
async fn cancelling_a_turn_whose_checkpoint_this_build_cannot_decode_ends_the_turn() {
    let (resumed, database, nodes, clock) =
        resumed_with_a_stale_checkpoint(|_| "another build's checkpoint".to_owned()).await;
    let parked = facade_park(&resumed).await;
    assert!(
        matches!(
            parked,
            Some(lash::SessionParkReason::UndecodableState {
                state: lash::ParkedTurnState::Checkpoint,
                ..
            })
        ),
        "the session did not park on its undecodable checkpoint: {parked:?}\n{}",
        nodes.script().rendered_trace()
    );

    let answer = lash_core::runtime::durable::session::request_turn_cancel(
        &resumed.backend(),
        lash_durable::domain::TurnCancelRequest {
            session: session(),
            run: run(),
            request_id: "stop-the-refused-turn".to_owned(),
            origin: None,
            reason: None,
            undelivered: lash_core::TurnCancelUndeliveredInputPolicy::Defer,
            mode: lash_core::TurnCancelMode::Immediate,
        },
    )
    .await
    .expect("the request commits");
    assert_eq!(answer, lash_durable::domain::TurnCancelAnswer::Requested);
    let horizon = clock.logical_ms() + 600_000;
    while !matches!(database.turn_end(&session(), &run()).await, Ok(Some(_)))
        && clock.logical_ms() < horizon
    {
        if nodes.step().await.is_none() {
            break;
        }
    }
    nodes.quiesce().await;

    let mut violations = Vec::new();
    match database.turn_end(&session(), &run()).await {
        Ok(Some(end)) if end.kind() == lash_core_store::store::RunTerminalKind::Cancelled => {}
        other => violations.push(format!("the cancelled turn's end is {other:?}")),
    }
    if !matches!(database.turn(&session()).await, Ok(None)) {
        violations.push("the cancelled turn is still open".to_owned());
    }
    match database.actor(&actor()).await {
        Ok(Some(snapshot)) if snapshot.state != ActorState::Parked => {}
        other => violations.push(format!("the session is still parked: {other:?}")),
    }
    let programs: usize = resumed.tripwire.counts().vm_programs.values().sum();
    if programs != 0 {
        violations.push(format!("the cell's program was entered {programs} times"));
    }
    let requests = resumed.seen.lock_recover().requests.len();
    if requests != 0 {
        violations.push(format!("the model was called {requests} times"));
    }
    assert!(
        violations.is_empty(),
        "the refused turn's cancel:\n  {}\n{}",
        violations.join("\n  "),
        nodes.script().rendered_trace()
    );
}
