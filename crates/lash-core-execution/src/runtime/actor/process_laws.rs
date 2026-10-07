//! The laws of process actors (L6, FIG-5175; ADR 0132 §3, §10, §11),
//! written once over a [`Backend`] and run by each dialect's tests over its
//! own store. Each law takes a fresh backend, serves it with the production
//! [`ProcessActivation`] on a real runner, and returns the first rule it saw
//! broken. Time is the store's real clock: graces and deadlines are a few
//! hundred milliseconds.
//!
//! Every process runs [`LawEngine`], which notes each `advance` call under
//! the tag its start payload carries, so a law counts the engine code a
//! process ran.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use lash_durable::domain::{PROCESS_FORMATS, ParkEventKind, ParkEventRow, SIGNAL_MAIL};
use lash_durable::runner::{Runner, RunnerConfig, Stopped};
use lash_durable::{
    ActorKey, ActorState, CommitLabel, DurableError, FormatSet, MailKind, MailTx, NoProbe, NodeId,
    NodeSpec,
};
use lash_sansio::{ExecutionLimit, ExecutionPolicy};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::process::ProcessActivation;
use super::wait_laws::{LawBroken, LawResult};
use crate::runtime::actor::round::ToolBody;
use crate::runtime::process::steps::{ProcessSteps, StepAdmission, StepRefusal};
use crate::{
    Ancestry, Backend, BackendParts, CancelOrigin, CompletionKeySecrets, DurableSettings,
    EngineAction, EngineEvent, EngineState, EngineStateFormat, LifetimeDecision, ProcessEngine,
    ProcessId, ProcessInfraError, ProcessInput, ProcessOutcome, ProcessProvenance, ProcessRecord,
    ProcessRegistration, ProcessSignal, ProcessSignalIdentity, ScopeGrant, ScopeId, StepName,
    StepRequest, ToolCallId, ToolCallOutput, ToolCancellation,
};

macro_rules! ensure {
    ($condition:expr, $($message:tt)+) => {
        if !$condition {
            return Err(LawBroken(format!($($message)+)));
        }
    };
}

/// The kind every law process runs.
pub const LAW_ENGINE_KIND: &str = "law-process";

/// The claim poll every law's backend runs with.
const CLAIM_POLL: Duration = Duration::from_millis(25);

/// The cancel grace of [`LawEngine`].
const GRACE: Duration = Duration::from_millis(400);

/// How far past its due time a claimed process may act: scheduling slack
/// on a loaded test host.
const EPSILON: Duration = Duration::from_millis(750);

/// A wait deadline that passes during a law.
const SHORT: Duration = Duration::from_millis(300);

/// A wait deadline that never passes during a law.
const LONG: Duration = Duration::from_secs(60);

/// How long a law waits for the processes it drives to settle.
const SETTLE: Duration = Duration::from_secs(20);

/// The activation-loop budget of every law's backend.
const LOOP_BUDGET: u32 = 3;

/// The cascade batch of every law's backend: smaller than each scope's
/// children, so every cascade takes several batches.
const CASCADE_BATCH: usize = 2;

const NODE: &str = "process-laws";
const TTL_MILLIS: i64 = 15_000;

/// The settings every law's backend is assembled with: the defaults, with a
/// short claim poll, a small activation-loop budget and a small cascade
/// batch.
#[must_use]
pub fn settings() -> DurableSettings {
    let mut settings = DurableSettings::default();
    settings.lease.claim_poll = CLAIM_POLL;
    settings.activation_loop_budget = LOOP_BUDGET;
    settings.cascade_batch = CASCADE_BATCH;
    settings
}

/// `backend`'s store set with [`settings`] and `engines`.
fn with_engines(
    backend: &Backend,
    engines: Vec<Arc<dyn ProcessEngine>>,
) -> Result<Backend, LawBroken> {
    Backend::assemble(BackendParts {
        stores: backend.stores(),
        settings: settings(),
        secrets: Some(CompletionKeySecrets::for_testing()),
        engines,
        providers: Arc::new(super::projection::NoProjectionProviders),
    })
    .map_err(|error| LawBroken(error.to_string()))
}

/// `backend`'s store set serving [`LawEngine`] at state format version 0.
fn law_backend(backend: &Backend) -> Result<Backend, LawBroken> {
    with_engines(backend, vec![Arc::new(LawEngine { version: 0 })])
}

// --- the engine -------------------------------------------------------------

/// Every `advance` call any law made: the calling process's tag and the
/// event it was handed.
static ADVANCES: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

static NEXT_TAG: AtomicU64 = AtomicU64::new(0);

/// A tag no other process of this test binary carries.
fn tag(name: &str) -> String {
    format!("{name}-{}", NEXT_TAG.fetch_add(1, Ordering::Relaxed))
}

/// The events `tag`'s process was advanced with, in order. A transition
/// whose commit failed is recomputed from the same state and event, so a
/// repeat of the event just before is the same transition and is not
/// counted again.
fn advances(tag: &str) -> Vec<String> {
    let mut events: Vec<String> = ADVANCES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|(seen, _)| seen == tag)
        .map(|(_, event)| event.clone())
        .collect();
    events.dedup();
    events
}

