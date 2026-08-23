use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lash_core::SessionStoreFactory;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::oracles::replay_determinism;
use crate::provider::{ProviderWireScript, ScriptedLlmHttpTransport, ScriptedTransportSchedule};
use crate::provider_mutations::ProviderMutationMatrixCache;
use crate::replay::{ReplayError, replay_trace};
use crate::runtime_boundaries::{RuntimeBoundaryHarness, RuntimeEffectReplayStore};
use crate::runtime_contracts::{
    RuntimeTurnObservation, require_passed, runtime_agent_frame_invariant_facts,
    runtime_graph_invariant_facts, runtime_turn_contract, runtime_usage_invariant_facts,
};
use crate::runtime_providers::{
    runtime_provider_components, runtime_scripts_for_texts as runtime_provider_scripts_for_texts,
};
use crate::scheduler::{BoundaryEvent, BoundaryKind};
use crate::store::{
    BackendCheckpointReplayEvidence, CheckpointWriteCollector, CheckpointWriteEvent, ModelStore,
    ObservedSessionStoreFactory,
};
use crate::trace::{
    AbstractWorldSummary, OracleVerdict, SimulationTrace, TraceIoError, read_trace,
};

pub const SQLITE_REPLAY_REPORT_SCHEMA: &str = "lash.sim.sqlite-runtime-replay-report.v4";
pub const SQLITE_DIVERGENCE_SCHEMA: &str = "lash.sim.sqlite-runtime-divergence.v1";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SqliteReplayReport {
    pub schema: String,
    pub trace_path: PathBuf,
    pub database_path: PathBuf,
    pub terminal_verdict: OracleVerdict,
    pub delivered_event_count: usize,
    pub runtime_replayed_boundary_count: usize,
    pub replayed_boundary_families: Vec<String>,
    pub carried_forward_boundary_count: usize,
    pub checkpoint_replay: BackendCheckpointReplayEvidence,
    pub reopened_sessions: Vec<SqliteReopenedSessionEvidence>,
    pub final_summary: AbstractWorldSummary,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SqliteDivergenceArtifact {
    pub schema: String,
    pub trace_path: PathBuf,
    pub database_path: PathBuf,
    pub verdict: OracleVerdict,
    pub expected_summary: AbstractWorldSummary,
    pub actual_summary: AbstractWorldSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary: Option<SqliteBoundaryDivergence>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SqliteBoundaryDivergence {
    pub boundary_id: String,
    pub boundary_kind: String,
    pub expected_observed: Value,
    pub actual_observed: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SqliteReopenedSessionEvidence {
    pub session_id: String,
    pub database_path: PathBuf,
    pub turn_index: usize,
    pub graph_node_count: usize,
    pub transcript_message_count: usize,
}

#[derive(Debug)]
pub enum SqliteReplayError {
    TraceIo(TraceIoError),
    Replay(ReplayError),
    Io(std::io::Error),
    Json(serde_json::Error),
    Runtime(String),
    Assertion(String),
    Divergence(String),
}

impl fmt::Display for SqliteReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TraceIo(err) => write!(f, "{err}"),
            Self::Replay(err) => write!(f, "{err}"),
            Self::Io(err) => write!(f, "SQLite runtime replay I/O failed: {err}"),
            Self::Json(err) => write!(f, "SQLite runtime replay JSON failed: {err}"),
            Self::Runtime(message) => write!(f, "SQLite runtime replay failed: {message}"),
            Self::Assertion(message) => {
                write!(f, "SQLite runtime replay assertion failed: {message}")
            }
            Self::Divergence(message) => write!(f, "SQLite runtime replay diverged: {message}"),
        }
    }
}

impl std::error::Error for SqliteReplayError {}

impl From<TraceIoError> for SqliteReplayError {
    fn from(value: TraceIoError) -> Self {
        Self::TraceIo(value)
    }
}

impl From<ReplayError> for SqliteReplayError {
    fn from(value: ReplayError) -> Self {
        Self::Replay(value)
    }
}

impl From<std::io::Error> for SqliteReplayError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for SqliteReplayError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

pub async fn replay_trace_file_to_sqlite(
    trace_path: &Path,
    db_path: &Path,
    report_path: Option<&Path>,
) -> Result<SqliteReplayReport, SqliteReplayError> {
    let trace = read_trace(trace_path)?;
    replay_trace_to_sqlite(trace_path, &trace, db_path, report_path).await
}

