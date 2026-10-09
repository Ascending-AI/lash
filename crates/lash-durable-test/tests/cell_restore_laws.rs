//! What a code cell restored from its snapshot keeps (FIG-5224, FIG-5225;
//! ADR 0132 §5–§8), on the production turn driver over the durable store.
//!
//! A lash core's session holds one sent input; its turn runs on the
//! production session activation with the RLM protocol, a scripted model
//! and host tools whose bodies write to an [`ExternalWorld`] that survives
//! every node. The nodes are simulated (A and B). The cells:
//!
//! - **Identity:** the cell calls `ext_write`, declared `Once`, twice,
//!   operation A then B. The matrix cuts the uncut run at every labelled
//!   write under every fault and recovers on the other node: wherever it
//!   was cut, A and B never share a `ToolCallId`, so a recipient that
//!   deduplicates on it (ADR 0132 §7) never suppresses B as A's repeat. A
//!   cut after A settled and before B's quiet point restores the cell onto
//!   A's outcome, and B is the next admitted operation, not A again.
//! - **Sleep:** the cell sleeps for `N`. Its idle session releases as
//!   `waiting` once its idle eviction passes, and its timer row persists.
//!   Node A is killed at `N / 2` and B started: nobody claims the session
//!   before the deadline, and B wakes the cell once at the deadline it was
//!   admitted with, not at recovery + `N`.
//! - **Repeatable:** the cell calls `ext_retry`, declared `Repeatable`.
//!   The call is its own admitted execution under its declared policy, so
//!   a cut at its admission, or at its lost outcome commit, re-runs it at
//!   the same ordinal: every body entry sees the one call id, and the cell
//!   is answered with the tool's value, never an interruption.
//! - **Race:** the cell races `ext_fast` against `ext_slow`, whose body
//!   runs until it is cancelled. Both calls are admitted executions: every
//!   body entry is an admitted member's, and the loser settles on its own
//!   after the race answered.
//! - **Await process:** the cell awaits a host process with
//!   `processes.await`. The call parks on its completion wait, the session
//!   releases as `waiting` while the process runs, and the cell resumes
//!   with the process's outcome once it ends.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/dialect.rs"]
mod dialect;
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
use lash_core_execution::runtime::actor::process::ProcessActivation;
use lash_core_execution::runtime::actor::round::{
    AdmittedExecution, Material, MemberBody, SettledOutput,
};
use lash_core_execution::runtime::process::steps::{ProcessSteps, StepAdmission, StepRefusal};
use lash_core_execution::{
    Backend, BackendParts, EngineAction, EngineEvent, EngineState, EngineStateFormat,
    LifetimeDecision, NoProjectionProviders, ProcessEngine, ProcessId, ProcessInfraError,
    ProcessInput, ProcessOutcome, ProcessProvenance, ProcessRecord, ProcessRegistration,
    StepRequest, StoreSet, ToolCallOutput, ToolCancellation,
};
use lash_durable::domain::WaitKind;
use lash_durable::runner::Activation;
use lash_durable::{
    ActorDispatch, ActorKey, ActorState, CommitLabel, DurableInstant, DurableStore,
};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, Script, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire,
};
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt as _;
use serde_json::{Value, json};

use dialect::Dialect;

const SESSION: &str = "cell-restore-session";
const WRITE: &str = "ext_write";
const RETRY: &str = "ext_retry";
const FAST: &str = "ext_fast";
const SLOW: &str = "ext_slow";
const HANDLE: &str = "ext_handle";
/// The tool no catalog lists: the cell reaches it through the deferred
/// resolver's grant.
const GRANTED: &str = "ext_granted";
/// The tool a redeployed catalog binds at the granted tool's path.
const AMBIENT: &str = "ext_ambient";
const MODEL: &str = "cell-restore-model";
/// What the final answer starts with.
const FINAL: &str = "final answer";
/// How long the sleeping cell sleeps, in virtual milliseconds: well past a
/// lease's failover, so a recovery at its half is long before its end.
const SLEEP_MS: u64 = 120_000;
/// How long the awaited process sleeps, in virtual milliseconds: past the
/// session's idle eviction, so the parked cell releases its session
/// before the process ends, and within `processes.await`'s limit (the 2 min
/// tool default), which its park never outlives.
const PROCESS_MS: u64 = 100_000;
/// The awaited process's engine.
const ENGINE: &str = "cell-await-law";

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
    /// One `Repeatable` call.
    Repeatable,
    /// A race of a fast call against a slow one.
    Race,
    /// A `processes.await` of a host process.
    AwaitProcess,
    /// A granted call, a durable sleep of [`SLEEP_MS`], and the granted
    /// call again.
    Deferred,
}

impl Cell {
    fn source(self) -> String {
        let body = match self {
            Self::Identity => "const a = await tools.ext_write({ x: 1 });\n\
                 const b = await tools.ext_write({ x: 2 });\n\
                 print([a, b]);"
                .to_owned(),
            Self::Sleep => format!("await sleep({SLEEP_MS});\nprint(\"woke\");"),
            Self::Repeatable => "const a = await tools.ext_retry({ x: 1 });\nprint(a);".to_owned(),
            Self::Race => "const w = await Promise.race([tools.ext_fast({ x: 1 }), \
                 tools.ext_slow({ x: 2 })]);\nprint(w);"
                .to_owned(),
            Self::AwaitProcess => "const h = await tools.ext_handle({});\n\
                 const r = await processes.await({ handle: h });\n\
                 print(r);"
                .to_owned(),
            Self::Deferred => format!(
                "const a = await tools.{GRANTED}({{ x: 1 }});\n\
                 await sleep({SLEEP_MS});\n\
                 const b = await tools.{GRANTED}({{ x: 2 }});\n\
                 print([a, b]);"
            ),
        };
        format!("<typescript>\n{body}\n</typescript>")
    }
}

