//! A host observes a durable process's lifecycle and effects (FIG-5372).
//!
//! Each law builds a deployment the way a host does: a durable backend over
//! a store set, and a lash core over it, whose own node runs the
//! processes. Work enters only through the core's process API.
//!
//! - **effects:** each committed step outcome that names an effect node is
//!   one `process.effect_outcome` event; one past the per-node cap is
//!   counted in the `process.effect_omissions` record its terminal carries.
//! - **publication across commits and cuts** (FIG-5396), on simulated nodes
//!   and virtual time: a host append held across the actor's terminal
//!   commit publishes each event once; a successor of an owner that died
//!   between a commit and its publication delivers what it committed, and
//!   enters no second wait.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/sim.rs"]
mod sim;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::ProcessEventLogTestSupport as _;
use lash_core_execution::StoreSet;

/// The law `$law(tier)` on SQLite in memory and PostgreSQL; the PostgreSQL
/// leg runs when the run is handed a server.
macro_rules! on_every_tier {
    ($law:ident) => {
        mod $law {
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn on_sqlite_memory() {
                super::$law(super::Tier::SqliteMemory).await;
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn on_postgres() {
                if super::postgres_url().is_none() {
                    return;
                }
                super::$law(super::Tier::Postgres).await;
            }
        }
    };
}

/// Where a law's database lives.
#[derive(Clone, Copy, Debug)]
enum Tier {
    SqliteMemory,
    Postgres,
}

fn postgres_url() -> Option<String> {
    std::env::var("LASH_POSTGRES_DATABASE_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
}

/// A fresh store set of `tier`, and what must outlive it.
async fn stores(tier: Tier) -> (Arc<dyn StoreSet>, Vec<Box<dyn std::any::Any + Send>>) {
    match tier {
        Tier::SqliteMemory => (
            Arc::new(
                lash_sqlite_store::SqliteStoreSet::memory()
                    .await
                    .expect("an in-memory store set opens"),
            ),
            Vec::new(),
        ),
        Tier::Postgres => {
            let url = postgres_url().expect("a PostgreSQL URL");
            let isolated = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
            let storage = lash_postgres_store::testing::connect(isolated.url())
                .await
                .expect("the isolated database opens");
            let stores = lash_postgres_store::PostgresStoreSet::new(
                &storage,
                Arc::new(lash_core_store::attachments::UnavailableAttachmentStore),
            );
            (Arc::new(stores), vec![Box::new(isolated)])
        }
    }
}

/// What a process event sink attached to a node's registry heard.
#[derive(Clone, Default)]
struct Heard(Arc<Mutex<Vec<lash::process::ProcessEvent>>>);

#[async_trait::async_trait]
impl lash::process::ProcessEventSink for Heard {
    async fn emit(&self, event: &lash::process::ProcessEvent) {
        self.0.lock().unwrap().push(event.clone());
    }
}

impl Heard {
    /// The sequences heard of `process`, in the order they were heard.
    fn sequences(&self, process: &lash_core::ProcessId) -> Vec<u64> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.process_id == *process)
            .map(|event| event.sequence)
            .collect()
    }
}

/// A core over `stores` whose node advances `engine`, with the write tool.
fn core(
    stores: &Arc<dyn StoreSet>,
    engine: Advance,
    world: &Arc<World>,
    boot: &str,
) -> (lash::Backend, lash::LashCore) {
    let backend = lash::durable::DurableBackendBuilder::new(Arc::clone(stores))
        .process_engine(Arc::new(ScriptEngine { advance: engine }))
        .build()
        .expect("the backend assembles");
    let core = lash::LashCore::standard_builder(backend.clone())
        .tools(write_tool(world))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("lifecycle-events-deployment"),
            lash::persistence::LeaseIncarnationId::new(boot),
        ))
        .expect("the core builds");
    (backend, core)
}

// --- the engine ---------------------------------------------------------------

const KIND: &str = "lifecycle-events-engine";

/// How the scripted engine answers each event: its state is a JSON value it
/// rewrites in place.
type Advance = fn(&mut serde_json::Value, lash_core::EngineEvent) -> lash_core::EngineAction;

struct ScriptEngine {
    advance: Advance,
}

fn infra(error: impl std::fmt::Display) -> lash_core::ProcessInfraError {
    lash_core::ProcessInfraError::new(lash_core::PluginError::Session(error.to_string()))
}

fn answer(value: serde_json::Value) -> lash_core::EngineAction {
    lash_core::EngineAction::Terminal(lash_core::ProcessOutcome::from_tool_output(
        lash_core::ToolCallOutput::success(value),
    ))
}