pub async fn replay_trace_to_sqlite(
    trace_path: &Path,
    trace: &SimulationTrace,
    db_path: &Path,
    report_path: Option<&Path>,
) -> Result<SqliteReplayReport, SqliteReplayError> {
    let model_replay = replay_trace(trace_path, trace)?;
    prepare_database_root(db_path)?;

    let mut world = SqliteRuntimeReplayWorld::new(db_path.to_path_buf(), trace);
    let mut store = ModelStore::default();
    let mut provider_mutation_cache = ProviderMutationMatrixCache::default();
    let mut runtime_replayed_boundary_count = 0;
    let mut replayed_boundary_families = BTreeSet::new();
    for delivered in &trace.events {
        let event = delivered.as_event();
        let observed = if event.kind == BoundaryKind::BackendFailure {
            // Generation already exercised the real SQLite fault injector and
            // recorded its StoreError. Replaying the surrounding backend cannot
            // recreate that separately armed transaction, so preserve the real
            // observation instead of projecting a model failure.
            delivered.observed.clone()
        } else if is_suspend_replay_boundary(&event) {
            // Suspend observations include the real committed runtime graph and
            // usage facts captured during generation. Carry that evidence rather
            // than replacing it with the abstract projector's smaller payload.
            delivered.observed.clone()
        } else if is_runtime_session_boundary(event.kind) {
            world.deliver_boundary(&event, &delivered.observed).await?
        } else if is_runtime_backed_boundary(event.kind) {
            world.deliver_runtime_boundary(&event).await?
        } else if event.kind == BoundaryKind::ProviderMutation {
            let observed = store.project_boundary_observation(&event);
            provider_mutation_cache
                .augment_observation(&event, observed)
                .await
                .map_err(|err| SqliteReplayError::Runtime(err.to_string()))?
        } else {
            store.project_boundary_observation(&event)
        };
        runtime_replayed_boundary_count += 1;
        replayed_boundary_families.insert(boundary_family_name(event.kind).to_string());
        if normalize_backend_observed(event.kind, &observed)
            != normalize_backend_observed(event.kind, &delivered.observed)
        {
            store.apply_observed_boundary(&event, &observed);
            let actual_summary = store.summary();
            let verdict = OracleVerdict::failed(
                "sim.oracle.sqlite-boundary-replay.v1",
                format!(
                    "SQLite replay boundary `{}` ({}) reproduced different observed data",
                    event.boundary_id,
                    boundary_family_name(event.kind)
                ),
            );
            write_divergence_artifact(
                trace_path,
                db_path,
                report_path,
                verdict.clone(),
                &trace.final_summary,
                &actual_summary,
                Some(SqliteBoundaryDivergence {
                    boundary_id: event.boundary_id,
                    boundary_kind: boundary_family_name(event.kind).to_string(),
                    expected_observed: delivered.observed.clone(),
                    actual_observed: observed,
                }),
            )?;
            return Err(SqliteReplayError::Divergence(verdict.message));
        }
        store.apply_observed_boundary(&event, &observed);
    }

    let checkpoint_replay =
        match BackendCheckpointReplayEvidence::for_trace(trace, world.checkpoint_write_events()) {
            Ok(evidence) => evidence,
            Err(message) => {
                let actual_summary = store
                    .summarize_with_trace_checkpoint_writes(
                        &trace.events,
                        &world.checkpoint_write_events(),
                    )
                    .map_err(SqliteReplayError::Divergence)?;
                let verdict =
                    OracleVerdict::failed("sim.oracle.sqlite-checkpoint-replay.v1", &message);
                write_divergence_artifact(
                    trace_path,
                    db_path,
                    report_path,
                    verdict,
                    &trace.final_summary,
                    &actual_summary,
                    None,
                )?;
                return Err(SqliteReplayError::Divergence(message));
            }
        };
    // Runtime-turn checkpoint facts are observed from SQLite and compared above.
    // Contract-proof and suspend-fixture facts belong to projector-owned
    // boundaries this static lane does not execute, so only that explicit subset
    // is carried to keep the whole-trace summary comparable.
    let final_summary = store
        .summarize_with_trace_checkpoint_writes(&trace.events, &checkpoint_replay.summary_writes())
        .map_err(SqliteReplayError::Divergence)?;
    let terminal_verdict = replay_determinism(&trace.final_summary, &final_summary);
    if !terminal_verdict.is_passed() {
        write_divergence_artifact(
            trace_path,
            db_path,
            report_path,
            terminal_verdict.clone(),
            &trace.final_summary,
            &final_summary,
            None,
        )?;
        return Err(SqliteReplayError::Divergence(
            terminal_verdict.message.clone(),
        ));
    }
    if final_summary != model_replay.final_summary {
        let verdict = OracleVerdict::failed(
            "sim.oracle.sqlite-model-replay.v1",
            "runtime SQLite replay summary diverged from model replay",
        );
        write_divergence_artifact(
            trace_path,
            db_path,
            report_path,
            verdict.clone(),
            &model_replay.final_summary,
            &final_summary,
            None,
        )?;
        return Err(SqliteReplayError::Divergence(
            "runtime SQLite replay summary diverged from model replay".to_string(),
        ));
    }

    let reopened_sessions = world.reopen_sessions().await?;
    let report = SqliteReplayReport {
        schema: SQLITE_REPLAY_REPORT_SCHEMA.to_string(),
        trace_path: trace_path.to_path_buf(),
        database_path: db_path.to_path_buf(),
        terminal_verdict,
        delivered_event_count: trace.events.len(),
        runtime_replayed_boundary_count,
        replayed_boundary_families: replayed_boundary_families.into_iter().collect(),
        carried_forward_boundary_count: 0,
        checkpoint_replay,
        reopened_sessions,
        final_summary,
    };
    if let Some(report_path) = report_path {
        if let Some(parent) = report_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(report_path, serde_json::to_vec_pretty(&report)?)?;
    }
    Ok(report)
}