fn note(tag: &str, event: &EngineEvent) {
    let event = match event {
        EngineEvent::Started { .. } => "started".to_owned(),
        EngineEvent::StepSettled { .. } => "step_settled".to_owned(),
        EngineEvent::Signal(_) => "signal".to_owned(),
        EngineEvent::Cancelled { origin, .. } => format!("cancelled:{}", origin_name(*origin)),
        EngineEvent::ProcessEnded { .. } => "process_ended".to_owned(),
        EngineEvent::ProcessWaitTimedOut { .. } => "process_wait_timed_out".to_owned(),
        other => format!("{other:?}"),
    };
    ADVANCES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((tag.to_owned(), event));
}

fn origin_name(origin: CancelOrigin) -> String {
    serde_json::to_value(origin)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The law engine. Its start payload is its state: `tag` names it in
/// [`ADVANCES`], and `act` says what it does:
///
/// - `hold`: idles until cancelled, then answers its cancel;
/// - `stuck`: runs one step whose body ignores its cancel token, and
///   ignores its own cancel, so lash forces its end at the grace;
/// - `await`: awaits process `await` with deadline `deadline_ms`, and ends
///   with what it saw;
/// - `await_signal`: idles until a signal names the process to await, then
///   behaves as `await`.
pub struct LawEngine {
    version: u32,
}

impl LawEngine {
    /// The law engine, writing its state at format `version`.
    #[must_use]
    pub fn new(version: u32) -> Self {
        Self { version }
    }
}

fn infra(error: impl std::fmt::Display) -> ProcessInfraError {
    ProcessInfraError::new(crate::PluginError::Session(error.to_string()))
}

fn await_action(script: &Value) -> Result<EngineAction, ProcessInfraError> {
    let target = script["await"]
        .as_str()
        .ok_or_else(|| infra("an await names no process"))?;
    Ok(EngineAction::AwaitProcess {
        process: ProcessId::parse(target).map_err(infra)?,
        deadline: script["deadline_ms"].as_u64().map(Duration::from_millis),
    })
}

fn ended(value: Value) -> EngineAction {
    EngineAction::Terminal(ProcessOutcome::from_tool_output(ToolCallOutput::success(
        value,
    )))
}

#[async_trait::async_trait]
impl ProcessEngine for LawEngine {
    fn kind(&self) -> &'static str {
        LAW_ENGINE_KIND
    }

    fn state_format(&self) -> EngineStateFormat {
        EngineStateFormat {
            kind: LAW_ENGINE_KIND.to_owned(),
            version: self.version,
        }
    }

    fn cancel_grace(&self) -> Duration {
        GRACE
    }

    fn program_identity(&self, _payload: &Value) -> Option<crate::ExecutableGeneration> {
        None
    }

    fn creation_config(
        &self,
        _env_spec: &crate::ProcessExecutionEnvSpec,
    ) -> Result<Option<Value>, crate::PluginError> {
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
        note(script["tag"].as_str().unwrap_or_default(), &event);
        let act = script["act"].as_str().unwrap_or("hold").to_owned();
        let action = match event {
            EngineEvent::Started { .. } => match act.as_str() {
                "stuck" => EngineAction::Steps(vec![StepRequest {
                    step: StepName("stuck".to_owned()),
                    tool: lash_sansio::ToolId::new("law_stuck"),
                    input: json!({}),
                }]),
                "await" => await_action(&script)?,
                _ => EngineAction::Idle,
            },
            EngineEvent::Signal(signal) if act == "await_signal" => {
                script["await"] = signal.payload["await"].clone();
                script["deadline_ms"] = signal.payload["deadline_ms"].clone();
                await_action(&script)?
            }
            EngineEvent::Cancelled { .. } if act == "stuck" => EngineAction::Idle,
            EngineEvent::Cancelled { origin, .. } => {
                EngineAction::Terminal(ProcessOutcome::from_tool_output(ToolCallOutput::cancelled(
                    ToolCancellation::runtime("the law engine answered its cancel")
                        .with_origin(origin),
                )))
            }
            EngineEvent::ProcessWaitTimedOut { process } => {
                ended(json!({ "timed_out": process.as_str() }))
            }
            EngineEvent::ProcessEnded { process, .. } => {
                ended(json!({ "ended": process.as_str() }))
            }
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
    ) -> Result<Vec<crate::ArtifactName>, crate::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &crate::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), crate::PluginError> {
        Err(crate::PluginError::Session(format!(
            "the law engine stores no artifact `{artifact_ref}`"
        )))
    }

    async fn resolve(
        &self,
        _reference: &crate::ProcessDefinitionRef,
    ) -> Result<crate::ProcessDefinitionResolution, crate::ProcessDefinitionRefusal> {
        Ok(crate::ProcessDefinitionResolution::new(
            crate::ProcessSignature::Unknown,
            Vec::new(),
        ))
    }
}

/// Every law step: a `Once` tool whose body never ends and ignores its
/// cancel token.
struct LawSteps;

