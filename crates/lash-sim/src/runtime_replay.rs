use lash_sansio::SessionId;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lash_core::SessionStoreFactory;
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
use crate::scheduler::{BoundaryEvent, BoundaryKind, QueuedIngressMode};
use crate::store::{
    BackendCheckpointReplayEvidence, CheckpointWriteCollector, CheckpointWriteEvent, ModelStore,
    ObservedSessionStoreFactory,
};
use crate::trace::{AbstractWorldSummary, OracleVerdict, SimulationTrace, TraceIoError};

/// Constructors shared by the per-backend replay error enums. Each backend
/// keeps its own error type so the display prefixes stay exactly as they read
/// today; shared code builds variants through this trait.
pub(crate) trait BackendReplayError:
    std::fmt::Display
    + From<TraceIoError>
    + From<ReplayError>
    + From<std::io::Error>
    + From<serde_json::Error>
{
    fn runtime(message: String) -> Self;
    fn assertion(message: String) -> Self;
    fn divergence(message: String) -> Self;
}

/// Backend-independent detail of one mismatched boundary observation. Each
/// backend file wraps this in its own artifact-visible divergence struct.
pub(crate) struct BoundaryDivergence {
    pub boundary_id: String,
    pub boundary_kind: String,
    pub expected_observed: Value,
    pub actual_observed: Value,
}

/// A store backend that can host the shared runtime replay world.
pub(crate) trait ReplayBackend {
    type Error: BackendReplayError;

    /// Human label ("SQLite"/"Postgres") interpolated into oracle ids,
    /// assertion messages and replayed turn text.
    const TARGET: &'static str;

    /// Whether ingress asserts the opened session id equals the actor alias.
    /// Historically a SQLite-only check; kept per-backend deliberately.
    const ASSERT_INGRESS_SESSION_ID: bool;

    /// Session store factory for the world's observed store stack.
    fn session_store_factory(
        &self,
        clock: &Arc<crate::clock::SimClock>,
    ) -> Arc<dyn SessionStoreFactory>;

    /// Effect-replay store backing the runtime boundary harness.
    fn effect_replay_store(&self) -> RuntimeEffectReplayStore;

    /// Process execution env store handed to each replay core.
    async fn process_env_store(
        &self,
    ) -> Result<Arc<dyn lash::persistence::ProcessExecutionEnvStore>, Self::Error>;

    /// Directory root for file attachments written by replay cores.
    fn attachment_root(&self) -> PathBuf;

    /// Backend-specific observed-value normalization applied on top of the
    /// shared rules before comparing replayed and recorded observations.
    fn normalize_observed_extra(_kind: BoundaryKind, _value: &mut Value) {}

    /// Persist the backend's divergence artifact alongside the report.
    fn write_divergence_artifact(
        &self,
        trace_path: &Path,
        report_path: Option<&Path>,
        verdict: OracleVerdict,
        expected_summary: &AbstractWorldSummary,
        actual_summary: &AbstractWorldSummary,
        boundary: Option<BoundaryDivergence>,
    ) -> Result<(), Self::Error>;
}

/// Reopened-session facts shared by both backends. Backend reports map this
/// into their own evidence structs (SQLite additionally records the catalog
/// path).
pub(crate) struct ReopenedSessionObservation {
    pub session_id: SessionId,
    pub turn_index: usize,
    pub graph_node_count: usize,
    pub transcript_message_count: usize,
}

/// The report fields both backends compute identically; each entry point wraps
/// this in its own report struct.
pub(crate) struct RuntimeReplayOutcome {
    pub terminal_verdict: OracleVerdict,
    pub runtime_replayed_boundary_count: usize,
    pub replayed_boundary_families: Vec<String>,
    pub checkpoint_replay: BackendCheckpointReplayEvidence,
    pub reopened_sessions: Vec<ReopenedSessionObservation>,
    pub final_summary: AbstractWorldSummary,
}