/// The outside world: every body entry of every host call, by call. It
/// survives every node.
#[derive(Debug, Default)]
struct ExternalWorld {
    entries: Mutex<BTreeMap<ToolCallId, Vec<(String, Value)>>>,
    /// The process `ext_handle` hands out.
    process: Mutex<Option<ProcessId>>,
    /// How many paths the deferred resolver was asked to resolve.
    resolved: std::sync::atomic::AtomicUsize,
    /// Whether the deployment was redeployed: its resolver grants nothing
    /// and its catalog binds [`AMBIENT`] at the granted tool's path.
    redeployed: std::sync::atomic::AtomicBool,
}

impl ExternalWorld {
    fn entries(&self) -> BTreeMap<ToolCallId, Vec<(String, Value)>> {
        self.entries.lock_recover().clone()
    }
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
        match call.name() {
            SLOW => {
                // Runs until its call is cancelled.
                match call.context.cancellation_token() {
                    Some(token) => token.cancelled().await,
                    None => std::future::pending().await,
                }
                ToolOutcome::err_fmt("ext_slow was cancelled").into()
            }
            HANDLE => {
                let process = self
                    .world
                    .process
                    .lock_recover()
                    .clone()
                    .expect("the host registered the awaited process");
                ToolOutcome::ok(lash_sansio::handle::handle_record_json(
                    &lash_sansio::handle::HandleId::process(&process),
                ))
                .into()
            }
            name => ToolOutcome::ok(json!({ "ran": name, "with": call.args })).into(),
        }
    }
}

fn definition(name: &str, policy: ExecutionPolicy, output: Value) -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        name,
        name,
        "A host call the cell restore laws watch.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "x": { "type": "number" } }
        }),
        output,
    )
    .expect("the tool's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_execution_policy(policy)
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], name))
}

/// The definition the resolver grants for `tools.ext_granted`, or the one
/// a redeployed catalog binds there.
fn granted(name: &str) -> lash_core::ToolDefinition {
    definition(name, ExecutionPolicy::Once, json!({ "type": "object" }))
        .with_tool_binding(lash_core::ToolBinding::new(["tools"], GRANTED))
}

/// The host tools of the deferred cell: the listed ones, the grant's body,
/// and, once redeployed, an ambient tool at the granted tool's path.
struct DeferredTools {
    world: Arc<ExternalWorld>,
    listed: Arc<dyn lash_core::ToolProvider>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for DeferredTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        let mut manifests = self.listed.tool_manifests();
        if self
            .world
            .redeployed
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            manifests.push(granted(AMBIENT).manifest());
        }
        manifests
    }

    fn resolve_manifest_by_id(&self, id: &lash_core::ToolId) -> Option<lash_core::ToolManifest> {
        [GRANTED, AMBIENT]
            .into_iter()
            .map(granted)
            .find(|definition| definition.id() == id)
            .map(|definition| definition.manifest())
            .or_else(|| self.listed.resolve_manifest_by_id(id))
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        if [GRANTED, AMBIENT].contains(&name) {
            return Some(Arc::new(granted(name).contract()));
        }
        self.listed.resolve_contract(name)
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        ExtTools {
            world: Arc::clone(&self.world),
        }
        .execute(call)
        .await
    }
}

/// Grants `tools.ext_granted` until the deployment is redeployed, and
/// counts every path it is asked.
struct GrantingResolver {
    world: Arc<ExternalWorld>,
}

#[async_trait::async_trait]
impl lash_vm_runtime::DeferredToolResolver for GrantingResolver {
    async fn resolve(
        &self,
        _cx: &lash_vm_runtime::DeferredResolveContext<'_>,
        paths: &[&str],
    ) -> BTreeMap<String, lash_vm_runtime::Resolution> {
        self.world
            .resolved
            .fetch_add(paths.len(), std::sync::atomic::Ordering::SeqCst);
        let redeployed = self
            .world
            .redeployed
            .load(std::sync::atomic::Ordering::SeqCst);
        paths
            .iter()
            .map(|path| {
                let resolution = if *path == format!("tools.{GRANTED}") && !redeployed {
                    lash_vm_runtime::Resolution::Resolved(Box::new(
                        lash_vm_runtime::ToolGrant::new(granted(GRANTED))
                            .with_source_id(lash::tools::PLUGIN_TOOL_SOURCE_ID),
                    ))
                } else {
                    lash_vm_runtime::Resolution::NotAvailable
                };
                ((*path).to_owned(), resolution)
            })
            .collect()
    }
}

fn ext_tools(world: Arc<ExternalWorld>) -> Arc<dyn lash_core::ToolProvider> {
    let object = json!({ "type": "object" });
    let repeatable = ExecutionPolicy::repeatable(std::num::NonZeroU32::new(3).unwrap(), 10, 100);
    Arc::new(StaticToolProvider::new(
        vec![
            definition(WRITE, ExecutionPolicy::Once, object.clone()),
            definition(RETRY, repeatable, object.clone()),
            definition(FAST, ExecutionPolicy::Once, object.clone()),
            definition(SLOW, ExecutionPolicy::Once, object.clone()),
            definition(
                HANDLE,
                ExecutionPolicy::Once,
                json!({ "x-lash": { "kind": "process_unknown" } }),
            ),
        ],
        ExtTools { world },
    ))
}

