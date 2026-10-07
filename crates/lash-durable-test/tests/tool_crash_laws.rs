//! The tool-semantics laws a node kill must not break, cut at every commit
//! label (FIG-4546; ported by FIG-5210 from the deleted lash-conformance
//! `tool_batch_parallelism/limit.rs` crash law).
//!
//! A host creates a session on a core and sends it an input; the core
//! serves no node of its own here, the simulated nodes A and B run its
//! sessions' turns with its turn services over the production store. The
//! matrix runs the uncut turn, then cuts it at every labelled write under
//! fail-before, ack-hidden, zombie, abort and commit-then-abort, recovers
//! on the other node, and checks the laws:
//!
//! - **Limit:** a turn whose first step's group fills `max_tool_calls` and
//!   whose next step's group asks past it refuses the same calls however it
//!   was cut: no refused call ever runs, and the model is shown the refusal
//!   counting the first group once, whichever executions formed it.
//! - **Identity:** every body entry of one call sees one call id, and two
//!   calls are two ids.
//! - The turn committed once and ended, and a zombie's writes after its reap
//!   are refused.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/dialect.rs"]
mod dialect;
#[path = "support/served.rs"]
mod served;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::ToolDefinitionBindingExt as _;
use lash_core::llm::types::LlmResponse;
use lash_core::runtime::durable::session::SessionActivation;
use lash_core::{ToolCall, ToolOutcome};
use lash_core_execution::StoreSet;
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableError, DurableStore, LeaseConfig};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire, WriteKind,
};
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt as _;

use dialect::Dialect;
use served::Tier;

const SESSION: &str = "tool-crash-session";
const INPUT: &str = "tool-crash-turn";
const PROBE: &str = "crash_probe";

/// The `max_tool_calls` the limit scenarios' session records. One call
/// fills it: a first group of one commits its outcome in one write, so
/// every rerun of the matrix makes the same writes on every database.
const LIMIT: usize = 1;

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

fn actor() -> ActorKey {
    ActorKey::session(SESSION).unwrap()
}

/// Which turn the scenario runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Turn {
    /// Two steps of native calls: `LIMIT`, then `LIMIT + 1`.
    LimitStep,
}

impl Turn {
    fn script(self) -> Vec<LlmResponse> {
        let leaves = |range: std::ops::Range<usize>| {
            range.map(|leaf| format!("leaf-{leaf}")).collect::<Vec<_>>()
        };
        match self {
            Self::LimitStep => [leaves(0..LIMIT), leaves(LIMIT..2 * LIMIT + 1)]
                .iter()
                .map(|step| {
                    served::response(
                        step.iter()
                            .map(|label| {
                                served::call(
                                    &format!("call-{label}"),
                                    PROBE,
                                    serde_json::json!({ "label": label }),
                                )
                            })
                            .collect(),
                    )
                })
                .collect(),
        }
    }

    /// The calls the turn must run, and the ones it must refuse.
    fn calls(self) -> (Vec<String>, Vec<String>) {
        let leaves = |range: std::ops::Range<usize>| {
            range.map(|leaf| format!("leaf-{leaf}")).collect::<Vec<_>>()
        };
        match self {
            Self::LimitStep => (leaves(0..LIMIT), leaves(LIMIT..2 * LIMIT + 1)),
        }
    }

    /// The refusal the model must be shown: after `counted` calls, `rest`
    /// more refused.
    fn refusal(self) -> Option<String> {
        let (counted, requested) = match self {
            Self::LimitStep => (0, LIMIT + 1),
        };
        let exceeded = lash::ToolCallLimitExceeded {
            scope: lash::ToolCallLimitScope::Cell,
            limit: lash::MaxToolCalls::new(LIMIT),
            counted,
            requested,
        };
        refusal_in(&exceeded.to_string())
    }

    fn max_tool_calls(self) -> usize {
        match self {
            Self::LimitStep => LIMIT,
        }
    }
}

/// The refusal sentence in `text`, through the count it refused.
fn refusal_in(text: &str) -> Option<String> {
    let start = text.find("tool call limit exceeded")?;
    let refusal = &text[start..];
    let end = refusal.find(" more")? + " more".len();
    Some(refusal[..end].to_owned())
}

/// Every body entry of every probe call, across every node: the outside
/// world, which no kill undoes.
#[derive(Default)]
struct World {
    entries: Mutex<BTreeMap<String, Vec<lash_core::ToolCallId>>>,
}

impl World {
    fn entries(&self) -> BTreeMap<String, Vec<lash_core::ToolCallId>> {
        self.entries.lock_recover().clone()
    }
}

fn probe_definition() -> lash_core::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_core::ToolDefinition::raw(
        format!("tool:{PROBE}"),
        PROBE,
        "Records each body entry's call id and answers its label.",
        object.clone(),
        object,
    )
    .expect("the probe's schemas")
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], PROBE))
    // A call a kill interrupted runs again at its ordinal, so every call
    // the turn makes is answered and the turn reaches the call past the
    // limit whatever was cut.
    .with_execution_policy(lash_core::ExecutionPolicy::repeatable(
        std::num::NonZeroU32::new(3).expect("a nonzero attempt bound"),
        1,
        1,
    ))
}

