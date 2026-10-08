//! A host observes a durable process's lifecycle and effects (FIG-5372).
//!
//! Each law builds a deployment the way a host does: a durable backend over
//! a store set, and a lash core over it, with a host process event sink,
//! whose own node runs the processes. Work enters only through the core's
//! process API.
//!
//! - **waiting:** a process parked on a signal is recorded `waiting` on it,
//!   once however many unrelated events reach it, and resumes when it asks
//!   for anything else.
//! - **effects:** each committed step outcome that names an effect node is
//!   one `process.effect_outcome` event; one past the per-node cap is
//!   counted in the `process.effect_omissions` record its terminal carries.
//! - **publication:** the host's sink hears every event the log holds, each
//!   once, in sequence order.
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

/// What a host's process event sink heard.
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

/// A core over `stores` whose node advances `engine`, with the write tool
/// and `heard` as its host's process event sink.
fn core(
    stores: &Arc<dyn StoreSet>,
    engine: Advance,
    world: &Arc<World>,
    heard: &Heard,
    boot: &str,
) -> (lash::Backend, lash::LashCore) {
    let backend = lash::durable::DurableBackendBuilder::new(Arc::clone(stores))
        .process_engine(Arc::new(ScriptEngine { advance: engine }))
        .build()
        .expect("the backend assembles");
    let core = lash::LashCore::standard_builder(backend.clone())
        .tools(write_tool(world))
        .process_event_sink(Arc::new(heard.clone()))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "lifecycle-events-deployment",
            boot,
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
            Vec::new(),
        ))
    }
}

// --- the write tool -------------------------------------------------------------

const WRITE_TOOL: &str = "lifecycle_events_write";

/// What the write tool wrote, per call.
#[derive(Debug, Default)]
struct World {
    writes: Mutex<Vec<serde_json::Value>>,
}

struct Write {
    world: Arc<World>,
}

#[async_trait::async_trait]
impl lash::tools::StaticToolExecute for Write {
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.world.writes.lock().unwrap().push(call.args.clone());
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
    lash_core::EngineAction::Steps(vec![lash_core::StepRequest::Tool {
        language_execution: None,
        step: lash_core::StepName(step.to_owned()),
        tool: lash_core::ToolId::new(WRITE_TOOL),
        input: serde_json::json!({ "x": x }),
        site: site.map(|(node_id, occurrence)| lash_core::StepEffectSite {
            node_id: node_id.to_owned(),
            occurrence,
        }),
    }])
}

// --- the host's side --------------------------------------------------------------

/// The environment every start in these laws captures.
fn environment() -> lash_core::ProcessExecutionEnvSpec {
    let mut environment = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        lash_core::SessionPolicy::new(lash::TurnBudget::Unbounded, lash::MaxToolCalls::new(16)),
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

fn signal_type(name: &str) -> lash_core::ProcessEventType {
    lash_core::ProcessEventType {
        name: format!("signal.{name}"),
        payload_schema: lash_sansio::JsonSchema::admit(serde_json::json!({ "type": "object" }))
            .expect("the signal's schema"),
        semantics: Default::default(),
    }
}

/// Start a detached process of the scripted engine, taking the signals
/// `go` and `other`.
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
    .with_env_ref(env_ref)
    .with_event_types([signal_type("go"), signal_type("other")]);
    core.processes()
        .start(request, core.effect_host())
        .await
        .expect("the process starts")
        .process_id
}

async fn signal(core: &lash::LashCore, process: &lash_core::ProcessId, name: &str, id: &str) {
    let identity = lash_core::ProcessSignalIdentity::new(process.clone(), name, id)
        .expect("a signal identity");
    core.processes()
        .signal(
            lash_core::ProcessSignal::new(identity, serde_json::json!({})),
            core.effect_host(),
        )
        .await
        .expect("the signal is delivered");
}