impl ProcessSteps for LawSteps {
    fn admit(
        &self,
        _process: &ProcessRecord,
        _step: &StepRequest,
        now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal> {
        Ok(StepAdmission {
            policy: ExecutionPolicy::Once,
            limit: ExecutionLimit::starting_at(now_ms, LONG, LONG),
        })
    }

    fn body(&self, _process: &ProcessRecord, _step: &StepRequest, _call: &ToolCallId) -> ToolBody {
        Box::new(|_token| Box::pin(std::future::pending()))
    }
}

// --- the rig ----------------------------------------------------------------

/// A runner serving `backend`'s process actors until stopped.
struct Serving {
    stop: CancellationToken,
    task: tokio::task::JoinHandle<Result<Stopped, DurableError>>,
}

fn serve(backend: &Backend) -> Serving {
    let config = RunnerConfig::new(
        NodeId::new(NODE),
        vec![FormatSet::new(PROCESS_FORMATS)],
        backend.config(),
    );
    let activation = Arc::new(ProcessActivation::new(
        backend.clone(),
        Arc::new(LawSteps),
        Arc::new(NoProbe),
    ));
    let runner = Runner::new(
        Arc::clone(backend.durable()),
        backend.clock(),
        config,
        activation,
    )
    .with_hints(backend.hints().clone());
    let stop = CancellationToken::new();
    let until = stop.clone();
    let task = crate::task::spawn(async move { runner.run(until.cancelled_owned()).await });
    Serving { stop, task }
}

impl Serving {
    async fn stop(self) {
        self.stop.cancel();
        let _ = self.task.await;
    }
}

fn payload(tag: &str, act: &str) -> Value {
    json!({ "tag": tag, "act": act })
}

fn registration(payload: Value, lifetime: LifetimeDecision) -> ProcessRegistration {
    ProcessRegistration::new(
        ProcessInput::Engine {
            kind: LAW_ENGINE_KIND.to_owned(),
            payload,
        },
        ProcessProvenance::host(),
        lifetime,
    )
    .with_execution_env_ref(Some(crate::testing::process_execution_env_fixture_ref()))
}

async fn register(
    backend: &Backend,
    registration: ProcessRegistration,
) -> Result<ProcessId, LawBroken> {
    backend
        .process_registry()
        .register_process(registration)
        .await
        .map(|record| record.id)
        .map_err(|error| LawBroken(format!("registration refused: {error}")))
}

/// A detached process running `payload`.
async fn root(backend: &Backend, payload: Value) -> Result<ProcessId, LawBroken> {
    register(backend, registration(payload, LifetimeDecision::Detached)).await
}

/// A process running `payload` that lives `Until` `parent`.
async fn child(
    backend: &Backend,
    parent: &ProcessId,
    payload: Value,
) -> Result<ProcessId, LawBroken> {
    let scope = ScopeId::process(parent.clone());
    let mut registration = registration(
        payload,
        LifetimeDecision::Until {
            scope: scope.clone(),
            grant: ScopeGrant::Ancestor,
        },
    );
    registration.ancestry = Ancestry::from_scopes([scope]);
    register(backend, registration).await
}

async fn record(backend: &Backend, process: &ProcessId) -> Result<ProcessRecord, LawBroken> {
    backend
        .process_registry()
        .get_process(process)
        .await
        .map_err(|error| LawBroken(error.to_string()))?
        .ok_or_else(|| LawBroken(format!("process {process} is not registered")))
}

/// `process`'s terminal outcome, encoded, once it has one.
async fn terminal(backend: &Backend, process: &ProcessId) -> Result<Option<Value>, LawBroken> {
    Ok(record(backend, process).await?.terminal().map(|terminal| {
        serde_json::to_value(terminal.clone().into_await_output()).unwrap_or(Value::Null)
    }))
}

fn actor(process: &ProcessId) -> Result<ActorKey, LawBroken> {
    ActorKey::process(process.as_str()).map_err(|error| LawBroken(error.to_string()))
}

async fn actor_state(
    backend: &Backend,
    process: &ProcessId,
) -> Result<Option<ActorState>, LawBroken> {
    Ok(backend
        .durable()
        .actor(&actor(process)?)
        .await?
        .map(|snapshot| snapshot.state))
}

async fn cancel(backend: &Backend, process: &ProcessId) -> LawResult {
    backend
        .process_registry()
        .request_process_cancel(
            process,
            CancelOrigin::OperatorRequested,
            "process-laws".to_owned(),
            None,
        )
        .await
        .map(drop)
        .map_err(|error| LawBroken(format!("the cancel of {process} was refused: {error}")))
}

/// Poll `check` until it answers `true`, for at most `within`.
async fn eventually<F, Fut>(within: Duration, what: &str, mut check: F) -> LawResult
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool, LawBroken>>,
{
    let started = Instant::now();
    loop {
        if check().await? {
            return Ok(());
        }
        ensure!(started.elapsed() < within, "{what} within {within:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn ended_all(backend: &Backend, processes: &[ProcessId]) -> Result<bool, LawBroken> {
    for process in processes {
        if terminal(backend, process).await?.is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `process`'s actor waits for mail, its engine having been advanced.
async fn settled_waiting(backend: &Backend, process: &ProcessId) -> Result<bool, LawBroken> {
    Ok(
        actor_state(backend, process).await? == Some(ActorState::Waiting)
            && backend
                .durable()
                .process(process)
                .await?
                .is_some_and(|row| row.state_rev > 0),
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

/// The cancel origin and forced flag `outcome` carries, when it is a
/// cancellation.
fn cancellation(outcome: &Value) -> Option<(String, bool)> {
    let origin = find(outcome, "origin")?.as_str()?.to_owned();
    let forced = find(outcome, "forced")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some((origin, forced))
}

/// Claim actors on fresh boots of the law node without committing, until
/// `process`'s failed-activation count reaches `count`: crashed claims.
/// Every claim must take only `process`.
async fn crash_claims(backend: &Backend, process: &ProcessId, count: u32) -> LawResult {
    let key = actor(process)?;
    // The first claim has no earlier claim to compare with: it counts none.
    for _ in 0..=count + 1 {
        let failed = backend
            .durable()
            .actor(&key)
            .await?
            .map_or(0, |snapshot| snapshot.failed_activations);
        if failed >= count {
            return Ok(());
        }
        // Registering the node again fences its earlier boot and readies
        // what that boot owned: the crash.
        let lease = backend
            .durable()
            .register_node(&NodeSpec {
                node: NodeId::new(NODE),
                decodes: vec![FormatSet::new(PROCESS_FORMATS)],
                ttl_millis: TTL_MILLIS,
            })
            .await?;
        let claimed = backend.durable().claim(&lease, 16).await?;
        ensure!(
            claimed.iter().map(|claimed| &claimed.actor).eq([&key]),
            "a crashed claim took {:?}, not only {key}",
            claimed
                .iter()
                .map(|claimed| claimed.actor.to_string())
                .collect::<Vec<_>>()
        );
    }
    Err(LawBroken(format!(
        "{count} crashed claims did not count {count} failed activations"
    )))
}

/// The park-feed entries of `process`'s actor, oldest first.
async fn park_feed(backend: &Backend, process: &ProcessId) -> Result<Vec<ParkEventRow>, LawBroken> {
    let key = actor(process)?;
    Ok(backend
        .durable()
        .park_events(None, 1_000)
        .await?
        .into_iter()
        .filter(|row| row.actor == key)
        .collect())
}

fn reason(row: &ParkEventRow) -> String {
    serde_json::from_str::<Value>(&row.reason_json)
        .ok()
        .and_then(|value| {
            find(&value, "reason")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

// --- the laws ---------------------------------------------------------------

/// C1 (FIG-5160), parked: P's `Until` child A is parked with
/// `ActivationLoop`, and A has an `Until` child B. Cancelling P ends A
/// `Cancelled` with zero `advance` calls, and B receives `ParentEnded` and
/// ends. A cancel alone ends a park: the feed records A's park and its end.
///
/// # Errors
///
/// The first rule broken.
pub async fn c1_a_parked_child_ends_engine_free_and_its_child_receives_parent_ended(
    backend: &Backend,
) -> LawResult {
    let backend = law_backend(backend)?;
    let (p_tag, a_tag, b_tag) = (tag("c1p"), tag("c1a"), tag("c1b"));
    let p = root(&backend, payload(&p_tag, "hold")).await?;
    let serving = serve(&backend);
    eventually(SETTLE, "P started and waits", || {
        settled_waiting(&backend, &p)
    })
    .await?;
    serving.stop().await;
    let a = child(&backend, &p, payload(&a_tag, "hold")).await?;
    crash_claims(&backend, &a, LOOP_BUDGET - 1).await?;
    let b = child(&backend, &a, payload(&b_tag, "hold")).await?;
    let serving = serve(&backend);
    eventually(SETTLE, "A parked and B waits", || async {
        Ok(actor_state(&backend, &a).await? == Some(ActorState::Parked)
            && settled_waiting(&backend, &b).await?)
    })
    .await?;
    ensure!(
        advances(&a_tag).is_empty(),
        "parked A ran its engine: {:?}",
        advances(&a_tag)
    );
    cancel(&backend, &p).await?;
    let tree = [p.clone(), a.clone(), b.clone()];
    eventually(SETTLE, "P, A and B ended", || ended_all(&backend, &tree)).await?;
    serving.stop().await;
    ensure!(
        advances(&a_tag).is_empty(),
        "cancelling parked A ran its engine: {:?}",
        advances(&a_tag)
    );
    let a_end = terminal(&backend, &a).await?.unwrap_or_default();
    ensure!(
        cancellation(&a_end) == Some(("parent_ended".to_owned(), false)),
        "A did not end cancelled by its parent: {a_end}"
    );
    ensure!(
        advances(&b_tag) == ["started", "cancelled:parent_ended"],
        "B was not advanced with ParentEnded: {:?}",
        advances(&b_tag)
    );
    let b_end = terminal(&backend, &b).await?.unwrap_or_default();
    ensure!(
        cancellation(&b_end).is_some_and(|(origin, _)| origin == "parent_ended"),
        "B did not end cancelled by its parent: {b_end}"
    );
    let feed = park_feed(&backend, &a).await?;
    ensure!(
        feed.iter()
            .map(|row| row.kind)
            .eq([ParkEventKind::Parked, ParkEventKind::Ended])
            && reason(&feed[0]) == "activation_loop",
        "A's park feed is not its activation-loop park and its end: {feed:?}"
    );
    Ok(())
}

/// C1 (FIG-5160), waiting: P's `Until` child A waits, with its own `Until`
/// child B. Cancelling P ends A `Cancelled` by its grace plus ε, with one
/// `advance` after the cancel, which receives `Cancelled`; B ends too.
///
/// # Errors
///
/// The first rule broken.
pub async fn c1_a_waiting_child_ends_within_its_grace_with_one_cancelled_advance(
    backend: &Backend,
) -> LawResult {
    let backend = law_backend(backend)?;
    let (p_tag, a_tag, b_tag) = (tag("c1wp"), tag("c1wa"), tag("c1wb"));
    let serving = serve(&backend);
    let p = root(&backend, payload(&p_tag, "hold")).await?;
    let a = child(&backend, &p, payload(&a_tag, "hold")).await?;
    let b = child(&backend, &a, payload(&b_tag, "hold")).await?;
    eventually(SETTLE, "P, A and B wait", || async {
        Ok(settled_waiting(&backend, &p).await?
            && settled_waiting(&backend, &a).await?
            && settled_waiting(&backend, &b).await?)
    })
    .await?;
    let before = advances(&a_tag).len();
    let cancelled_at = Instant::now();
    cancel(&backend, &p).await?;
    eventually(GRACE + EPSILON, "A ended by its grace", || async {
        Ok(terminal(&backend, &a).await?.is_some())
    })
    .await?;
    let elapsed = cancelled_at.elapsed();
    let rest = [p.clone(), b.clone()];
    eventually(SETTLE, "P and B ended", || ended_all(&backend, &rest)).await?;
    serving.stop().await;
    let after = advances(&a_tag)[before..].to_vec();
    ensure!(
        after == ["cancelled:parent_ended"],
        "A was advanced {after:?} after the cancel, not once with Cancelled"
    );
    ensure!(
        elapsed <= GRACE + EPSILON,
        "A ended {elapsed:?} after the cancel, past its grace"
    );
    for (name, process) in [("A", &a), ("B", &b)] {
        let end = terminal(&backend, process).await?.unwrap_or_default();
        ensure!(
            cancellation(&end).is_some_and(|(origin, _)| origin == "parent_ended"),
            "{name} did not end cancelled by its parent: {end}"
        );
    }
    Ok(())
}

/// C2: a process whose step ignores its cancel token, and whose engine
/// ignores its cancel, ends `Cancelled { forced: true }` by its grace
/// plus ε.
///
/// # Errors
///
/// The first rule broken.
pub async fn c2_a_cancel_the_engine_ignores_is_forced_at_its_grace(backend: &Backend) -> LawResult {
    let backend = law_backend(backend)?;
    let stuck_tag = tag("c2");
    let serving = serve(&backend);
    let process = root(&backend, payload(&stuck_tag, "stuck")).await?;
    if let Err(broken) = eventually(SETTLE, "the stuck step started", || async {
        Ok(advances(&stuck_tag)
            .first()
            .is_some_and(|event| event == "started")
            && backend
                .durable()
                .process(&process)
                .await?
                .is_some_and(|row| row.state_rev > 0))
    })
    .await
    {
        let snapshot = backend.durable().actor(&actor(&process)?).await?;
        let row = backend.durable().process(&process).await?;
        return Err(LawBroken(format!(
            "{broken}: advances {:?}, actor {snapshot:?}, row {row:?}, terminal {:?}",
            advances(&stuck_tag),
            terminal(&backend, &process).await?
        )));
    }
    // The step's body is running in this activation: nothing settles it
    // before the cancel.
    tokio::time::sleep(GRACE / 4).await;
    ensure!(
        advances(&stuck_tag) == ["started"],
        "a step was settled while its body still ran: {:?}",
        advances(&stuck_tag)
    );
    let cancelled_at = Instant::now();
    cancel(&backend, &process).await?;
    eventually(
        GRACE + EPSILON,
        "the process ended by its grace",
        || async { Ok(terminal(&backend, &process).await?.is_some()) },
    )
    .await?;
    let elapsed = cancelled_at.elapsed();
    serving.stop().await;
    let end = terminal(&backend, &process).await?.unwrap_or_default();
    ensure!(
        cancellation(&end) == Some(("operator_requested".to_owned(), true)),
        "the process did not end forced: {end}"
    );
    ensure!(
        elapsed >= GRACE,
        "the process was forced {elapsed:?} after the cancel, before its grace"
    );
    ensure!(
        advances(&stuck_tag)
            .iter()
            .filter(|event| event.starts_with("cancelled"))
            .count()
            == 1,
        "Cancelled was not delivered exactly once: {:?}",
        advances(&stuck_tag)
    );
    Ok(())
}

/// W1: `AwaitProcess` times out at its deadline; cancelling the awaiter
/// ends it and revokes its wait, leaving its target running.
///
/// # Errors
///
/// The first rule broken.
pub async fn w1_await_process_times_out_and_its_awaiters_cancel_ends_it(
    backend: &Backend,
) -> LawResult {
    let backend = law_backend(backend)?;
    let serving = serve(&backend);
    let target = root(&backend, payload(&tag("w1t"), "hold")).await?;
    let mut bounded = payload(&tag("w1b"), "await");
    bounded["await"] = json!(target.as_str());
    bounded["deadline_ms"] = json!(u64::try_from(SHORT.as_millis()).unwrap_or(u64::MAX));
    let started = Instant::now();
    let bounded = root(&backend, bounded).await?;
    let mut unbounded = payload(&tag("w1u"), "await");
    unbounded["await"] = json!(target.as_str());
    unbounded["deadline_ms"] = json!(u64::try_from(LONG.as_millis()).unwrap_or(u64::MAX));
    let unbounded = root(&backend, unbounded).await?;
    eventually(SHORT + EPSILON, "the bounded await timed out", || async {
        Ok(terminal(&backend, &bounded).await?.is_some())
    })
    .await?;
    let elapsed = started.elapsed();
    let end = terminal(&backend, &bounded).await?.unwrap_or_default();
    ensure!(
        find(&end, "timed_out").and_then(Value::as_str) == Some(target.as_str()),
        "the bounded await did not end timed out: {end}"
    );
    ensure!(
        elapsed >= SHORT,
        "the await timed out after {elapsed:?}, before its deadline"
    );
    eventually(SETTLE, "the unbounded awaiter waits", || {
        settled_waiting(&backend, &unbounded)
    })
    .await?;
    cancel(&backend, &unbounded).await?;
    eventually(SETTLE, "the cancelled awaiter ended", || async {
        Ok(terminal(&backend, &unbounded).await?.is_some())
    })
    .await?;
    let pending = backend.durable().pending_waits(&actor(&unbounded)?).await?;
    serving.stop().await;
    let end = terminal(&backend, &unbounded).await?.unwrap_or_default();
    ensure!(
        cancellation(&end).is_some_and(|(origin, _)| origin == "operator_requested"),
        "the awaiter did not end cancelled: {end}"
    );
    ensure!(
        pending.is_empty(),
        "the cancelled awaiter left waits pending: {pending:?}"
    );
    ensure!(
        terminal(&backend, &target).await?.is_none(),
        "cancelling an awaiter ended its target"
    );
    Ok(())
}

/// Send `signal`'s payload to `process` as its mail.
async fn signal(backend: &Backend, process: &ProcessId, payload: Value) -> LawResult {
    let identity = ProcessSignalIdentity::new(process.clone(), "await", tag("signal"))
        .map_err(|error| LawBroken(error.to_string()))?;
    let body = serde_json::to_string(&ProcessSignal::new(identity, payload))
        .map_err(|error| LawBroken(error.to_string()))?;
    let mut tx = MailTx::new();
    tx.append(actor(process)?, MailKind::new(SIGNAL_MAIL), body);
    backend.commit_mail(tx, CommitLabel::MAIL_PROCESS).await?;
    Ok(())
}

/// W1: an A↔B await cycle is bounded, each side timing out, and is
/// cancellable: cancelling one side ends it and resolves the other's wait.
///
/// # Errors
///
/// The first rule broken.
pub async fn w1_an_await_cycle_times_out_on_each_side_and_is_cancellable(
    backend: &Backend,
) -> LawResult {
    let backend = law_backend(backend)?;
    let serving = serve(&backend);
    let short = u64::try_from(SHORT.as_millis()).unwrap_or(u64::MAX);
    let long = u64::try_from(LONG.as_millis()).unwrap_or(u64::MAX);
    let a = root(&backend, payload(&tag("w1a"), "await_signal")).await?;
    let b = root(&backend, payload(&tag("w1b"), "await_signal")).await?;
    signal(
        &backend,
        &a,
        json!({ "await": b.as_str(), "deadline_ms": short }),
    )
    .await?;
    signal(
        &backend,
        &b,
        json!({ "await": a.as_str(), "deadline_ms": short }),
    )
    .await?;
    let bounded = [a.clone(), b.clone()];
    eventually(SETTLE, "both sides of the bounded cycle ended", || {
        ended_all(&backend, &bounded)
    })
    .await?;
    for (side, other) in [(&a, &b), (&b, &a)] {
        let end = terminal(&backend, side).await?.unwrap_or_default();
        ensure!(
            find(&end, "timed_out").and_then(Value::as_str) == Some(other.as_str()),
            "a side of the bounded cycle did not time out: {end}"
        );
    }
    let c = root(&backend, payload(&tag("w1c"), "await_signal")).await?;
    let d = root(&backend, payload(&tag("w1d"), "await_signal")).await?;
    signal(
        &backend,
        &c,
        json!({ "await": d.as_str(), "deadline_ms": long }),
    )
    .await?;
    signal(
        &backend,
        &d,
        json!({ "await": c.as_str(), "deadline_ms": long }),
    )
    .await?;
    eventually(SETTLE, "both sides of the unbounded cycle wait", || async {
        Ok(!backend
            .durable()
            .pending_waits(&actor(&c)?)
            .await?
            .is_empty()
            && !backend
                .durable()
                .pending_waits(&actor(&d)?)
                .await?
                .is_empty())
    })
    .await?;
    cancel(&backend, &c).await?;
    let cancelled = [c.clone(), d.clone()];
    eventually(SETTLE, "both sides of the cancelled cycle ended", || {
        ended_all(&backend, &cancelled)
    })
    .await?;
    serving.stop().await;
    let c_end = terminal(&backend, &c).await?.unwrap_or_default();
    ensure!(
        cancellation(&c_end).is_some_and(|(origin, _)| origin == "operator_requested"),
        "the cancelled side did not end cancelled: {c_end}"
    );
    let d_end = terminal(&backend, &d).await?.unwrap_or_default();
    ensure!(
        find(&d_end, "ended").and_then(Value::as_str) == Some(c.as_str()),
        "the other side did not see the cancelled side end: {d_end}"
    );
    Ok(())
}

/// Engine-free end: a process cancelled before it started, one whose engine
/// this node does not have, and one whose state this node's engine cannot
/// decode each end `Cancelled` without engine code. The parked ones were
/// parked with their typed reason, and the cancel ended the park.
///
/// # Errors
///
/// The first rule broken.
pub async fn engine_free_end_runs_no_engine_code(backend: &Backend) -> LawResult {
    let backend = law_backend(backend)?;
    let unstarted_tag = tag("free-unstarted");
    let unstarted = root(&backend, payload(&unstarted_tag, "hold")).await?;
    cancel(&backend, &unstarted).await?;
    let mut unknown = registration(
        payload(&tag("free-unknown"), "hold"),
        LifetimeDecision::Detached,
    );
    unknown.input = Arc::new(ProcessInput::Engine {
        kind: "law-missing".to_owned(),
        payload: json!({}),
    });
    let unknown = register(&backend, unknown).await?;
    let undecodable_tag = tag("free-undecodable");
    let undecodable = root(&backend, payload(&undecodable_tag, "hold")).await?;
    let serving = serve(&backend);
    eventually(
        SETTLE,
        "the unstarted process ended and the others settled",
        || async {
            Ok(terminal(&backend, &unstarted).await?.is_some()
                && actor_state(&backend, &unknown).await? == Some(ActorState::Parked)
                && settled_waiting(&backend, &undecodable).await?)
        },
    )
    .await?;
    serving.stop().await;
    ensure!(
        advances(&unstarted_tag).is_empty(),
        "a process cancelled before it started ran its engine: {:?}",
        advances(&unstarted_tag)
    );
    // The next node's engine writes a state format the stored state is not in.
    let newer = with_engines(&backend, vec![Arc::new(LawEngine { version: 1 })])?;
    let serving = serve(&newer);
    newer.wake_process(&undecodable).await?;
    eventually(SETTLE, "the undecodable process parked", || async {
        Ok(actor_state(&newer, &undecodable).await? == Some(ActorState::Parked))
    })
    .await?;
    let before = advances(&undecodable_tag);
    cancel(&newer, &unknown).await?;
    cancel(&newer, &undecodable).await?;
    let parked = [unknown.clone(), undecodable.clone()];
    eventually(SETTLE, "the parked processes ended", || {
        ended_all(&newer, &parked)
    })
    .await?;
    serving.stop().await;
    ensure!(
        advances(&undecodable_tag) == before,
        "the undecodable process ran engine code: {:?}",
        advances(&undecodable_tag)
    );
    for (name, process, parked_for) in [
        ("unknown", &unknown, "unknown_engine"),
        ("undecodable", &undecodable, "undecodable_state"),
    ] {
        let end = terminal(&newer, process).await?.unwrap_or_default();
        ensure!(
            cancellation(&end) == Some(("operator_requested".to_owned(), false)),
            "the {name} process did not end cancelled: {end}"
        );
        let feed = park_feed(&newer, process).await?;
        ensure!(
            feed.iter()
                .map(|row| row.kind)
                .eq([ParkEventKind::Parked, ParkEventKind::Ended])
                && reason(&feed[0]) == parked_for,
            "the {name} process's park feed is not its {parked_for} park and its end: {feed:?}"
        );
    }
    let end = terminal(&newer, &unstarted).await?.unwrap_or_default();
    ensure!(
        cancellation(&end) == Some(("operator_requested".to_owned(), false)),
        "the unstarted process did not end cancelled: {end}"
    );
    Ok(())
}

/// P1: a process whose claims crash without progress parks with
/// `ActivationLoop` at its budget, before running engine code; an
/// operator's redrive resumes it. A process whose crashed claims are
/// interrupted by progress never parks, however many crashed in total.
///
/// # Errors
///
/// The first rule broken.
pub async fn p1_a_crash_loop_parks_at_its_budget_and_progress_resets_the_count(
    backend: &Backend,
) -> LawResult {
    let backend = law_backend(backend)?;
    let looping_tag = tag("p1-loop");
    let looping = root(&backend, payload(&looping_tag, "hold")).await?;
    crash_claims(&backend, &looping, LOOP_BUDGET - 1).await?;
    let serving = serve(&backend);
    eventually(SETTLE, "the looping process parked", || async {
        Ok(actor_state(&backend, &looping).await? == Some(ActorState::Parked))
    })
    .await?;
    serving.stop().await;
    ensure!(
        advances(&looping_tag).is_empty(),
        "a process at its activation-loop budget ran its engine: {:?}",
        advances(&looping_tag)
    );
    let feed = park_feed(&backend, &looping).await?;
    ensure!(
        feed.len() == 1 && reason(&feed[0]) == "activation_loop",
        "the looping process's park feed is not one activation-loop park: {feed:?}"
    );
    ensure!(
        backend.redrive_process(&looping, "process-laws").await?,
        "the redrive did not find the process parked"
    );
    let serving = serve(&backend);
    eventually(SETTLE, "the redriven process started and waits", || {
        settled_waiting(&backend, &looping)
    })
    .await?;
    serving.stop().await;
    ensure!(
        advances(&looping_tag) == ["started"],
        "the redriven process was not advanced once with Started: {:?}",
        advances(&looping_tag)
    );

    let progressing_tag = tag("p1-progress");
    let progressing = root(&backend, payload(&progressing_tag, "hold")).await?;
    crash_claims(&backend, &progressing, LOOP_BUDGET - 2).await?;
    let serving = serve(&backend);
    eventually(SETTLE, "the progressing process started", || {
        settled_waiting(&backend, &progressing)
    })
    .await?;
    serving.stop().await;
    backend.wake_process(&progressing).await?;
    crash_claims(&backend, &progressing, LOOP_BUDGET - 2).await?;
    let serving = serve(&backend);
    eventually(SETTLE, "the progressing process waits again", || {
        settled_waiting(&backend, &progressing)
    })
    .await?;
    serving.stop().await;
    ensure!(
        park_feed(&backend, &progressing).await?.is_empty(),
        "a process whose crashes were interrupted by progress parked"
    );
    Ok(())
}

/// The batched cascade: a tree three levels below its root, every scope
/// with more `Until` children than one cascade batch marks, ends fully
/// when its root is cancelled, every descendant `ParentEnded`.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_cascade_wider_than_its_batch_ends_a_tree_three_levels_deep(
    backend: &Backend,
) -> LawResult {
    const WIDTH: usize = CASCADE_BATCH + 1;
    let backend = law_backend(backend)?;
    let serving = serve(&backend);
    let root_process = root(&backend, payload(&tag("tree"), "hold")).await?;
    let mut levels: Vec<Vec<ProcessId>> = vec![vec![root_process.clone()]];
    for _ in 0..3 {
        let mut level = Vec::new();
        for parent in levels.last().into_iter().flatten() {
            for _ in 0..WIDTH {
                level.push(child(&backend, parent, payload(&tag("tree"), "hold")).await?);
            }
        }
        levels.push(level);
    }
    let everyone: Vec<ProcessId> = levels.iter().flatten().cloned().collect();
    eventually(SETTLE, "the tree started", || async {
        for process in &everyone {
            if !settled_waiting(&backend, process).await? {
                return Ok(false);
            }
        }
        Ok(true)
    })
    .await?;
    cancel(&backend, &root_process).await?;
    eventually(SETTLE, "the whole tree ended", || {
        ended_all(&backend, &everyone)
    })
    .await?;
    eventually(SETTLE, "every actor of the tree is terminal", || async {
        for process in &everyone {
            if actor_state(&backend, process).await? != Some(ActorState::Terminal) {
                return Ok(false);
            }
        }
        Ok(true)
    })
    .await?;
    serving.stop().await;
    // The actors ran the cascade: no ended scope left a relay anything to
    // publish or cascade.
    let registry = backend.process_registry();
    for process in &everyone {
        let owed = |error: crate::PluginError| LawBroken(error.to_string());
        ensure!(
            registry
                .terminal_publication(process)
                .await
                .map_err(owed)?
                .is_none(),
            "{process}'s terminal armed a terminal publication"
        );
        let plan = registry
            .get_parent_end_plan(&ScopeId::process(process.clone()))
            .await
            .map_err(owed)?;
        ensure!(
            plan.as_ref().is_some_and(|plan| {
                plan.obligation_state == crate::store::ObligationState::Delivered
            }),
            "{process}'s scope left no settled parent-end fence: {plan:?}"
        );
    }
    let mut origins = BTreeMap::<String, usize>::new();
    for process in &everyone[1..] {
        let end = terminal(&backend, process).await?.unwrap_or_default();
        let origin = cancellation(&end).map_or_else(|| end.to_string(), |(origin, _)| origin);
        *origins.entry(origin).or_default() += 1;
    }
    ensure!(
        origins.len() == 1 && origins.get("parent_ended") == Some(&(everyone.len() - 1)),
        "the descendants did not all end ParentEnded: {origins:?}"
    );
    Ok(())
}