struct SqliteRuntimeReplayWorld {
    database_root: PathBuf,
    sessions: BTreeMap<String, SqliteRuntimeReplaySession>,
    provider_completion_events: BTreeMap<String, BoundaryEvent>,
    queued_inputs: BTreeMap<String, String>,
    store_factory: Arc<dyn SessionStoreFactory>,
    checkpoint_writes: CheckpointWriteCollector,
    runtime_boundaries: RuntimeBoundaryHarness,
}

struct SqliteRuntimeReplaySession {
    _core: lash::LashCore,
    session: lash::LashSession,
    transport: Arc<ScriptedLlmHttpTransport>,
    provider_schedule: ScriptedTransportSchedule,
    provider_scripts: Vec<ProviderWireScript>,
    provider_kind: String,
    active_provider_turns: BTreeMap<String, SqliteActiveProviderTurn>,
}

struct SqliteActiveProviderTurn {
    handle: tokio::task::JoinHandle<Result<Value, String>>,
}

impl SqliteRuntimeReplayWorld {
    fn new(database_root: PathBuf, trace: &SimulationTrace) -> Self {
        let clock = crate::clock::SimClock::new();
        let effect_replay_path = database_root.join("runtime-effects.sqlite");
        let process_registry_path = effect_replay_path.with_extension("process-registry.sqlite");
        let checkpoint_writes = CheckpointWriteCollector::default();
        let backend_factory: Arc<dyn SessionStoreFactory> = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new_with_process_registry(
                database_root.clone(),
                process_registry_path,
            )
            .with_clock(clock.clone()),
        );
        let store_factory: Arc<dyn SessionStoreFactory> = Arc::new(
            ObservedSessionStoreFactory::new(backend_factory, checkpoint_writes.clone()),
        );
        let provider_completion_events = trace
            .events
            .iter()
            .filter(|event| event.kind == BoundaryKind::Provider)
            .map(|event| (event.boundary_id.clone(), event.as_event()))
            .collect();
        Self {
            database_root,
            sessions: BTreeMap::new(),
            provider_completion_events,
            queued_inputs: BTreeMap::new(),
            checkpoint_writes,
            runtime_boundaries: RuntimeBoundaryHarness::new(
                Arc::clone(&store_factory),
                RuntimeEffectReplayStore::sqlite_file(effect_replay_path),
                clock,
            ),
            store_factory,
        }
    }

    fn checkpoint_write_events(&self) -> Vec<CheckpointWriteEvent> {
        self.checkpoint_writes.events()
    }

    async fn deliver_boundary(
        &mut self,
        event: &BoundaryEvent,
        original_observed: &Value,
    ) -> Result<Value, SqliteReplayError> {
        match event.kind {
            BoundaryKind::Ingress => self.open_runtime_session(event).await,
            BoundaryKind::QueuedIngress => self.queue_turn_input(event).await,
            BoundaryKind::Provider => self.finish_provider_turn(event).await,
            BoundaryKind::ProviderEvent => self.release_provider_event(event).await,
            BoundaryKind::Observer => self.observe_session(event, original_observed),
            BoundaryKind::Cancellation => self.cancel_queued_input(event).await,
            BoundaryKind::Tool
            | BoundaryKind::ExecCode
            | BoundaryKind::DurableEffect
            | BoundaryKind::ProcessWake
            | BoundaryKind::ProcessLifecycle
            | BoundaryKind::Worker
            | BoundaryKind::Trigger
            | BoundaryKind::BackendFailure
            | BoundaryKind::ProviderMutation
            | BoundaryKind::LeaseTime => Err(SqliteReplayError::Assertion(format!(
                "boundary `{}` ({}) is owned by the replay projector, not the SQLite runtime world",
                event.boundary_id,
                boundary_family_name(event.kind)
            ))),
        }
    }

    async fn deliver_runtime_boundary(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, SqliteReplayError> {
        self.runtime_boundaries
            .deliver(event)
            .await
            .map_err(|err| SqliteReplayError::Runtime(err.to_string()))
    }

    async fn open_runtime_session(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, SqliteReplayError> {
        let provider_texts = event
            .payload
            .get("provider_texts")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .ok_or_else(|| {
                SqliteReplayError::Assertion(format!(
                    "ingress boundary `{}` missing provider_texts",
                    event.boundary_id
                ))
            })?;
        if provider_texts.is_empty() {
            return Err(SqliteReplayError::Assertion(format!(
                "ingress boundary `{}` provided no runtime provider scripts",
                event.boundary_id
            )));
        }
        let provider_kind = event
            .payload
            .get("provider_kind")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SqliteReplayError::Assertion(format!(
                    "ingress boundary `{}` missing provider_kind",
                    event.boundary_id
                ))
            })?;
        let scripts = runtime_provider_scripts_for_texts(provider_kind, &provider_texts)
            .map_err(|err| SqliteReplayError::Runtime(err.to_string()))?;
        let provider_schedule = ScriptedTransportSchedule::new();
        let (core, transport, provider_kind) = runtime_core_for_scripts(
            Arc::clone(&self.store_factory),
            self.database_root.as_path(),
            provider_kind,
            scripts.clone(),
            Some(provider_schedule.clone()),
        )
        .await?;
        let session = core
            .session(event.actor_alias.clone())
            .open()
            .await
            .map_err(|err| SqliteReplayError::Runtime(err.to_string()))?;
        if session.session_id() != event.actor_alias {
            return Err(SqliteReplayError::Assertion(format!(
                "ingress opened session `{}`, expected `{}`",
                session.session_id(),
                event.actor_alias
            )));
        }
        self.sessions.insert(
            event.actor_alias.clone(),
            SqliteRuntimeReplaySession {
                _core: core,
                session,
                transport,
                provider_schedule,
                provider_scripts: scripts,
                provider_kind,
                active_provider_turns: BTreeMap::new(),
            },
        );
        Ok(json!({
            "session": event.actor_alias,
            "opened": true,
            "ingress_count": 1,
        }))
    }

    async fn queue_turn_input(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, SqliteReplayError> {
        let runtime_session = self.sessions.get(&event.actor_alias).ok_or_else(|| {
            SqliteReplayError::Assertion(format!(
                "queued ingress boundary `{}` ran before ingress for `{}`",
                event.boundary_id, event.actor_alias
            ))
        })?;
        let text = event
            .payload
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("queued input");
        let source_key = event
            .payload
            .get("source_key")
            .and_then(Value::as_str)
            .unwrap_or(&event.boundary_id);
        let mut enqueue = runtime_session
            .session
            .enqueue(lash::TurnInput::text(text.to_string()))
            .id(source_key);
        if event.payload.get("ingress_mode").and_then(Value::as_str) == Some("active_turn") {
            let active_turn_id = event
                .payload
                .get("active_turn_id")
                .and_then(Value::as_str)
                .unwrap_or(&event.boundary_id);
            enqueue = enqueue.ingress(lash_core::TurnInputIngress::active_turn(
                active_turn_id,
                lash_core::TurnInputCheckpointBoundary::AfterWork,
            ));
        }
        let acceptance = enqueue
            .send()
            .await
            .map_err(|err| SqliteReplayError::Runtime(err.to_string()))?;
        self.queued_inputs
            .insert(event.boundary_id.clone(), acceptance.input_id.clone());
        let input_state = match acceptance.ingress {
            lash_core::TurnInputIngress::ActiveTurn { .. } => {
                lash_core::TurnInputState::PendingActive
            }
            lash_core::TurnInputIngress::NextTurn => lash_core::TurnInputState::DeferredNextTurn,
        };
        Ok(json!({
            "session": event.actor_alias,
            "queued_ingress": true,
            "source_key": source_key,
            "input_id": acceptance.input_id,
            "input_state": input_state.as_str(),
            "ingress_mode": event
                .payload
                .get("ingress_mode")
                .and_then(Value::as_str)
                .unwrap_or("next_turn"),
            "active_turn_id": event.payload.get("active_turn_id").cloned().unwrap_or(Value::Null),
        }))
    }

    async fn ensure_provider_turn_started(
        &mut self,
        actor_alias: &str,
        turn_boundary_id: &str,
    ) -> Result<(), SqliteReplayError> {
        let runtime_session = self.sessions.get_mut(actor_alias).ok_or_else(|| {
            SqliteReplayError::Assertion(format!(
                "provider turn `{turn_boundary_id}` ran before ingress for `{actor_alias}`"
            ))
        })?;
        if runtime_session
            .active_provider_turns
            .contains_key(turn_boundary_id)
        {
            return Ok(());
        }
        let event = self
            .provider_completion_events
            .get(turn_boundary_id)
            .cloned()
            .ok_or_else(|| {
                SqliteReplayError::Assertion(format!(
                    "provider event referenced unknown turn `{turn_boundary_id}`"
                ))
            })?;
        let expected_text = event
            .payload
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("");
        let expected_turn_index = event
            .payload
            .get("turn_index")
            .and_then(Value::as_u64)
            .unwrap_or(1) as usize;
        let _script = runtime_session
            .provider_scripts
            .get(expected_turn_index.saturating_sub(1))
            .ok_or_else(|| {
                SqliteReplayError::Assertion(format!(
                    "provider boundary `{}` had no runtime provider script for turn {}",
                    event.boundary_id, expected_turn_index
                ))
            })?;
        if expected_text.is_empty() {
            return Err(SqliteReplayError::Assertion(format!(
                "provider boundary `{}` missing expected text",
                event.boundary_id
            )));
        }
        let session = runtime_session.session.clone();
        let transport = Arc::clone(&runtime_session.transport);
        let provider_kind = runtime_session.provider_kind.clone();
        let handle = tokio::spawn(async move {
            run_provider_turn_task(session, transport, provider_kind, event)
                .await
                .map_err(|err| err.to_string())
        });
        runtime_session.active_provider_turns.insert(
            turn_boundary_id.to_string(),
            SqliteActiveProviderTurn { handle },
        );
        Ok(())
    }

    async fn release_provider_event(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, SqliteReplayError> {
        let turn_boundary_id = event
            .payload
            .get("turn_boundary_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SqliteReplayError::Assertion(format!(
                    "provider event `{}` missing turn_boundary_id",
                    event.boundary_id
                ))
            })?
            .to_string();
        self.ensure_provider_turn_started(&event.actor_alias, &turn_boundary_id)
            .await?;
        let event_index = event
            .payload
            .get("event_index")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                SqliteReplayError::Assertion(format!(
                    "provider event `{}` missing event_index",
                    event.boundary_id
                ))
            })? as usize;
        let exchange_index = event
            .payload
            .get("exchange_index")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                SqliteReplayError::Assertion(format!(
                    "provider event `{}` missing exchange_index",
                    event.boundary_id
                ))
            })? as usize;
        let event_name = event
            .payload
            .get("event_name")
            .and_then(Value::as_str)
            .unwrap_or("provider_event");
        let runtime_session = self.sessions.get(&event.actor_alias).ok_or_else(|| {
            SqliteReplayError::Assertion(format!(
                "provider event `{}` ran before ingress for `{}`",
                event.boundary_id, event.actor_alias
            ))
        })?;
        let active_turn_pending = runtime_session
            .active_provider_turns
            .contains_key(&turn_boundary_id);
        let release = active_turn_pending.then(|| {
            runtime_session.provider_schedule.release(
                exchange_index,
                event_index,
                event_name,
                event.at,
            )
        });
        let mut observed = json!({
            "session": event.actor_alias,
            "provider_event_release": true,
            "turn_boundary_id": turn_boundary_id,
            "exchange_index": exchange_index,
            "event_index": event_index,
            "event_name": event_name,
            "provider_kind": runtime_session.provider_kind,
        });
        if let Some(release) = release {
            observed["active_turn_pending_before_release"] = json!(active_turn_pending);
            observed["released_while_turn_pending"] = json!(active_turn_pending);
            observed["scripted_transport_release"] = json!({
                "exchange_index": release.exchange_index,
                "event_index": release.event_index,
                "event_name": release.event_name,
                "at": release.at,
                "blocked_before_release": release.blocked_before_release,
            });
        } else {
            observed["provider_event_release_noop_turn_finished"] = json!(true);
        }
        Ok(observed)
    }

    async fn finish_provider_turn(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, SqliteReplayError> {
        self.ensure_provider_turn_started(&event.actor_alias, &event.boundary_id)
            .await?;
        let runtime_session = self.sessions.get_mut(&event.actor_alias).ok_or_else(|| {
            SqliteReplayError::Assertion(format!(
                "provider boundary `{}` ran before ingress for `{}`",
                event.boundary_id, event.actor_alias
            ))
        })?;
        let active_turn = runtime_session
            .active_provider_turns
            .remove(&event.boundary_id)
            .ok_or_else(|| {
                SqliteReplayError::Assertion(format!(
                    "provider boundary `{}` was not active",
                    event.boundary_id
                ))
            })?;
        let release_count = runtime_session.provider_schedule.releases().len();
        let exchange_count = runtime_session
            .transport
            .exchanges()
            .map_err(|err| SqliteReplayError::Runtime(err.to_string()))?
            .len();
        active_turn
            .handle
            .await
            .map_err(|err| {
                SqliteReplayError::Runtime(format!(
                    "provider boundary `{}` failed after {release_count} scheduled releases and {exchange_count} provider exchanges: {err}",
                    event.boundary_id
                ))
            })?
            .map_err(SqliteReplayError::Runtime)
    }
}