pub(crate) async fn replay_trace_through_backend<B: ReplayBackend>(
    backend: B,
    trace_path: &Path,
    trace: &SimulationTrace,
    report_path: Option<&Path>,
) -> Result<RuntimeReplayOutcome, B::Error> {
    let model_replay = replay_trace(trace_path, trace)?;

    let mut world = RuntimeReplayWorld::new(backend, trace);
    let mut store = ModelStore::default();
    let mut provider_mutation_cache = ProviderMutationMatrixCache::default();
    let mut runtime_replayed_boundary_count = 0;
    let mut replayed_boundary_families = BTreeSet::new();
    for delivered in &trace.events {
        let event = delivered.as_event();
        let observed = if event.kind == BoundaryKind::BackendFailure {
            // Generation already exercised the real fault injector and recorded
            // its StoreError. Replaying the surrounding backend cannot recreate
            // that separately armed transaction, so preserve the real
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
                .map_err(|err| B::Error::runtime(err.to_string()))?
        } else {
            store.project_boundary_observation(&event)
        };
        runtime_replayed_boundary_count += 1;
        replayed_boundary_families.insert(event.kind.to_string());
        if normalize_backend_observed::<B>(event.kind, &observed)
            != normalize_backend_observed::<B>(event.kind, &delivered.observed)
        {
            store.apply_observed_boundary(&event, &observed);
            let actual_summary = store.summary();
            let verdict = OracleVerdict::failed(
                format!("sim.oracle.{}-boundary-replay.v1", B::TARGET.to_lowercase()),
                format!(
                    "{} replay boundary `{}` ({}) reproduced different observed data",
                    B::TARGET,
                    event.boundary_id,
                    event.kind
                ),
            );
            world.backend.write_divergence_artifact(
                trace_path,
                report_path,
                verdict.clone(),
                &trace.final_summary,
                &actual_summary,
                Some(BoundaryDivergence {
                    boundary_id: event.boundary_id,
                    boundary_kind: event.kind.to_string(),
                    expected_observed: delivered.observed.clone(),
                    actual_observed: observed,
                }),
            )?;
            return Err(B::Error::divergence(verdict.message));
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
                    .map_err(B::Error::divergence)?;
                let verdict = OracleVerdict::failed(
                    format!(
                        "sim.oracle.{}-checkpoint-replay.v1",
                        B::TARGET.to_lowercase()
                    ),
                    &message,
                );
                world.backend.write_divergence_artifact(
                    trace_path,
                    report_path,
                    verdict,
                    &trace.final_summary,
                    &actual_summary,
                    None,
                )?;
                return Err(B::Error::divergence(message));
            }
        };
    // Runtime-turn checkpoint facts are observed from the backend and compared
    // above. Contract-proof and suspend-fixture facts belong to projector-owned
    // boundaries this static lane does not execute, so only that explicit subset
    // is carried to keep the whole-trace summary comparable.
    let final_summary = store
        .summarize_with_trace_checkpoint_writes(&trace.events, &checkpoint_replay.summary_writes())
        .map_err(B::Error::divergence)?;
    let terminal_verdict = replay_determinism(&trace.final_summary, &final_summary);
    if !terminal_verdict.is_passed() {
        world.backend.write_divergence_artifact(
            trace_path,
            report_path,
            terminal_verdict.clone(),
            &trace.final_summary,
            &final_summary,
            None,
        )?;
        return Err(B::Error::divergence(terminal_verdict.message.clone()));
    }
    if final_summary != model_replay.final_summary {
        let verdict = OracleVerdict::failed(
            format!("sim.oracle.{}-model-replay.v1", B::TARGET.to_lowercase()),
            format!(
                "runtime {} replay summary diverged from model replay",
                B::TARGET
            ),
        );
        world.backend.write_divergence_artifact(
            trace_path,
            report_path,
            verdict.clone(),
            &model_replay.final_summary,
            &final_summary,
            None,
        )?;
        return Err(B::Error::divergence(format!(
            "runtime {} replay summary diverged from model replay",
            B::TARGET
        )));
    }

    let reopened_sessions = world.reopen_sessions().await?;
    Ok(RuntimeReplayOutcome {
        terminal_verdict,
        runtime_replayed_boundary_count,
        replayed_boundary_families: replayed_boundary_families.into_iter().collect(),
        checkpoint_replay,
        reopened_sessions,
        final_summary,
    })
}

struct RuntimeReplayWorld<B: ReplayBackend> {
    backend: B,
    sessions: BTreeMap<String, RuntimeReplaySession>,
    provider_completion_events: BTreeMap<String, BoundaryEvent>,
    queued_inputs: BTreeMap<String, String>,
    store_factory: Arc<dyn SessionStoreFactory>,
    checkpoint_writes: CheckpointWriteCollector,
    runtime_boundaries: RuntimeBoundaryHarness,
}