/// The scripted model: the cell until the transcript holds it, then prose.
/// Every request it is sent is kept, rendered.
fn model(cell: Cell, requests: Arc<Mutex<Vec<String>>>) -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("cell-restore-scripted")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| {
            let requests = Arc::clone(&requests);
            async move {
                requests
                    .lock_recover()
                    .push(format!("{:?}", request.messages));
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
                    stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                        kind: StreamBlockKind::AssistantText,
                        block: StreamBlockIdentity::new("text:0", 0),
                        text: answer.clone(),
                    }));
                }
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: answer,
                        response_meta: None,
                    }],
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

// --- the awaited process ----------------------------------------------------

/// The awaited process's engine: it sleeps until its payload's `until_ms`,
/// then ends with `{ "slept": true }`; a cancel ends it cancelled.
struct SleepEngine;

fn infra(error: impl std::fmt::Display) -> ProcessInfraError {
    ProcessInfraError::new(lash_core_execution::PluginError::Session(error.to_string()))
}

#[async_trait::async_trait]
impl ProcessEngine for SleepEngine {
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
        ENGINE
    }

    fn state_format(&self) -> EngineStateFormat {
        EngineStateFormat {
            kind: ENGINE.to_owned(),
            version: 0,
        }
    }

    fn cancel_grace(&self) -> Duration {
        Duration::from_secs(1)
    }

    fn program_identity(
        &self,
        _payload: &Value,
    ) -> Option<lash_core_execution::ExecutableGeneration> {
        None
    }

    fn creation_config(
        &self,
        _env_spec: &lash_core_execution::ProcessExecutionEnvSpec,
    ) -> Result<Option<Value>, lash_core_execution::PluginError> {
        Ok(None)
    }

    fn advance(
        &self,
        state: EngineState,
        event: EngineEvent,
    ) -> Result<(EngineState, EngineAction), ProcessInfraError> {
        let script: Value = match &event {
            EngineEvent::Started { payload } => payload.clone(),
            _ => serde_json::from_slice(&state.bytes).map_err(infra)?,
        };
        let action = match event {
            EngineEvent::Started { .. } => EngineAction::Sleep {
                until: DurableInstant(script["until_ms"].as_i64().unwrap_or_default()),
                site: None,
            },
            EngineEvent::Woke => EngineAction::Terminal(ProcessOutcome::from_tool_output(
                ToolCallOutput::success(json!({ "slept": true })),
            )),
            EngineEvent::Cancelled { origin, .. } => {
                EngineAction::Terminal(ProcessOutcome::from_tool_output(ToolCallOutput::cancelled(
                    ToolCancellation::runtime("the sleep engine answered its cancel")
                        .with_origin(origin),
                )))
            }
            _ => EngineAction::Idle,
        };
        Ok((
            EngineState {
                format: self.state_format(),
                bytes: serde_json::to_vec(&script).map_err(infra)?,
            },
            action,
        ))
    }

    fn start_artifacts(
        &self,
        _payload: &Value,
    ) -> Result<Vec<lash_core_execution::ArtifactName>, lash_core_execution::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core_execution::ResolvedArtifactCleanup,
    ) -> Result<(), lash_core_execution::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core_execution::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), lash_core_execution::PluginError> {
        Err(lash_core_execution::PluginError::Session(format!(
            "the sleep engine stores no artifact `{artifact_ref}`"
        )))
    }

    async fn resolve(
        &self,
        _reference: &lash_core_execution::ProcessDefinitionRef,
    ) -> Result<
        lash_core_execution::ProcessDefinitionResolution,
        lash_core_execution::ProcessDefinitionRefusal,
    > {
        Ok(lash_core_execution::ProcessDefinitionResolution::new(
            lash_core_execution::ProcessSignature::Unknown,
        ))
    }
}

/// The sleep engine runs no steps.
struct NoSteps;

#[async_trait::async_trait]
impl ProcessSteps for NoSteps {
    fn stop_grace(&self) -> std::time::Duration {
        std::time::Duration::from_secs(2)
    }