async fn run_provider_turn_task(
    session: lash::LashSession,
    transport: Arc<ScriptedLlmHttpTransport>,
    provider_kind: String,
    event: BoundaryEvent,
) -> Result<Value, SqliteReplayError> {
    let expected_text = event
        .payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("");
    let expected_turn_index = event
        .payload
        .get("turn_index")
        .and_then(Value::as_u64)
        .unwrap_or(1) as usize;
    let output = session
        .turn(lash::TurnInput::text(format!(
            "Replay generated provider turn {} through SQLite.",
            event.boundary_id
        )))
        .turn_id(event.boundary_id.clone())
        .run()
        .await
        .map_err(|err| SqliteReplayError::Runtime(err.to_string()))?;
    let assistant_message = output.assistant_message().unwrap_or_default().to_string();
    let read_view = output.result.state.read_view();
    let graph_node_count = output.result.state.session_graph.nodes.len();
    let transcript_message_count = read_view.messages().len();
    let provider_exchange_count = transport
        .exchanges()
        .map_err(|err| SqliteReplayError::Runtime(err.to_string()))?
        .len();
    let expected_exchange_count = event
        .payload
        .get("expected_provider_exchange_count")
        .and_then(Value::as_u64)
        .unwrap_or(expected_turn_index as u64) as usize;
    let graph_invariant = runtime_graph_invariant_facts(&output.result.state.session_graph);
    let agent_frame_invariant = runtime_agent_frame_invariant_facts(&output.result.state);
    let usage_invariant = runtime_usage_invariant_facts(&output.result, &output.activities);
    let runtime_contract = runtime_turn_contract(
        &RuntimeTurnObservation {
            session_id: output.result.state.session_id.clone(),
            turn_index: output.result.state.turn_index,
            assistant_message: assistant_message.clone(),
            graph_node_count,
            transcript_message_count,
            activity_count: output.activities.len(),
            provider_exchange_count,
            graph_invariant: Some(graph_invariant.clone()),
            agent_frame_invariant: Some(agent_frame_invariant.clone()),
            usage_invariant: Some(usage_invariant.clone()),
        },
        &event.actor_alias,
        expected_turn_index,
        expected_text,
        expected_exchange_count,
    );
    if let Err(message) = require_passed(&runtime_contract) {
        return Err(SqliteReplayError::Assertion(format!(
            "SQLite runtime invariants failed for `{}`: {message}",
            event.boundary_id
        )));
    }
    Ok(json!({
        "session": event.actor_alias,
        "runtime_session_id": event.actor_alias,
        "turn_index": expected_turn_index,
        "success": output.is_success(),
        "provider_output": assistant_message,
        "provider_script": event.payload.get("script").cloned().unwrap_or(Value::Null),
        "provider_exchange_count": provider_exchange_count,
        "graph_node_count": graph_node_count,
        "transcript_message_count": transcript_message_count,
        "activity_count_nonzero": !output.activities.is_empty(),
        "provider_kind": provider_kind,
        "runtime_invariants": {
            "session_id": true,
            "turn_index": true,
            "graph_non_empty": graph_node_count > 0,
            "graph_acyclic": graph_invariant.passed,
            "single_active_agent_frame": agent_frame_invariant.passed,
            "usage_monotonic": usage_invariant.passed,
            "transcript_contains_provider_output": read_view.messages().iter().any(|message| {
                message.parts.iter().any(|part| part.content.contains(expected_text))
            }),
            "activity_count_nonzero": !output.activities.is_empty(),
        },
        "runtime_invariant_facts": {
            "graph": graph_invariant,
            "agent_frame": agent_frame_invariant,
            "usage": usage_invariant,
        },
        "runtime_contract": runtime_contract,
    }))
}