fn cancelled(origin: lash_sansio::CancelOrigin) -> lash_core::EngineAction {
    lash_core::EngineAction::Terminal(lash_core::ProcessOutcome::from_tool_output(
        lash_core::ToolCallOutput::cancelled(
            lash_core::ToolCancellation::runtime("the scripted engine answered its cancel")
                .with_origin(origin),
        ),
    ))
}

#[async_trait::async_trait]
impl lash_core::ProcessEngine for ScriptEngine {
    async fn check_args(
        &self,
        _signature: &lash_core_execution::ProcessSignature,
        _args: &serde_json::Map<String, serde_json::Value>,
        _mode: lash_core_execution::ArgsMode,
    ) -> std::result::Result<(), lash_core_execution::ArgsMismatch> {
        Err(lash_core_execution::ArgsMismatch::UnsupportedSignature {
            engine_kind: self.kind().into(),
        })
    }

    fn kind(&self) -> &'static str {
        KIND
    }

    fn state_format(&self) -> lash_core::EngineStateFormat {
        lash_core::EngineStateFormat {
            kind: KIND.to_owned(),
            version: 0,
        }
    }

    fn cancel_grace(&self) -> Duration {
        Duration::from_secs(1)
    }

    fn program_identity(
        &self,
        _payload: &serde_json::Value,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    fn creation_config(
        &self,
        _env_spec: &lash_core::ProcessExecutionEnvSpec,
    ) -> Result<Option<serde_json::Value>, lash_core::PluginError> {
        Ok(None)
    }

    fn advance(
        &self,
        state: lash_core::EngineState,
        event: lash_core::EngineEvent,
    ) -> Result<(lash_core::EngineState, lash_core::EngineAction), lash_core::ProcessInfraError>
    {
        let mut script = if state.bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&state.bytes).map_err(infra)?
        };
        let action = (self.advance)(&mut script, event);
        Ok((
            lash_core::EngineState {
                format: self.state_format(),
                bytes: serde_json::to_vec(&script).map_err(infra)?,
            },
            action,
        ))
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Vec<lash_core::ArtifactName>, lash_core::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), lash_core::PluginError> {
        Err(lash_core::PluginError::Session(format!(
            "a scripted engine stores no artifact `{artifact_ref}`"
        )))
    }

    async fn resolve(
        &self,
        _reference: &lash_core::ProcessDefinitionRef,
    ) -> Result<lash_core::ProcessDefinitionResolution, lash_core::ProcessDefinitionRefusal> {
        Ok(lash_core::ProcessDefinitionResolution::new(
            lash_core::ProcessSignature::Unknown,
        ))
    }
}

// --- the write tool -------------------------------------------------------------

const WRITE_TOOL: &str = "lifecycle_events_write";

/// What the write tool wrote, per call.
#[derive(Debug, Default)]
struct World {
    writes: Mutex<Vec<serde_json::Value>>,
    calls: Mutex<Vec<lash_core::ToolCallId>>,
    parked: Mutex<Option<(String, lash_core::ToolCallId)>>,
}

struct Write {
    world: Arc<World>,
}

#[async_trait::async_trait]
impl lash::tools::StaticToolExecute for Write {
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.args["x"] == 999 {
            let key = call
                .context
                .completion_key()
                .expect("a deferring call has a key");
            *self.world.parked.lock().unwrap() =
                Some((key.as_str().to_owned(), call.context.call_id().clone()));
            return lash_core::ToolAttemptOutcome::Pending(lash_core::PendingCompletion::new());
        }
        self.world.writes.lock().unwrap().push(call.args.clone());
        self.world
            .calls
            .lock()
            .unwrap()
            .push(call.context.call_id().clone());
        lash_core::ToolOutcome::ok(serde_json::json!({ "wrote": call.args })).into()
    }
}

fn write_tool(world: &Arc<World>) -> Arc<dyn lash_core::ToolProvider> {
    use lash_core::ToolDefinitionBindingExt as _;
    let definition = lash_core::ToolDefinition::raw(
        WRITE_TOOL,
        WRITE_TOOL,
        "Writes x to the outside world, once.",
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "x": { "type": "number" } },
            "required": ["x"]
        }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("the write tool's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_declaration(lash_core::ToolDeclaration::deferring())
    .with_park(lash_core::ParkBound::Within(Duration::from_secs(300)))
    .with_execution_policy(lash_core::ExecutionPolicy::Once)
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], WRITE_TOOL));
    Arc::new(lash::tools::StaticToolProvider::new(
        vec![definition],
        Write {
            world: Arc::clone(world),
        },
    ))
}

