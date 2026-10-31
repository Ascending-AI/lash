//! L9d (FIG-5182): the process laws, written on the durable harness (ADR 0132 §10, §11, §14).
//!
//! Each law runs the production process activation over a SQLite memory
//! store set on simulated nodes and virtual time. Producers outside the
//! deployment (a host registering, pruning or redriving) write
//! straight to the store set, uncut; the nodes' own writes go through the
//! fault script. Every process runs [`LawEngine`], whose start payload is
//! its state and says what it does.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/scripted_steps.rs"]
mod scripted_steps;
#[path = "support/sim.rs"]
mod sim;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core_execution::DurableSettings;
use lash_core_execution::StartKey;
use lash_core_execution::ToolCallOutput;
use lash_core_execution::ToolCancellation;
use lash_core_execution::runtime::actor::process::ProcessActivation;
use lash_core_execution::runtime::process::steps::{ProcessSteps, StepAdmission, StepRefusal};
use lash_core_store::tool_run::Material;
use lash_core_store::tool_run::SettledOutput;
use lash_core_store::tool_run::{MaterialOwner, MaterialRole};
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableInstant, LeaseConfig};
use lash_durable_test::{Fault, Script, SimClock, SimNodes, SimNodesConfig, Tripwire};
use lash_execution_contracts::backend::Backend;
use lash_execution_contracts::backend::BackendParts;
use lash_execution_contracts::process::EngineAction;
use lash_execution_contracts::process::EngineEvent;
use lash_execution_contracts::process::EngineState;
use lash_execution_contracts::process::EngineStateFormat;
use lash_execution_contracts::process::LifetimeDecision;
use lash_execution_contracts::process::ProcessEngine;
use lash_execution_contracts::process::ProcessEventLogTestSupport as _;
use lash_execution_contracts::process::ProcessInfraError;
use lash_execution_contracts::process::ProcessInput;
use lash_execution_contracts::process::ProcessOutcome;
use lash_execution_contracts::process::ProcessProvenance;
use lash_execution_contracts::process::ProcessRecord;
use lash_execution_contracts::process::ProcessRegistration;
use lash_execution_contracts::process::ProcessStatus;
use lash_execution_contracts::process::ProjectionWatermark;
use lash_execution_contracts::process::StepName;
use lash_execution_contracts::process::StepRequest;
use lash_kernel_value::tool_contract::ExecutionPolicy;
use lash_protocol_types::ExecutionLimit;
use lash_protocol_types::sync::MutexExt as _;
use lash_vocabulary::identity::ProcessId;
use serde_json::{Value, json};

const KIND: &str = "l9d-law";
/// The tool a step names that its admission refuses.
const REFUSED_TOOL: &str = "l9d_refused";
/// A wait deadline no law reaches.
const LONG: Duration = Duration::from_secs(3_600);
/// How many clock steps a law drives before it gives up.
const STEPS: usize = 400;

// --- the engine -------------------------------------------------------------

/// Every `advance` call: the calling process's tag and the event's name.
type AdvanceLog = Arc<Mutex<Vec<(String, String)>>>;

/// The law engine. `act` in its start payload says what it does:
///
/// - `hold`: idles until cancellation;
/// - `sleep`: sleeps until `until_ms`, then ends;
/// - `await`: awaits process `await` for `deadline_ms` (default [`LONG`]),
///   and ends with what it saw;
/// - `refused_step`: asks for one step whose admission is refused;
/// - `flaky`: ends at once.
///
/// Every process answers its cancel with a cancelled terminal.
struct LawEngine {
    log: AdvanceLog,
}

fn infra(error: impl std::fmt::Display) -> ProcessInfraError {
    ProcessInfraError::new(
        lash_execution_contracts::plugin_error::PluginError::Session(error.to_string()),
    )
}

fn success(value: Value) -> EngineAction {
    EngineAction::Terminal(ProcessOutcome::from_tool_output(ToolCallOutput::success(
        value,
    )))
}

fn event_name(event: &EngineEvent) -> String {
    match event {
        EngineEvent::Started { .. } => "started".to_owned(),
        EngineEvent::StepSettled { .. } => "step_settled".to_owned(),
        EngineEvent::ProcessEnded { .. } => "process_ended".to_owned(),
        EngineEvent::ProcessWaitTimedOut { .. } => "process_wait_timed_out".to_owned(),
        EngineEvent::Woke => "woke".to_owned(),
        EngineEvent::Cancelled { .. } => "cancelled".to_owned(),
        other => format!("{other:?}"),
    }
}