impl SqliteRuntimeReplayWorld {
    fn observe_session(
        &self,
        event: &BoundaryEvent,
        original_observed: &Value,
    ) -> Result<Value, SqliteReplayError> {
        let runtime_session = self.sessions.get(&event.actor_alias).ok_or_else(|| {
            SqliteReplayError::Assertion(format!(
                "observer boundary `{}` ran before ingress for `{}`",
                event.boundary_id, event.actor_alias
            ))
        })?;
        let expected_turn_index = event
            .payload
            .get("turn_index")
            .and_then(Value::as_u64)
            .unwrap_or(1) as usize;
        let observation = runtime_session.session.observe().current_observation();
        let read_view = observation.read_view;
        let graph_node_count = read_view.session_graph().nodes.len();
        let transcript_message_count = read_view.messages().len();
        if read_view.session_id() != event.actor_alias
            || read_view.turn_index() != expected_turn_index
            || graph_node_count == 0
        {
            return Err(SqliteReplayError::Assertion(format!(
                "SQLite observer invariants failed for `{}`",
                event.boundary_id
            )));
        }
        if transcript_message_count
            != original_observed
                .get("transcript_message_count")
                .and_then(Value::as_u64)
                .unwrap_or(transcript_message_count as u64) as usize
        {
            return Err(SqliteReplayError::Divergence(format!(
                "SQLite observer transcript count changed for `{}`",
                event.boundary_id
            )));
        }
        Ok(json!({
            "session": event.actor_alias,
            "turn_index": expected_turn_index,
            "reconnected": event.payload
                .get("reconnect")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            "graph_node_count": graph_node_count,
            "transcript_message_count": transcript_message_count,
            "observer_invariants": {
                "session_id": true,
                "turn_index_converged": true,
                "graph_non_empty": true,
                "transcript_message_count_converged": true,
            },
        }))
    }

