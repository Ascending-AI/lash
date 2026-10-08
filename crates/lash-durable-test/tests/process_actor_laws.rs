//! L9d (FIG-5182): the process laws, written on the durable harness (ADR 0132 §10, §11, §14).
//!
//! Each law runs the production process activation over a SQLite memory
//! store set on simulated nodes and virtual time. Producers outside the
//! deployment (a host registering, signalling, pruning or redriving) write
//! straight to the store set, uncut; the nodes' own writes go through the
//! fault script. Every process runs [`LawEngine`], whose start payload is
//! its state and says what it does.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/sim.rs"]
mod sim;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core_execution::runtime::actor::process::ProcessActivation;
use lash_core_execution::runtime::actor::round::{Material, SettledOutput};
use lash_core_execution::runtime::process::steps::{ProcessSteps, StepAdmission, StepRefusal};
use lash_core_execution::{
    Backend, BackendParts, DurableSettings, EngineAction, EngineEvent, EngineState,
    EngineStateFormat, JsonSchema, LifetimeDecision, NoProjectionProviders, ProcessEngine,
    ProcessEventLogTestSupport as _, ProcessEventSemanticsSpec, ProcessEventType, ProcessId,
    ProcessInfraError, ProcessInput, ProcessOutcome, ProcessProvenance, ProcessRecord,
    ProcessRegistration, ProcessSignal, ProcessSignalIdentity, ProcessStatus, ProjectionWatermark,
    StartKey, StepName, StepRequest, ToolCallOutput, ToolCancellation,
};
use lash_core_store::tool_run::{MaterialOwner, MaterialRole};
use lash_durable::domain::ParkEventKind;
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableInstant, LeaseConfig};
use lash_durable_test::{Fault, Script, SimClock, SimNodes, SimNodesConfig, Tripwire};
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{ExecutionLimit, ExecutionPolicy, ToolId};
use serde_json::{Value, json};

const KIND: &str = "l9d-law";
/// The tool a step names that its admission refuses.
const REFUSED_TOOL: &str = "l9d_refused";
/// The signal every signalled law process declares.
const SIGNAL: &str = "nudge";
/// The event type an emitting law process declares.
const EMITTED: &str = "l9d.emitted";
/// A wait deadline no law reaches.
const LONG: Duration = Duration::from_secs(3_600);
/// How many clock steps a law drives before it gives up.
const STEPS: usize = 400;

// --- the engine -------------------------------------------------------------

/// Every `advance` call: the calling process's tag and the event's name.
type AdvanceLog = Arc<Mutex<Vec<(String, String)>>>;

/// The law engine. `act` in its start payload says what it does:
///
/// - `hold`: idles; keeps each signal's payload, and ends with them once it
///   has `until` of them;
/// - `sleep`: sleeps until `until_ms`, then ends;
/// - `await`: awaits process `await` for `deadline_ms` (default [`LONG`]),
///   and ends with what it saw;
/// - `refused_step`: asks for one step whose admission is refused;
/// - `emit`: emits one event of type `event`, then ends;
/// - `flaky`: ends at once, but its `advance` fails opaquely while the
///   engine's `refuse` flag is set.
///
/// Every process answers its cancel with a cancelled terminal.
struct LawEngine {
    log: AdvanceLog,
    refuse: Arc<AtomicBool>,
}

fn infra(error: impl std::fmt::Display) -> ProcessInfraError {
    ProcessInfraError::new(lash_core_execution::PluginError::Session(error.to_string()))
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
        EngineEvent::Emitted => "emitted".to_owned(),
        EngineEvent::ProcessEnded { .. } => "process_ended".to_owned(),
        EngineEvent::ProcessWaitTimedOut { .. } => "process_wait_timed_out".to_owned(),
        EngineEvent::Woke => "woke".to_owned(),
        EngineEvent::Signal(_) => "signal".to_owned(),
        EngineEvent::Cancelled { .. } => "cancelled".to_owned(),
        other => format!("{other:?}"),
    }
}