struct Probe {
    world: Arc<World>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Probe {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![probe_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == PROBE).then(|| Arc::new(probe_definition().contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let label = call.args["label"].as_str().unwrap_or_default().to_owned();
        self.world
            .entries
            .lock_recover()
            .entry(label.clone())
            .or_default()
            .push(call.context.call_id().clone());
        ToolOutcome::ok(serde_json::json!({ "label": label })).into()
    }
}

/// The scenario on one turn and one dialect, fresh for every matrix cell.
struct Crash {
    turn: Turn,
    dialect: Dialect,
    postgres_url: Option<String>,
    world: Arc<World>,
    scripts: Arc<served::Scripts>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<lash::Backend>>,
    core: Mutex<Option<lash::LashCore>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl Crash {
    fn new(turn: Turn, dialect: Dialect, postgres_url: Option<String>) -> Self {
        let scripts = Arc::new(served::Scripts::default());
        scripts.register(INPUT, turn.script());
        Self {
            turn,
            dialect,
            postgres_url,
            world: Arc::default(),
            scripts,
            tripwire: Arc::default(),
            backend: Mutex::default(),
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

    /// The deployment's core over the scenario's backend: it serves no
    /// node of its own, the simulated nodes run its sessions' turns.
    fn core(&self) -> lash::LashCore {
        let backend = self.backend();
        self.core
            .lock_recover()
            .get_or_insert_with(|| {
                lash::LashCore::standard_builder(backend.clone())
                    .serve_sessions(false)
                    .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .serve_test_llm_profile(
                        served::model(Arc::clone(&self.scripts)),
                        served::metadata(),
                    )
                    .tools(Arc::new(Probe {
                        world: Arc::clone(&self.world),
                    }))
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(
                        "tool-crash-deployment",
                        "tool-crash-boot",
                    ))
                    .expect("the core builds")
            })
            .clone()
    }

    /// The laws of one run, cut at `cut`.
    async fn laws(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let database = nodes.database();
        let trace = nodes.script().trace();

        let commits = trace
            .iter()
            .filter(|write| write.point.label == CommitLabel::TURN_COMMIT && write.committed())
            .count();
        if commits != 1 {
            violations.push(format!("the turn committed {commits} times"));
        }
        match database.turn(&session()).await {
            Ok(None) => {}
            other => violations.push(format!("the turn did not end: {other:?}")),
        }

        // Every call the turn must run ran under one call id per call, and
        // distinct calls are distinct ids.
        let entries = self.world.entries();
        let (run, refused) = self.turn.calls();
        let mut ids = BTreeSet::new();
        for label in &run {
            match entries.get(label) {
                None => violations.push(format!("call `{label}` never ran")),
                Some(seen) => {
                    if seen.iter().any(|id| *id != seen[0]) {
                        violations.push(format!(
                            "call `{label}`'s body entries saw more than one call id: {seen:?}"
                        ));
                    }
                    if !ids.insert(seen[0].clone()) {
                        violations.push(format!("call `{label}` shares another call's id"));
                    }
                }
            }
        }
        // No refused call ever ran, in any execution.
        for label in &refused {
            if let Some(seen) = entries.get(label) {
                violations.push(format!("refused call `{label}` ran {} times", seen.len()));
            }
        }
        // The model was shown the refusal, counting the first group once.
        if let Some(expected) = self.turn.refusal() {
            let shown = self
                .scripts
                .requests(INPUT)
                .iter()
                .filter_map(|request| refusal_in(&request.replace("\\\"", "\"")))
                .collect::<BTreeSet<_>>();
            if shown != BTreeSet::from([expected.clone()]) {
                violations.push(format!(
                    "the model was shown {shown:?}, not only `{expected}`"
                ));
            }
        }

        if let Some(cut) = cut {
            violations.extend(zombie_laws(cut, &trace));
        }
        if !violations.is_empty() {
            violations.push(format!("body entries: {entries:?}"));
            if let Some(last) = self.scripts.requests(INPUT).last() {
                violations.push(format!("last request: {last}"));
            }
        }
        violations
    }
}

#[async_trait::async_trait]
impl Scenario for Crash {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let (stores, database): (Arc<dyn StoreSet>, Arc<dyn DurableStore>) = dialect::open(
            self.dialect,
            self.postgres_url.as_deref(),
            clock,
            &self.keep,
        )
        .await;
        *self.backend.lock_recover() = Some(served::backend(stores));
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
        // The host is outside the deployment under test: its send is uncut.
        let session = self
            .core()
            .session(session())
            .create(lash::SessionCreation::root(served::spec(
                self.turn.max_tool_calls(),
            )))
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
        self.laws(nodes, cut).await
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

/// Cut `turn` on `tier` at every label of its uncut run.
async fn prove(turn: Turn, tier: Tier) {
    let (dialect, postgres_url) = match tier {
        Tier::SqliteMemory => (Dialect::SqliteMemory, None),
        Tier::SqliteFile => (Dialect::SqliteFile, None),
        Tier::Postgres => {
            let Some(url) = dialect::postgres_url() else {
                eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
                return;
            };
            (Dialect::Postgres, Some(url))
        }
    };
    let report = Matrix::new()
        .faults(&[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
        .run(|| Crash::new(turn, dialect, postgres_url.clone()))
        .await;
    let labels: Vec<&str> = report.labels().iter().map(|label| label.as_str()).collect();
    eprintln!(
        "{turn:?} on {dialect:?}: {} cells over {} labels ({})",
        report.cells.len(),
        labels.len(),
        labels.join(", ")
    );
    report.assert_held();
    for label in [
        CommitLabel::MODEL_DONE,
        CommitLabel::ROUND_OUTCOME,
        CommitLabel::TURN_COMMIT,
    ] {
        assert!(
            report.labels().contains(&label),
            "{turn:?}: the matrix never cut {label}"
        );
    }
}

/// A turn that is cut anywhere refuses the same call the same way: the
/// first group's calls are counted once, however many executions formed
/// the group, and the refused calls run in no execution. A step of native
/// calls counts its own group.
async fn tool_call_limit_refuses_the_same_call_across_a_crash(tier: Tier) {
    prove(Turn::LimitStep, tier).await;
}

tiered_laws!(
    current_thread:
    tool_call_limit_refuses_the_same_call_across_a_crash,
);
