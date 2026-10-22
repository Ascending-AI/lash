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

#[path = "support/operations.rs"]
mod operations;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_execution_contracts::backend::StoreSet;
use lash_execution_contracts::process::ProcessEventLogTestSupport as _;

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
    fn sequences(&self, process: &lash_vocabulary::identity::ProcessId) -> Vec<u64> {
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
    let (backend, builder) = core_builder(stores, engine, world, None);
    let core = builder.build(owner(boot)).expect("the core builds");
    (backend, core)
}

fn owner(boot: &str) -> lash::persistence::LeaseOwnerIdentity {
    lash::persistence::LeaseOwnerIdentity::opaque(
        lash::persistence::LeaseOwnerId::new("lifecycle-events-deployment"),
        lash::persistence::LeaseIncarnationId::new(boot),
    )
}

fn core_builder(
    stores: &Arc<dyn StoreSet>,
    engine: Advance,
    world: &Arc<World>,
    refuse_at: Option<u64>,
) -> (lash::Backend, lash::LashCoreBuilder) {
    let backend = lash::durable::DurableBackendBuilder::new(Arc::clone(stores))
        .process_engine(Arc::new(ScriptEngine {
            advance: engine,
            refuse_at,
        }))
        .build()
        .expect("the backend assembles");
    let builder = lash::LashCore::standard_builder(backend.clone())
        .tools(lash::tool_provider_key!("tools"), write_tool(world))
        .process_component(
            lash_core::testing::tool_component(WRITE_COMPONENT, write_tool(world)),
            WRITE_COMPONENT,
        )
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended());
    (backend, builder)
}

// --- the engine ---------------------------------------------------------------

const KIND: &str = "lifecycle-events-engine";

/// How the scripted engine answers each event: its state is a JSON value it
/// rewrites in place.
type Advance = fn(
    &mut serde_json::Value,
    lash_execution_contracts::process::EngineEvent,
) -> lash_execution_contracts::process::EngineAction;

struct ScriptEngine {
    advance: Advance,
    refuse_at: Option<u64>,
}

fn infra(error: impl std::fmt::Display) -> lash_execution_contracts::process::ProcessInfraError {
    lash_execution_contracts::process::ProcessInfraError::new(
        lash_execution_contracts::plugin_error::PluginError::Session(error.to_string()),
    )
}

fn answer(value: serde_json::Value) -> lash_execution_contracts::process::EngineAction {
    lash_execution_contracts::process::EngineAction::Terminal(
        lash_execution_contracts::process::ProcessOutcome::from_tool_output(
            lash_core::ToolCallOutput::success(value),
        ),
    )
}

fn cancelled(
    origin: lash_protocol_types::CancelOrigin,
) -> lash_execution_contracts::process::EngineAction {
    lash_execution_contracts::process::EngineAction::Terminal(
        lash_execution_contracts::process::ProcessOutcome::from_tool_output(
            lash_core::ToolCallOutput::cancelled(
                lash_core::ToolCancellation::runtime("the scripted engine answered its cancel")
                    .with_origin(origin),
            ),
        ),
    )
}

#[async_trait::async_trait]
impl lash_execution_contracts::process::ProcessEngine for ScriptEngine {
    fn settled_ran(
        &self,
        _kind: &lash_execution_contracts::process::EngineStepKind,
        _output: &lash_core_store::tool_run::SettledOutput,
    ) -> lash_execution_contracts::process::SettledRan {
        Default::default()
    }

    fn trace_class(&self) -> lash_execution_contracts::process::ProcessTraceClass {
        lash_execution_contracts::process::ProcessTraceClass::Internal
    }