#[async_trait::async_trait]
impl ProcessEngine for LawEngine {
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
        let mut script: Value = match &event {
            EngineEvent::Started { payload } => payload.clone(),
            _ => serde_json::from_slice(&state.bytes).map_err(infra)?,
        };
        let tag = script["tag"].as_str().unwrap_or_default().to_owned();
        self.log.lock_recover().push((tag, event_name(&event)));
        let act = script["act"].as_str().unwrap_or_default().to_owned();
        if act == "flaky" && self.refuse.load(Ordering::SeqCst) {
            return Err(infra("the engine's host is unreachable"));
        }
        let action = match (act.as_str(), event) {
            (_, EngineEvent::Cancelled { origin, .. }) => {
                EngineAction::Terminal(ProcessOutcome::from_tool_output(ToolCallOutput::cancelled(
                    ToolCancellation::runtime("the law engine answered its cancel")
                        .with_origin(origin),
                )))
            }
            ("hold", EngineEvent::Signal(signal)) => {
                let mut signals = script["signals"].as_array().cloned().unwrap_or_default();
                signals.push(signal.payload.clone());
                script["signals"] = Value::Array(signals.clone());
                if Some(signals.len() as u64) == script["until"].as_u64() {
                    success(json!({ "signals": signals }))
                } else {
                    EngineAction::Idle
                }
            }
            ("sleep", EngineEvent::Started { .. }) => EngineAction::Sleep {
                until: DurableInstant(script["until_ms"].as_i64().unwrap_or_default()),
            },
            ("sleep", EngineEvent::Woke) => success(json!({ "slept": true })),
            ("await", EngineEvent::Started { .. }) => EngineAction::AwaitProcess {
                process: ProcessId::parse(script["await"].as_str().unwrap_or_default())
                    .map_err(infra)?,
                deadline: Some(
                    script["deadline_ms"]
                        .as_u64()
                        .map_or(LONG, Duration::from_millis),
                ),
            },
            ("await", EngineEvent::ProcessEnded { outcome, .. }) => success(json!({
                "ended": serde_json::to_value(&outcome).map_err(infra)?,
            })),
            ("await", EngineEvent::ProcessWaitTimedOut { .. }) => {
                success(json!({ "timed_out": true }))
            }
            ("refused_step", EngineEvent::Started { .. }) => {
                EngineAction::Steps(vec![StepRequest::Tool {
                    language_execution: None,
                    step: StepName("refused".to_owned()),
                    tool: ToolId::new(REFUSED_TOOL),
                    input: json!({}),
                    site: None,
                }])
            }
            ("refused_step", EngineEvent::StepSettled { .. }) => success(json!({ "ran": true })),
            ("emit", EngineEvent::Started { .. }) => EngineAction::Emit {
                event_type: event_type(script["event"].as_str().unwrap_or_default()),
                payload: json!({ "emitted_by": script["tag"] }),
            },
            ("emit", EngineEvent::Emitted) => success(json!({ "emitted": true })),
            ("flaky", EngineEvent::Started { .. }) => success(json!({ "ran": true })),
            _ => EngineAction::Idle,
        };
        let bytes = serde_json::to_vec(&script).map_err(infra)?;
        Ok((
            EngineState {
                format: self.state_format(),
                bytes,
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
            "the law engine stores no artifact `{artifact_ref}`"
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
            Vec::new(),
        ))
    }
}

fn event_type(name: &str) -> ProcessEventType {
    ProcessEventType {
        name: name.to_owned(),
        payload_schema: JsonSchema::any(),
        semantics: ProcessEventSemanticsSpec::default(),
    }
}

/// The host's half of a step: [`REFUSED_TOOL`]'s admission is refused; any
/// other tool is a `Once` step that completes.
struct LawSteps;