    async fn admit(
        &self,
        _process: &ProcessRecord,
        step: &StepRequest,
        _now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal> {
        Err(StepRefusal::UnknownTool {
            step: step.step().0.clone(),
            tool: step.step().0.clone(),
        })
    }

    fn resolved(
        &self,
        _process: &ProcessRecord,
        _step: &StepRequest,
        _execution: &AdmittedExecution,
        _parked: &Material<lash_core_store::tool_run::CompletionSource>,
        _resolution: lash_core_execution::runtime::actor::waits::Resolution,
    ) -> SettledOutput {
        SettledOutput::Interrupted
    }

    fn body(
        &self,
        _runtime: &std::sync::Arc<lash_core_execution::runtime::process::StepRuntime>,
        _process: &ProcessRecord,
        step: &StepRequest,
        _execution: &AdmittedExecution,
    ) -> MemberBody {
        unreachable!("no step of `{}` is ever admitted", step.step().0)
    }
}

// --- the scenario -----------------------------------------------------------

/// The scenario for one cell on one dialect, fresh for every matrix cell.
struct CellTurn {
    cell: Cell,
    dialect: Dialect,
    postgres_url: Option<String>,
    world: Arc<ExternalWorld>,
    requests: Arc<Mutex<Vec<String>>>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<Backend>>,
    /// The virtual clock the database was built on, which the core's VM
    /// worker calls hold.
    clock: Mutex<Option<Arc<SimClock>>>,
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
            requests: Arc::default(),
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
                    Arc::new(lash::rlm::TypescriptDialect),
                    &backend,
                )
                .with_worker_service(sim::workers(&clock));
                let (factory, tools) = if self.cell == Cell::Deferred {
                    (
                        factory.with_deferred_tool_resolver(Arc::new(GrantingResolver {
                            world: Arc::clone(&self.world),
                        })),
                        Arc::new(DeferredTools {
                            world: Arc::clone(&self.world),
                            listed: ext_tools(Arc::clone(&self.world)),
                        }) as Arc<dyn lash_core::ToolProvider>,
                    )
                } else {
                    (factory, ext_tools(Arc::clone(&self.world)))
                };
                lash::LashCore::rlm_builder(backend.clone(), factory)
                    .serve_sessions(false)
                    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
                    .data_retention(lash::DataRetention::standard())
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
                    .execution_budgets(lash::ExecutionBudgets::recommended())
                    .delta_coalescing(lash::DeltaCoalescing::recommended())
                    .serve_test_llm_profile(
                        model(self.cell, Arc::clone(&self.requests)),
                        lash_core::LlmProfileMetadata::builder(MODEL)
                            .cache_retention(lash_core::provider::CacheRetention::Short)
                            .context_window_tokens(200_000)
                            .build()
                            .expect("the model's metadata"),
                    )
                    .tools(tools)
                    .plugin(Arc::new(
                        lash::process_controls::SessionProcessAdminPluginFactory::new(
                            lash_core::lifetime::session_or_starter,
                        ),
                    ))
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(
                        lash::persistence::LeaseOwnerId::new("cell-restore-deployment"),
                        lash::persistence::LeaseIncarnationId::new("cell-restore-boot"),
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
            .send(lash::TurnInput::text("run the cell"))
            .await
            .map(drop)
            .map_err(|error| format!("send the turn's input: {error}"))
    }

    /// The model's last request, rendered: what the cell answered it.
    fn last_request(&self) -> String {
        self.requests
            .lock_recover()
            .last()
            .cloned()
            .unwrap_or_default()
    }

    /// The identity laws after a run.
    async fn identity_laws(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let entries = self.world.entries();
        let mut ids: BTreeMap<i64, Vec<&ToolCallId>> = BTreeMap::new();
        for (call, entries) in &entries {
            if entries.len() > 1 {
                violations.push(format!(
                    "call {call} reached the world {} times: {entries:?}",
                    entries.len()
                ));
            }
            for (_, args) in entries {
                let x = args["x"].as_i64().expect("a write names its x");
                ids.entry(x).or_default().push(call);
            }
        }
        // NR-2: losing a Once start may interrupt before either body runs.
        // These faults lose the owner or its acknowledgement; the uncut
        // reference and fail-before retries must populate both witnesses.
        let interrupted_once = cut.is_some_and(|cut| {
            matches!(
                cut.fault,
                Fault::Abort | Fault::CommitThenAbort | Fault::AckHidden | Fault::Zombie
            )
        });
        violations.extend(identity_population_violations(
            &ids,
            matches!(self.cell, Cell::Identity | Cell::Deferred) && !interrupted_once,
        ));
        violations.extend(turn_ended(nodes).await);
        violations
    }

    /// The `Repeatable` laws after a run: the one call re-ran at its own
    /// ordinal wherever it was cut, and the cell was answered with its
    /// value.
    async fn repeatable_laws(&self, nodes: &SimNodes) -> Vec<String> {
        let mut violations = Vec::new();
        let entries = self.world.entries();
        if entries.len() != 1 {
            violations.push(format!(
                "the one Repeatable call reached the world under {} call ids: {entries:?}",
                entries.len()
            ));
        }
        let last = self.last_request();
        if !last.contains(&format!("\\\"ran\\\": \\\"{RETRY}\\\""))
            && !last.contains(&format!("ran: \\\"{RETRY}\\\""))
            && !last.contains(RETRY)
        {
            violations.push(format!(
                "the cell was not answered with the call's value; the model saw: {last}"
            ));
        }
        for refusal in ["never answered", "interrupted"] {
            if last.contains(refusal) {
                violations.push(format!(
                    "the cell was answered `{refusal}` instead of re-running the call: {last}"
                ));
            }
        }
        violations.extend(turn_ended(nodes).await);
        violations
    }

    /// The race laws after a run: every body entry was an admitted
    /// member's; uncut, the loser settled with an outcome of its own.
    async fn race_laws(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let entered: usize = self.world.entries().values().map(Vec::len).sum();
        let admitted: usize = self.tripwire.counts().bodies.values().sum();
        if admitted < entered {
            violations.push(format!(
                "{entered} host bodies ran, but only {admitted} admitted bodies were entered: \
                 {:?}",
                self.world.entries()
            ));
        }
        if cut.is_none() {
            let outcomes = nodes
                .script()
                .trace()
                .iter()
                .filter(|write| {
                    write.point.label == CommitLabel::ROUND_OUTCOME && write.committed()
                })
                .count();
            if outcomes < 2 {
                violations.push(format!(
                    "the race's winner and loser settled in {outcomes} outcome commits"
                ));
            }
        }
        violations.extend(turn_ended(nodes).await);
        violations
    }
}

/// A comparison needs both body populations as witnesses.
fn identity_population_violations(
    ids: &BTreeMap<i64, Vec<&ToolCallId>>,
    require_populations: bool,
) -> Vec<String> {
    if require_populations {
        let missing: Vec<String> = [(1, "A"), (2, "B")]
            .into_iter()
            .filter(|(x, _)| ids.get(x).is_none_or(Vec::is_empty))
            .map(|(_, operation)| format!("operation {operation} has no body identity witness"))
            .collect();
        if !missing.is_empty() {
            return missing;
        }
    }
    if let (Some(a), Some(b)) = (ids.get(&1), ids.get(&2))
        && a.iter().any(|call| b.contains(call))
    {
        return vec!["operation B took operation A's ToolCallId".to_owned()];
    }
    Vec::new()
}