    async fn inspect_link(
        &self,
        _link: lash_vocabulary::LinkId,
        _selector: &lash_execution_contracts::process::linking::ReachSelector,
    ) -> Result<
        lash_execution_contracts::process::linking::LinkInspection,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        Err(
            lash_execution_contracts::process::linking::EnginePortError::Unsupported {
                operation: "this scripted engine has no executable document".into(),
            },
        )
    }
    async fn inspect_start(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<
        lash_execution_contracts::process::linking::LinkInspection,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        Err(
            lash_execution_contracts::process::linking::EnginePortError::Unsupported {
                operation: "this scripted engine has no executable document".into(),
            },
        )
    }
    fn capabilities(&self) -> lash_vocabulary::claims::NodeRequirements {
        Default::default()
    }

    async fn check_args(
        &self,
        _signature: &lash_execution_contracts::process::ProcessSignature,
        _args: &serde_json::Map<String, serde_json::Value>,
        _mode: lash_execution_contracts::process::ArgsMode,
    ) -> std::result::Result<(), lash_execution_contracts::process::ArgsMismatch> {
        Err(
            lash_execution_contracts::process::ArgsMismatch::UnsupportedSignature {
                engine_kind: self.kind().into(),
            },
        )
    }

    fn kind(&self) -> &'static str {
        KIND
    }

    fn state_format(&self) -> lash_execution_contracts::process::EngineStateFormat {
        lash_execution_contracts::process::EngineStateFormat {
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

    fn check_settings(
        &self,
        _payload: &serde_json::Value,
        settings: Option<&lash_execution_contracts::process::EngineSettings>,
    ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
        match settings {
            None => Ok(()),
            Some(_) => Err(
                lash_execution_contracts::plugin_error::PluginError::Registration(format!(
                    "process engine `{}` takes no settings",
                    self.kind()
                )),
            ),
        }
    }

    fn default_settings(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<
        Option<lash_execution_contracts::process::EngineSettings>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(None)
    }

    fn advance(
        &self,
        state: lash_execution_contracts::process::EngineState,
        event: lash_execution_contracts::process::EngineEvent,
    ) -> Result<
        (
            lash_execution_contracts::process::EngineState,
            lash_execution_contracts::process::EngineAction,
        ),
        lash_execution_contracts::process::ProcessInfraError,
    > {
        let mut script = if state.bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&state.bytes).map_err(infra)?
        };
        if self.refuse_at.is_some() && script.as_u64() == self.refuse_at {
            return Err(infra("the fixture engine needs repair before redrive"));
        }
        let action = (self.advance)(&mut script, event);
        Ok((
            lash_execution_contracts::process::EngineState {
                staged: Vec::new(),
                format: self.state_format(),
                bytes: serde_json::to_vec(&script).map_err(infra)?,
                pages: None,
                requirements: Default::default(),
            },
            action,
        ))
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Vec<lash_core::ArtifactName>, lash_execution_contracts::plugin_error::PluginError>
    {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        Err(
            lash_execution_contracts::plugin_error::PluginError::Session(format!(
                "a scripted engine stores no artifact `{artifact_ref}`"
            )),
        )
    }

    async fn resolve(
        &self,
        _reference: &lash_execution_contracts::process::ProcessDefinitionRef,
    ) -> Result<
        lash_execution_contracts::process::ProcessDefinitionResolution,
        lash_execution_contracts::process::ProcessDefinitionError,
    > {
        Ok(
            lash_execution_contracts::process::ProcessDefinitionResolution::new(
                lash_execution_contracts::process::ProcessSignature::Unknown,
            ),
        )
    }
}

// --- the write tool -------------------------------------------------------------

const WRITE_TOOL: &str = "lifecycle_events_write";

const WRITE_TOOL_OPERATION: &str = "tools.lifecycle_events_write";

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

/// The component that serves the write tool to a process.
const WRITE_COMPONENT: lash_core::ProcessConfigKey<()> = lash_core::ProcessConfigKey::new(
    "lifecycle-events-write",
    lash_vocabulary::providers::FormatVersion::ONE,
);