/// Wait until `process`'s record shows it waiting on the signal wait of
/// `ordinal`, as the host reads it, and answer that wait.
async fn waiting(
    core: &lash::LashCore,
    backend: &lash::Backend,
    process: &lash_core::ProcessId,
    ordinal: u64,
) -> lash_core::WaitState {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let observed = core
                .processes()
                .get(process)
                .await
                .expect("the process is read")
                .expect("the process is known");
            let record = backend
                .process_registry()
                .get_process(process)
                .await
                .expect("the record is read")
                .expect("the record is known");
            if observed.lifecycle == lash_core::ProcessStatus::Waiting
                && let Some(wait) = record.wait()
                && matches!(&wait.kind, lash_core::WaitKind::Signal { ordinal: at, .. } if *at == ordinal)
            {
                return wait.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the process waits within a minute")
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

/// Wait until the host's sink heard `process`'s whole log, and check it heard
/// each event once, in sequence order.
async fn heard_the_log(
    heard: &Heard,
    backend: &lash::Backend,
    process: &lash_core::ProcessId,
) -> Vec<lash::process::ProcessEvent> {
    let events = log(backend, process).await;
    let logged: Vec<u64> = events.iter().map(|event| event.sequence).collect();
    tokio::time::timeout(Duration::from_secs(60), async {
        while heard.sequences(process).len() < logged.len() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the sink hears the log within a minute");
    assert_eq!(
        heard.sequences(process),
        logged,
        "the sink heard every logged event once, in order"
    );
    events
}

fn of_type<'a>(
    events: &'a [lash::process::ProcessEvent],
    event_type: &str,
) -> Vec<&'a lash::process::ProcessEvent> {
    events
        .iter()
        .filter(|event| event.event_type == event_type)
        .collect()
}

// --- waiting ----------------------------------------------------------------------

/// Waits on `go`; an unrelated signal leaves it waiting; the first `go`
/// runs a write, then it waits on `go` again; the second ends it.
fn waiting_advance(
    state: &mut serde_json::Value,
    event: lash_core::EngineEvent,
) -> lash_core::EngineAction {
    let wait = || lash_core::EngineAction::AwaitSignal {
        name: "go".to_owned(),
    };
    match event {
        lash_core::EngineEvent::Started { .. } => wait(),
        lash_core::EngineEvent::Signal(signal) if signal.identity.signal_name() == "go" => {
            if state.is_null() {
                *state = serde_json::json!("wrote");
                write("write", 1, None)
            } else {
                answer(serde_json::json!("done"))
            }
        }
        lash_core::EngineEvent::Cancelled { origin, .. } => cancelled(origin),
        _ => wait(),
    }
}

/// A process parked on a signal is observed waiting on it, as its first
/// wait on that name; an unrelated signal leaves its one wait standing; a
/// transition that asks for anything else resumes it, and its next wait on
/// the name is the second. The host's sink hears the whole log, once.
async fn a_process_waiting_on_a_signal_is_observed_waiting_until_it_resumes(tier: Tier) {
    let (stores, _keep) = stores(tier).await;
    let world = Arc::new(World::default());
    let heard = Heard::default();
    let (backend, core) = core(&stores, waiting_advance, &world, &heard, "waiting");
    let process = start(&core).await;

    let first = waiting(&core, &backend, &process, 1).await;
    let lash_core::WaitKind::Signal {
        name,
        event_type,
        key,
        ..
    } = &first.kind;
    assert_eq!(name, "go");
    assert_eq!(event_type, "signal.go");
    assert_eq!(
        key,
        &lash_core::runtime::process_signal_wait_key(&process, "go", 1)
    );

    signal(&core, &process, "other", "other-1").await;
    signal(&core, &process, "go", "go-1").await;
    waiting(&core, &backend, &process, 2).await;
    signal(&core, &process, "go", "go-2").await;
    ended(&core, &process).await;

    let events = heard_the_log(&heard, &backend, &process).await;
    let waits: Vec<u64> = of_type(&events, "process.waiting")
        .into_iter()
        .map(|event| {
            event.payload["wait"]["kind"]["ordinal"]
                .as_u64()
                .expect("a signal wait's ordinal")
        })
        .collect();
    assert_eq!(
        waits,
        vec![1, 2],
        "one wait per signal wait, however many events reached it: {events:#?}"
    );
    assert_eq!(
        of_type(&events, "process.resumed").len(),
        1,
        "the write resumed the first wait; the terminal ended the second"
    );
    assert_eq!(world.writes.lock().unwrap().len(), 1);
}

on_every_tier!(a_process_waiting_on_a_signal_is_observed_waiting_until_it_resumes);

// --- effects ----------------------------------------------------------------------

const NODE: &str = "node.write";

/// Writes as the first occurrence of [`NODE`], then as one past the
/// per-node cap, then ends.
fn effects_advance(
    state: &mut serde_json::Value,
    event: lash_core::EngineEvent,
) -> lash_core::EngineAction {
    match event {
        lash_core::EngineEvent::Started { .. } => write("first", 1, Some((NODE, 1))),
        lash_core::EngineEvent::StepSettled { .. } if state.is_null() => {
            *state = serde_json::json!("first");
            write(
                "past-cap",
                2,
                Some((NODE, lash_core::PROCESS_EFFECT_OCCURRENCE_CAP + 1)),
            )
        }
        lash_core::EngineEvent::StepSettled { .. } => answer(serde_json::json!("done")),
        lash_core::EngineEvent::Cancelled { origin, .. } => cancelled(origin),
        _ => lash_core::EngineAction::Idle,
    }
}

/// Each committed step outcome that names an effect node is one
/// `process.effect_outcome` of that node, keyed by its call; an occurrence
/// past the cap is counted in the omissions record committed just before
/// the terminal. The host's sink hears the whole log, once.
async fn each_committed_effect_is_one_effect_outcome_event(tier: Tier) {
    let (stores, _keep) = stores(tier).await;
    let world = Arc::new(World::default());
    let heard = Heard::default();
    let (backend, core) = core(&stores, effects_advance, &world, &heard, "effects");
    let process = start(&core).await;
    ended(&core, &process).await;

    let events = heard_the_log(&heard, &backend, &process).await;
    let fleet = lash_core::FleetFormat::current();
    let outcomes: Vec<lash_core::ProcessEffectOccurrence> =
        of_type(&events, lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE)
            .into_iter()
            .map(|event| {
                lash_core::ProcessEffectOccurrence::decode(event.payload.clone(), fleet)
                    .expect("an effect outcome decodes")
            })
            .collect();
    assert_eq!(outcomes.len(), 1, "one outcome within the cap: {events:#?}");
    assert_eq!(outcomes[0].node_id, NODE);
    assert_eq!(outcomes[0].occurrence, 1);
    assert_eq!(outcomes[0].operation, WRITE_TOOL);
    assert_eq!(
        outcomes[0].outcome_class,
        lash_core::ProcessEffectOutcomeClass::Success
    );
    assert!(outcomes[0].code.is_none());

    let omissions = of_type(&events, lash_core::PROCESS_EFFECT_OMISSIONS_EVENT_TYPE);
    assert_eq!(omissions.len(), 1, "one omissions record: {events:#?}");
    let omitted = lash_core::ProcessEffectOmissions::decode(omissions[0].payload.clone(), fleet)
        .expect("the omissions decode");
    assert_eq!(omitted.nodes[NODE].success, 1);
    assert_eq!(omitted.nodes[NODE].total(), 1);
    let terminal = events.last().expect("the log ends");
    assert_eq!(terminal.event_type, "process.completed");
    assert_eq!(
        omissions[0].sequence + 1,
        terminal.sequence,
        "the omissions record commits just before the terminal"
    );
    assert_eq!(world.writes.lock().unwrap().len(), 2);
}

on_every_tier!(each_committed_effect_is_one_effect_outcome_event);

// --- publication across commits and cuts -------------------------------------------

/// Waits on `go` and ends on it.
fn takeover_advance(
    _state: &mut serde_json::Value,
    event: lash_core::EngineEvent,
) -> lash_core::EngineAction {
    match event {
        lash_core::EngineEvent::Signal(signal) if signal.identity.signal_name() == "go" => {
            answer(serde_json::json!("done"))
        }
        lash_core::EngineEvent::Cancelled { origin, .. } => cancelled(origin),
        _ => lash_core::EngineAction::AwaitSignal {
            name: "go".to_owned(),
        },
    }
}

/// The host's half of a step: these laws' processes ask for none.
struct NoSteps;

#[async_trait::async_trait]
impl lash_core_execution::runtime::process::steps::ProcessSteps for NoSteps {
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
    async fn new(script: lash_durable_test::Script) -> Self {
        let clock = lash_durable_test::SimClock::new();
        let stores = sim::memory(Arc::clone(&clock)).await;
        let database: Arc<dyn lash_durable::DurableStore> = Arc::new(stores.durable_store());
        let backend = lash_core_execution::Backend::assemble(lash_core_execution::BackendParts {
            stores: Arc::new(stores),
            settings: lash_core_execution::DurableSettings::default(),
            engines: vec![Arc::new(ScriptEngine {
                advance: takeover_advance,
            })],
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
    ) -> (
        lash_core_execution::WatchedRegistry,
        lash_core_execution::runtime::ProcessEventSinkRegistration,
    ) {
        let watched = lash_core_execution::runtime::watch_process_registry(inner);
        let registration = watched.add_event_sink(Arc::new(heard.clone()));
        self.nodes
            .activate_on(node, Self::activation(&self.backend, Some(&watched)));
        (watched, registration)
    }

    /// Register a detached process of the scripted engine, taking the
    /// signal `go`.
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
        ))
        .with_extra_event_types([signal_type("go")]);
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
            .map(|event| event.event_type)
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

/// `go` from a host, through its node's `registry`.
async fn go(registry: &Arc<dyn lash_core::ProcessRegistry>, process: &lash_core::ProcessId) {
    let identity =
        lash_core::ProcessSignalIdentity::new(process.clone(), "go", "go-1").expect("an identity");
    registry
        .append_event(
            process,
            lash_core::ProcessSignal::new(identity, serde_json::json!({})).append_request(),
        )
        .await
        .expect("the signal is admitted");
}

/// A host's signal append holds its node's publication from before its
/// append until after the woken actor committed the terminal the signal
/// ends the process with; the actor's own publication waits behind it.
/// The host's sink hears each logged event once, in sequence order: the
/// two publishers share one mark, which the terminal does not drop
/// (FIG-5396).
async fn a_host_append_held_across_the_terminal_commit_publishes_each_event_once() {
    let fleet = Fleet::new(lash_durable_test::Script::new()).await;
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
    let signalled = tokio::spawn({
        let registry = Arc::clone(watched.registry());
        let process = process.clone();
        async move { go(&registry, &process).await }
    });
    held.wait_until_validated().await;
    fleet
        .until("the woken actor commits the terminal", || async {
            fleet.ended(&process).await
        })
        .await;
    held.resume();
    signalled.await.expect("the host's signal task");
    fleet
        .until("the ended process released", || async {
            fleet.actor_state(&process).await == Some(lash_durable::ActorState::Terminal)
        })
        .await;

    assert_eq!(
        fleet.types(&process).await,
        ["process.waiting", "signal.go", "process.completed"]
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

/// A's first transition commits its wait, appending `process.waiting`,
/// and A dies before it publishes anything. B takes the process over and
/// publishes from the durable publication mark, not the log's end: B's host
/// hears the wait A committed, then the rest, each
/// once, in order, and each by the `(process, sequence)` the log holds it
/// under (FIG-5396).
async fn a_takeover_after_a_cut_between_commit_and_publish_delivers_the_dead_owners_events() {
    let script = lash_durable_test::Script::new();
    script.cut_on(
        "a",
        lash_durable::CommitLabel::PROCESS_ADVANCE,
        1,
        lash_durable_test::Fault::CommitThenAbort,
    );
    let fleet = Fleet::new(script).await;
    let (a_heard, b_heard) = (Heard::default(), Heard::default());
    let (_a, _a_sink) = fleet.host("a", fleet.backend.process_registry(), &a_heard);
    let (b, _b_sink) = fleet.host("b", fleet.backend.process_registry(), &b_heard);
    let process = fleet.register().await;
    fleet.nodes.start("a");
    fleet
        .until("A commits its wait and dies", || async {
            !fleet.nodes.script().cuts().is_empty()
        })
        .await;
    assert!(
        a_heard.sequences(&process).is_empty(),
        "A died before it published"
    );
    assert_eq!(fleet.types(&process).await, ["process.waiting"], "A's wait");
    let committed = fleet.log(&process).await;

    fleet.nodes.start("b");
    fleet
        .until("B publishes what A committed", || async {
            b_heard.sequences(&process).starts_with(&committed)
        })
        .await;
    go(b.registry(), &process).await;
    fleet
        .until("the ended process released", || async {
            fleet.actor_state(&process).await == Some(lash_durable::ActorState::Terminal)
        })
        .await;

    assert_eq!(
        fleet.types(&process).await,
        ["process.waiting", "signal.go", "process.completed"]
    );
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
            (&event.process_id, event.sequence, &event.event_type),
            (&logged.process_id, logged.sequence, &logged.event_type),
            "a heard event is the logged event of its (process, sequence)"
        );
    }
}

#[tokio::test]
async fn a_takeover_after_a_cut_between_commit_and_publish_delivers_the_dead_owners_events_on_sqlite_memory()
 {
    a_takeover_after_a_cut_between_commit_and_publish_delivers_the_dead_owners_events().await;
}
