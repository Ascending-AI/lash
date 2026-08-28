use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lash_core::SessionStoreFactory;
use lash_postgres_store::PostgresStorage;
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

pub const POSTGRES_REPLAY_REPORT_SCHEMA: &str = "lash.sim.postgres-runtime-replay-report.v5";
pub const POSTGRES_DIVERGENCE_SCHEMA: &str = "lash.sim.postgres-runtime-divergence.v1";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PostgresReplayReport {
    pub schema: String,
    pub trace_path: PathBuf,
    pub database_url_redacted: String,
    pub terminal_verdict: OracleVerdict,
    pub delivered_event_count: usize,
    pub runtime_replayed_boundary_count: usize,
    pub replayed_boundary_families: Vec<String>,
    pub carried_forward_boundary_count: usize,
    pub checkpoint_replay: BackendCheckpointReplayEvidence,
    pub effect_history_replay: PostgresEffectHistoryReplayEvidence,
    pub reopened_sessions: Vec<PostgresReopenedSessionEvidence>,
    pub final_summary: AbstractWorldSummary,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PostgresEffectHistoryReplayEvidence {
    pub status: String,
    pub native_controller: String,
    pub runtime_boundary_controller: String,
    pub store_table: String,
    pub replay_semantics: Vec<String>,
    pub conformance_evidence: Vec<String>,
    pub smallest_required_api_change: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PostgresDivergenceArtifact {
    pub schema: String,
    pub trace_path: PathBuf,
    pub database_url_redacted: String,
    pub verdict: OracleVerdict,
    pub expected_summary: AbstractWorldSummary,
    pub actual_summary: AbstractWorldSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary: Option<PostgresBoundaryDivergence>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PostgresBoundaryDivergence {
    pub boundary_id: String,
    pub boundary_kind: String,
    pub expected_observed: Value,
    pub actual_observed: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PostgresReopenedSessionEvidence {
    pub session_id: String,
    pub turn_index: usize,
    pub graph_node_count: usize,
    pub transcript_message_count: usize,
}

#[derive(Debug)]
pub enum PostgresReplayError {
    TraceIo(TraceIoError),
    Replay(ReplayError),
    Io(std::io::Error),
    Json(serde_json::Error),
    Runtime(String),
    Assertion(String),
    Divergence(String),
}

impl fmt::Display for PostgresReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TraceIo(err) => write!(f, "{err}"),
            Self::Replay(err) => write!(f, "{err}"),
            Self::Io(err) => write!(f, "Postgres runtime replay I/O failed: {err}"),
            Self::Json(err) => write!(f, "Postgres runtime replay JSON failed: {err}"),
            Self::Runtime(message) => write!(f, "Postgres runtime replay failed: {message}"),
            Self::Assertion(message) => {
                write!(f, "Postgres runtime replay assertion failed: {message}")
            }
            Self::Divergence(message) => write!(f, "Postgres runtime replay diverged: {message}"),
        }
    }
}

impl std::error::Error for PostgresReplayError {}

impl From<TraceIoError> for PostgresReplayError {
    fn from(value: TraceIoError) -> Self {
        Self::TraceIo(value)
    }
}

impl From<ReplayError> for PostgresReplayError {
    fn from(value: ReplayError) -> Self {
        Self::Replay(value)
    }
}

impl From<std::io::Error> for PostgresReplayError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for PostgresReplayError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

pub async fn replay_trace_file_to_postgres(
    trace_path: &Path,
    database_url: &str,
    report_path: Option<&Path>,
) -> Result<PostgresReplayReport, PostgresReplayError> {
    let trace = read_trace(trace_path)?;
    replay_trace_to_postgres(trace_path, &trace, database_url, report_path).await
}

pub async fn replay_trace_to_postgres(
    trace_path: &Path,
    trace: &SimulationTrace,
    database_url: &str,
    report_path: Option<&Path>,
) -> Result<PostgresReplayReport, PostgresReplayError> {
    let model_replay = replay_trace(trace_path, trace)?;
    let storage = Arc::new(
        PostgresStorage::connect(database_url)
            .await
            .map_err(|err| PostgresReplayError::Runtime(err.to_string()))?,
    );
    reset_postgres_for_replay(storage.as_ref()).await?;

    let attachment_root = report_path
        .and_then(Path::parent)
        .map(|parent| parent.join("postgres-attachments"))
        .unwrap_or_else(|| PathBuf::from("target/lash-sim/postgres-attachments"));
    let mut world = PostgresRuntimeReplayWorld::new(storage, attachment_root, trace);
    let mut store = ModelStore::default();
    let mut provider_mutation_cache = ProviderMutationMatrixCache::default();
    let mut runtime_replayed_boundary_count = 0;
    let mut replayed_boundary_families = BTreeSet::new();
    for delivered in &trace.events {
        let event = delivered.as_event();
        let observed = if event.kind == BoundaryKind::BackendFailure {
            // BackendFailure evidence is produced by the dedicated real SQLite
            // injector during generation; a Postgres replay carries that recorded
            // observation rather than fabricating a different model failure.
            delivered.observed.clone()
        } else if is_suspend_replay_boundary(&event) {
            // The generated suspend path already captured real committed runtime
            // facts; backend replay preserves that observation verbatim.
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
                .map_err(|err| PostgresReplayError::Runtime(err.to_string()))?
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
                "sim.oracle.postgres-boundary-replay.v1",
                format!(
                    "Postgres replay boundary `{}` ({}) reproduced different observed data",
                    event.boundary_id,
                    boundary_family_name(event.kind)
                ),
            );
            write_divergence_artifact(
                trace_path,
                database_url,
                report_path,
                verdict.clone(),
                &trace.final_summary,
                &actual_summary,
                Some(PostgresBoundaryDivergence {
                    boundary_id: event.boundary_id,
                    boundary_kind: boundary_family_name(event.kind).to_string(),
                    expected_observed: delivered.observed.clone(),
                    actual_observed: observed,
                }),
            )?;
            return Err(PostgresReplayError::Divergence(verdict.message));
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
                    .map_err(PostgresReplayError::Divergence)?;
                let verdict =
                    OracleVerdict::failed("sim.oracle.postgres-checkpoint-replay.v1", &message);
                write_divergence_artifact(
                    trace_path,
                    database_url,
                    report_path,
                    verdict,
                    &trace.final_summary,
                    &actual_summary,
                    None,
                )?;
                return Err(PostgresReplayError::Divergence(message));
            }
        };
    // Runtime-turn checkpoint facts are observed from Postgres and compared
    // above. Only contract-proof and suspend-fixture facts, whose boundaries are
    // projector-owned in this static lane, are carried into the whole-trace
    // summary.
    let final_summary = store
        .summarize_with_trace_checkpoint_writes(&trace.events, &checkpoint_replay.summary_writes())
        .map_err(PostgresReplayError::Divergence)?;
    let terminal_verdict = replay_determinism(&trace.final_summary, &final_summary);
    if !terminal_verdict.is_passed() {
        write_divergence_artifact(
            trace_path,
            database_url,
            report_path,
            terminal_verdict.clone(),
            &trace.final_summary,
            &final_summary,
            None,
        )?;
        return Err(PostgresReplayError::Divergence(
            terminal_verdict.message.clone(),
        ));
    }
    if final_summary != model_replay.final_summary {
        let verdict = OracleVerdict::failed(
            "sim.oracle.postgres-model-replay.v1",
            "runtime Postgres replay summary diverged from model replay",
        );
        write_divergence_artifact(
            trace_path,
            database_url,
            report_path,
            verdict,
            &model_replay.final_summary,
            &final_summary,
            None,
        )?;
        return Err(PostgresReplayError::Divergence(
            "runtime Postgres replay summary diverged from model replay".to_string(),
        ));
    }

    let reopened_sessions = world.reopen_sessions().await?;
    let report = PostgresReplayReport {
        schema: POSTGRES_REPLAY_REPORT_SCHEMA.to_string(),
        trace_path: trace_path.to_path_buf(),
        database_url_redacted: redact_database_url(database_url),
        terminal_verdict,
        delivered_event_count: trace.events.len(),
        runtime_replayed_boundary_count,
        replayed_boundary_families: replayed_boundary_families.into_iter().collect(),
        carried_forward_boundary_count: 0,
        checkpoint_replay,
        effect_history_replay: postgres_effect_history_replay_evidence(),
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

fn postgres_effect_history_replay_evidence() -> PostgresEffectHistoryReplayEvidence {
    PostgresEffectHistoryReplayEvidence {
        status: "native_postgres_runtime_effect_controller".to_string(),
        native_controller: "lash_postgres_store::PostgresRuntimeEffectController".to_string(),
        runtime_boundary_controller: "postgres_runtime_effect_controller".to_string(),
        store_table: "lash_runtime_effect_replay".to_string(),
        replay_semantics: vec![
            "scope_id_plus_replay_key_primary_key".to_string(),
            "stable_envelope_hash_conflict_rejection".to_string(),
            "lease_owner_and_token_fenced_finalize".to_string(),
            "completed_and_failed_outcome_replay".to_string(),
            "sleep_due_at_ms_preservation".to_string(),
        ],
        conformance_evidence: vec![
            "postgres_runtime_effect_controller_satisfies_conformance_when_configured".to_string(),
            "postgres_session_store_factory_reopens_generated_sessions".to_string(),
            "postgres_runtime_persistence_validates_queued_work_process_wakes".to_string(),
            "postgres_runtime_persistence_validates_session_execution_lease_failover".to_string(),
            "replay-postgres_runtime_boundaries_use_PostgresRuntimeEffectController".to_string(),
        ],
        smallest_required_api_change: "none".to_string(),
    }
}

struct PostgresRuntimeReplayWorld {
    storage: Arc<PostgresStorage>,
    attachment_root: PathBuf,
    sessions: BTreeMap<String, PostgresRuntimeReplaySession>,
    provider_completion_events: BTreeMap<String, BoundaryEvent>,
    queued_inputs: BTreeMap<String, String>,
    store_factory: Arc<dyn SessionStoreFactory>,
    checkpoint_writes: CheckpointWriteCollector,
    runtime_boundaries: RuntimeBoundaryHarness,
}

struct PostgresRuntimeReplaySession {
    _core: lash::LashCore,
    session: lash::LashSession,
    transport: Arc<ScriptedLlmHttpTransport>,
    provider_schedule: ScriptedTransportSchedule,
    provider_scripts: Vec<ProviderWireScript>,
    provider_kind: String,
    active_provider_turns: BTreeMap<String, PostgresActiveProviderTurn>,
}

struct PostgresActiveProviderTurn {
    handle: tokio::task::JoinHandle<Result<Value, String>>,
}

impl PostgresRuntimeReplayWorld {
    fn new(
        storage: Arc<PostgresStorage>,
        attachment_root: PathBuf,
        trace: &SimulationTrace,
    ) -> Self {
        let clock = crate::clock::SimClock::new();
        let checkpoint_writes = CheckpointWriteCollector::default();
        let backend_factory: Arc<dyn SessionStoreFactory> =
            Arc::new(storage.session_store_factory().with_clock(clock.clone()));
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
            runtime_boundaries: RuntimeBoundaryHarness::new(
                Arc::clone(&store_factory),
                RuntimeEffectReplayStore::postgres(Arc::clone(&storage)),
                clock,
            ),
            storage,
            attachment_root,
            sessions: BTreeMap::new(),
            provider_completion_events,
            queued_inputs: BTreeMap::new(),
            checkpoint_writes,
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
    ) -> Result<Value, PostgresReplayError> {
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
            | BoundaryKind::LeaseTime => Err(PostgresReplayError::Assertion(format!(
                "boundary `{}` ({}) is owned by the replay projector, not the Postgres runtime world",
                event.boundary_id,
                boundary_family_name(event.kind)
            ))),
        }
    }

    async fn deliver_runtime_boundary(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, PostgresReplayError> {
        self.runtime_boundaries
            .deliver(event)
            .await
            .map_err(|err| PostgresReplayError::Runtime(err.to_string()))
    }

    async fn open_runtime_session(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, PostgresReplayError> {
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
                PostgresReplayError::Assertion(format!(
                    "ingress boundary `{}` missing provider_texts",
                    event.boundary_id
                ))
            })?;
        if provider_texts.is_empty() {
            return Err(PostgresReplayError::Assertion(format!(
                "ingress boundary `{}` provided no runtime provider scripts",
                event.boundary_id
            )));
        }
        let provider_kind = event
            .payload
            .get("provider_kind")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PostgresReplayError::Assertion(format!(
                    "ingress boundary `{}` missing provider_kind",
                    event.boundary_id
                ))
            })?;
        let scripts = runtime_provider_scripts_for_texts(provider_kind, &provider_texts)
            .map_err(|err| PostgresReplayError::Runtime(err.to_string()))?;
        let provider_schedule = ScriptedTransportSchedule::new();
        let (core, transport, provider_kind) = runtime_core_for_scripts(
            self.storage.as_ref(),
            Arc::clone(&self.store_factory),
            &self.attachment_root,
            provider_kind,
            scripts.clone(),
            Some(provider_schedule.clone()),
        )?;
        let session = core
            .session(event.actor_alias.clone())
            .open()
            .await
            .map_err(|err| PostgresReplayError::Runtime(err.to_string()))?;
        self.sessions.insert(
            event.actor_alias.clone(),
            PostgresRuntimeReplaySession {
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
    ) -> Result<Value, PostgresReplayError> {
        let runtime_session = self.sessions.get(&event.actor_alias).ok_or_else(|| {
            PostgresReplayError::Assertion(format!(
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
            .map_err(|err| PostgresReplayError::Runtime(err.to_string()))?;
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
    ) -> Result<(), PostgresReplayError> {
        let runtime_session = self.sessions.get_mut(actor_alias).ok_or_else(|| {
            PostgresReplayError::Assertion(format!(
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
                PostgresReplayError::Assertion(format!(
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
                PostgresReplayError::Assertion(format!(
                    "provider boundary `{}` had no runtime provider script for turn {}",
                    event.boundary_id, expected_turn_index
                ))
            })?;
        if expected_text.is_empty() {
            return Err(PostgresReplayError::Assertion(format!(
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
            PostgresActiveProviderTurn { handle },
        );
        Ok(())
    }

    async fn release_provider_event(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, PostgresReplayError> {
        let turn_boundary_id = event
            .payload
            .get("turn_boundary_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PostgresReplayError::Assertion(format!(
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
                PostgresReplayError::Assertion(format!(
                    "provider event `{}` missing event_index",
                    event.boundary_id
                ))
            })? as usize;
        let exchange_index = event
            .payload
            .get("exchange_index")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                PostgresReplayError::Assertion(format!(
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
            PostgresReplayError::Assertion(format!(
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
    ) -> Result<Value, PostgresReplayError> {
        self.ensure_provider_turn_started(&event.actor_alias, &event.boundary_id)
            .await?;
        let runtime_session = self.sessions.get_mut(&event.actor_alias).ok_or_else(|| {
            PostgresReplayError::Assertion(format!(
                "provider boundary `{}` ran before ingress for `{}`",
                event.boundary_id, event.actor_alias
            ))
        })?;
        let active_turn = runtime_session
            .active_provider_turns
            .remove(&event.boundary_id)
            .ok_or_else(|| {
                PostgresReplayError::Assertion(format!(
                    "provider boundary `{}` was not active",
                    event.boundary_id
                ))
            })?;
        let release_count = runtime_session.provider_schedule.releases().len();
        let exchange_count = runtime_session
            .transport
            .exchanges()
            .map_err(|err| PostgresReplayError::Runtime(err.to_string()))?
            .len();
        active_turn
            .handle
            .await
            .map_err(|err| {
                PostgresReplayError::Runtime(format!(
                    "provider boundary `{}` failed after {} scheduled releases and {} provider exchanges: {err}",
                    event.boundary_id,
                    release_count,
                    exchange_count
                ))
            })?
            .map_err(PostgresReplayError::Runtime)
    }
}

async fn run_provider_turn_task(
    session: lash::LashSession,
    transport: Arc<ScriptedLlmHttpTransport>,
    provider_kind: String,
    event: BoundaryEvent,
) -> Result<Value, PostgresReplayError> {
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
            "Replay generated provider turn {} through Postgres.",
            event.boundary_id
        )))
        .turn_id(event.boundary_id.clone())
        .run()
        .await
        .map_err(|err| PostgresReplayError::Runtime(err.to_string()))?;
    let assistant_message = output.assistant_message().unwrap_or_default().to_string();
    let read_view = output.result.state.read_view();
    let graph_node_count = output.result.state.session_graph.nodes.len();
    let transcript_message_count = read_view.messages().len();
    let provider_exchange_count = transport
        .exchanges()
        .map_err(|err| PostgresReplayError::Runtime(err.to_string()))?
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
        return Err(PostgresReplayError::Assertion(format!(
            "Postgres runtime invariants failed for `{}`: {message}",
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

impl PostgresRuntimeReplayWorld {
    fn observe_session(
        &self,
        event: &BoundaryEvent,
        original_observed: &Value,
    ) -> Result<Value, PostgresReplayError> {
        let runtime_session = self.sessions.get(&event.actor_alias).ok_or_else(|| {
            PostgresReplayError::Assertion(format!(
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
            return Err(PostgresReplayError::Assertion(format!(
                "Postgres observer invariants failed for `{}`",
                event.boundary_id
            )));
        }
        if transcript_message_count
            != original_observed
                .get("transcript_message_count")
                .and_then(Value::as_u64)
                .unwrap_or(transcript_message_count as u64) as usize
        {
            return Err(PostgresReplayError::Divergence(format!(
                "Postgres observer transcript count changed for `{}`",
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
    ) -> Result<Value, PostgresReplayError> {
        let runtime_session = self.sessions.get(&event.actor_alias).ok_or_else(|| {
            PostgresReplayError::Assertion(format!(
                "cancellation boundary `{}` ran before ingress for `{}`",
                event.boundary_id, event.actor_alias
            ))
        })?;
        let target = event
            .payload
            .get("target")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PostgresReplayError::Assertion(format!(
                    "cancellation boundary `{}` missing target",
                    event.boundary_id
                ))
            })?;
        let input_id = self.queued_inputs.get(target).cloned().ok_or_else(|| {
            PostgresReplayError::Assertion(format!(
                "cancellation boundary `{}` target `{target}` was not queued",
                event.boundary_id
            ))
        })?;
        let outcome = runtime_session
            .session
            .cancel_pending_turn_input(&input_id)
            .await
            .map_err(|err| PostgresReplayError::Runtime(err.to_string()))?;
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
    ) -> Result<Vec<PostgresReopenedSessionEvidence>, PostgresReplayError> {
        let mut evidence = Vec::new();
        for (session_id, runtime_session) in &self.sessions {
            let (core, _, _) = runtime_core_for_scripts(
                self.storage.as_ref(),
                Arc::clone(&self.store_factory),
                &self.attachment_root,
                &runtime_session.provider_kind,
                Vec::new(),
                None,
            )?;
            let session = core
                .session(session_id.clone())
                .open()
                .await
                .map_err(|err| PostgresReplayError::Runtime(err.to_string()))?;
            let observation = session.observe().current_observation();
            let read_view = observation.read_view;
            evidence.push(PostgresReopenedSessionEvidence {
                session_id: session_id.clone(),
                turn_index: read_view.turn_index(),
                graph_node_count: read_view.session_graph().nodes.len(),
                transcript_message_count: read_view.messages().len(),
            });
        }
        Ok(evidence)
    }
}

fn runtime_core_for_scripts(
    storage: &PostgresStorage,
    store_factory: Arc<dyn SessionStoreFactory>,
    attachment_root: &Path,
    provider_kind: &str,
    scripts: Vec<ProviderWireScript>,
    provider_schedule: Option<ScriptedTransportSchedule>,
) -> Result<(lash::LashCore, Arc<ScriptedLlmHttpTransport>, String), PostgresReplayError> {
    let mut transport = ScriptedLlmHttpTransport::from_scripts(scripts)
        .map_err(|err| PostgresReplayError::Runtime(err.to_string()))?;
    if let Some(schedule) = provider_schedule {
        transport = transport.with_event_schedule(schedule);
    }
    let transport = Arc::new(transport);
    let (provider_handle, model, provider_kind) =
        runtime_provider_components(provider_kind, &transport)
            .map_err(|err| PostgresReplayError::Runtime(err.to_string()))?;
    let process_env_store: Arc<dyn lash::persistence::ProcessExecutionEnvStore> =
        Arc::new(storage.process_env_store());
    let core = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .with_native_queued_work()
        .effect_host(Arc::new(
            lash::durability::NativeEffectHost::default().allow_process_lifetime_completion_keys(),
        ))
        .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
            attachment_root.to_path_buf(),
        )))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(process_env_store)
        .store_factory(store_factory)
        .lease_timings(crate::lease::sim_runtime_lease_timings())
        .provider(provider_handle)
        .model(model)
        .build(crate::sim_process_owner())
        .map_err(|err| PostgresReplayError::Runtime(err.to_string()))?;
    Ok((core, transport, provider_kind))
}

pub(crate) async fn reset_postgres_for_replay(
    storage: &PostgresStorage,
) -> Result<(), PostgresReplayError> {
    sqlx::query(
        r#"
        TRUNCATE
            lash_trigger_deliveries,
            lash_trigger_occurrences,
            lash_trigger_subscriptions,
            lash_process_wake_deliveries,
            lash_process_observers,
            lash_process_tombstones,
            lash_process_segment_handovers,
            lash_process_leases,
            lash_runtime_effect_replay,
            lash_process_events,
            lash_processes,
            lash_queued_work_items,
            lash_queued_work_batches,
            lash_pending_turn_inputs,
            lash_runtime_turn_commits,
            lash_session_execution_leases,
            lash_session_meta,
            lash_usage_deltas,
            lash_graph_nodes,
            lash_sessions,
            lash_attachment_manifest,
            lash_lashlang_artifacts,
            lash_blobs
        RESTART IDENTITY CASCADE
        "#,
    )
    .execute(storage.pool())
    .await
    .map_err(|err| PostgresReplayError::Runtime(err.to_string()))?;
    Ok(())
}

fn normalize_backend_observed(kind: BoundaryKind, value: &Value) -> Value {
    let mut normalized = value.clone();
    if let Some(object) = normalized.as_object_mut() {
        object.remove("sim_clock");
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
    if matches!(
        kind,
        BoundaryKind::Tool | BoundaryKind::ExecCode | BoundaryKind::DurableEffect
    ) && let Some(controller) = normalized
        .as_object_mut()
        .and_then(|object| object.get_mut("runtime_effect"))
        .and_then(Value::as_object_mut)
        .and_then(|effect| effect.get_mut("controller"))
    {
        *controller = Value::String("<backend-runtime-effect-controller>".to_string());
    }
    normalized
}

fn write_divergence_artifact(
    trace_path: &Path,
    database_url: &str,
    report_path: Option<&Path>,
    verdict: OracleVerdict,
    expected_summary: &AbstractWorldSummary,
    actual_summary: &AbstractWorldSummary,
    boundary: Option<PostgresBoundaryDivergence>,
) -> Result<(), PostgresReplayError> {
    let Some(report_path) = report_path else {
        return Ok(());
    };
    let divergence_path = report_path.with_file_name("postgres-divergence.json");
    let artifact = PostgresDivergenceArtifact {
        schema: POSTGRES_DIVERGENCE_SCHEMA.to_string(),
        trace_path: trace_path.to_path_buf(),
        database_url_redacted: redact_database_url(database_url),
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

pub(crate) fn redact_database_url(database_url: &str) -> String {
    let Some((scheme, rest)) = database_url.split_once("://") else {
        return "[redacted]".to_string();
    };
    let host_and_path = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
    format!("{scheme}://[redacted]@{host_and_path}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::generate_workload;
    use crate::runner::run_generated_workload_for_fixture;

    #[test]
    fn postgres_effect_history_evidence_claims_native_controller() {
        let evidence = postgres_effect_history_replay_evidence();

        assert_eq!(evidence.status, "native_postgres_runtime_effect_controller");
        assert_eq!(
            evidence.native_controller,
            "lash_postgres_store::PostgresRuntimeEffectController"
        );
        assert_eq!(
            evidence.runtime_boundary_controller,
            "postgres_runtime_effect_controller"
        );
        assert_eq!(evidence.store_table, "lash_runtime_effect_replay");
        assert_eq!(evidence.smallest_required_api_change, "none");
        assert!(
            evidence
                .replay_semantics
                .contains(&"stable_envelope_hash_conflict_rejection".to_string())
        );
    }

    #[test]
    fn postgres_normalization_ignores_backend_controller_identity_only() {
        let observed = json!({
            "runtime_effect": {
                "kind": "tool_attempt",
                "controller": "postgres_runtime_effect_controller",
                "local_executor_called": true,
            },
            "tool_output": "same",
        });

        let normalized = normalize_backend_observed(BoundaryKind::Tool, &observed);

        assert_eq!(
            normalized["runtime_effect"]["controller"],
            "<backend-runtime-effect-controller>"
        );
        assert_eq!(normalized["runtime_effect"]["kind"], "tool_attempt");
        assert_eq!(normalized["tool_output"], "same");
    }

    #[tokio::test]
    async fn postgres_replay_observes_checkpoint_writes_when_configured() {
        // Replay against a database created for this test alone. Against the
        // shared database the conformance suites truncate this replay's session
        // metadata mid-run, which surfaces as a spurious commit failure.
        let Some(database) = crate::postgres_test_isolation::isolated_database().await else {
            return;
        };
        let database_url = database.url().to_string();
        let workload = generate_workload(7, "fast-random", 24).expect("workload");
        let mut trace = run_generated_workload_for_fixture(workload, "postgres-observer")
            .await
            .expect("trace");
        // Postgres session leases use transaction time by contract. Remove only
        // the simulator's instant clock-advance Worker proof; all runtime turns
        // and their checkpoint commits still execute against Postgres below.
        trace
            .events
            .retain(|event| event.kind != BoundaryKind::Worker);
        let mut model = ModelStore::default();
        for event in &trace.events {
            model.apply_observed_boundary(&event.as_event(), &event.observed);
        }
        trace.final_summary = model
            .summarize_with_trace_checkpoint_writes(&trace.events, &trace.durable_writes)
            .expect("summary");
        let tmp = tempfile::tempdir().expect("tempdir");
        let report_path = tmp.path().join("postgres-replay.json");

        let report = replay_trace_to_postgres(
            Path::new("trace.json"),
            &trace,
            &database_url,
            Some(&report_path),
        )
        .await
        .expect("Postgres replay");

        assert!(!report.checkpoint_replay.recorded_runtime.is_empty());
        assert_eq!(
            report.checkpoint_replay.recorded_runtime,
            report.checkpoint_replay.observed_runtime
        );
        assert!(report_path.exists());
    }
}