fn write_definition() -> lash_vocabulary::tool_contract::ToolDefinition {
    lash_vocabulary::tool_contract::ToolDefinition::raw(
        WRITE_TOOL,
        WRITE_TOOL_OPERATION,
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
    .with_declaration(
        lash_vocabulary::tool_declaration::ToolDeclaration::deferring(),
        Some(lash_vocabulary::tool_contract::ParkBound::Within(
            Duration::from_secs(300),
        )),
    )
    .expect("a deferring tool declares its park bound")
    .with_execution_policy(lash_vocabulary::tool_contract::ExecutionPolicy::Once)
}

fn write_tool(world: &Arc<World>) -> Arc<dyn lash_core::ToolProvider> {
    Arc::new(lash::tools::StaticToolProvider::new(
        vec![write_definition()],
        Write {
            world: Arc::clone(world),
        },
    ))
}

/// A write step for `x`, run for `site` when it names one.
fn write(
    step: &str,
    x: u64,
    site: Option<(&str, u64)>,
) -> lash_execution_contracts::process::EngineAction {
    lash_execution_contracts::process::EngineAction::Steps {
        steps: vec![lash_execution_contracts::process::StepRequest::Tool {
            step: lash_execution_contracts::process::StepName(step.to_owned()),
            use_: operations::use_for(&write_definition()),
            input: serde_json::json!({ "x": x }),
            site: site.map(|(node_id, occurrence)| {
                Box::new(lash_vocabulary::sites::EffectLocation::authored(
                    lash_protocol_types::effect_identity_fixture(node_id, occurrence),
                ))
            }),
        }],
        wake: None,
    }
}

// --- the host's side --------------------------------------------------------------

/// The environment a host start states in these laws: the write tool, from
/// its component.
fn environment() -> lash_core::ProcessEnv {
    lash_core::ProcessEnv::builder(lash_core::MaxToolCalls::new(16))
        .component(WRITE_COMPONENT, &())
        .execution(operations::environment(
            WRITE_COMPONENT.tool_provider(lash::tool_provider_key!("tools")),
            &[write_definition()],
        ))
        .build()
        .expect("the environment builds")
}

/// Start a detached process of the scripted engine.
async fn start(core: &lash::LashCore) -> lash_vocabulary::identity::ProcessId {
    let pin = lash_core::HostArtifactPin::mint();
    operations::retain(core.backend(), &pin, &[write_definition()]).await;
    let env_ref = core
        .host_artifacts()
        .publish_process_env(&pin, &environment())
        .await
        .expect("the environment is published");
    let request = lash_execution_contracts::process::ProcessStartRequest::engine(
        KIND.to_owned(),
        serde_json::Value::Null,
        lash_execution_contracts::process::EngineLaunch::new(env_ref, None),
        lash_execution_contracts::process::ProcessOriginator::host(),
        lash_execution_contracts::process::LifetimeDecision::Detached,
    );
    core.processes()
        .start(request, core.effect_host())
        .await
        .expect("the process starts")
        .process_id
}

async fn ended(core: &lash::LashCore, process: &lash_vocabulary::identity::ProcessId) {
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        core.processes().await_output(process),
    )
    .await
    .expect("the process ends within a minute");
    succeeded(output);
}

/// Awaits the end of a process that writes to `world` as it goes. The
/// deadline is a minute without a new write rather than a minute in all: a
/// long sequential run ends however loaded the host is, and a stalled one
/// still fails a minute after its last write.
async fn ended_while_writing(
    core: &lash::LashCore,
    process: &lash_vocabulary::identity::ProcessId,
    world: &World,
) {
    let processes = core.processes();
    let output = processes.await_output(process);
    tokio::pin!(output);
    let mut written = 0;
    loop {
        tokio::select! {
            output = &mut output => return succeeded(output),
            () = tokio::time::sleep(Duration::from_secs(60)) => {
                let now = world.writes.lock().unwrap().len();
                assert!(now > written, "the process wrote nothing for a minute after {written} writes");
                written = now;
            }
        }
    }
}

fn succeeded<E: std::fmt::Debug>(
    output: Result<lash_execution_contracts::process::ProcessAwaitOutput, E>,
) {
    let output = output.expect("the process's end is read");
    let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the process ended without an answer: {output:?}");
    };
    assert!(output.is_success(), "the process failed: {output:?}");
}