/// A write step for `x`, run for `site` when it names one.
fn write(step: &str, x: u64, site: Option<(&str, u64)>) -> lash_core::EngineAction {
    lash_core::EngineAction::Steps {
        steps: vec![lash_core::StepRequest::Tool {
            step: lash_core::StepName(step.to_owned()),
            tool: lash_core::ToolId::new(WRITE_TOOL),
            input: serde_json::json!({ "x": x }),
            site: site.map(|(node_id, occurrence)| lash_core::StepEffectSite {
                node_id: node_id.to_owned(),
                occurrence,
                context: Default::default(),
            }),
        }],
        wake: None,
    }
}

// --- the host's side --------------------------------------------------------------

/// The environment every start in these laws captures.
fn environment() -> lash_core::ProcessExecutionEnvSpec {
    let mut environment = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        lash_core::SessionPolicy::new(
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(16),
            lash_core::NoProgressBudget::bounded(12),
        ),
        lash_core::SessionToolAccess::ambient(),
    );
    environment.render = Some(lash_core::RecordedRender {
        renderer_id: lash::render::ToolOutputRendererSlot::default()
            .0
            .id()
            .to_owned(),
        params: serde_json::to_value(lash::render::ResolvedStandardRenderConfig {
            defaults: lash::render::ToolRenderParams::default(),
            per_tool: std::collections::BTreeMap::new(),
        })
        .expect("the render config encodes"),
    });
    environment
}

/// Start a detached process of the scripted engine.
async fn start(core: &lash::LashCore) -> lash_core::ProcessId {
    let env_ref = core
        .host_artifacts()
        .publish_process_env(&lash_core::HostArtifactPin::mint(), &environment())
        .await
        .expect("the environment is published");
    let request = lash_core::ProcessStartRequest::new(
        lash_core::ProcessInput::Engine {
            kind: KIND.to_owned(),
            payload: serde_json::Value::Null,
        },
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .with_env_ref(env_ref);
    core.processes()
        .start(request, core.effect_host())
        .await
        .expect("the process starts")
        .process_id
}

async fn ended(core: &lash::LashCore, process: &lash_core::ProcessId) {
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        core.processes().await_output(process),
    )
    .await
    .expect("the process ends within a minute")
    .expect("the process's end is read");
    let lash_core::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the process ended without an answer: {output:?}");
    };
    assert!(output.is_success(), "the process failed: {output:?}");
}

/// `process`'s whole durable log.
async fn log(
    backend: &lash::Backend,
    process: &lash_core::ProcessId,
) -> Vec<lash::process::ProcessEvent> {
    backend
        .process_registry()
        .full_event_window(process, 0)
        .await
        .expect("the log is read")
}

fn of_type<'a>(
    events: &'a [lash::process::ProcessEvent],
    event_type: &str,
) -> Vec<&'a lash::process::ProcessEvent> {
    events
        .iter()
        .filter(|event| event.fact.event_type() == event_type)
        .collect()
}

// --- waiting ----------------------------------------------------------------------

// --- effects ----------------------------------------------------------------------

const NODE: &str = "node.write";

/// How many writes [`effects_advance`] issues: one more than a node records.
const NODE_WRITES: u64 = lash_core::PROCESS_EFFECT_OCCURRENCE_CAP + 1;

/// The site of the `n`-th write of [`NODE`], from 0: the writes alternate
/// between two call sites of the one node, each counting its own occurrences
/// from 1.
fn node_site(n: u64) -> lash_core::StepEffectSite {
    lash_core::StepEffectSite {
        node_id: NODE.to_owned(),
        occurrence: n / 2 + 1,
        context: lash_sansio::WorkflowOccurrenceContext {
            site_path: lash_sansio::WorkflowSitePath::slots([lash_sansio::ExprSlot::Item(
                u32::try_from(n % 2).expect("0 or 1"),
            )]),
            loops: Vec::new(),
        },
    }
}

/// Writes [`NODE_WRITES`] times for [`NODE`], one settled before the next,
/// alternating its two sites, then ends.
fn effects_advance(
    state: &mut serde_json::Value,
    event: lash_core::EngineEvent,
) -> lash_core::EngineAction {
    let write_at = |n: u64| {
        let mut action = write(&format!("write-{n}"), n + 1, None);
        if let lash_core::EngineAction::Steps { steps, .. } = &mut action
            && let Some(lash_core::StepRequest::Tool { site, .. }) = steps.first_mut()
        {
            *site = Some(node_site(n));
        }
        action
    };
    match event {
        lash_core::EngineEvent::Started { .. } => write_at(0),
        lash_core::EngineEvent::StepSettled { .. } => {
            let settled = state.as_u64().unwrap_or(0) + 1;
            *state = serde_json::json!(settled);
            if settled < NODE_WRITES {
                write_at(settled)
            } else {
                answer(serde_json::json!("done"))
            }
        }
        lash_core::EngineEvent::Cancelled { origin, .. } => cancelled(origin),
        _ => lash_core::EngineAction::Idle,
    }
}

