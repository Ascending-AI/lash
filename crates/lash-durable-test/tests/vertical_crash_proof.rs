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

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/dialect.rs"]
mod dialect;
#[path = "support/images.rs"]
mod images;
#[path = "support/sim.rs"]
mod sim;

#[path = "support/matrix.rs"]
mod matrix;

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
const CELL: &str = "<typescript>\nconst written = await tools.ext_write({ x: 7 });\nprint(written);\n</typescript>";
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
                // The model's own call is in the transcript once the
                // operation answered it, however it ended.
                let answered = request
                    .messages
                    .iter()
                    .any(|message| message.role == lash_core::llm::types::LlmRole::Assistant);
                let response = match (answered, protocol) {
                    (true, _) => {
                        let told = if rendered.contains(WROTE) {
                            "the write completed"
                        } else {
                            "the write did not complete"
                        };
                        text(&request, &format!("{FINAL}{told}"))
                    }
                    (false, Protocol::Code) => text(&request, CELL),
                    (false, Protocol::Tools) => LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "v0-call".to_owned(),
                            tool_name: TOOL.to_owned(),
                            input_json: r#"{"x":7}"#.to_owned(),
                            replay: None,
                        }],
                        ..LlmResponse::default()
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
                Arc::new(lash::rlm::TypescriptDialect),
                backend,
            )
            .with_worker_service(sim::workers(clock)),
        ),
        Protocol::Tools => lash::LashCore::standard_builder(backend.clone()),
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
            "v0-deployment",
            "v0-boot",
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

    /// The protocol records the session's committed head holds.
    async fn committed_protocol_records(&self) -> Vec<serde_json::Value> {
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
        view.active_events()
            .iter()
            .filter(|record| matches!(record, lash_core::SessionHistoryRecord::Protocol(_)))
            .map(|record| serde_json::to_value(record).expect("a record encodes"))
            .collect()
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
        Protocol::Tools => vec![
            CommitLabel::TURN_ADMIT,
            CommitLabel::MODEL_START,
            CommitLabel::MODEL_DONE,
            CommitLabel::ROUND_OUTCOME,
            CommitLabel::ROUND_PRESENT_MODEL_START,
            CommitLabel::TURN_COMMIT,
            CommitLabel::SESSION_RELEASE,
        ],
    }
}

async fn uncut(protocol: Protocol) {
    let report = Matrix::new()
        .faults(&[])
        .run_test(|| V0::new(protocol, Dialect::SqliteMemory, None))
        .await;
    let labels: Vec<CommitLabel> = report
        .baseline
        .iter()
        .filter(|write| write.kind == WriteKind::Actor && write.committed())
        .map(|write| write.point.label)
        .collect();
    assert_eq!(labels, uncut_labels(protocol));
}

/// A code turn runs to its commit uncut, through every commit label, on
/// one owner with one body entry and one fresh program entry.
#[tokio::test]
async fn the_uncut_code_turn_commits_through_every_label() {
    uncut(Protocol::Code).await;
}

/// A tool turn runs to its commit uncut, through every commit label.
#[tokio::test]
async fn the_uncut_tool_turn_commits_through_every_label() {
    uncut(Protocol::Tools).await;
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
    let uncut = V0::new(Protocol::Code, dialect, postgres_url.clone());
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
    let expected = uncut.committed_protocol_records().await;
    assert!(
        !expected.is_empty(),
        "the uncut code turn committed no protocol record"
    );

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
        // The cut kills A, which leaves no timer armed: the step that cuts
        // can be the last.
        let stepped = before.step().await;
        assert!(
            stepped.is_some() || !before.script().cuts().is_empty(),
            "the turn stalled before its second model call started:\n{}",
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
    assert_eq!(
        cold.committed_protocol_records().await,
        expected,
        "the resumed turn on {dialect:?} committed other protocol history than the uncut one"
    );
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