/// `process`'s whole durable log.
async fn log(
    backend: &lash::Backend,
    process: &lash_vocabulary::identity::ProcessId,
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

/// A hundred visits to the same node exercise the default evidence cut.
const NODE_WRITES: u64 = 100;

// These laws each run a full hundred-effect deployment. Serialize that
// fixture work so parallel libtest runs keep each process's deadline useful.
static EFFECT_LAW_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The identity of the `n`-th write of [`NODE`], from 0: the one site of
/// the node, at its `n`-th occurrence.
fn node_site(n: u64) -> lash_vocabulary::sites::EffectId {
    lash_protocol_types::effect_identity_fixture(NODE, n)
}

/// Writes [`NODE_WRITES`] times for [`NODE`], one settled before the next,
/// then ends.
fn effects_advance(
    state: &mut serde_json::Value,
    event: lash_execution_contracts::process::EngineEvent,
) -> lash_execution_contracts::process::EngineAction {
    effects_advance_to(state, event, NODE_WRITES)
}

fn effects_advance_to(
    state: &mut serde_json::Value,
    event: lash_execution_contracts::process::EngineEvent,
    writes: u64,
) -> lash_execution_contracts::process::EngineAction {
    let write_at = |n: u64| {
        // x=999 is the fixture's deliberate parked call. The large-cost
        // probe uses a disjoint input range and still executes every visit.
        let x = n + if writes > NODE_WRITES { 1001 } else { 1 };
        let mut action = write(&format!("write-{n}"), x, None);
        if let lash_execution_contracts::process::EngineAction::Steps { steps, .. } = &mut action
            && let Some(lash_execution_contracts::process::StepRequest::Tool { site, .. }) =
                steps.first_mut()
        {
            *site = Some(Box::new(lash_vocabulary::sites::EffectLocation::authored(
                node_site(n),
            )));
        }
        action
    };
    match event {
        lash_execution_contracts::process::EngineEvent::Started { .. } => write_at(0),
        lash_execution_contracts::process::EngineEvent::StepSettled { .. } => {
            let settled = state.as_u64().unwrap_or(0) + 1;
            *state = serde_json::json!(settled);
            if settled < writes {
                write_at(settled)
            } else {
                answer(serde_json::json!("done"))
            }
        }
        lash_execution_contracts::process::EngineEvent::Cancelled { origin, .. } => {
            cancelled(origin)
        }
        _ => lash_execution_contracts::process::EngineAction::Idle,
    }
}

/// Each committed step outcome that names an effect node is one
/// `process.effect_outcome` of that node at its exact site, keyed by its
/// call. By default a node records 64 of 100 visits, with the other 36
/// counted in the omissions record committed just before the terminal.
async fn each_committed_effect_is_one_effect_outcome_event(tier: Tier) {
    let _guard = EFFECT_LAW_GUARD.lock().await;
    let (stores, _keep) = stores(tier).await;
    let world = Arc::new(World::default());
    let (backend, builder) = core_builder(&stores, effects_advance, &world, None);
    let core = builder
        .build(owner("effects"))
        .expect("default evidence cut");
    let process = start(&core).await;
    ended(&core, &process).await;

    let events = log(&backend, &process).await;
    let fleet = lash_core::FleetFormat::current();
    let outcomes: Vec<lash_execution_contracts::process::ProcessEffectOccurrence> = of_type(
        &events,
        lash_execution_contracts::process::PROCESS_EFFECT_OUTCOME_EVENT_TYPE,
    )
    .into_iter()
    .map(|event| {
        lash_execution_contracts::process::ProcessEffectOccurrence::decode(
            event.fact.payload(),
            fleet,
        )
        .expect("an effect outcome decodes")
    })
    .collect();
    let recorded: Vec<lash_vocabulary::sites::EffectId> =
        outcomes.iter().map(|outcome| outcome.at.clone()).collect();
    let first_cap: Vec<_> = (0..64).map(node_site).collect();
    assert_eq!(
        recorded, first_cap,
        "the site's first cap occurrences: {events:#?}"
    );
    for outcome in &outcomes {
        assert_eq!(
            outcome.operation,
            lash_core_execution::testing::effect_of(&write_definition()).as_str()
        );
        assert_eq!(
            outcome.outcome_class,
            lash_execution_contracts::process::ProcessEffectOutcomeClass::Success
        );
        assert!(outcome.code.is_none());
    }

    let omissions = of_type(
        &events,
        lash_execution_contracts::process::PROCESS_EFFECT_OMISSIONS_EVENT_TYPE,
    );
    assert_eq!(omissions.len(), 1, "one omissions record: {events:#?}");
    let omitted = lash_execution_contracts::process::ProcessEffectOmissions::decode(
        omissions[0].fact.payload(),
        fleet,
    )
    .expect("the omissions decode");
    let node = node_site(0).position().to_string();
    assert_eq!(omitted.occurrence_cap, 64);
    assert_eq!(omitted.nodes[&node].success, 36);
    assert_eq!(omitted.nodes[&node].total(), 36);
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

/// FIG-6249: a host can retain every visit to a node above the standard cut.
async fn a_configured_200_cut_records_all_100_effect_occurrences(tier: Tier) {
    let _guard = EFFECT_LAW_GUARD.lock().await;
    let (stores, _keep) = stores(tier).await;
    let world = Arc::new(World::default());
    let (backend, builder) = core_builder(&stores, effects_advance, &world, None);
    let core = builder
        .evidence_limits(
            lash::EvidenceLimits::builder()
                .process_effect_occurrences(200)
                .build()
                .expect("a valid evidence cut"),
        )
        .build(owner("configured-200"))
        .expect("valid evidence cut");
    let process = start(&core).await;
    ended(&core, &process).await;
    assert_effect_cut(&log(&backend, &process).await, 200);
    assert_eq!(world.writes.lock().unwrap().len(), 100);
}

on_every_tier!(a_configured_200_cut_records_all_100_effect_occurrences);

/// Counts physical store work and actual fact decodes in the process
/// activation, whether facts arrive from a read or its acknowledged commit.
struct MeasuredActivation {
    inner: Arc<dyn lash_durable::runner::Activation>,
    window: lash_core_execution::facade_support::sql::Window,
    decodes: lash_core_execution::testing::fact_decodes::FactDecodes,
}

#[async_trait::async_trait]
impl lash_durable::runner::Activation for MeasuredActivation {
    async fn activate(&self, owned: lash_durable::runner::Owned) -> lash_durable::runner::Exit {
        self.decodes
            .within(self.window.within(self.inner.activate(owned)))
            .await
    }

    async fn capabilities(
        &self,
    ) -> Result<lash_durable::NodeCapabilities, lash_durable::DurableError> {
        self.inner.capabilities().await
    }
}

/// FIG-6260: incorporating a sequential effect must not hydrate and decode
/// the already-consumed history. Count actual RoundFact decoder entries:
/// acknowledged facts bypass SQL, but still pass through the production fold.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequential_effect_occurrences_decode_constant_facts_per_occurrence() {
    let _guard = EFFECT_LAW_GUARD.lock().await;
    for count in [12_u64, 1000] {
        fn dozen(
            state: &mut serde_json::Value,
            event: lash_execution_contracts::process::EngineEvent,
        ) -> lash_execution_contracts::process::EngineAction {
            effects_advance_to(state, event, 12)
        }
        fn thousand(
            state: &mut serde_json::Value,
            event: lash_execution_contracts::process::EngineEvent,
        ) -> lash_execution_contracts::process::EngineAction {
            effects_advance_to(state, event, 1000)
        }
        let (stores, _keep) = stores(Tier::SqliteMemory).await;
        let database = stores.durable_store();
        let world = Arc::new(World::default());
        let advance = if count == 12 { dozen } else { thousand };
        let (backend, builder) = core_builder(&stores, advance, &world, None);
        let core = builder
            .serve_sessions(false)
            .evidence_limits(
                lash::EvidenceLimits::builder()
                    .process_effect_occurrences(4096)
                    .build()
                    .expect("a valid evidence cut"),
            )
            .build(owner("constant-fact-work"))
            .unwrap();
        let (node_backend, inner) =
            lash::testing::node_activation(&core, Arc::new(lash_durable::NoProbe)).unwrap();
        let window = lash_core_execution::facade_support::sql::Window::new(
            format!("effect-occurrences-{count}"),
            "sqlite",
        );
        let fact_decodes = lash_core_execution::testing::fact_decodes::FactDecodes::default();
        let runner = lash_durable::runner::Runner::new(
            database,
            stores.clock(),
            lash_durable::runner::RunnerConfig {
                node: lash_durable::NodeId::new("counted-effects"),
                lease: lash_durable::LeaseConfig::default(),
                decodes: node_backend.formats().decodes(),
                serves: lash::testing::node_serves(&core),
                offers: Default::default(),
                max_active: 1,
                claim_batch: 1,
                retirement: lash_durable::RetirementSettings::standard(),
            },
            Arc::new(MeasuredActivation {
                inner,
                window: window.clone(),
                decodes: fact_decodes.clone(),
            }),
        );
        let process = start(&core).await;
        let started = std::time::Instant::now();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            runner
                .run(async {
                    let _ = stopped.await;
                })
                .await
        });
        ended_while_writing(&core, &process, &world).await;
        let elapsed = started.elapsed();
        stop.send(()).unwrap();
        task.await.unwrap().unwrap();
        let receipt = window.snapshot();
        let decodes = fact_decodes.count();
        let events = log(&backend, &process).await;
        assert_eq!(
            of_type(
                &events,
                lash_execution_contracts::process::PROCESS_EFFECT_OUTCOME_EVENT_TYPE
            )
            .len() as u64,
            count
        );
        assert_eq!(world.writes.lock().unwrap().len() as u64, count);
        eprintln!(
            "FIG-6260 occurrences={count} elapsed={elapsed:?} fact_decodes={decodes} statements={} normalized_sql_texts={}",
            receipt.statements, receipt.normalized_sql_texts
        );
        assert!(
            decodes >= 3 * count,
            "the counter must observe the admitted, started and settled facts"
        );
        // Each call has three facts, read on at most four lifecycle passes.
        // The bound is independent of how many earlier calls were consumed.
        assert!(
            decodes <= 12 * count,
            "consumed history was decoded again: {decodes} facts for {count} occurrences"
        );
        // FIG-6276: observing a statement must not normalize its text again.
        // Each effect runs about 156 statements whose start, rows and end are
        // all observed; normalizing on every event was half the run's time.
        let shapes = receipt.shapes.len() as u64;
        assert!(
            receipt.normalized_sql_texts <= shapes,
            "observation normalized {} statement texts for {shapes} shapes over {count} occurrences",
            receipt.normalized_sql_texts
        );
        core.shutdown().await.unwrap();
    }
}