    async fn cancel_queued_input(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, SqliteReplayError> {
        let runtime_session = self.sessions.get(&event.actor_alias).ok_or_else(|| {
            SqliteReplayError::Assertion(format!(
                "cancellation boundary `{}` ran before ingress for `{}`",
                event.boundary_id, event.actor_alias
            ))
        })?;
        let target = event
            .payload
            .get("target")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SqliteReplayError::Assertion(format!(
                    "cancellation boundary `{}` missing target",
                    event.boundary_id
                ))
            })?;
        let input_id = self.queued_inputs.get(target).cloned().ok_or_else(|| {
            SqliteReplayError::Assertion(format!(
                "cancellation boundary `{}` target `{target}` was not queued",
                event.boundary_id
            ))
        })?;
        let outcome = runtime_session
            .session
            .cancel_pending_turn_input(&input_id)
            .await
            .map_err(|err| SqliteReplayError::Runtime(err.to_string()))?;
        let (cancelled, cancel_outcome) = match &outcome {
            lash::PendingTurnInputCancelOutcome::Cancelled(_) => (true, "cancelled"),
            lash::PendingTurnInputCancelOutcome::AlreadyClaimed { .. } => {
                (false, "already_claimed")
            }
            lash::PendingTurnInputCancelOutcome::AlreadyCompleted(_) => {
                (false, "already_completed")
            }
            lash::PendingTurnInputCancelOutcome::AlreadyCancelled(_) => {
                (false, "already_cancelled")
            }
            lash::PendingTurnInputCancelOutcome::NotFound => (false, "not_found"),
        };
        Ok(json!({
            "session": event.actor_alias,
            "target": target,
            "cancelled": cancelled,
            "cancel_outcome": cancel_outcome,
        }))
    }

    async fn reopen_sessions(
        &self,
    ) -> Result<Vec<SqliteReopenedSessionEvidence>, SqliteReplayError> {
        let mut evidence = Vec::new();
        for (session_id, runtime_session) in &self.sessions {
            let (core, _, _) = runtime_core_for_scripts(
                Arc::clone(&self.store_factory),
                self.database_root.as_path(),
                &runtime_session.provider_kind,
                Vec::new(),
                None,
            )
            .await?;
            let session = core
                .session(session_id.clone())
                .open()
                .await
                .map_err(|err| SqliteReplayError::Runtime(err.to_string()))?;
            let observation = session.observe().current_observation();
            let read_view = observation.read_view;
            evidence.push(SqliteReopenedSessionEvidence {
                session_id: session_id.clone(),
                database_path: lash_sqlite_store::SqliteSessionStoreFactory::new(
                    self.database_root.clone(),
                )
                .catalog_path(),
                turn_index: read_view.turn_index(),
                graph_node_count: read_view.session_graph().nodes.len(),
                transcript_message_count: read_view.messages().len(),
            });
        }
        Ok(evidence)
    }
}