#[async_trait::async_trait]
impl ProcessSteps for LawSteps {
    async fn admit(
        &self,
        _process: &ProcessRecord,
        step: &StepRequest,
        now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal> {
        if step.admitted_tool(KIND).as_str() == REFUSED_TOOL {
            return Err(StepRefusal::Refused {
                step: step.step().0.clone(),
                reason: "its input does not match the tool's declaration".to_owned(),
            });
        }
        Ok(StepAdmission {
            wait: None,
            policy: ExecutionPolicy::Once,
            limit: ExecutionLimit::starting_at(
                now_ms,
                Duration::from_secs(60),
                Duration::from_secs(60),
            ),
        })
    }

    /// Never asked: no step of these parks.
    fn resolved(
        &self,
        _process: &ProcessRecord,
        _step: &StepRequest,
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
    refuse: Arc<AtomicBool>,
}

impl World {
    async fn new(script: Script) -> Self {
        let clock = SimClock::new();
        let stores = sim::memory(Arc::clone(&clock)).await;
        let database: Arc<dyn lash_durable::DurableStore> = Arc::new(stores.durable_store());
        let log = AdvanceLog::default();
        let refuse = Arc::new(AtomicBool::new(false));
        let backend = Backend::assemble(BackendParts {
            stores: Arc::new(stores),
            settings: DurableSettings::default(),
            engines: vec![Arc::new(LawEngine {
                log: Arc::clone(&log),
                refuse: Arc::clone(&refuse),
            })],
            providers: Arc::new(NoProjectionProviders),
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
                max_active: 8,
            },
            activation,
        );
        Self {
            clock,
            backend,
            nodes,
            log,
            refuse,
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
            Err(lash_core_execution::PluginError::ProcessNoLongerRetained { .. }) => None,
            Err(error) => panic!("read the process: {error}"),
        }
    }

    /// `process`'s terminal outcome, encoded, once it has one.
    async fn terminal(&self, process: &ProcessId) -> Option<Value> {
        let record = self.record(process).await?;
        record
            .terminal()
            .map(|terminal| serde_json::to_value(terminal.clone().into_await_output()).unwrap())
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
        },
        ProcessProvenance::host(),
        LifetimeDecision::Detached,
    )
    .with_execution_env_ref(Some(
        lash_core_execution::testing::process_execution_env_fixture_ref(),
    ))
}

fn signal_type() -> ProcessEventType {
    event_type(&lash_core_execution::runtime::process_signal_event_type(SIGNAL).unwrap())
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
                event.event_type.as_str(),
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

/// A signal is admitted by its first append: a repeat of it after the
/// engine consumed it reaches the engine never again, a resend under the
/// same identity with a changed payload is refused before anything
/// resolves, and the next distinct signal arrives as the next event
/// (signal admission, FIG-4298).
#[tokio::test]
async fn a_signal_is_admitted_once_and_a_changed_resend_is_refused() {
    let world = World::new(Script::new()).await;
    let process = world
        .register(
            registration(json!({ "tag": "signalled", "act": "hold", "until": 2 }))
                .with_extra_event_types([signal_type()]),
        )
        .await;
    let registry = world.backend.process_registry();
    let send = |id: &str, payload: Value| {
        let signal = ProcessSignal::new(
            ProcessSignalIdentity::new(process.clone(), SIGNAL, id).expect("a signal identity"),
            payload,
        );
        let registry = Arc::clone(&registry);
        let process = process.clone();
        async move {
            registry
                .append_event(&process, signal.append_request())
                .await
        }
    };
    world.nodes.start("a");
    world
        .until("the process started", || async {
            world.advances("signalled") == ["started"]
        })
        .await;

    let first = send("first", json!({ "n": 1 }))
        .await
        .expect("the first signal is admitted");
    world
        .until("the first signal reached the engine", || async {
            world.advances("signalled") == ["started", "signal"]
        })
        .await;
    let repeat = send("first", json!({ "n": 1 }))
        .await
        .expect("a repeat of an admitted signal is served its admission");
    assert_eq!(
        repeat.event.sequence, first.event.sequence,
        "the repeat is the admitted event"
    );
    let changed = send("first", json!({ "n": 99 })).await;
    assert!(
        changed.is_err(),
        "a resend with a changed payload is refused: {changed:?}"
    );
    for _ in 0..5 {
        world.nodes.step().await;
    }
    assert_eq!(
        world.advances("signalled"),
        ["started", "signal"],
        "neither the repeat nor the refused resend reached the engine"
    );

    send("second", json!({ "n": 2 }))
        .await
        .expect("the next signal is admitted");
    world
        .until("the process ended", || async {
            world.terminal(&process).await.is_some()
        })
        .await;
    let end = world.terminal(&process).await.expect("its end");
    assert_eq!(
        find(&end, "signals"),
        Some(&json!([{ "n": 1 }, { "n": 2 }])),
        "the engine saw each admitted signal once, in order: {end}"
    );
    let signals = registry
        .full_event_window(&process, 0)
        .await
        .expect("read its events")
        .into_iter()
        .filter(|event| event.event_type == signal_type().name)
        .count();
    assert_eq!(signals, 2, "the log holds each admitted signal once");
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

/// An opaque failure of an engine's `advance` never becomes the process's
/// terminal: the process parks with `AdvanceRefused`, holds no outcome, and
/// an operator's redrive runs it on to its own terminal (an opaque
/// infrastructure failure).
#[tokio::test]
async fn an_opaque_advance_failure_parks_the_process_and_a_redrive_runs_it_on() {
    let world = World::new(Script::new()).await;
    world.refuse.store(true, Ordering::SeqCst);
    let process = world
        .register(registration(json!({ "tag": "flaky", "act": "flaky" })))
        .await;
    world.nodes.start("a");
    world
        .until("the process parked", || async {
            world.actor_state(&process).await == Some(ActorState::Parked)
        })
        .await;
    assert!(
        world.terminal(&process).await.is_none(),
        "the failure is no terminal"
    );
    let record = world.record(&process).await.expect("the process");
    assert!(!record.is_terminal(), "{record:?}");
    let feed: Vec<_> = world
        .nodes
        .database()
        .park_events(None, 100)
        .await
        .expect("read the park feed")
        .into_iter()
        .filter(|row| row.actor == actor(&process))
        .collect();
    assert!(
        feed.len() == 1
            && feed[0].kind == ParkEventKind::Parked
            && feed[0].reason_json.contains("advance_refused"),
        "one AdvanceRefused park: {feed:?}"
    );

    world.refuse.store(false, Ordering::SeqCst);
    assert!(
        world
            .backend
            .redrive_process(&process, "l9d-operator")
            .await
            .expect("redrive"),
        "the redrive found the process parked"
    );
    world
        .until("the redriven process ended", || async {
            world.terminal(&process).await.is_some()
        })
        .await;
    let end = world.terminal(&process).await.expect("its end");
    assert_eq!(find(&end, "ran"), Some(&json!(true)), "{end}");
}

/// An `Emit` whose commit meets a transient store fault, or whose commit's
/// answer is lost, is never the step's answer: the transition is
/// recomputed from the committed rows, and the event is appended exactly
/// once (store faults, FIG-4649).
#[tokio::test]
async fn an_emit_whose_commit_meets_a_store_fault_is_appended_exactly_once() {
    for fault in [Fault::FailBefore, Fault::AckHidden] {
        let script = Script::new();
        script.cut(CommitLabel::PROCESS_ADVANCE, 1, fault);
        let world = World::new(script).await;
        let process = world
            .register(
                registration(json!({ "tag": "emitter", "act": "emit", "event": EMITTED }))
                    .with_extra_event_types([event_type(EMITTED)]),
            )
            .await;
        world.nodes.start("a");
        world
            .until("the emitter ended", || async {
                world.terminal(&process).await.is_some()
            })
            .await;
        assert_eq!(
            world.nodes.script().cuts().len(),
            1,
            "{fault}: the emit's commit was cut"
        );
        let emitted = world
            .backend
            .process_registry()
            .full_event_window(&process, 0)
            .await
            .expect("read its events")
            .into_iter()
            .filter(|event| event.event_type == EMITTED)
            .count();
        assert_eq!(emitted, 1, "{fault}: the event is appended exactly once");
        assert_eq!(
            world.advances("emitter"),
            ["started", "emitted"],
            "{fault}: the engine saw its emit committed once"
        );
    }
}

/// An `Emit` the registry refuses, typed, is the process's recorded answer:
/// the process ends `Failed` with the refusal instead of retrying a commit
/// that can never land (the typed half of the store-fault laws, FIG-4649).
#[tokio::test]
async fn an_emit_the_registry_refuses_ends_the_process_failed_with_the_refusal() {
    let world = World::new(Script::new()).await;
    let process = world
        .register(registration(
            json!({ "tag": "undeclared", "act": "emit", "event": "l9d.undeclared" }),
        ))
        .await;
    world.nodes.start("a");
    world
        .until("the emitter ended", || async {
            world.terminal(&process).await.is_some()
        })
        .await;
    let record = world.record(&process).await.expect("the process");
    assert_eq!(record.status(), ProcessStatus::Failed, "{record:?}");
    let end = world.terminal(&process).await.expect("its end");
    assert!(
        end.to_string().contains("l9d.undeclared"),
        "the failure names the refused event type: {end}"
    );
    assert_eq!(world.advances("undeclared"), ["started"]);
}