struct RuntimeReplaySession {
    _core: lash::LashCore,
    session: lash::LashSession,
    transport: Arc<ScriptedLlmHttpTransport>,
    provider_schedule: ScriptedTransportSchedule,
    provider_scripts: Vec<ProviderWireScript>,
    provider_kind: String,
    active_provider_turns: BTreeMap<String, ActiveProviderTurn>,
}

struct ActiveProviderTurn {
    handle: tokio::task::JoinHandle<Result<Value, String>>,
}

impl<B: ReplayBackend> RuntimeReplayWorld<B> {
    fn new(backend: B, trace: &SimulationTrace) -> Self {
        let clock = crate::clock::SimClock::new();
        let checkpoint_writes = CheckpointWriteCollector::default();
        let backend_factory: Arc<dyn SessionStoreFactory> = backend.session_store_factory(&clock);
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
                backend.effect_replay_store(),
                clock,
            ),
            backend,
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
    ) -> Result<Value, B::Error> {
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
            | BoundaryKind::LeaseTime => Err(B::Error::assertion(format!(
                "boundary `{}` ({}) is owned by the replay projector, not the {} runtime world",
                event.boundary_id,
                event.kind,
                B::TARGET
            ))),
        }
    }

    async fn deliver_runtime_boundary(&mut self, event: &BoundaryEvent) -> Result<Value, B::Error> {
        self.runtime_boundaries
            .deliver(event)
            .await
            .map_err(|err| B::Error::runtime(err.to_string()))
    }

    async fn open_runtime_session(&mut self, event: &BoundaryEvent) -> Result<Value, B::Error> {
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
                B::Error::assertion(format!(
                    "ingress boundary `{}` missing provider_texts",
                    event.boundary_id
                ))
            })?;
        if provider_texts.is_empty() {
            return Err(B::Error::assertion(format!(
                "ingress boundary `{}` provided no runtime provider scripts",
                event.boundary_id
            )));
        }
        let provider_kind = event
            .payload
            .get("provider_kind")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                B::Error::assertion(format!(
                    "ingress boundary `{}` missing provider_kind",
                    event.boundary_id
                ))
            })?;
        let scripts = runtime_provider_scripts_for_texts(provider_kind, &provider_texts)
            .map_err(|err| B::Error::runtime(err.to_string()))?;
        let provider_schedule = ScriptedTransportSchedule::new();
        let (core, transport, provider_kind) = runtime_core_for_scripts(
            &self.backend,
            Arc::clone(&self.store_factory),
            provider_kind,
            scripts.clone(),
            Some(provider_schedule.clone()),
        )
        .await?;
        let session = core
            .session(event.actor_alias.clone())
            .open()
            .await
            .map_err(|err| B::Error::runtime(err.to_string()))?;
        if B::ASSERT_INGRESS_SESSION_ID && session.session_id() != event.actor_alias {
            return Err(B::Error::assertion(format!(
                "ingress opened session `{}`, expected `{}`",
                session.session_id(),
                event.actor_alias
            )));
        }
        self.sessions.insert(
            event.actor_alias.clone(),
            RuntimeReplaySession {
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

    async fn queue_turn_input(&mut self, event: &BoundaryEvent) -> Result<Value, B::Error> {
        let runtime_session = self.sessions.get(&event.actor_alias).ok_or_else(|| {
            B::Error::assertion(format!(
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
            .durable()
            .enqueue(lash::TurnInput::text(text.to_string()))
            .id(source_key);
        let ingress_mode = event
            .queued_ingress_mode()
            .map_err(|err| B::Error::runtime(err.to_string()))?;
        if ingress_mode == QueuedIngressMode::ActiveTurn {
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
            .map_err(|err| B::Error::runtime(err.to_string()))?;
        self.queued_inputs
            .insert(event.boundary_id.clone(), acceptance.input_id.to_string());
        let input_state = lash_core::TurnInputState::open(acceptance.ingress.clone());
        Ok(json!({
            "session": event.actor_alias,
            "queued_ingress": true,
            "source_key": source_key,
            "input_id": acceptance.input_id,
            "input_state": input_state.as_str(),
            "ingress_mode": ingress_mode.as_str(),
            "active_turn_id": event.payload.get("active_turn_id").cloned().unwrap_or(Value::Null),
        }))
    }

    async fn ensure_provider_turn_started(
        &mut self,
        actor_alias: &str,
        turn_boundary_id: &str,
    ) -> Result<(), B::Error> {
        let runtime_session = self.sessions.get_mut(actor_alias).ok_or_else(|| {
            B::Error::assertion(format!(
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
                B::Error::assertion(format!(
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
                B::Error::assertion(format!(
                    "provider boundary `{}` had no runtime provider script for turn {}",
                    event.boundary_id, expected_turn_index
                ))
            })?;
        if expected_text.is_empty() {
            return Err(B::Error::assertion(format!(
                "provider boundary `{}` missing expected text",
                event.boundary_id
            )));
        }
        let session = runtime_session.session.clone();
        let transport = Arc::clone(&runtime_session.transport);
        let provider_kind = runtime_session.provider_kind.clone();
        let handle = tokio::spawn(async move {
            run_provider_turn_task::<B>(session, transport, provider_kind, event)
                .await
                .map_err(|err| err.to_string())
        });
        runtime_session
            .active_provider_turns
            .insert(turn_boundary_id.to_string(), ActiveProviderTurn { handle });
        Ok(())
    }

    async fn release_provider_event(&mut self, event: &BoundaryEvent) -> Result<Value, B::Error> {
        let turn_boundary_id = event
            .payload
            .get("turn_boundary_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                B::Error::assertion(format!(
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
                B::Error::assertion(format!(
                    "provider event `{}` missing event_index",
                    event.boundary_id
                ))
            })? as usize;
        let exchange_index = event
            .payload
            .get("exchange_index")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                B::Error::assertion(format!(
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
            B::Error::assertion(format!(
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

    async fn finish_provider_turn(&mut self, event: &BoundaryEvent) -> Result<Value, B::Error> {
        self.ensure_provider_turn_started(&event.actor_alias, &event.boundary_id)
            .await?;
        let runtime_session = self.sessions.get_mut(&event.actor_alias).ok_or_else(|| {
            B::Error::assertion(format!(
                "provider boundary `{}` ran before ingress for `{}`",
                event.boundary_id, event.actor_alias
            ))
        })?;
        let active_turn = runtime_session
            .active_provider_turns
            .remove(&event.boundary_id)
            .ok_or_else(|| {
                B::Error::assertion(format!(
                    "provider boundary `{}` was not active",
                    event.boundary_id
                ))
            })?;
        let release_count = runtime_session.provider_schedule.releases().len();
        let exchange_count = runtime_session
            .transport
            .exchanges()
            .map_err(|err| B::Error::runtime(err.to_string()))?
            .len();
        active_turn
            .handle
            .await
            .map_err(|err| {
                B::Error::runtime(format!(
                    "provider boundary `{}` failed after {release_count} scheduled releases and {exchange_count} provider exchanges: {err}",
                    event.boundary_id
                ))
            })?
            .map_err(B::Error::runtime)
    }

    fn observe_session(
        &self,
        event: &BoundaryEvent,
        original_observed: &Value,
    ) -> Result<Value, B::Error> {
        let runtime_session = self.sessions.get(&event.actor_alias).ok_or_else(|| {
            B::Error::assertion(format!(
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
            return Err(B::Error::assertion(format!(
                "{} observer invariants failed for `{}`",
                B::TARGET,
                event.boundary_id
            )));
        }
        if transcript_message_count
            != original_observed
                .get("transcript_message_count")
                .and_then(Value::as_u64)
                .unwrap_or(transcript_message_count as u64) as usize
        {
            return Err(B::Error::divergence(format!(
                "{} observer transcript count changed for `{}`",
                B::TARGET,
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

    async fn cancel_queued_input(&mut self, event: &BoundaryEvent) -> Result<Value, B::Error> {
        let runtime_session = self.sessions.get(&event.actor_alias).ok_or_else(|| {
            B::Error::assertion(format!(
                "cancellation boundary `{}` ran before ingress for `{}`",
                event.boundary_id, event.actor_alias
            ))
        })?;
        let target = event
            .payload
            .get("target")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                B::Error::assertion(format!(
                    "cancellation boundary `{}` missing target",
                    event.boundary_id
                ))
            })?;
        let input_id = self.queued_inputs.get(target).cloned().ok_or_else(|| {
            B::Error::assertion(format!(
                "cancellation boundary `{}` target `{target}` was not queued",
                event.boundary_id
            ))
        })?;
        let outcome = runtime_session
            .session
            .durable()
            .cancel_pending_turn_input(&lash_core::InputId::from(input_id.as_str()))
            .await
            .map_err(|err| B::Error::runtime(err.to_string()))?;
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

    async fn reopen_sessions(&self) -> Result<Vec<ReopenedSessionObservation>, B::Error> {
        let mut evidence = Vec::new();
        for (session_id, runtime_session) in &self.sessions {
            let (core, _, _) = runtime_core_for_scripts(
                &self.backend,
                Arc::clone(&self.store_factory),
                &runtime_session.provider_kind,
                Vec::new(),
                None,
            )
            .await?;
            let session = core
                .session(session_id.clone())
                .open()
                .await
                .map_err(|err| B::Error::runtime(err.to_string()))?;
            let observation = session.observe().current_observation();
            let read_view = observation.read_view;
            evidence.push(ReopenedSessionObservation {
                session_id: SessionId::from(session_id.clone()),
                turn_index: read_view.turn_index(),
                graph_node_count: read_view.session_graph().nodes.len(),
                transcript_message_count: read_view.messages().len(),
            });
        }
        Ok(evidence)
    }
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
async fn run_provider_turn_task<B: ReplayBackend>(
    session: lash::LashSession,
    transport: Arc<ScriptedLlmHttpTransport>,
    provider_kind: String,
    event: BoundaryEvent,
) -> Result<Value, B::Error> {
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
            "Replay generated provider turn {} through {}.",
            event.boundary_id,
            B::TARGET
        )))
        .turn_id(event.boundary_id.clone())
        .run()
        .await
        .map_err(|err| B::Error::runtime(err.to_string()))?;
    let assistant_message = output.assistant_message().unwrap_or_default().to_string();
    let read_view = output
        .result
        .state
        .read_view()
        .expect("runtime frame scope resolves");
    let graph_node_count = output.result.state.session_graph.nodes.len();
    let transcript_message_count = read_view.messages().len();
    let provider_exchange_count = transport
        .exchanges()
        .map_err(|err| B::Error::runtime(err.to_string()))?
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
        &SessionId::from(event.actor_alias.clone()),
        expected_turn_index,
        expected_text,
        expected_exchange_count,
    );
    if let Err(message) = require_passed(&runtime_contract) {
        return Err(B::Error::assertion(format!(
            "{} runtime invariants failed for `{}`: {message}",
            B::TARGET,
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
            "graph_acyclic": graph_invariant.cycle_node_ids.is_empty(),
            "single_active_agent_frame": agent_frame_invariant.active_frame_ids.len() == 1,
            "usage_monotonic": usage_invariant.usage_events_monotonic,
            "transcript_contains_provider_output": read_view.messages().iter().any(|message| {
                message.parts.iter().any(|part| part.content().contains(expected_text))
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

async fn runtime_core_for_scripts<B: ReplayBackend>(
    backend: &B,
    store_factory: Arc<dyn SessionStoreFactory>,
    provider_kind: &str,
    scripts: Vec<ProviderWireScript>,
    provider_schedule: Option<ScriptedTransportSchedule>,
) -> Result<(lash::LashCore, Arc<ScriptedLlmHttpTransport>, String), B::Error> {
    let mut transport = ScriptedLlmHttpTransport::from_scripts(scripts)
        .map_err(|err| B::Error::runtime(err.to_string()))?;
    if let Some(schedule) = provider_schedule {
        transport = transport.with_event_schedule(schedule);
    }
    let transport = Arc::new(transport);
    let (provider_handle, model, provider_kind) =
        runtime_provider_components(provider_kind, &transport)
            .map_err(|err| B::Error::runtime(err.to_string()))?;
    let process_env_store = backend.process_env_store().await?;
    let core = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        // Recorded provider boundaries own execution, just as in generation.
        // A native queued-input wake can acquire an admission lease before the
        // spawned provider task starts, making replay depend on task scheduling.
        // Dedicated runtime boundaries exercise queued work separately.
        .without_queued_work()
        .effect_host(Arc::new(
            lash::durability::NativeEffectHost::default().allow_process_lifetime_completion_keys(),
        ))
        .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
            backend.attachment_root(),
        )))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(process_env_store)
        .store_factory(store_factory)
        .lease_timings(crate::lease::sim_runtime_lease_timings())
        .provider(provider_handle)
        .model(model)
        .build(crate::sim_process_owner())
        .map_err(|err| B::Error::runtime(err.to_string()))?;
    Ok((core, transport, provider_kind))
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

pub(crate) fn normalize_backend_observed<B: ReplayBackend>(
    kind: BoundaryKind,
    value: &Value,
) -> Value {
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
    B::normalize_observed_extra(kind, &mut normalized);
    normalized
}