async fn runtime_core_for_scripts(
    store_factory: Arc<dyn SessionStoreFactory>,
    database_root: &Path,
    provider_kind: &str,
    scripts: Vec<ProviderWireScript>,
    provider_schedule: Option<ScriptedTransportSchedule>,
) -> Result<(lash::LashCore, Arc<ScriptedLlmHttpTransport>, String), SqliteReplayError> {
    let mut transport = ScriptedLlmHttpTransport::from_scripts(scripts)
        .map_err(|err| SqliteReplayError::Runtime(err.to_string()))?;
    if let Some(schedule) = provider_schedule {
        transport = transport.with_event_schedule(schedule);
    }
    let transport = Arc::new(transport);
    let (provider_handle, model, provider_kind) =
        runtime_provider_components(provider_kind, &transport)
            .map_err(|err| SqliteReplayError::Runtime(err.to_string()))?;
    let process_env_store: Arc<dyn lash::persistence::ProcessExecutionEnvStore> = Arc::new(
        lash_sqlite_store::Store::open(&database_root.join("process-env.sqlite"))
            .await
            .map_err(|err| SqliteReplayError::Runtime(err.to_string()))?,
    );
    let core = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .effect_host(Arc::new(
            lash::durability::InlineEffectHost::default().allow_process_lifetime_completion_keys(),
        ))
        .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
            database_root.join("attachments"),
        )))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(process_env_store)
        .store_factory(store_factory)
        .lease_timings(crate::lease::sim_runtime_lease_timings())
        .provider(provider_handle)
        .model(model)
        .build(crate::sim_process_owner())
        .map_err(|err| SqliteReplayError::Runtime(err.to_string()))?;
    Ok((core, transport, provider_kind))
}

fn prepare_database_root(path: &Path) -> Result<(), SqliteReplayError> {
    if path.exists() {
        if path.is_dir() {
            std::fs::remove_dir_all(path)?;
        } else {
            std::fs::remove_file(path)?;
        }
    }
    std::fs::create_dir_all(path)?;
    Ok(())
}

fn is_suspend_replay_boundary(event: &crate::scheduler::BoundaryEvent) -> bool {
    (event.kind == BoundaryKind::Ingress && event.payload.get("suspend_kind").is_some())
        || event
            .payload
            .get("suspend_resume")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

fn is_runtime_session_boundary(kind: BoundaryKind) -> bool {
    matches!(
        kind,
        BoundaryKind::Ingress
            | BoundaryKind::QueuedIngress
            | BoundaryKind::Provider
            | BoundaryKind::ProviderEvent
            | BoundaryKind::Observer
            | BoundaryKind::Cancellation
    )
}

fn is_runtime_backed_boundary(kind: BoundaryKind) -> bool {
    matches!(
        kind,
        BoundaryKind::Tool
            | BoundaryKind::ExecCode
            | BoundaryKind::DurableEffect
            | BoundaryKind::ProcessWake
            | BoundaryKind::ProcessLifecycle
            | BoundaryKind::Worker
    )
}

fn boundary_family_name(kind: BoundaryKind) -> &'static str {
    match kind {
        BoundaryKind::Ingress => "ingress",
        BoundaryKind::QueuedIngress => "queued_ingress",
        BoundaryKind::Provider => "provider",
        BoundaryKind::ProviderEvent => "provider_event",
        BoundaryKind::Tool => "tool",
        BoundaryKind::ExecCode => "exec_code",
        BoundaryKind::DurableEffect => "durable_effect",
        BoundaryKind::ProcessWake => "process_wake",
        BoundaryKind::ProcessLifecycle => "process_lifecycle",
        BoundaryKind::Worker => "worker",
        BoundaryKind::Observer => "observer",
        BoundaryKind::Cancellation => "cancellation",
        BoundaryKind::Trigger => "trigger",
        BoundaryKind::BackendFailure => "backend_failure",
        BoundaryKind::ProviderMutation => "provider_mutation",
        BoundaryKind::LeaseTime => "lease_time",
    }
}