/// Each committed step outcome that names an effect node is one
/// `process.effect_outcome` of that node at its exact site, keyed by its
/// call. A node records `PROCESS_EFFECT_OCCURRENCE_CAP` occurrences across
/// all of its sites, however each site numbers its own; a later one is
/// counted in the omissions record committed just before the terminal.
async fn each_committed_effect_is_one_effect_outcome_event(tier: Tier) {
    let (stores, _keep) = stores(tier).await;
    let world = Arc::new(World::default());
    let (backend, core) = core(&stores, effects_advance, &world, "effects");
    let process = start(&core).await;
    ended(&core, &process).await;

    let events = log(&backend, &process).await;
    let fleet = lash_core::FleetFormat::current();
    let outcomes: Vec<lash_core::ProcessEffectOccurrence> =
        of_type(&events, lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE)
            .into_iter()
            .map(|event| {
                lash_core::ProcessEffectOccurrence::decode(event.fact.payload(), fleet)
                    .expect("an effect outcome decodes")
            })
            .collect();
    let recorded: Vec<(lash_sansio::WorkflowOccurrenceContext, u64)> = outcomes
        .iter()
        .map(|outcome| (outcome.context.clone(), outcome.occurrence))
        .collect();
    let first_cap: Vec<_> = (0..lash_core::PROCESS_EFFECT_OCCURRENCE_CAP)
        .map(node_site)
        .map(|site| (site.context, site.occurrence))
        .collect();
    assert_eq!(
        recorded, first_cap,
        "the node's first cap occurrences, each at its own site: {events:#?}"
    );
    for outcome in &outcomes {
        assert_eq!(outcome.node_id, NODE);
        assert_eq!(outcome.operation, WRITE_TOOL);
        assert_eq!(
            outcome.outcome_class,
            lash_core::ProcessEffectOutcomeClass::Success
        );
        assert!(outcome.code.is_none());
    }

    let omissions = of_type(&events, lash_core::PROCESS_EFFECT_OMISSIONS_EVENT_TYPE);
    assert_eq!(omissions.len(), 1, "one omissions record: {events:#?}");
    let omitted = lash_core::ProcessEffectOmissions::decode(omissions[0].fact.payload(), fleet)
        .expect("the omissions decode");
    assert_eq!(omitted.nodes[NODE].success, 1);
    assert_eq!(omitted.nodes[NODE].total(), 1);
    let terminal = events.last().expect("the log ends");
    assert_eq!(terminal.fact.event_type(), "process.completed");
    assert_eq!(
        omissions[0].sequence + 1,
        terminal.sequence,
        "the omissions record commits just before the terminal"
    );
    assert_eq!(
        world.writes.lock().unwrap().len(),
        usize::try_from(NODE_WRITES).expect("a small count")
    );
}

on_every_tier!(each_committed_effect_is_one_effect_outcome_event);

/// D-TOOLRECORD: both tool calls join from durable effect occurrences after
/// the deployment and its SQLite file store have been reopened (FIG-5550).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn process_effects_name_their_tool_call_records_after_a_sqlite_reopen() {
    fn advance(
        state: &mut serde_json::Value,
        event: lash_core::EngineEvent,
    ) -> lash_core::EngineAction {
        match event {
            lash_core::EngineEvent::Started { .. } => write("first", 1, Some((NODE, 1))),
            lash_core::EngineEvent::StepSettled { .. } if state.is_null() => {
                *state = serde_json::json!("first");
                write("second", 2, Some((NODE, 2)))
            }
            lash_core::EngineEvent::StepSettled { .. } => answer(serde_json::json!("done")),
            _ => lash_core::EngineAction::Idle,
        }
    }
    let directory = tempfile::tempdir().expect("a SQLite file directory");
    let path = directory.path().join("lash.db");
    let world = Arc::new(World::default());
    let process = {
        let stores: Arc<dyn StoreSet> = Arc::new(
            lash_sqlite_store::SqliteStoreSet::open(
                &path,
                lash_sqlite_store::SqliteSynchronous::Normal,
            )
            .await
            .expect("the SQLite file opens"),
        );
        let (_backend, core) = core(&stores, advance, &world, "before-reopen");
        let process = start(&core).await;
        ended(&core, &process).await;
        core.shutdown().await.expect("the first deployment stops");
        process
    };
    let stores: Arc<dyn StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(
            &path,
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .expect("the SQLite file reopens"),
    );
    let (backend, core) = core(&stores, advance, &world, "after-reopen");
    let events = log(&backend, &process).await;
    let outcomes = of_type(&events, lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE);
    assert_eq!(
        outcomes.len(),
        2,
        "both calls retain their effect occurrences"
    );
    let executed = world.calls.lock().unwrap().clone();
    assert_eq!(executed.len(), 2);
    for (index, (event, expected)) in outcomes.iter().zip(&executed).enumerate() {
        let occurrence = lash_core::ProcessEffectOccurrence::decode(
            event.fact.payload(),
            lash_core::FleetFormat::current(),
        )
        .expect("the stored occurrence decodes");
        let call_id = occurrence
            .call_id
            .expect("a tool effect retains its call id");
        assert_eq!(
            &call_id, expected,
            "the effect names the executed tool call"
        );
        let record = core
            .processes()
            .tool_call(&process, &call_id)
            .await
            .expect("the retained tool record reads")
            .expect("the tool call has a record");
        assert_eq!(record.call_id, call_id);
        assert_eq!(record.tool, WRITE_TOOL);
        let args = serde_json::json!({ "x": index + 1 });
        assert_eq!(record.args, args);
        assert_eq!(
            record.output,
            lash_core::ToolCallOutput::success(serde_json::json!({ "wrote": args }))
        );
    }
    core.shutdown()
        .await
        .expect("the reopened deployment stops");
}