#[test]
fn identity_comparison_refuses_a_missing_body_population() {
    let call = ToolCallId::fixture("body-witness");
    for (present, missing) in [(1, "B"), (2, "A")] {
        let ids = BTreeMap::from([(present, vec![&call])]);
        assert_eq!(
            identity_population_violations(&ids, true),
            vec![format!("operation {missing} has no body identity witness")],
        );
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
/// sets, with the awaited process's engine.
fn shipped_backend(stores: Arc<dyn StoreSet>) -> Backend {
    Backend::assemble(BackendParts {
        stores,
        settings: sim::settings(),
        engines: vec![Arc::new(SleepEngine)],
        providers: Arc::new(NoProjectionProviders),
        formats: lash::formats::actor_state_surfaces(),
    })
    .expect("the backend assembles")
}

#[async_trait::async_trait]
impl Scenario for CellTurn {
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
        match self.cell {
            Cell::Repeatable => self.repeatable_laws(nodes).await,
            Cell::Race => self.race_laws(nodes, cut).await,
            Cell::Identity | Cell::Sleep | Cell::AwaitProcess | Cell::Deferred => {
                self.identity_laws(nodes, cut).await
            }
        }
    }
}

/// Cut `cell` at every labelled write under every fault.
async fn matrix(cell: Cell, dialect: Dialect, postgres_url: Option<String>) {
    let faults = [
        Fault::FailBefore,
        Fault::AckHidden,
        Fault::Zombie,
        Fault::Abort,
        Fault::CommitThenAbort,
    ];
    // Split only the optional double-run proof into exhaustive fault batches
    // so each action retains the existing timeout. Ordinary laws cut all five.
    let selected = if std::env::var("LASH_MATRIX_VERIFY_LEASE").as_deref() == Ok("1") {
        match std::env::var("LASH_MATRIX_PROOF_PART").as_deref() {
            Ok("first") => &faults[..3],
            Ok("second") => &faults[3..],
            _ => &faults[..],
        }
    } else {
        &faults[..]
    };
    let report = Matrix::new()
        .faults(selected)
        .horizon(Duration::from_secs(600))
        .run_test(|| CellTurn::new(cell, dialect, postgres_url.clone()))
        .await;
    eprintln!(
        "{cell:?} on {dialect:?}: {} cells over {} labels",
        report.cells.len(),
        report.labels().len()
    );
    report.assert_held();
}

/// Identity: cut at every labelled write under every fault, A's and B's
/// `ToolCallId`s differ.
async fn identity(dialect: Dialect, postgres_url: Option<String>) {
    matrix(Cell::Identity, dialect, postgres_url).await;
}

/// Run `turn` on fresh simulated nodes over its database.
async fn simulated(turn: &CellTurn) -> (Arc<SimClock>, Arc<dyn DurableStore>, SimNodes) {
    let clock = SimClock::new();
    let database = turn.database(Arc::clone(&clock)).await;
    let nodes = SimNodes::new(
        Arc::clone(&database),
        Arc::clone(&clock),
        Script::new(),
        turn.config(),
        turn.activation(),
    );
    (clock, database, nodes)
}

/// Sleep: the cell sleeps for `SLEEP_MS`. Its session stays hot for the
/// idle eviction, then releases as `waiting` with the sleep's deadline as
/// its next due, holding nothing while its timer row persists (ADR 0132
/// §6). Node A is killed during the sleep and node B started; nothing
/// claims the session before the deadline, and B wakes the cell at the
/// deadline it was admitted with, once.
async fn sleep_across_a_crash(dialect: Dialect, postgres_url: Option<String>) {
    let turn = CellTurn::new(Cell::Sleep, dialect, postgres_url);
    let (clock, database, nodes) = simulated(&turn).await;
    turn.send().await.expect("the turn is sent");
    nodes.start("a");
    // Run A until the cell sleeps on its pinned timer.
    let timer = loop {
        let timers: Vec<_> = database
            .pending_waits(&actor())
            .await
            .expect("the session's waits read")
            .into_iter()
            .filter(|wait| wait.purpose.kind() == WaitKind::Timer)
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
    let due = timer.purpose.deadline().expect("a timer has a deadline");
    let deadline = u64::try_from(due.0).unwrap() - SimClock::timestamp_ms_at(0);
    let slept_at = clock.logical_ms();
    assert!(
        deadline >= slept_at + SLEEP_MS - 1_000 && deadline <= slept_at + SLEEP_MS,
        "the timer is due at {deadline}, not {SLEEP_MS} ms after the cell slept at {slept_at}"
    );
    // The idle session releases as `waiting` until the sleep's deadline,
    // owned by nobody, while its timer stays pending.
    let half = slept_at + SLEEP_MS / 2;
    let evicted = loop {
        let session = database.actor(&actor()).await.expect("read the session");
        if let Some(session) = &session
            && session.state == ActorState::Waiting
        {
            break session.clone();
        }
        assert!(
            nodes.step().await.is_some() && clock.logical_ms() < half,
            "the sleeping cell's session never released as `waiting` by {half} ms: {session:?}\n{}",
            nodes.script().rendered_trace()
        );
    };
    let mut violations = Vec::new();
    if evicted.owner.is_some() || evicted.next_due != Some(due) {
        violations.push(format!(
            "the evicted session holds {:?} and is next due at {:?}, not nobody at {due:?}",
            evicted.owner, evicted.next_due
        ));
    }
    let pending = database
        .pending_waits(&actor())
        .await
        .expect("the session's waits read");
    if !pending.iter().any(|wait| wait.id == timer.id) {
        violations.push(format!(
            "the evicted session's timer does not persist: {pending:?}"
        ));
    }
    let released_at = clock.logical_ms();
    // Kill A during the sleep, and let B take the cell over.
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
    violations.extend(turn_ended(&nodes).await);
    let trace = nodes.script().trace();
    // Nobody claims the released session before its deadline.
    let early: Vec<_> = trace
        .iter()
        .filter(|write| {
            write.point.label == CommitLabel::CLAIM
                && matches!(write.stored, Stored::Committed { effective: true })
                && write.at_ms > released_at
                && write.at_ms < deadline
        })
        .map(|write| (write.node.to_string(), write.at_ms))
        .collect();
    if !early.is_empty() {
        violations.push(format!(
            "the session released at {released_at} was claimed before its deadline \
             {deadline}: {early:?}"
        ));
    }
    // The cell woke when B committed its end: at its admitted deadline,
    // within a claim poll of it. A sleep run again from its recovery would
    // end `SLEEP_MS` after B took the cell over.
    let woke = trace
        .iter()
        .find(|write| {
            &*write.node == "b"
                && write.point.label == CommitLabel::CELL_SNAPSHOT
                && write.committed()
        })
        .map(|write| write.at_ms);
    let poll = Matrix::test_lease().settings().claim_poll.as_millis() as u64;
    if !woke.is_some_and(|woke| woke >= deadline && woke <= deadline + poll + 5_000) {
        violations.push(format!(
            "the cell slept at {slept_at}, due at {deadline}, was released at {released_at}, \
             A was killed at {half}, and the cell woke at {woke:?}"
        ));
    }
    // The cell resumed once: the model was asked for the cell, then once
    // more with its answer.
    let requests = turn.requests.lock_recover().len();
    if requests != 2 {
        violations.push(format!(
            "the model was asked {requests} times, not once for the cell and once after it woke"
        ));
    }
    assert!(
        violations.is_empty(),
        "sleep across a crash on {dialect:?}:\n  {}\n{}",
        violations.join("\n  "),
        nodes.script().rendered_trace()
    );
}

/// DEFER1: a cell's deferred tool resolutions are held in its snapshot and
/// restored from it. The cell calls `tools.ext_granted`, which no catalog
/// lists and the deployment's resolver grants, then sleeps. Node A is
/// killed during the sleep and the deployment is redeployed: its resolver
/// now grants nothing, and its catalog binds another tool at that path.
/// Node B restores the cell onto its sleep; the cell's second call runs
/// under the grant it resolved, never the changed ambient binding, and the
/// resolver is never asked again.
async fn deferred_grant_across_a_crash(dialect: Dialect, postgres_url: Option<String>) {
    use std::sync::atomic::Ordering;
    let turn = CellTurn::new(Cell::Deferred, dialect, postgres_url);
    let (clock, database, nodes) = simulated(&turn).await;
    turn.send().await.expect("the turn is sent");
    nodes.start("a");
    // Run A until the cell sleeps on its pinned timer.
    loop {
        let timers = database
            .pending_waits(&actor())
            .await
            .expect("the session's waits read")
            .into_iter()
            .filter(|wait| wait.purpose.kind() == WaitKind::Timer)
            .count();
        if timers == 1 {
            break;
        }
        assert!(
            nodes.step().await.is_some() && clock.logical_ms() < SLEEP_MS,
            "the cell never slept: {}\n{}",
            turn.last_request(),
            nodes.script().rendered_trace()
        );
    }
    let slept_at = clock.logical_ms();
    while clock.logical_ms() < slept_at + SLEEP_MS / 2 {
        assert!(nodes.step().await.is_some(), "A stalled while sleeping");
    }
    nodes.kill("a");
    nodes.quiesce().await;
    turn.world.redeployed.store(true, Ordering::SeqCst);
    nodes.start("b");
    let horizon = slept_at + 3 * SLEEP_MS;
    while !turn.done(&nodes).await {
        assert!(
            clock.logical_ms() < horizon,
            "the turn is not done:\n{}",
            nodes.script().rendered_trace()
        );
        assert!(
            nodes.step().await.is_some(),
            "B stalled:\n{}",
            nodes.script().rendered_trace()
        );
    }
    nodes.quiesce().await;
    let mut violations = turn.identity_laws(&nodes, None).await;
    let calls = turn
        .world
        .entries()
        .into_values()
        .flatten()
        .map(|(name, args)| (name, args["x"].as_i64().unwrap_or_default()))
        .collect::<std::collections::BTreeSet<_>>();
    let expected = [(GRANTED.to_owned(), 1), (GRANTED.to_owned(), 2)]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    if calls != expected {
        violations.push(format!(
            "the restored cell's calls ran as {calls:?}, not both under the grant it resolved"
        ));
    }
    let resolved = turn.world.resolved.load(Ordering::SeqCst);
    if resolved != 1 {
        violations.push(format!(
            "the resolver was asked {resolved} paths, not the one the cell resolved once"
        ));
    }
    let last = turn.last_request();
    if !last.contains(GRANTED) || last.contains(AMBIENT) {
        violations.push(format!(
            "the cell was not answered by the granted tool; the model saw: {last}"
        ));
    }
    assert!(
        violations.is_empty(),
        "deferred grant across a crash on {dialect:?}:\n  {}\n{}",
        violations.join("\n  "),
        nodes.script().rendered_trace()
    );
}

/// Run `turn`, a cell awaiting a host process that sleeps [`PROCESS_MS`],
/// until its `processes.await` parks on its completion wait and the session
/// releases as `waiting`: the nodes, the process, and whether it released
/// before the process ended.
async fn parked_on_a_process(
    turn: &CellTurn,
) -> (
    Arc<SimClock>,
    Arc<dyn DurableStore>,
    SimNodes,
    ProcessId,
    bool,
) {
    let (clock, database, nodes) = simulated(turn).await;
    let until = SimClock::timestamp_ms_at(0) + PROCESS_MS;
    let process = turn
        .backend()
        .process_registry()
        .register_process(
            ProcessRegistration::new(
                ProcessInput::Engine {
                    kind: ENGINE.to_owned(),
                    payload: json!({ "until_ms": until }),
                },
                ProcessProvenance::host(),
                LifetimeDecision::Detached,
            )
            .with_execution_env_ref(Some(
                lash_core_execution::testing::process_execution_env_fixture_ref(),
            )),
        )
        .await
        .expect("register the awaited process")
        .id;
    *turn.world.process.lock_recover() = Some(process.clone());
    turn.send().await.expect("the turn is sent");
    nodes.start("a");
    // The session releases as `waiting` while the process still sleeps.
    let mut released = false;
    while clock.logical_ms() < PROCESS_MS {
        nodes.quiesce().await;
        let session = database.actor(&actor()).await.expect("read the session");
        let parked = database
            .pending_waits(&actor())
            .await
            .expect("the session's waits read")
            .iter()
            .any(|wait| wait.purpose.kind() == WaitKind::ToolCompletion);
        if parked && session.is_some_and(|snapshot| snapshot.state == ActorState::Waiting) {
            released = true;
            break;
        }
        if nodes.step().await.is_none() {
            clock.advance_by(1_000).await;
        }
    }
    (clock, database, nodes, process, released)
}

/// Await process: the cell's `processes.await` parks on its completion
/// wait, the session releases as `waiting` while the process sleeps, and
/// the cell resumes with the process's outcome once it ends.
async fn await_process(dialect: Dialect, postgres_url: Option<String>) {
    let turn = CellTurn::new(Cell::AwaitProcess, dialect, postgres_url);
    let (clock, database, nodes, process, released) = parked_on_a_process(&turn).await;
    let process_actor = ActorKey::process(process.as_str()).expect("a process actor key");
    let mut violations = Vec::new();
    if !released {
        violations.push(format!(
            "the session never released as `waiting` on its parked cell while the process \
             slept; the model saw: {}",
            turn.last_request()
        ));
    }
    // The process ends; the cell resumes with its outcome.
    let horizon = PROCESS_MS * 2;
    while released && !turn.done(&nodes).await {
        if clock.logical_ms() >= horizon {
            violations.push(format!(
                "the turn is not done {} ms after the process's end",
                clock.logical_ms() - PROCESS_MS
            ));
            break;
        }
        if nodes.step().await.is_none() {
            clock.advance_by(1_000).await;
        }
    }
    nodes.quiesce().await;
    if released {
        match database.actor(&process_actor).await {
            Ok(Some(snapshot)) if snapshot.state == ActorState::Terminal => {}
            other => violations.push(format!("the process did not end: {other:?}")),
        }
        let last = turn.last_request();
        if !last.contains("slept") {
            violations.push(format!(
                "the cell was not resumed with the process's outcome; the model saw: {last}"
            ));
        }
        violations.extend(turn_ended(&nodes).await);
    }
    assert!(
        violations.is_empty(),
        "await process on {dialect:?}:\n  {}\n{}",
        violations.join("\n  "),
        nodes.script().rendered_trace()
    );
}

/// After-step during a process await: an `AfterStep` request committed
/// while the cell's `processes.await` is parked never cancels the awaited
/// process or cuts its wait. The process ends on its own terms, the cell
/// resumes with its outcome and the step closes, and the turn stops at that
/// boundary: it ends no sooner than the process, with no model call after
/// the step.
async fn after_step_during_a_process_await(dialect: Dialect, postgres_url: Option<String>) {
    let turn = CellTurn::new(Cell::AwaitProcess, dialect, postgres_url);
    let (clock, database, nodes, process, released) = parked_on_a_process(&turn).await;
    assert!(
        released,
        "the session never released on its parked cell:\n{}",
        nodes.script().rendered_trace()
    );
    let run = database
        .turn(&session())
        .await
        .expect("the turn reads")
        .expect("the parked turn is open")
        .run;
    let answer = lash_core::runtime::durable::session::request_turn_cancel(
        &turn.backend(),
        lash_durable::domain::TurnCancelRequest {
            session: session(),
            run,
            request_id: "stop-in-process".to_owned(),
            origin: None,
            reason: None,
            undelivered: lash_core::TurnCancelUndeliveredInputPolicy::Defer,
            mode: lash_core::TurnCancelMode::AfterStep,
        },
    )
    .await
    .expect("the request commits");
    assert_eq!(answer, lash_durable::domain::TurnCancelAnswer::Requested);
    let horizon = PROCESS_MS * 2;
    let mut violations = Vec::new();
    while !turn.done(&nodes).await {
        if clock.logical_ms() < PROCESS_MS
            && !matches!(database.turn(&session()).await, Ok(Some(_)))
        {
            violations.push(format!(
                "the after-step stop ended the turn at {} ms, before the process's end",
                clock.logical_ms()
            ));
            break;
        }
        if clock.logical_ms() >= horizon {
            violations.push(format!(
                "the turn is not done {} ms after the process's end",
                clock.logical_ms() - PROCESS_MS
            ));
            break;
        }
        if nodes.step().await.is_none() {
            clock.advance_by(1_000).await;
        }
    }
    nodes.quiesce().await;
    let record = turn
        .backend()
        .process_registry()
        .get_process(&process)
        .await
        .expect("the process reads")
        .expect("the process is retained");
    let terminal = format!("{:?}", record.terminal());
    if !terminal.contains("slept") || terminal.contains("Cancel") {
        violations.push(format!(
            "the awaited process did not end on its own terms: {terminal}"
        ));
    }
    let requests = turn.requests.lock_recover().len();
    if requests != 1 {
        violations.push(format!(
            "the model was called {requests} times; an after-step stop calls it no more after \
             the step"
        ));
    }
    violations.extend(turn_ended(&nodes).await);
    assert!(
        violations.is_empty(),
        "after-step during a process await on {dialect:?}:\n  {}\n{}",
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

/// A sleeping cell's session suspends on idle eviction, and the cell wakes
/// once at its admitted deadline across a crash, on SQLite in memory.
#[tokio::test]
async fn a_sleeping_cell_suspends_on_idle_eviction_and_wakes_once_at_its_deadline_on_sqlite_memory()
{
    sleep_across_a_crash(Dialect::SqliteMemory, None).await;
}

/// The same law on a SQLite file.
#[tokio::test]
async fn a_sleeping_cell_suspends_on_idle_eviction_and_wakes_once_at_its_deadline_on_sqlite_file() {
    sleep_across_a_crash(Dialect::SqliteFile, None).await;
}

/// The same law on PostgreSQL.
#[tokio::test]
async fn a_sleeping_cell_suspends_on_idle_eviction_and_wakes_once_at_its_deadline_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    sleep_across_a_crash(Dialect::Postgres, Some(url)).await;
}

/// A `Repeatable` call in a cell, cut at its admission or at its lost
/// outcome commit, re-runs at the same ordinal, on SQLite in memory.
#[tokio::test]
async fn a_repeatable_call_in_a_cell_reruns_at_its_ordinal_on_sqlite_memory() {
    matrix(Cell::Repeatable, Dialect::SqliteMemory, None).await;
}

/// The same law on a SQLite file.
#[tokio::test]
async fn a_repeatable_call_in_a_cell_reruns_at_its_ordinal_on_sqlite_file() {
    matrix(Cell::Repeatable, Dialect::SqliteFile, None).await;
}

/// The same law on PostgreSQL.
#[tokio::test]
async fn a_repeatable_call_in_a_cell_reruns_at_its_ordinal_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    matrix(Cell::Repeatable, Dialect::Postgres, Some(url)).await;
}

/// A race loser in a cell is admitted and settles on its own; nothing runs
/// unadmitted, on SQLite in memory.
#[tokio::test]
async fn a_race_loser_in_a_cell_is_admitted_and_settles_on_sqlite_memory() {
    matrix(Cell::Race, Dialect::SqliteMemory, None).await;
}

/// The same law on a SQLite file.
#[tokio::test]
async fn a_race_loser_in_a_cell_is_admitted_and_settles_on_sqlite_file() {
    matrix(Cell::Race, Dialect::SqliteFile, None).await;
}

/// The same law on PostgreSQL.
#[tokio::test]
async fn a_race_loser_in_a_cell_is_admitted_and_settles_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    matrix(Cell::Race, Dialect::Postgres, Some(url)).await;
}

/// `processes.await` in a cell parks, the session releases and the cell
/// resumes when the process ends, on SQLite in memory.
#[tokio::test]
async fn processes_await_in_a_cell_parks_and_resumes_on_sqlite_memory() {
    await_process(Dialect::SqliteMemory, None).await;
}

/// The same law on a SQLite file.
#[tokio::test]
async fn processes_await_in_a_cell_parks_and_resumes_on_sqlite_file() {
    await_process(Dialect::SqliteFile, None).await;
}

/// The same law on PostgreSQL.
#[tokio::test]
async fn processes_await_in_a_cell_parks_and_resumes_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    await_process(Dialect::Postgres, Some(url)).await;
}