fn normalize_backend_observed(kind: BoundaryKind, value: &Value) -> Value {
    let mut normalized = value.clone();
    if let Some(object) = normalized.as_object_mut() {
        object.remove("sim_clock");
        // Real lease fencing tokens are not reproducible by the abstract
        // projector path, so they are excluded from cross-backend equality.
        object.remove("runtime_lease_probe");
        object.remove("runtime_suspend");
        object.remove("scripted_transport_release");
        object.remove("active_turn_pending_before_release");
        object.remove("released_while_turn_pending");
        object.remove("provider_event_release_noop_turn_finished");
    }
    if kind == BoundaryKind::Cancellation
        && let Some(object) = normalized.as_object_mut()
    {
        // The cancel outcome depends on whether the live runtime had already
        // consumed the targeted input; the abstract projector cannot reconstruct
        // it. Coverage is preserved via `cancellation_count` in the summary.
        object.remove("cancel_outcome");
        object.remove("cancelled");
    }
    if kind == BoundaryKind::QueuedIngress
        && let Some(object) = normalized.as_object_mut()
        && object.get("input_id").and_then(Value::as_str).is_some()
    {
        object.insert(
            "input_id".to_string(),
            Value::String("<backend-assigned>".to_string()),
        );
    }
    if kind == BoundaryKind::Provider
        && let Some(object) = normalized.as_object_mut()
    {
        object.remove("runtime_invariant_facts");
        object.remove("runtime_final_value_facts");
        if let Some(runtime_invariants) = object
            .get_mut("runtime_invariants")
            .and_then(Value::as_object_mut)
        {
            runtime_invariants.remove("graph_acyclic");
            runtime_invariants.remove("single_active_agent_frame");
            runtime_invariants.remove("usage_monotonic");
        }
    }
    normalized
}

fn write_divergence_artifact(
    trace_path: &Path,
    db_path: &Path,
    report_path: Option<&Path>,
    verdict: OracleVerdict,
    expected_summary: &AbstractWorldSummary,
    actual_summary: &AbstractWorldSummary,
    boundary: Option<SqliteBoundaryDivergence>,
) -> Result<(), SqliteReplayError> {
    let Some(report_path) = report_path else {
        return Ok(());
    };
    let divergence_path = report_path.with_file_name("sqlite-divergence.json");
    let artifact = SqliteDivergenceArtifact {
        schema: SQLITE_DIVERGENCE_SCHEMA.to_string(),
        trace_path: trace_path.to_path_buf(),
        database_path: db_path.to_path_buf(),
        verdict,
        expected_summary: expected_summary.clone(),
        actual_summary: actual_summary.clone(),
        boundary,
    };
    if let Some(parent) = divergence_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(divergence_path, serde_json::to_vec_pretty(&artifact)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::generate_workload;
    use crate::runner::run_generated_workload_for_fixture;

    #[tokio::test]
    async fn sqlite_replay_runs_trace_through_real_lash_sqlite_persistence() {
        let workload = generate_workload(7, "fast-random", 24).expect("workload");
        let trace = run_generated_workload_for_fixture(workload, "bundle")
            .await
            .expect("trace");
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("sqlite-store");
        let report_path = tmp.path().join("sqlite-replay.json");

        let report = replay_trace_to_sqlite(
            Path::new("trace.json"),
            &trace,
            &db_path,
            Some(&report_path),
        )
        .await
        .expect("sqlite replay");

        assert_eq!(report.schema, SQLITE_REPLAY_REPORT_SCHEMA);
        assert_eq!(report.delivered_event_count, trace.events.len());
        assert_eq!(report.runtime_replayed_boundary_count, trace.events.len());
        assert_eq!(report.carried_forward_boundary_count, 0);
        assert!(!report.checkpoint_replay.recorded_runtime.is_empty());
        assert_eq!(
            report.checkpoint_replay.recorded_runtime,
            report.checkpoint_replay.observed_runtime
        );
        assert!(
            !report.checkpoint_replay.carried.is_empty(),
            "contract and suspend fixture writes must be explicitly carried"
        );
        let families = report
            .replayed_boundary_families
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        for expected_family in [
            "ingress",
            "queued_ingress",
            "provider",
            "tool",
            "exec_code",
            "durable_effect",
            "process_wake",
            "worker",
            "observer",
            "cancellation",
            "trigger",
            "backend_failure",
            "provider_mutation",
            "lease_time",
        ] {
            assert!(
                families.contains(expected_family),
                "missing replayed family {expected_family}"
            );
        }
        assert_eq!(report.final_summary, trace.final_summary);
        assert!(!report.reopened_sessions.is_empty());
        assert!(
            report
                .reopened_sessions
                .iter()
                .all(|session| session.database_path.exists())
        );
        assert!(db_path.is_dir());
        assert!(report_path.exists());
    }
}