// --- publication across commits and cuts -------------------------------------------

/// The host's half of a step: these laws' processes ask for none.
struct NoSteps;

#[async_trait::async_trait]
impl lash_core_execution::runtime::process::steps::ProcessSteps for NoSteps {
    fn stop_grace(&self) -> std::time::Duration {
        std::time::Duration::from_secs(2)
    }

    async fn admit(
        &self,
        _process: &lash_core::ProcessRecord,
        step: &lash_core::StepRequest,
        _now_ms: u64,
    ) -> Result<
        lash_core_execution::runtime::process::steps::StepAdmission,
        lash_core_execution::runtime::process::steps::StepRefusal,
    > {
        Err(
            lash_core_execution::runtime::process::steps::StepRefusal::UnknownTool {
                step: step.step().0.clone(),
                tool: step.admitted_tool(KIND).as_str().to_owned(),
            },
        )
    }

    /// Never asked: no step of these parks.
    fn resolved(
        &self,
        _process: &lash_core::ProcessRecord,
        _step: &lash_core::StepRequest,
        _execution: &lash_core_execution::runtime::actor::round::AdmittedExecution,
        _parked: &lash_core_execution::runtime::actor::round::Material<
            lash_core_store::tool_run::CompletionSource,
        >,
        _resolution: lash_core_execution::runtime::actor::waits::Resolution,
    ) -> lash_core_execution::runtime::actor::round::SettledOutput {
        lash_core_execution::runtime::actor::round::SettledOutput::Interrupted
    }

    fn body(
        &self,
        _runtime: &Arc<lash_core_execution::runtime::process::StepRuntime>,
        _process: &lash_core::ProcessRecord,
        step: &lash_core::StepRequest,
        _execution: &lash_core_execution::runtime::actor::round::AdmittedExecution,
    ) -> lash_core_execution::runtime::actor::round::MemberBody {
        unreachable!("no step of `{}` is ever admitted", step.step().0)
    }
}

/// How many virtual-time steps a fleet law drives before it gives up.
const STEPS: usize = 400;

/// A simulated deployment: the production process activation over a SQLite
/// memory store set on virtual time, each node publishing to its own
/// host's sinks. A host outside the deployment writes through its node's
/// watched registry; the nodes' own writes go through the fault script.
struct Fleet {
    clock: Arc<lash_durable_test::SimClock>,
    backend: lash_core_execution::Backend,
    nodes: lash_durable_test::SimNodes,
}

impl Fleet {
    async fn new(
        script: lash_durable_test::Script,
        advance: fn(&mut serde_json::Value, lash_core::EngineEvent) -> lash_core::EngineAction,
    ) -> Self {
        let clock = lash_durable_test::SimClock::new();
        let stores = sim::memory(Arc::clone(&clock)).await;
        let database: Arc<dyn lash_durable::DurableStore> = Arc::new(stores.durable_store());
        let backend = lash_core_execution::Backend::assemble(lash_core_execution::BackendParts {
            stores: Arc::new(stores),
            settings: lash_core_execution::DurableSettings::default(),
            engines: vec![Arc::new(ScriptEngine { advance })],
            providers: Arc::new(lash_core_execution::NoProjectionProviders),
            formats: Vec::new(),
        })
        .expect("the fleet's backend assembles");
        let nodes = lash_durable_test::SimNodes::new(
            database,
            Arc::clone(&clock),
            script,
            lash_durable_test::SimNodesConfig {
                lease: lash_durable::LeaseConfig::default(),
                decodes: backend.formats().decodes(),
                max_active: 8,
            },
            Self::activation(&backend, None),
        );
        Self {
            clock,
            backend,
            nodes,
        }
    }