/// FIG-6249: redrive retains the run's first cut after the host changes it.
async fn redrive_uses_the_recorded_64_cut_under_a_16_cut_host(tier: Tier) {
    let _guard = EFFECT_LAW_GUARD.lock().await;
    let (stores, _keep) = stores(tier).await;
    let world = Arc::new(World::default());
    let (_, builder) = core_builder(&stores, effects_advance, &world, Some(20));
    let first = builder
        .evidence_limits(
            lash::EvidenceLimits::builder()
                .process_effect_occurrences(64)
                .build()
                .expect("a valid evidence cut"),
        )
        .build(owner("before-redrive"))
        .expect("first host");
    let process = start(&first).await;
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let record = first.processes().get(&process).await.unwrap().unwrap();
            if let lash_execution_contracts::process::ProcessParkState::Parked(reason) = record.park
            {
                assert!(matches!(
                    reason,
                    lash_execution_contracts::park::ProcessParkReason::AdvanceRefused { .. }
                ));
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the run parks after its first transition");
    first.shutdown().await.expect("first host stops");
    let (backend, builder) = core_builder(&stores, effects_advance, &world, None);
    let second = builder
        .evidence_limits(
            lash::EvidenceLimits::builder()
                .process_effect_occurrences(16)
                .build()
                .expect("a valid evidence cut"),
        )
        .build(owner("after-redrive"))
        .expect("replacement host");
    assert!(
        second
            .processes()
            .redrive(&process, "effect cut law")
            .await
            .expect("redrive")
    );
    ended(&second, &process).await;
    assert_effect_cut(&log(&backend, &process).await, 64);
    assert_eq!(
        world.writes.lock().unwrap().len(),
        100,
        "redrive repeats no settled effect"
    );
}

on_every_tier!(redrive_uses_the_recorded_64_cut_under_a_16_cut_host);

fn assert_effect_cut(events: &[lash::process::ProcessEvent], cap: u64) {
    let outcomes = of_type(
        events,
        lash_execution_contracts::process::PROCESS_EFFECT_OUTCOME_EVENT_TYPE,
    );
    let retained = NODE_WRITES.min(cap);
    assert_eq!(u64::try_from(outcomes.len()).unwrap(), retained);
    for (n, event) in outcomes.iter().enumerate() {
        let outcome = lash_execution_contracts::process::ProcessEffectOccurrence::decode(
            event.fact.payload(),
            lash_core::FleetFormat::current(),
        )
        .unwrap();
        assert_eq!(outcome.at, node_site(u64::try_from(n).unwrap()));
    }
    let omissions = of_type(
        events,
        lash_execution_contracts::process::PROCESS_EFFECT_OMISSIONS_EVENT_TYPE,
    );
    if retained == NODE_WRITES {
        assert!(omissions.is_empty(), "all occurrences were retained");
    } else {
        assert_eq!(omissions.len(), 1);
        let omitted = lash_execution_contracts::process::ProcessEffectOmissions::decode(
            omissions[0].fact.payload(),
            lash_core::FleetFormat::current(),
        )
        .unwrap();
        assert_eq!(omitted.occurrence_cap, cap);
        assert_eq!(
            omitted.nodes[&node_site(0).position().to_string()].success,
            NODE_WRITES - retained
        );
        assert_eq!(
            omitted.nodes[&node_site(0).position().to_string()].total(),
            NODE_WRITES - retained
        );
    }
}

/// D-TOOLRECORD: both tool calls join from durable effect occurrences after
/// the deployment and its SQLite file store have been reopened (FIG-5550).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn process_effects_name_their_tool_call_records_after_a_sqlite_reopen() {
    fn advance(
        state: &mut serde_json::Value,
        event: lash_execution_contracts::process::EngineEvent,
    ) -> lash_execution_contracts::process::EngineAction {
        match event {
            lash_execution_contracts::process::EngineEvent::Started { .. } => {
                write("first", 1, Some((NODE, 1)))
            }
            lash_execution_contracts::process::EngineEvent::StepSettled { .. }
                if state.is_null() =>
            {
                *state = serde_json::json!("first");
                write("second", 2, Some((NODE, 2)))
            }
            lash_execution_contracts::process::EngineEvent::StepSettled { .. } => {
                answer(serde_json::json!("done"))
            }
            _ => lash_execution_contracts::process::EngineAction::Idle,
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
    let outcomes = of_type(
        &events,
        lash_execution_contracts::process::PROCESS_EFFECT_OUTCOME_EVENT_TYPE,
    );
    assert_eq!(
        outcomes.len(),
        2,
        "both calls retain their effect occurrences"
    );
    let executed = world.calls.lock().unwrap().clone();
    assert_eq!(executed.len(), 2);
    for (index, (event, expected)) in outcomes.iter().zip(&executed).enumerate() {
        let occurrence = lash_execution_contracts::process::ProcessEffectOccurrence::decode(
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
        assert_eq!(record.tool, WRITE_TOOL_OPERATION);
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
        _runtime: &Arc<lash_core_execution::runtime::process::StepRuntime>,
        _process: &lash_execution_contracts::process::ProcessRecord,
        step: &lash_execution_contracts::process::StepRequest,
        _target: Option<&lash_core_execution::runtime::process::steps::InvocationTarget>,
        _now_ms: u64,
    ) -> Result<
        lash_core_execution::runtime::process::steps::StepAdmission,
        lash_core_execution::runtime::process::steps::StepRefusal,
    > {
        Err(
            lash_core_execution::runtime::process::steps::StepRefusal::UnknownTool {
                step: step.step().0.clone(),
                tool: lash_core_execution::testing::step_tool(step, KIND)
                    .as_str()
                    .to_owned(),
            },
        )
    }

    /// Never asked: no step of these parks.
    fn resolved(
        &self,
        _process: &lash_execution_contracts::process::ProcessRecord,
        _step: &lash_execution_contracts::process::StepRequest,
        _execution: &lash_core_execution::runtime::actor::round::AdmittedExecution,
        _parked: &lash_core_store::tool_run::Material<lash_core_store::tool_run::CompletionSource>,
        _resolution: lash_core_execution::runtime::actor::waits::Resolution,
    ) -> lash_core_store::tool_run::SettledOutput {
        lash_core_store::tool_run::SettledOutput::Interrupted
    }

    fn body(
        &self,
        _runtime: &Arc<lash_core_execution::runtime::process::StepRuntime>,
        _process: &lash_execution_contracts::process::ProcessRecord,
        step: &lash_execution_contracts::process::StepRequest,
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
    backend: lash_execution_contracts::backend::Backend,
    nodes: lash_durable_test::SimNodes,
}

impl Fleet {
    async fn new(
        script: lash_durable_test::Script,
        advance: fn(
            &mut serde_json::Value,
            lash_execution_contracts::process::EngineEvent,
        ) -> lash_execution_contracts::process::EngineAction,
    ) -> Self {
        let clock = lash_durable_test::SimClock::new();
        let stores = sim::memory(Arc::clone(&clock)).await;
        let database: Arc<dyn lash_durable::DurableStore> = Arc::new(stores.durable_store());
        let backend = lash_execution_contracts::backend::Backend::assemble(
            lash_execution_contracts::backend::BackendParts {
                stores: Arc::new(stores),
                settings: lash_core_execution::DurableSettings::default(),
                engines: vec![Arc::new(ScriptEngine {
                    advance,
                    refuse_at: None,
                })],
                providers: Arc::new(lash_execution_contracts::projection::NoProjectionProviders),
                formats: Vec::new(),
            },
        )
        .expect("the fleet's backend assembles");
        let nodes = lash_durable_test::SimNodes::new(
            database,
            Arc::clone(&clock),
            script,
            lash_durable_test::SimNodesConfig {
                lease: lash_durable::LeaseConfig::default(),
                decodes: backend.formats().decodes(),
                serves: Vec::new(),
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
        backend: &lash_execution_contracts::backend::Backend,
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
        inner: Arc<dyn lash_execution_contracts::process::ProcessRegistry>,
        heard: &Heard,
    ) -> (lash_core_execution::WatchedRegistry, impl Send + Sync) {
        let watched = lash_core_execution::runtime::watch_process_registry(inner);
        let registration = watched.add_event_sink(Arc::new(heard.clone()));
        self.nodes
            .activate_on(node, Self::activation(&self.backend, Some(&watched)));
        (watched, registration)
    }

    /// Register a detached process of the scripted engine.
    async fn register(&self) -> lash_vocabulary::identity::ProcessId {
        let registration = lash_execution_contracts::process::ProcessRegistration::new(
            lash_execution_contracts::process::ProcessInput::Engine {
                kind: KIND.to_owned(),
                payload: serde_json::Value::Null,
                launch: lash_execution_contracts::process::EngineLaunch::new(
                    lash_execution_contracts::testing::process_execution_env_fixture_ref(),
                    None,
                ),
            },
            lash_execution_contracts::process::ProcessProvenance::host(),
            lash_execution_contracts::process::LifetimeDecision::Detached,
        );
        self.backend
            .process_registry()
            .register_process(registration)
            .await
            .expect("the process registers")
            .id
    }

    async fn events(
        &self,
        process: &lash_vocabulary::identity::ProcessId,
    ) -> Vec<lash::process::ProcessEvent> {
        self.backend
            .process_registry()
            .full_event_window(process, 0)
            .await
            .expect("the log is read")
    }

    async fn log(&self, process: &lash_vocabulary::identity::ProcessId) -> Vec<u64> {
        self.events(process)
            .await
            .iter()
            .map(|event| event.sequence)
            .collect()
    }

    async fn types(&self, process: &lash_vocabulary::identity::ProcessId) -> Vec<String> {
        self.events(process)
            .await
            .into_iter()
            .map(|event| event.fact.event_type().to_owned())
            .collect()
    }

    async fn ended(&self, process: &lash_vocabulary::identity::ProcessId) -> bool {
        self.backend
            .process_registry()
            .get_process(process)
            .await
            .expect("the record is read")
            .is_some_and(|record| record.is_terminal())
    }

    async fn actor_state(
        &self,
        process: &lash_vocabulary::identity::ProcessId,
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
    event: lash_execution_contracts::process::EngineEvent,
) -> lash_execution_contracts::process::EngineAction {
    match event {
        lash_execution_contracts::process::EngineEvent::Started { .. } => {
            write("deferred", 999, None)
        }
        lash_execution_contracts::process::EngineEvent::StepSettled { .. } => {
            answer(serde_json::json!("done"))
        }
        lash_execution_contracts::process::EngineEvent::Cancelled { origin, .. } => {
            cancelled(origin)
        }
        _ => lash_execution_contracts::process::EngineAction::Idle,
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
    process: &lash_vocabulary::identity::ProcessId,
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
    event: lash_execution_contracts::process::EngineEvent,
) -> lash_execution_contracts::process::EngineAction {
    match event {
        lash_execution_contracts::process::EngineEvent::Cancelled { origin, .. } => {
            cancelled(origin)
        }
        _ => lash_execution_contracts::process::EngineAction::Idle,
    }
}

fn publication_terminal(
    _state: &mut serde_json::Value,
    _event: lash_execution_contracts::process::EngineEvent,
) -> lash_execution_contracts::process::EngineAction {
    answer(serde_json::json!("done"))
}