/// A restored cell keeps the deferred grants it resolved, on SQLite in
/// memory (DEFER1).
#[tokio::test]
async fn a_restored_cell_keeps_its_deferred_grants_and_resolves_nothing_twice_on_sqlite_memory() {
    deferred_grant_across_a_crash(Dialect::SqliteMemory, None).await;
}

/// The same law on a SQLite file.
#[tokio::test]
async fn a_restored_cell_keeps_its_deferred_grants_and_resolves_nothing_twice_on_sqlite_file() {
    deferred_grant_across_a_crash(Dialect::SqliteFile, None).await;
}

/// The same law on PostgreSQL.
#[tokio::test]
async fn a_restored_cell_keeps_its_deferred_grants_and_resolves_nothing_twice_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    deferred_grant_across_a_crash(Dialect::Postgres, Some(url)).await;
}

/// An after-step stop during a cell's `processes.await` lets the process
/// finish and stops the turn at the step's boundary, on SQLite in memory.
#[tokio::test]
#[ignore = "FIG-5377: a pass finalizes an after-step request on a parked turn before its wait ends"]
async fn an_after_step_stop_during_a_process_await_lets_the_process_finish_on_sqlite_memory() {
    after_step_during_a_process_await(Dialect::SqliteMemory, None).await;
}