    fn activation(
        backend: &lash_core_execution::Backend,
        watched: Option<&lash_core_execution::WatchedRegistry>,
    ) -> Arc<dyn lash_durable::runner::Activation> {
        let activation = lash_core_execution::runtime::actor::process::ProcessActivation::new(
            backend.clone(),
            Arc::new(NoSteps),
            Arc::new(lash_durable_test::Tripwire::default()),
        );
        Arc::new(match watched {
            Some(watched) => activation.with_process_events(watched.clone()),
            None => activation,
        })
    }

    /// A host on `node`: a watched registry over `inner` whose sink is
    /// `heard`, which `node`'s activation publishes to.
    fn host(
        &self,
        node: &str,
        inner: Arc<dyn lash_core::ProcessRegistry>,
        heard: &Heard,
    ) -> (lash_core_execution::WatchedRegistry, impl Send + Sync) {
        let watched = lash_core_execution::runtime::watch_process_registry(inner);
        let registration = watched.add_event_sink(Arc::new(heard.clone()));
        self.nodes
            .activate_on(node, Self::activation(&self.backend, Some(&watched)));
        (watched, registration)
    }

    /// Register a detached process of the scripted engine.
    async fn register(&self) -> lash_core::ProcessId {
        let registration = lash_core::ProcessRegistration::new(
            lash_core::ProcessInput::Engine {
                kind: KIND.to_owned(),
                payload: serde_json::Value::Null,
            },
            lash_core::ProcessProvenance::host(),
            lash_core::LifetimeDecision::Detached,
        )
        .with_execution_env_ref(Some(
            lash_core_execution::testing::process_execution_env_fixture_ref(),
        ));
        self.backend
            .process_registry()
            .register_process(registration)
            .await
            .expect("the process registers")
            .id
    }

    async fn events(&self, process: &lash_core::ProcessId) -> Vec<lash::process::ProcessEvent> {
        self.backend
            .process_registry()
            .full_event_window(process, 0)
            .await
            .expect("the log is read")
    }

    async fn log(&self, process: &lash_core::ProcessId) -> Vec<u64> {
        self.events(process)
            .await
            .iter()
            .map(|event| event.sequence)
            .collect()
    }

    async fn types(&self, process: &lash_core::ProcessId) -> Vec<String> {
        self.events(process)
            .await
            .into_iter()
            .map(|event| event.fact.event_type().to_owned())
            .collect()
    }

    async fn ended(&self, process: &lash_core::ProcessId) -> bool {
        self.backend
            .process_registry()
            .get_process(process)
            .await
            .expect("the record is read")
            .is_some_and(|record| record.is_terminal())
    }

    async fn actor_state(
        &self,
        process: &lash_core::ProcessId,
    ) -> Option<lash_durable::ActorState> {
        self.nodes
            .database()
            .actor(&lash_durable::ActorKey::process(process.as_str()).expect("an actor key"))
            .await
            .expect("the actor is read")
            .map(|snapshot| snapshot.state)
    }

    /// Step virtual time until `done` holds, at most [`STEPS`] times.
    async fn until<F, Fut>(&self, what: &str, mut done: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        for _ in 0..STEPS {
            self.nodes.quiesce().await;
            if done().await {
                return;
            }
            if self.nodes.step().await.is_none() {
                self.clock.advance_by(1_000).await;
            }
        }
        panic!(
            "{what} never happened\n{}",
            self.nodes.script().rendered_trace()
        );
    }
}

/// Host cancellation holds publication across the actor's terminal commit.
/// Both publishers share one mark, so every event arrives once in log order
/// even after the terminal is committed (FIG-5396).
async fn a_host_append_held_across_the_terminal_commit_publishes_each_event_once() {
    let fleet = Fleet::new(lash_durable_test::Script::new(), publication_idle).await;
    let faults =
        lash_core_execution::runtime::ProcessRegistryFaults::new(fleet.backend.process_registry());
    let heard = Heard::default();
    let (watched, _sink) = fleet.host("a", Arc::new(faults.clone()), &heard);
    let process = fleet.register().await;
    fleet.nodes.start("a");
    fleet
        .until("the waiting process published and released", || async {
            fleet.actor_state(&process).await == Some(lash_durable::ActorState::Waiting)
                && heard.sequences(&process) == fleet.log(&process).await
        })
        .await;

    let held = faults.pause_next_event_page();
    let requested = tokio::spawn({
        let registry = Arc::clone(watched.registry());
        let process = process.clone();
        async move {
            registry
                .request_process_cancel(
                    &process,
                    lash_core::CancelOrigin::OperatorRequested,
                    "publication-host".to_owned(),
                    None,
                )
                .await
                .expect("host cancellation is admitted");
        }
    });
    held.wait_until_validated().await;
    fleet
        .until("the cancelled actor commits the terminal", || async {
            fleet.ended(&process).await
        })
        .await;
    held.resume();
    requested.await.expect("the host cancellation task");
    fleet
        .until("the ended process released", || async {
            fleet.actor_state(&process).await == Some(lash_durable::ActorState::Terminal)
        })
        .await;

    assert_eq!(
        fleet.types(&process).await,
        ["process.cancel_requested", "process.cancelled"]
    );
    let logged = fleet.log(&process).await;
    assert_eq!(
        heard.sequences(&process),
        logged,
        "the sink heard every logged event once, in order"
    );
}