#[async_trait::async_trait]
impl ProcessEngine for LawEngine {
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
        link: lash_kernel_value::LinkId,
        selector: &lash_execution_contracts::process::linking::ReachSelector,
    ) -> Result<
        lash_execution_contracts::process::linking::LinkInspection,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        Ok(lash_execution_contracts::process::linking::LinkInspection {
            link,
            document: lash_kernel_value::DocumentId::from_bytes([0; 32]),
            selector: selector.clone(),
            operations: Default::default(),
            machine: Default::default(),
            artifacts: Vec::new(),
            signature: None,
        })
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
                operation: "generic fixture has no linked code".into(),
            },
        )
    }
    fn capabilities(&self) -> lash_kernel_value::claims::NodeRequirements {
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

    fn state_format(&self) -> EngineStateFormat {
        EngineStateFormat {
            kind: KIND.to_owned(),
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
        state: EngineState,
        event: EngineEvent,
    ) -> Result<(EngineState, EngineAction), ProcessInfraError> {
        let script: Value = match &event {
            EngineEvent::Started { payload } => payload.clone(),
            _ => serde_json::from_slice(&state.bytes).map_err(infra)?,
        };
        let tag = script["tag"].as_str().unwrap_or_default().to_owned();
        self.log.lock_recover().push((tag, event_name(&event)));
        let act = script["act"].as_str().unwrap_or_default().to_owned();
        let action = match (act.as_str(), event) {
            (_, EngineEvent::Cancelled { origin, .. }) => {
                EngineAction::Terminal(ProcessOutcome::from_tool_output(ToolCallOutput::cancelled(
                    ToolCancellation::runtime("the law engine answered its cancel")
                        .with_origin(origin),
                )))
            }
            ("sleep", EngineEvent::Started { .. }) => EngineAction::Sleep {
                site: None,
                until: DurableInstant(script["until_ms"].as_i64().unwrap_or_default()),
            },
            ("sleep", EngineEvent::Woke) => success(json!({ "slept": true })),
            ("await", EngineEvent::Started { .. }) => EngineAction::AwaitProcess {
                site: None,
                process: ProcessId::parse(script["await"].as_str().unwrap_or_default())
                    .map_err(infra)?,
                bound: lash_vocabulary::tool_contract::ParkBound::Within(
                    script["deadline_ms"]
                        .as_u64()
                        .map_or(LONG, Duration::from_millis),
                ),
            },
            ("await", EngineEvent::ProcessEnded { outcome, .. }) => success(json!({
                "ended": serde_json::to_value(lash_protocol_types::Durable(&outcome)).map_err(infra)?,
            })),
            ("await", EngineEvent::ProcessWaitTimedOut { .. }) => {
                success(json!({ "timed_out": true }))
            }
            ("refused_step", EngineEvent::Started { .. }) => EngineAction::Steps {
                steps: vec![StepRequest::Engine {
                    step: StepName("refused".to_owned()),
                    kind: lash_execution_contracts::process::EngineStepKind::new(REFUSED_TOOL),
                    input: json!({}),
                }],
                wake: None,
            },
            ("refused_step", EngineEvent::StepSettled { .. }) => success(json!({ "ran": true })),
            ("flaky", EngineEvent::Started { .. }) => success(json!({ "ran": true })),
            _ => EngineAction::Idle,
        };
        let bytes = serde_json::to_vec(&script).map_err(infra)?;
        Ok((
            EngineState {
                staged: Vec::new(),
                format: self.state_format(),
                bytes,
                pages: None,
                requirements: Default::default(),
            },
            action,
        ))
    }

    fn start_artifacts(
        &self,
        _payload: &Value,
    ) -> Result<
        Vec<lash_core_execution::ArtifactName>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core_execution::ResolvedArtifactCleanup,
    ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core_execution::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        Err(
            lash_execution_contracts::plugin_error::PluginError::Session(format!(
                "the law engine stores no artifact `{artifact_ref}`"
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

/// The host's half of a step: [`REFUSED_TOOL`]'s admission is refused; any
/// other tool is a `Once` step that completes.
struct LawSteps;

#[async_trait::async_trait]
impl ProcessSteps for LawSteps {
    fn stop_grace(&self) -> std::time::Duration {
        std::time::Duration::from_secs(2)
    }

    fn assess(
        &self,
        _runtime: &Arc<lash_core_execution::runtime::process::StepRuntime>,
        _process: &ProcessRecord,
        step: &StepRequest,
        _target: Option<&lash_core_execution::runtime::process::steps::InvocationTarget>,
    ) -> Result<lash_core_execution::runtime::process::steps::StepAssessment, StepRefusal> {
        if scripted_steps::tool(step).as_str() == REFUSED_TOOL {
            return Err(StepRefusal::Refused {
                step: step.step().0.clone(),
                reason: "its input does not match the tool's declaration".to_owned(),
            });
        }
        Ok(
            lash_core_execution::runtime::process::steps::StepAssessment::Current {
                tool: scripted_steps::tool(step),
                policy: ExecutionPolicy::Once,
            },
        )
    }

    async fn admit(
        &self,
        _runtime: &Arc<lash_core_execution::runtime::process::StepRuntime>,
        _process: &ProcessRecord,
        step: &StepRequest,
        _target: Option<&lash_core_execution::runtime::process::steps::InvocationTarget>,
        now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal> {
        if scripted_steps::tool(step).as_str() == REFUSED_TOOL {
            return Err(StepRefusal::Refused {
                step: step.step().0.clone(),
                reason: "its input does not match the tool's declaration".to_owned(),
            });
        }
        Ok(StepAdmission::new(
            scripted_steps::tool(step),
            ExecutionPolicy::Once,
            ExecutionLimit::starting_at(now_ms, Duration::from_secs(60), Duration::from_secs(60)),
            None,
        ))
    }

    /// Never asked: no step of these parks.
    fn resolved(
        &self,
        _process: &ProcessRecord,
        _step: &StepRequest,
        _execution: &lash_core_execution::runtime::actor::round::AdmittedExecution,
        _parked: &lash_core_store::tool_run::Material<lash_core_store::tool_run::CompletionSource>,
        _resolution: lash_core_execution::runtime::actor::waits::Resolution,
    ) -> lash_core_store::tool_run::SettledOutput {
        lash_core_store::tool_run::SettledOutput::Interrupted
    }

    fn body(
        &self,
        _runtime: &std::sync::Arc<lash_core_execution::runtime::process::StepRuntime>,
        process: &ProcessRecord,
        _step: &StepRequest,
        _execution: &lash_core_execution::runtime::actor::round::AdmittedExecution,
    ) -> lash_core_execution::runtime::actor::round::MemberBody {
        lash_core_execution::runtime::actor::round::member_body({
            let process = process.id.clone();
            Box::new(move |_token| {
                Box::pin(async move {
                    let output = json!({ "ok": true }).to_string();
                    SettledOutput::Completed(Material::journal_local(
                        MaterialOwner::Process {
                            process_id: process,
                        },
                        MaterialRole::AttemptOutput,
                        output,
                    ))
                })
            })
        })
    }
}

// --- the deployment ---------------------------------------------------------

/// One deployment under a law: a store set on the virtual clock, its
/// backend serving [`LawEngine`], and the simulated nodes.
struct World {
    clock: Arc<SimClock>,
    backend: Backend,
    nodes: SimNodes,
    log: AdvanceLog,
}

impl World {
    async fn new(script: Script) -> Self {
        Self::retained(
            script,
            lash_durable::OwnerRetentionSettings::default(),
            |activation| activation,
        )
        .await
    }

    async fn retained(
        script: Script,
        retention: lash_durable::OwnerRetentionSettings,
        wrap: impl FnOnce(Arc<ProcessActivation>) -> Arc<dyn lash_durable::runner::Activation>,
    ) -> Self {
        let clock = SimClock::new();
        let stores = sim::memory(Arc::clone(&clock)).await;
        let database: Arc<dyn lash_durable::DurableStore> = Arc::new(stores.durable_store());
        let log = AdvanceLog::default();
        let backend = Backend::assemble(BackendParts {
            stores: Arc::new(stores),
            settings: DurableSettings {
                owner_retention: retention,
                ..DurableSettings::default()
            },
            engines: vec![Arc::new(LawEngine {
                log: Arc::clone(&log),
            })],

            formats: Vec::new(),
        })
        .expect("the law backend assembles");
        let activation = Arc::new(ProcessActivation::new(
            backend.clone(),
            Arc::new(LawSteps),
            Arc::new(Tripwire::default()) as _,
        ));
        let nodes = SimNodes::new(
            database,
            Arc::clone(&clock),
            script,
            SimNodesConfig {
                lease: LeaseConfig::default(),
                decodes: backend.formats().decodes(),
                serves: Vec::new(),
                max_active: 8,
            },
            wrap(activation),
        )
        .with_retention(retention);
        Self {
            clock,
            backend,
            nodes,
            log,
        }
    }

    async fn register(&self, registration: ProcessRegistration) -> ProcessId {
        self.backend
            .process_registry()
            .register_process(registration)
            .await
            .expect("register the process")
            .id
    }

    /// `process`'s registry row; `None` once it was pruned.
    async fn record(&self, process: &ProcessId) -> Option<ProcessRecord> {
        match self.backend.process_registry().get_process(process).await {
            Ok(record) => record,
            Err(lash_execution_contracts::plugin_error::PluginError::ProcessNoLongerRetained {
                ..
            }) => None,
            Err(error) => panic!("read the process: {error}"),
        }
    }

    /// `process`'s terminal outcome, encoded, once it has one.
    async fn terminal(&self, process: &ProcessId) -> Option<Value> {
        let record = self.record(process).await?;
        record.terminal().map(|terminal| {
            serde_json::to_value(lash_protocol_types::Durable(
                &terminal.clone().into_await_output(),
            ))
            .unwrap()
        })
    }

    async fn actor_state(&self, process: &ProcessId) -> Option<ActorState> {
        self.nodes
            .database()
            .actor(&actor(process))
            .await
            .expect("read the actor")
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

    /// The events `tag`'s process was advanced with, in order, a recomputed
    /// transition (the same event again) counted once.
    fn advances(&self, tag: &str) -> Vec<String> {
        let mut events: Vec<String> = self
            .log
            .lock_recover()
            .iter()
            .filter(|(seen, _)| seen == tag)
            .map(|(_, event)| event.clone())
            .collect();
        events.dedup();
        events
    }
}

fn actor(process: &ProcessId) -> ActorKey {
    ActorKey::process(process.as_str()).expect("a process actor key")
}

fn registration(payload: Value) -> ProcessRegistration {
    ProcessRegistration::new(
        ProcessInput::Engine {
            kind: KIND.to_owned(),
            payload,
            launch: lash_execution_contracts::process::EngineLaunch::new(
                lash_execution_contracts::testing::process_execution_env_fixture_ref(),
                None,
            ),
        },
        ProcessProvenance::host(),
        LifetimeDecision::Detached,
    )
}

/// The first value under `key` anywhere in `value`.
fn find<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(entries) => entries
            .get(key)
            .or_else(|| entries.values().find_map(|nested| find(nested, key))),
        Value::Array(items) => items.iter().find_map(|nested| find(nested, key)),
        _ => None,
    }
}

// --- the laws ---------------------------------------------------------------

/// A step whose admission is refused ends its process `Failed`, typed, with
/// exactly one terminal event, and the process awaiting it reads that
/// failure: nothing is stranded running with its awaiter waiting
/// (the admission invariant, FIG-3819).
#[tokio::test]
async fn a_refused_step_admission_ends_the_process_failed_and_its_awaiter_reads_it() {
    let world = World::new(Script::new()).await;
    let refused = world
        .register(registration(
            json!({ "tag": "refused", "act": "refused_step" }),
        ))
        .await;
    let awaiter = world
        .register(registration(
            json!({ "tag": "awaiter", "act": "await", "await": refused.as_str() }),
        ))
        .await;
    world.nodes.start("a");
    world
        .until("the awaiter ended", || async {
            world.terminal(&awaiter).await.is_some()
        })
        .await;

    let record = world.record(&refused).await.expect("the refused process");
    assert_eq!(record.status(), ProcessStatus::Failed, "{record:?}");
    let end = world.terminal(&refused).await.expect("its terminal");
    assert!(
        end.to_string().contains("process_action_refused")
            && end.to_string().contains("its input does not match"),
        "the failure is typed and names the refusal: {end}"
    );
    let terminals = world
        .backend
        .process_registry()
        .full_event_window(&refused, 0)
        .await
        .expect("read its events")
        .into_iter()
        .filter(|event| {
            matches!(
                event.fact.event_type(),
                "process.completed" | "process.failed" | "process.cancelled" | "process.abandoned"
            )
        })
        .count();
    assert_eq!(terminals, 1, "exactly one terminal event");
    let seen = world.terminal(&awaiter).await.expect("the awaiter's end");
    assert_eq!(
        find(&seen, "ended"),
        Some(&end),
        "the awaiter read the stored failure"
    );
    assert_eq!(
        world.advances("refused"),
        ["started"],
        "the refused step settled nothing back into the engine"
    );
}

/// A process awaiting another reads the outcome its wait was resolved
/// with, even when the awaited process was pruned before the awaiter ran
/// again: the wait keeps the material, not the producer's row (source-seal
/// laws L02/L12).
///
/// Node A dies the instant the awaited process's cascade drains, so the
/// awaiter has not run since; the host prunes the awaited process; then
/// node B claims the awaiter.
#[tokio::test]
async fn an_awaiter_reads_the_outcome_of_a_process_pruned_after_it_ended() {
    let script = Script::new();
    script.cut(CommitLabel::CASCADE_BATCH, 1, Fault::CommitThenAbort);
    let world = World::new(script).await;
    let producer = world
        .register(registration(json!({
            "tag": "producer",
            "act": "sleep",
            "until_ms": SimClock::timestamp_ms_at(10_000),
        })))
        .await;
    let awaiter = world
        .register(registration(
            json!({ "tag": "awaiter", "act": "await", "await": producer.as_str() }),
        ))
        .await;
    world.nodes.start("a");
    world
        .until("the producer's cascade drained", || async {
            world.actor_state(&producer).await == Some(ActorState::Terminal)
        })
        .await;
    let produced = world.terminal(&producer).await.expect("the producer's end");
    assert!(
        world.terminal(&awaiter).await.is_none(),
        "node A died before the awaiter ran"
    );
    let now = world.backend.clock().timestamp_ms();
    let report = world
        .backend
        .process_registry()
        .prune_terminal_processes(now + 1, None, ProjectionWatermark::NoProjector)
        .await
        .expect("prune the ended producer");
    assert!(
        world.record(&producer).await.is_none(),
        "the producer is pruned: {report:?}"
    );

    world.nodes.start("b");
    world
        .until("the awaiter ended", || async {
            world.terminal(&awaiter).await.is_some()
        })
        .await;
    let seen = world.terminal(&awaiter).await.expect("the awaiter's end");
    assert_eq!(
        find(&seen, "ended"),
        Some(&produced),
        "the awaiter read the pruned producer's outcome: {seen}"
    );
}

/// A process awaiting another takes its event from its wait row's committed
/// winner: a timeout that committed before the awaited process ended is
/// answered as a timeout, though the awaited process has ended by the time
/// the awaiter next reads (FIG-5227).
///
/// The awaiter's `wait.timeout` commits, and its answer reaches the awaiter
/// only after the awaited process's terminal has committed.
#[tokio::test]
async fn a_timeout_that_wins_before_the_awaited_terminal_is_answered_as_a_timeout() {
    let script = Script::new();
    script.cut(
        CommitLabel::WAIT_TIMEOUT,
        1,
        Fault::DelayedAck(Duration::from_secs(20)),
    );
    let world = World::new(script).await;
    let producer = world
        .register(registration(json!({
            "tag": "producer",
            "act": "sleep",
            "until_ms": SimClock::timestamp_ms_at(8_000),
        })))
        .await;
    let awaiter = world
        .register(registration(json!({
            "tag": "awaiter",
            "act": "await",
            "await": producer.as_str(),
            "deadline_ms": 2_000,
        })))
        .await;
    world.nodes.start("a");
    world
        .until("the awaiter ended", || async {
            world.terminal(&awaiter).await.is_some()
        })
        .await;

    let timed_out = world
        .nodes
        .script()
        .trace()
        .into_iter()
        .find(|write| write.point.label == CommitLabel::WAIT_TIMEOUT)
        .expect("the awaiter's wait timed out");
    let ended = world
        .nodes
        .script()
        .trace()
        .into_iter()
        .find(|write| write.point.label == CommitLabel::PROCESS_TERMINAL)
        .expect("the producer ended");
    assert!(
        timed_out.at_ms < ended.at_ms && world.terminal(&producer).await.is_some(),
        "the timeout committed before the producer's terminal: {}",
        world.nodes.script().rendered_trace()
    );
    let seen = world.terminal(&awaiter).await.expect("the awaiter's end");
    assert_eq!(
        find(&seen, "timed_out"),
        Some(&json!(true)),
        "the awaiter was answered its committed timeout: {seen}"
    );
    assert_eq!(
        world.advances("awaiter"),
        ["started", "process_wait_timed_out"]
    );
}

/// A process that awaits one already ended and pruned before its wait was
/// pinned has no outcome left to read: its wait's deadline answers it, and
/// the awaiter ends instead of failing every pass on the pruned row
/// (FIG-5227).
#[tokio::test]
async fn an_await_pinned_after_its_target_was_pruned_is_answered_by_its_deadline() {
    let world = World::new(Script::new()).await;
    let gone = world
        .register(registration(json!({ "tag": "gone", "act": "flaky" })))
        .await;
    world.nodes.start("a");
    world
        .until("the awaited process ended", || async {
            world.terminal(&gone).await.is_some()
        })
        .await;
    let now = world.backend.clock().timestamp_ms();
    world
        .backend
        .process_registry()
        .prune_terminal_processes(now + 1, None, ProjectionWatermark::NoProjector)
        .await
        .expect("prune the ended process");
    assert!(world.record(&gone).await.is_none(), "it is pruned");

    let awaiter = world
        .register(registration(json!({
            "tag": "awaiter",
            "act": "await",
            "await": gone.as_str(),
            "deadline_ms": 2_000,
        })))
        .await;
    world
        .until("the awaiter ended", || async {
            world.terminal(&awaiter).await.is_some()
        })
        .await;
    let seen = world.terminal(&awaiter).await.expect("the awaiter's end");
    assert_eq!(find(&seen, "timed_out"), Some(&json!(true)), "{seen}");
}

/// A transient store failure on the read a process activation makes right
/// after its claim strands nothing: the process runs on and reaches its
/// terminal on the node that claimed it, and its actor is never left owned
/// with no activation (FIG-5227).
#[tokio::test]
async fn a_transient_read_failure_after_claim_never_strands_the_process() {
    let world = World::new(Script::new()).await;
    let process = world
        .register(registration(json!({
            "tag": "sleeper",
            "act": "sleep",
            "until_ms": SimClock::timestamp_ms_at(2_000),
        })))
        .await;
    world.nodes.script().fail_actor_read(actor(&process));
    world.nodes.start("a");
    world
        .until("the process ended", || async {
            world.terminal(&process).await.is_some()
        })
        .await;

    assert_eq!(world.advances("sleeper"), ["started", "woke"]);
    world
        .until("its actor is terminal", || async {
            world.actor_state(&process).await == Some(ActorState::Terminal)
        })
        .await;
}

/// A process started under the start key of a pruned process is a new
/// process: its own id, its own engine run from `Started`, its own terminal
/// (successor after prune, FIG-3611).
#[tokio::test]
async fn a_start_under_a_pruned_processs_key_is_a_new_process_with_its_own_lifecycle() {
    let world = World::new(Script::new()).await;
    let key = StartKey::for_host("l9d-successor");
    let first = world
        .register(
            registration(json!({ "tag": "first", "act": "flaky" }))
                .with_start_key(Some(key.clone())),
        )
        .await;
    world.nodes.start("a");
    world
        .until("the first process's actor is terminal", || async {
            world.actor_state(&first).await == Some(ActorState::Terminal)
        })
        .await;
    let now = world.backend.clock().timestamp_ms();
    world
        .backend
        .process_registry()
        .prune_terminal_processes(now + 1, None, ProjectionWatermark::NoProjector)
        .await
        .expect("prune the first process");
    assert!(world.record(&first).await.is_none(), "the first is pruned");

    let successor = world
        .register(
            registration(json!({ "tag": "successor", "act": "flaky" }))
                .with_start_key(Some(key.clone())),
        )
        .await;
    assert_ne!(successor, first, "the key minted a new process");
    world
        .until("the successor ended", || async {
            world.terminal(&successor).await.is_some()
        })
        .await;
    assert_eq!(
        world.advances("successor"),
        ["started"],
        "the successor ran its own engine from its start, once"
    );
    assert_eq!(world.advances("first"), ["started"]);
    let record = world.record(&successor).await.expect("the successor");
    assert_eq!(record.start_key.as_ref(), Some(&key));
}

#[path = "process_actor_laws/retention.rs"]
mod retention;