#[tokio::test]
async fn a_host_append_held_across_the_terminal_commit_publishes_each_event_once_on_sqlite_memory()
{
    a_host_append_held_across_the_terminal_commit_publishes_each_event_once().await;
}

/// A dies after committing a terminal and before publishing it. B takes over
/// and publishes that durable event once with its logged identity (FIG-5396).
async fn a_takeover_after_a_cut_between_commit_and_publish_delivers_the_dead_owners_events() {
    let script = lash_durable_test::Script::new();
    script.cut_on(
        "a",
        lash_durable::CommitLabel::PROCESS_TERMINAL,
        1,
        lash_durable_test::Fault::CommitThenAbort,
    );
    let fleet = Fleet::new(script, publication_terminal).await;
    let (a_heard, b_heard) = (Heard::default(), Heard::default());
    let (_a, _a_sink) = fleet.host("a", fleet.backend.process_registry(), &a_heard);
    let (_b, _b_sink) = fleet.host("b", fleet.backend.process_registry(), &b_heard);
    let process = fleet.register().await;
    fleet.nodes.start("a");
    fleet
        .until("A commits its terminal and dies", || async {
            !fleet.nodes.script().cuts().is_empty()
        })
        .await;
    assert!(
        a_heard.sequences(&process).is_empty(),
        "A died before it published"
    );
    assert_eq!(
        fleet.types(&process).await,
        ["process.completed"],
        "A's terminal"
    );
    let committed = fleet.log(&process).await;

    fleet.nodes.start("b");
    fleet
        .until("B publishes what A committed", || async {
            b_heard.sequences(&process).starts_with(&committed)
        })
        .await;
    fleet
        .until("the ended process released", || async {
            fleet.actor_state(&process).await == Some(lash_durable::ActorState::Terminal)
        })
        .await;

    assert_eq!(fleet.types(&process).await, ["process.completed"]);
    let logged = fleet.log(&process).await;
    assert_eq!(
        b_heard.sequences(&process),
        logged,
        "B's sink heard every logged event once, in order"
    );
    let log = fleet.events(&process).await;
    let heard = b_heard.0.lock().unwrap().clone();
    for (event, logged) in heard.iter().zip(&log) {
        assert_eq!(
            (&event.process_id, event.sequence, &event.fact.event_type()),
            (
                &logged.process_id,
                logged.sequence,
                &logged.fact.event_type()
            ),
            "a heard event is the logged event of its (process, sequence)"
        );
    }
}

#[tokio::test]
async fn a_takeover_after_a_cut_between_commit_and_publish_delivers_the_dead_owners_events_on_sqlite_memory()
 {
    a_takeover_after_a_cut_between_commit_and_publish_delivers_the_dead_owners_events().await;
}

// FIG-5411: a deferred call has one waiting transition and resolution resumes it.
fn deferred_advance(
    _: &mut serde_json::Value,
    event: lash_core::EngineEvent,
) -> lash_core::EngineAction {
    match event {
        lash_core::EngineEvent::Started { .. } => write("deferred", 999, None),
        lash_core::EngineEvent::StepSettled { .. } => answer(serde_json::json!("done")),
        lash_core::EngineEvent::Cancelled { origin, .. } => cancelled(origin),
        _ => lash_core::EngineAction::Idle,
    }
}

async fn a_deferred_call_records_one_waiting_event_and_resumes(tier: Tier) {
    let (stores, _keep) = stores(tier).await;
    let world = Arc::new(World::default());
    let (backend, core) = core(&stores, deferred_advance, &world, "deferred");
    let process = start(&core).await;
    let (key, call_id) = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let parked = world.parked.lock().unwrap().clone();
            if let Some(parked) = parked {
                let actor = lash_core::durable_port::ActorKey::process(process.as_str()).unwrap();
                if backend
                    .durable()
                    .actor(&actor)
                    .await
                    .unwrap()
                    .is_some_and(|row| row.state == lash_core::durable_port::ActorState::Waiting)
                {
                    break parked;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the process releases on its deferred call");
    let events = log(&backend, &process).await;
    let waits = of_type(&events, "process.waiting");
    assert_eq!(
        waits.len(),
        1,
        "a deferred call records exactly one waiting event"
    );
    assert_eq!(
        waits[0].fact.payload()["wait"]["kind"]["call_id"],
        serde_json::json!(call_id)
    );
    assert_eq!(
        waits[0].fact.payload()["wait"]["kind"]["tool_id"],
        WRITE_TOOL
    );
    assert!(
        !waits[0].fact.payload().to_string().contains(&key),
        "the event never publishes a bearer key"
    );
    core.completions()
        .resolve(&key, lash_core::Resolution::Ok(serde_json::json!({})))
        .await
        .unwrap();
    ended(&core, &process).await;
    let events = log(&backend, &process).await;
    assert_eq!(of_type(&events, "process.waiting").len(), 1);
    assert_eq!(of_type(&events, "process.resumed").len(), 1);
}
on_every_tier!(a_deferred_call_records_one_waiting_event_and_resumes);

/// The actor released on this process's one deferred call, including its
/// durable call identity and the deadline its admission pinned.
async fn parked_call(
    core: &lash::LashCore,
    backend: &lash::Backend,
    process: &lash_core::ProcessId,
) -> lash::admin::ParkedCall {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let actor = lash::durable::ActorKey::process(process.as_str()).unwrap();
            if backend
                .durable()
                .actor(&actor)
                .await
                .unwrap()
                .is_some_and(|row| row.state == lash::durable::ActorState::Waiting)
            {
                let mut calls = core
                    .completions()
                    .parked(lash::admin::CallOwner::Process(process.clone()))
                    .await
                    .unwrap();
                assert_eq!(
                    calls.len(),
                    1,
                    "the owner lists exactly its unresolved call"
                );
                return calls.remove(0);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the process parks on its call")
}

/// FIG-5411: takeover retains the pending call, identity, capability and
/// deadline; settled calls are absent and the earlier log is preserved.
async fn a_deferred_call_survives_takeover(tier: Tier) {
    let (stores, _keep) = stores(tier).await;
    let world = Arc::new(World::default());
    let (backend, first) = core(&stores, deferred_advance, &world, "park-first");
    let process = start(&first).await;
    let before = parked_call(&first, &backend, &process).await;
    assert_eq!(
        before.owner,
        lash::admin::CallOwner::Process(process.clone())
    );
    assert_eq!(before.tool_id.as_str(), WRITE_TOOL);
    let wait = backend
        .durable()
        .wait(&lash::durable::domain::WaitId::parse_hex(before.key.as_str()).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before.deadline, wait.purpose.deadline());
    assert!(
        before.deadline.is_some(),
        "the call's admission pins a deadline"
    );
    let settled = start(&first).await;
    let other = parked_call(&first, &backend, &settled).await;
    first
        .completions()
        .resolve(
            other.key.as_str(),
            lash_core::Resolution::Ok(serde_json::json!({})),
        )
        .await
        .unwrap();
    ended(&first, &settled).await;
    assert!(
        first
            .completions()
            .parked(lash::admin::CallOwner::Process(settled.clone()))
            .await
            .unwrap()
            .is_empty()
    );
    let prefix = log(&backend, &process).await;
    first.shutdown().await.unwrap();
    let (backend, second) = core(&stores, deferred_advance, &world, "park-second");
    let after = parked_call(&second, &backend, &process).await;
    assert!(
        before == after,
        "takeover preserves exactly the call, key and deadline"
    );
    assert!(
        second
            .completions()
            .parked(lash::admin::CallOwner::Process(settled))
            .await
            .unwrap()
            .is_empty()
    );
    second
        .completions()
        .resolve(
            after.key.as_str(),
            lash_core::Resolution::Ok(serde_json::json!({})),
        )
        .await
        .unwrap();
    ended(&second, &process).await;
    assert!(
        second
            .completions()
            .parked(lash::admin::CallOwner::Process(process.clone()))
            .await
            .unwrap()
            .is_empty()
    );
    let events = log(&backend, &process).await;
    assert_eq!(of_type(&events, "process.waiting").len(), 1);
    assert_eq!(of_type(&events, "process.resumed").len(), 1);
    assert_eq!(
        &events[..prefix.len()],
        prefix.as_slice(),
        "takeover preserves the earlier log"
    );
    second.shutdown().await.unwrap();
}
on_every_tier!(a_deferred_call_survives_takeover);

fn publication_idle(
    _state: &mut serde_json::Value,
    event: lash_core::EngineEvent,
) -> lash_core::EngineAction {
    match event {
        lash_core::EngineEvent::Cancelled { origin, .. } => cancelled(origin),
        _ => lash_core::EngineAction::Idle,
    }
}

fn publication_terminal(
    _state: &mut serde_json::Value,
    _event: lash_core::EngineEvent,
) -> lash_core::EngineAction {
    answer(serde_json::json!("done"))
}
