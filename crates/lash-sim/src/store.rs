use std::collections::{BTreeMap, BTreeSet};

use lash_core::StoreError;
use serde_json::{Value, json};

use crate::runtime_contracts::{RuntimeTurnObservation, runtime_turn_contract};
use crate::scheduler::{BoundaryEvent, BoundaryKind};
use crate::trace::{
    AbstractWorldSummary, DurableEffectAbstractSummary, SessionAbstractSummary,
    WorkerAbstractSummary, value_digest,
};

pub fn backend_fault_observation(
    session: Value,
    operation: String,
    attempt: usize,
    retryable: bool,
) -> Value {
    let store_error = backend_fault_store_error(&operation, attempt, retryable);
    let store_error_variant = store_error_variant(&store_error);
    json!({
        "session": session,
        "backend_failure": true,
        "operation": operation,
        "attempt": attempt,
        "retryable": retryable,
        "store_error_class": if retryable { "retryable_conflict" } else { "terminal_backend_error" },
        "production_store_error": {
            "type": "lash_core::StoreError",
            "variant": store_error_variant,
            "message": store_error.to_string(),
            "retryable_class": retryable,
        },
    })
}

fn backend_fault_store_error(operation: &str, attempt: usize, retryable: bool) -> StoreError {
    if retryable {
        StoreError::HeadRevisionConflict {
            expected: attempt.checked_sub(1).map(|value| value as u64),
            actual: attempt as u64,
        }
    } else {
        StoreError::Backend(format!(
            "simulated terminal backend failure during {operation}"
        ))
    }
}

fn store_error_variant(error: &StoreError) -> &'static str {
    match error {
        StoreError::HeadRevisionConflict { .. } => "HeadRevisionConflict",
        StoreError::Backend(_) => "Backend",
        StoreError::SessionBindingMismatch { .. } => "SessionBindingMismatch",
        StoreError::UnsupportedReadScope(_) => "UnsupportedReadScope",
        StoreError::RuntimeTurnCommitConflict { .. } => "RuntimeTurnCommitConflict",
        StoreError::QueuedWorkClaimSuperseded { .. } => "QueuedWorkClaimSuperseded",
        StoreError::TurnInputClaimSuperseded { .. } => "TurnInputClaimSuperseded",
        StoreError::UnsettledQueuedWorkClaim { .. } => "UnsettledQueuedWorkClaim",
        StoreError::UnsettledTurnInputClaim { .. } => "UnsettledTurnInputClaim",
        StoreError::PendingTurnInputSourceKeyConflict { .. } => "PendingTurnInputSourceKeyConflict",
        StoreError::SessionExecutionLeaseExpired { .. } => "SessionExecutionLeaseExpired",
        StoreError::UnsupportedRecordSchemaVersion { .. } => "UnsupportedRecordSchemaVersion",
        StoreError::MissingRecordSchemaVersion { .. } => "MissingRecordSchemaVersion",
        StoreError::InvalidRecordSchemaVersion { .. } => "InvalidRecordSchemaVersion",
    }
}

#[derive(Clone, Debug, Default)]
pub struct ModelStore {
    sessions: BTreeMap<String, ModelSession>,
    durable_effects: BTreeMap<String, ModelDurableEffect>,
    workers: BTreeMap<String, ModelWorker>,
    #[allow(clippy::struct_field_names)]
    durable_projection_entries: BTreeMap<String, ModelDurableProjectionEntry>,
    backend_attempts_by_operation: BTreeMap<String, usize>,
    tool_completions: BTreeMap<String, usize>,
    exec_executions: BTreeMap<String, usize>,
    rejected_provider_mutations: BTreeSet<String>,
    delivered_process_wake_ids: BTreeSet<String>,
    queued_input_boundaries: BTreeSet<String>,
    pending_turn_input_seq_by_session: BTreeMap<String, u64>,
    total_events: usize,
}

impl ModelStore {
    pub fn open_session(&mut self, alias: impl Into<String>) {
        let alias = alias.into();
        self.sessions
            .entry(alias.clone())
            .or_insert_with(|| ModelSession::new(alias))
            .opened = true;
    }

    pub fn apply_boundary(&mut self, event: &BoundaryEvent) -> Value {
        let observed = self.project_boundary_observation(event);
        self.apply_observed_boundary(event, &observed);
        observed
    }

    pub fn apply_observed_boundary(&mut self, event: &BoundaryEvent, observed: &Value) {
        self.total_events += 1;
        // Suspend sessions are a generated-runtime mechanism (a real turn parked
        // on an await key), not an abstract runtime session. They are delivered
        // and counted, but never tracked in the abstract session model, so the
        // session-shaped oracles do not see a session without provider/observer
        // structure.
        if is_suspend_boundary(event) {
            return;
        }
        match event.kind {
            BoundaryKind::Ingress => {
                self.open_session(event.actor_alias.clone());
                let session = self
                    .sessions
                    .get_mut(&event.actor_alias)
                    .expect("session was opened");
                session.ingress_count += 1;
            }
            BoundaryKind::QueuedIngress => {
                let session = self.ensure_session(event.actor_alias.clone());
                session.queued_ingress_count += 1;
                self.queued_input_boundaries
                    .insert(event.boundary_id.clone());
            }
            BoundaryKind::Provider => {
                let session = self.ensure_session(event.actor_alias.clone());
                let text = observed
                    .get("provider_output")
                    .and_then(Value::as_str)
                    .or_else(|| event.payload.get("text").and_then(Value::as_str))
                    .unwrap_or("")
                    .to_string();
                session.provider_outputs.push(text);
                if let Some(provider_exchange_count) = observed
                    .get("provider_exchange_count")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize)
                {
                    session
                        .provider_exchange_counts
                        .push(provider_exchange_count);
                }
                if let Some(graph_node_count) = observed
                    .get("graph_node_count")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize)
                {
                    session.graph_node_counts.push(graph_node_count);
                }
                if let Some(transcript_message_count) = observed
                    .get("transcript_message_count")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize)
                {
                    session
                        .transcript_message_counts
                        .push(transcript_message_count);
                }
            }
            BoundaryKind::ProviderEvent => {
                self.ensure_session(event.actor_alias.clone());
            }
            BoundaryKind::Tool => {
                let session = self.ensure_session(event.actor_alias.clone());
                let output = observed
                    .get("tool_output")
                    .and_then(Value::as_str)
                    .or_else(|| event.payload.get("output").and_then(Value::as_str))
                    .unwrap_or("")
                    .to_string();
                session.tool_outputs.push(output);
            }
            BoundaryKind::ExecCode => {
                let session = self.ensure_session(event.actor_alias.clone());
                let output = observed
                    .get("exec_output")
                    .and_then(Value::as_str)
                    .or_else(|| event.payload.get("output").and_then(Value::as_str))
                    .unwrap_or("")
                    .to_string();
                session.exec_code_outputs.push(output);
            }
            BoundaryKind::DurableEffect => {
                let session_alias = boundary_session_alias(event);
                let key = observed
                    .get("durable_key")
                    .and_then(Value::as_str)
                    .or_else(|| event.payload.get("durable_key").and_then(Value::as_str))
                    .unwrap_or(&event.boundary_id)
                    .to_string();
                self.ensure_session(session_alias.clone())
                    .durable_effect_keys
                    .push(key.clone());
                self.durable_effects.insert(
                    key.clone(),
                    ModelDurableEffect::from_observed(key, observed),
                );
            }
            BoundaryKind::Worker => {
                let worker_alias = observed
                    .get("worker_alias")
                    .and_then(Value::as_str)
                    .unwrap_or(&event.actor_alias)
                    .to_string();
                self.workers.insert(
                    worker_alias.clone(),
                    ModelWorker::from_observed(worker_alias, observed),
                );
            }
            BoundaryKind::ProcessWake => {
                let session = self.ensure_session(boundary_session_alias(event));
                session.process_wake_count += 1;
            }
            BoundaryKind::ProcessLifecycle => {
                // The disposition/evidence verdicts live in the boundary's real
                // observed (`runtime_process_lifecycle`, read by the lifecycle
                // oracles); the abstract model tracks only the presence count so
                // the cross-backend summary stays reproducible.
                let session = self.ensure_session(boundary_session_alias(event));
                session.process_lifecycle_count += 1;
            }
            BoundaryKind::Observer => {
                let session = self.ensure_session(event.actor_alias.clone());
                let turn_index = observed
                    .get("turn_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(session.provider_outputs.len() as u64)
                    as usize;
                session.observer_turn_indices.push(turn_index);
                if observed
                    .get("reconnected")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    session.observer_reconnects += 1;
                }
            }
            BoundaryKind::Cancellation => {
                let session = self.ensure_session(event.actor_alias.clone());
                session.cancellation_count += 1;
            }
            BoundaryKind::Trigger => {
                let session = self.ensure_session(boundary_session_alias(event));
                session.trigger_count += 1;
            }
            BoundaryKind::BackendFailure => {
                let session = self.ensure_session(boundary_session_alias(event));
                session.backend_failure_count += 1;
            }
            BoundaryKind::ProviderMutation => {
                let session = self.ensure_session(event.actor_alias.clone());
                session.provider_mutation_count += 1;
            }
            BoundaryKind::LeaseTime => {
                let session = self.ensure_session(event.actor_alias.clone());
                let tick = event
                    .payload
                    .get("tick")
                    .and_then(Value::as_u64)
                    .unwrap_or(event.at);
                session.lease_time_ticks.push(tick);
            }
        }
    }

    pub fn summary(&self) -> AbstractWorldSummary {
        let sessions = self
            .sessions
            .values()
            .map(ModelSession::summary)
            .collect::<Vec<_>>();
        let durable_effects = self
            .durable_effects
            .values()
            .map(ModelDurableEffect::summary)
            .collect::<Vec<_>>();
        let workers = self
            .workers
            .values()
            .map(ModelWorker::summary)
            .collect::<Vec<_>>();
        AbstractWorldSummary::with_digest(
            self.sessions.len(),
            self.total_events,
            sessions,
            durable_effects,
            workers,
        )
    }

    pub fn project_boundary_observation(&mut self, event: &BoundaryEvent) -> Value {
        if let Some(observed) = project_suspend_boundary(event) {
            return observed;
        }
        match event.kind {
            BoundaryKind::Ingress => json!({
                "session": event.actor_alias,
                "opened": true,
                "ingress_count": self
                    .sessions
                    .get(&event.actor_alias)
                    .map_or(1, |session| session.ingress_count + 1),
            }),
            BoundaryKind::QueuedIngress => {
                let next_seq = self
                    .pending_turn_input_seq_by_session
                    .entry(event.actor_alias.clone())
                    .or_default();
                *next_seq = next_seq.saturating_add(1);
                let ingress_mode = event
                    .payload
                    .get("ingress_mode")
                    .and_then(Value::as_str)
                    .unwrap_or("next_turn");
                let input_state = if ingress_mode == "active_turn" {
                    "pending_active"
                } else {
                    "deferred_next_turn"
                };
                json!({
                    "session": event.actor_alias,
                    "queued_ingress": true,
                    "source_key": event.payload.get("source_key").cloned().unwrap_or(Value::Null),
                    "input_id": format!("recording-ti-{}", *next_seq),
                    "input_state": input_state,
                    "ingress_mode": ingress_mode,
                    "active_turn_id": event.payload.get("active_turn_id").cloned().unwrap_or(Value::Null),
                })
            }
            BoundaryKind::Provider => {
                let turn_index = self
                    .sessions
                    .get(&event.actor_alias)
                    .map_or(1, |session| session.provider_outputs.len() + 1);
                let text = event
                    .payload
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let provider_exchange_count = event
                    .payload
                    .get("expected_provider_exchange_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(turn_index as u64)
                    as usize;
                let graph_node_count = event
                    .payload
                    .get("expected_graph_node_count")
                    .and_then(Value::as_u64)
                    .unwrap_or((turn_index * 2) as u64)
                    as usize;
                let transcript_message_count = event
                    .payload
                    .get("expected_transcript_message_count")
                    .and_then(Value::as_u64)
                    .unwrap_or((turn_index * 2) as u64)
                    as usize;
                let runtime_contract = runtime_turn_contract(
                    &RuntimeTurnObservation {
                        session_id: event.actor_alias.clone(),
                        turn_index,
                        assistant_message: text.clone(),
                        graph_node_count,
                        transcript_message_count,
                        activity_count: 1,
                        provider_exchange_count,
                        graph_invariant: Default::default(),
                        agent_frame_invariant: Default::default(),
                        usage_invariant: Default::default(),
                    },
                    &event.actor_alias,
                    turn_index,
                    &text,
                    provider_exchange_count,
                );
                json!({
                    "session": event.actor_alias,
                    "runtime_session_id": event.actor_alias,
                    "turn_index": turn_index,
                    "success": true,
                    "provider_output": text,
                    "provider_script": event.payload.get("script").cloned().unwrap_or(Value::Null),
                    "provider_exchange_count": provider_exchange_count,
                    "graph_node_count": graph_node_count,
                    "transcript_message_count": transcript_message_count,
                    "activity_count_nonzero": true,
                    "provider_kind": event
                        .payload
                        .get("provider_kind")
                        .and_then(Value::as_str)
                        .unwrap_or("openai-compatible"),
                    "runtime_invariants": {
                        "session_id": true,
                        "turn_index": true,
                        "graph_non_empty": true,
                        "transcript_contains_provider_output": true,
                        "activity_count_nonzero": true,
                    },
                    "runtime_contract": runtime_contract,
                })
            }
            BoundaryKind::ProviderEvent => json!({
                "session": event.actor_alias,
                "provider_event_release": true,
                "turn_boundary_id": event
                    .payload
                    .get("turn_boundary_id")
                    .cloned()
                    .unwrap_or(Value::Null),
                "exchange_index": event
                    .payload
                    .get("exchange_index")
                    .cloned()
                    .unwrap_or(Value::Null),
                "event_index": event
                    .payload
                    .get("event_index")
                    .cloned()
                    .unwrap_or(Value::Null),
                "event_name": event
                    .payload
                    .get("event_name")
                    .cloned()
                    .unwrap_or(Value::Null),
                "provider_kind": event
                    .payload
                    .get("provider_kind")
                    .cloned()
                    .unwrap_or(Value::Null),
                "active_turn_pending_before_release": true,
                "released_while_turn_pending": true,
                "scripted_transport_release": {
                    "exchange_index": event
                        .payload
                        .get("exchange_index")
                        .cloned()
                        .unwrap_or(Value::Null),
                    "event_index": event
                        .payload
                        .get("event_index")
                        .cloned()
                        .unwrap_or(Value::Null),
                    "event_name": event
                        .payload
                        .get("event_name")
                        .cloned()
                        .unwrap_or(Value::Null),
                    "at": event.at,
                    "blocked_before_release": true,
                },
            }),
            BoundaryKind::Tool => {
                let count = self
                    .tool_completions
                    .entry(event.boundary_id.clone())
                    .or_insert(0);
                *count += 1;
                let output = event
                    .payload
                    .get("output")
                    .cloned()
                    .unwrap_or_else(|| json!(""));
                let tool_name = event
                    .payload
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("sim_tool");
                let tool_output = lash_core::ToolCallOutput::success(output.clone());
                json!({
                    "session": event.actor_alias,
                    "tool_output": output,
                    "tool_name": tool_name,
                    "tool_call_id": event.boundary_id,
                    "execution_count": *count,
                    "runtime_tool_output": tool_output,
                })
            }
            BoundaryKind::ExecCode => {
                let count = self
                    .exec_executions
                    .entry(event.boundary_id.clone())
                    .or_insert(0);
                *count += 1;
                let output = event
                    .payload
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let exit_code = event
                    .payload
                    .get("exit_code")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let response = lash_core::ExecResponse {
                    observations: vec![output.clone()],
                    observation_truncation: Vec::new(),
                    tool_calls: Vec::new(),
                    images: Vec::new(),
                    printed_images: Vec::new(),
                    error: (exit_code != 0).then(|| format!("exit code {exit_code}")),
                    duration_ms: 0,
                    terminal_finish: Some(json!({
                        "output": output,
                        "exit_code": exit_code,
                    })),
                };
                let outcome = lash_core::RuntimeEffectOutcome::ExecCode {
                    result: Box::new(Ok(response)),
                };
                json!({
                    "session": event.actor_alias,
                    "exec_output": output,
                    "exit_code": exit_code,
                    "execution_count": *count,
                    "runtime_effect_outcome": outcome,
                })
            }
            BoundaryKind::DurableEffect => {
                let durable_key = event
                    .payload
                    .get("durable_key")
                    .and_then(Value::as_str)
                    .unwrap_or(&event.boundary_id)
                    .to_string();
                let result = event
                    .payload
                    .get("result")
                    .cloned()
                    .unwrap_or_else(|| json!({"completed": true}));
                self.project_durable_effect(event, durable_key, result)
            }
            BoundaryKind::Worker => {
                // Worker lease fencing (incarnation change, monotonic fencing
                // token, stale-completion rejection, and second-owner work
                // continuation) is produced by the REAL session-execution lease
                // store in `runtime_boundaries::run_worker_stale_completion`, and
                // is NOT abstractly derivable from the boundary stream. The model
                // carries the REAL reclaim/fence facts, threaded from the
                // recorded observation via `apply_observed_boundary` (see
                // `replay_trace`); cross-store reproduction is re-verified by the
                // SQLite/Postgres backend replays, which re-run the real lease
                // store. This abstract projection therefore reports identity only
                // and deliberately fabricates NO fencing, so no path can make the
                // worker oracle pass without the real store actually fencing.
                json!({
                    "worker_alias": event.actor_alias,
                    "session": boundary_session_alias(event),
                })
            }
            BoundaryKind::ProcessWake => {
                let session = boundary_session_alias(event);
                let process_id = format!("sim-process-{}", event.boundary_id.replace(':', "-"));
                let dedupe_key = event
                    .payload
                    .get("dedupe_key")
                    .and_then(Value::as_str)
                    .unwrap_or(&event.boundary_id)
                    .to_string();
                let wake =
                    lash_core::process_wake_delivery(lash_core::ProcessWakeDeliveryRequest {
                        target_scope: lash_core::SessionScope::new(session.clone()),
                        process_id: process_id.clone(),
                        sequence: 1,
                        event_type: "process.wake".to_string(),
                        event_invocation: lash_core::RuntimeInvocation {
                            scope: lash_core::runtime::RuntimeScope::new(session.clone()),
                            subject: lash_core::runtime::RuntimeSubject::ProcessEvent {
                                process_id: process_id.clone(),
                                sequence: 1,
                                event_type: "process.wake".to_string(),
                            },
                            caused_by: None,
                            replay: Some(lash_core::runtime::RuntimeReplay {
                                key: format!("process:{process_id}:wake:{dedupe_key}"),
                            }),
                        },
                        process_caused_by: None,
                        wake: lash_core::ProcessWake {
                            input: format!("wake for {session}"),
                            dedupe_key: dedupe_key.clone(),
                        },
                        occurred_at: std::time::UNIX_EPOCH
                            + std::time::Duration::from_millis(event.at),
                    })
                    .expect("sim process wake delivery request is deterministic and valid");
                let wake_id = wake.wake_id.clone();
                let claimed_once = self
                    .delivered_process_wake_ids
                    .insert(wake.dedupe_key.clone());
                let mut observed = json!({
                    "process_wake": true,
                    "process_id": process_id,
                    "sequence": 1,
                    "wake_id": wake_id,
                    "dedupe_key": dedupe_key,
                    "claimed_once": claimed_once,
                    "runtime_process_wake": wake,
                });
                if !event
                    .payload
                    .get("omit_join_session")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    observed
                        .as_object_mut()
                        .expect("process wake observed object")
                        .insert("session".to_string(), Value::String(session));
                }
                observed
            }
            BoundaryKind::ProcessLifecycle => {
                // Identity only: the disposition-driven recovery verdicts are
                // produced by the REAL `DurableProcessWorker` sweep in
                // `runtime_boundaries::run_process_lifecycle` and are not
                // re-derivable from the boundary stream. The model carries the
                // recorded real facts (threaded via `apply_observed_boundary` on
                // replay, like `Worker`), and `runtime_process_lifecycle` is
                // normalized away for cross-backend equality — so no path can make
                // a lifecycle oracle pass without the real sweep producing them.
                json!({
                    "session": boundary_session_alias(event),
                    "process_lifecycle": true,
                })
            }
            BoundaryKind::Observer => {
                let turn_index = self
                    .sessions
                    .get(&event.actor_alias)
                    .map_or(0, |session| session.provider_outputs.len());
                json!({
                    "session": event.actor_alias,
                    "turn_index": turn_index,
                    "reconnected": event.payload
                        .get("reconnect")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    "graph_node_count": event.payload
                        .get("expected_graph_node_count")
                        .and_then(Value::as_u64)
                        .unwrap_or((turn_index * 2) as u64),
                    "transcript_message_count": event.payload
                        .get("expected_transcript_message_count")
                        .and_then(Value::as_u64)
                        .unwrap_or((turn_index * 2) as u64),
                    "observer_invariants": {
                        "session_id": true,
                        "turn_index_converged": true,
                        "graph_non_empty": turn_index > 0,
                        "transcript_message_count_converged": true,
                    },
                })
            }
            BoundaryKind::Cancellation => {
                let target = event.payload.get("target").and_then(Value::as_str);
                let cancelled =
                    target.is_some_and(|target| self.queued_input_boundaries.contains(target));
                json!({
                    "session": event.actor_alias,
                    "target": event.payload.get("target").cloned().unwrap_or(Value::Null),
                    "cancelled": cancelled,
                    "cancel_outcome": if cancelled { "cancelled" } else { "not_found" },
                })
            }
            BoundaryKind::Trigger => {
                let session = boundary_session_alias(event);
                let source_key = event
                    .payload
                    .get("source_key")
                    .and_then(Value::as_str)
                    .unwrap_or(&event.boundary_id)
                    .to_string();
                let request = lash_core::TriggerOccurrenceRequest::new(
                    "sim.trigger",
                    source_key.clone(),
                    json!({
                        "boundary_id": event.boundary_id,
                        "session": session,
                    }),
                    format!("sim-trigger:{}", event.boundary_id),
                )
                .with_source(json!({"sim": true}));
                let occurrence_id = lash_core::deterministic_occurrence_id(&request)
                    .unwrap_or_else(|_| format!("trigger:{}", event.boundary_id));
                let mut observed = json!({
                    "session": session,
                    "trigger_delivered": true,
                    "source_key": source_key,
                    "occurrence_id": occurrence_id,
                    "reservation_count": 1,
                    "started_process": event.payload.get("started_process").cloned().unwrap_or(Value::Bool(true)),
                });
                if let Some(execution) = event.payload.get("contract_execution") {
                    observed
                        .as_object_mut()
                        .expect("trigger observed object")
                        .insert("contract_execution".to_string(), execution.clone());
                }
                observed
            }
            BoundaryKind::BackendFailure => {
                let operation = event
                    .payload
                    .get("operation")
                    .and_then(Value::as_str)
                    .unwrap_or("backend_operation")
                    .to_string();
                let attempts = self
                    .backend_attempts_by_operation
                    .entry(operation.clone())
                    .or_insert(0);
                *attempts += 1;
                let retryable = event
                    .payload
                    .get("retryable")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                backend_fault_observation(
                    json!(boundary_session_alias(event)),
                    operation,
                    *attempts,
                    retryable,
                )
            }
            BoundaryKind::ProviderMutation => {
                let mutation = event
                    .payload
                    .get("mutation")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown_mutation")
                    .to_string();
                let mutation_key = format!("{}:{mutation}", event.actor_alias);
                let first_rejection = self.rejected_provider_mutations.insert(mutation_key);
                json!({
                    "session": event.actor_alias,
                    "provider_mutation": true,
                    "mutation": mutation,
                    "rejected": true,
                    "first_rejection": first_rejection,
                    "oracle": event.payload.get("oracle").cloned().unwrap_or(Value::Null),
                })
            }
            BoundaryKind::LeaseTime => {
                let tick = event
                    .payload
                    .get("tick")
                    .and_then(Value::as_u64)
                    .unwrap_or(event.at);
                let previous_tick = self
                    .sessions
                    .get(&event.actor_alias)
                    .and_then(|session| session.lease_time_ticks.last().copied());
                json!({
                    "session": event.actor_alias,
                    "lease_time_tick": tick,
                    "monotonic": previous_tick.is_none_or(|previous| previous <= tick),
                })
            }
        }
    }

    fn ensure_session(&mut self, alias: impl Into<String>) -> &mut ModelSession {
        let alias = alias.into();
        self.sessions
            .entry(alias.clone())
            .or_insert_with(|| ModelSession::new(alias))
    }

    /// COVERAGE-ONLY abstract model projection.
    ///
    /// This projection makes generated model states comparable; its synthetic
    /// `execution_count` is not evidence for the `durable_effect_exactly_once`
    /// runtime oracle. That oracle consumes the real replay controller's local
    /// execution count from `runtime_boundaries`.
    fn project_durable_effect(
        &mut self,
        event: &BoundaryEvent,
        durable_key: String,
        result: Value,
    ) -> Value {
        let effect_id = event
            .payload
            .get("runtime_effect")
            .and_then(|runtime_effect| runtime_effect.get("effect_id"))
            .and_then(Value::as_str)
            .unwrap_or(&event.boundary_id)
            .to_string();
        let (result_digest, execution_count, replay_count, replayed) =
            if let Some(entry) = self.durable_projection_entries.get_mut(&durable_key) {
                entry.replay_count += 1;
                (
                    entry.result_digest.clone(),
                    entry.execution_count,
                    entry.replay_count,
                    true,
                )
            } else {
                let entry = ModelDurableProjectionEntry {
                    result_digest: value_digest(&result),
                    execution_count: 1,
                    replay_count: 0,
                };
                let result = (
                    entry.result_digest.clone(),
                    entry.execution_count,
                    entry.replay_count,
                    false,
                );
                self.durable_projection_entries
                    .insert(durable_key.clone(), entry);
                result
            };
        json!({
            "durable_key": durable_key,
            "result_digest": result_digest,
            "execution_count": execution_count,
            "replay_count": replay_count,
            "replayed": replayed,
            "runtime_effect": {
                "kind": "tool_attempt",
                "effect_id": effect_id,
                "replay_key": durable_key,
                "controller": "abstract_model_projection",
            },
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ModelDurableProjectionEntry {
    result_digest: String,
    execution_count: usize,
    replay_count: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ModelSession {
    alias: String,
    opened: bool,
    ingress_count: usize,
    provider_outputs: Vec<String>,
    provider_exchange_counts: Vec<usize>,
    graph_node_counts: Vec<usize>,
    transcript_message_counts: Vec<usize>,
    tool_outputs: Vec<String>,
    exec_code_outputs: Vec<String>,
    observer_turn_indices: Vec<usize>,
    observer_reconnects: usize,
    queued_ingress_count: usize,
    cancellation_count: usize,
    trigger_count: usize,
    backend_failure_count: usize,
    provider_mutation_count: usize,
    process_wake_count: usize,
    process_lifecycle_count: usize,
    durable_effect_keys: Vec<String>,
    lease_time_ticks: Vec<u64>,
}

impl ModelSession {
    fn new(alias: String) -> Self {
        Self {
            alias,
            opened: false,
            ingress_count: 0,
            provider_outputs: Vec::new(),
            provider_exchange_counts: Vec::new(),
            graph_node_counts: Vec::new(),
            transcript_message_counts: Vec::new(),
            tool_outputs: Vec::new(),
            exec_code_outputs: Vec::new(),
            observer_turn_indices: Vec::new(),
            observer_reconnects: 0,
            queued_ingress_count: 0,
            cancellation_count: 0,
            trigger_count: 0,
            backend_failure_count: 0,
            provider_mutation_count: 0,
            process_wake_count: 0,
            process_lifecycle_count: 0,
            durable_effect_keys: Vec::new(),
            lease_time_ticks: Vec::new(),
        }
    }

    fn summary(&self) -> SessionAbstractSummary {
        SessionAbstractSummary {
            alias: self.alias.clone(),
            opened: self.opened,
            ingress_count: self.ingress_count,
            provider_outputs: self.provider_outputs.clone(),
            provider_exchange_counts: self.provider_exchange_counts.clone(),
            graph_node_counts: self.graph_node_counts.clone(),
            transcript_message_counts: self.transcript_message_counts.clone(),
            tool_outputs: self.tool_outputs.clone(),
            exec_code_outputs: self.exec_code_outputs.clone(),
            observer_turn_indices: self.observer_turn_indices.clone(),
            observer_reconnects: self.observer_reconnects,
            queued_ingress_count: self.queued_ingress_count,
            cancellation_count: self.cancellation_count,
            trigger_count: self.trigger_count,
            backend_failure_count: self.backend_failure_count,
            provider_mutation_count: self.provider_mutation_count,
            process_wake_count: self.process_wake_count,
            process_lifecycle_count: self.process_lifecycle_count,
            durable_effect_keys: self.durable_effect_keys.clone(),
            lease_time_ticks: self.lease_time_ticks.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ModelDurableEffect {
    durable_key: String,
    execution_count: usize,
    replay_count: usize,
    result_digest: String,
}

impl ModelDurableEffect {
    fn from_observed(durable_key: String, observed: &Value) -> Self {
        Self {
            durable_key,
            execution_count: observed
                .get("execution_count")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize,
            replay_count: observed
                .get("replay_count")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize,
            result_digest: observed
                .get("result_digest")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        }
    }

    fn summary(&self) -> DurableEffectAbstractSummary {
        DurableEffectAbstractSummary {
            durable_key: self.durable_key.clone(),
            execution_count: self.execution_count,
            replay_count: self.replay_count,
            result_digest: self.result_digest.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ModelWorker {
    worker_alias: String,
    session_alias: String,
    active_incarnation_id: String,
    active_fencing_token: u64,
    lease_owner_changes: usize,
    stale_completion_rejections: usize,
    process_stale_completion_rejected: bool,
    process_stale_output_absent: bool,
    process_terminal_writer: String,
    process_terminal_event_count: usize,
}

impl ModelWorker {
    fn from_observed(worker_alias: String, observed: &Value) -> Self {
        Self {
            worker_alias,
            session_alias: observed
                .get("session")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            active_incarnation_id: observed
                .get("active_owner")
                .and_then(|owner| owner.get("incarnation_id"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            active_fencing_token: observed
                .get("active_fencing_token")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            lease_owner_changes: usize::from(
                observed
                    .get("lease_owner_changed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ),
            stale_completion_rejections: usize::from(
                observed
                    .get("stale_completion_rejected")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ),
            process_stale_completion_rejected: observed
                .get("process_stale_completion_rejected")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            process_stale_output_absent: observed
                .get("process_stale_output_absent")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            process_terminal_writer: observed
                .get("process_terminal_writer")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            process_terminal_event_count: observed
                .get("process_terminal_event_count")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize,
        }
    }

    fn summary(&self) -> WorkerAbstractSummary {
        WorkerAbstractSummary {
            worker_alias: self.worker_alias.clone(),
            session_alias: self.session_alias.clone(),
            active_incarnation_id: self.active_incarnation_id.clone(),
            active_fencing_token: self.active_fencing_token,
            lease_owner_changes: self.lease_owner_changes,
            stale_completion_rejections: self.stale_completion_rejections,
            process_stale_completion_rejected: self.process_stale_completion_rejected,
            process_stale_output_absent: self.process_stale_output_absent,
            process_terminal_writer: self.process_terminal_writer.clone(),
            process_terminal_event_count: self.process_terminal_event_count,
        }
    }
}

fn boundary_session_alias(event: &BoundaryEvent) -> String {
    event
        .payload
        .get("session")
        .and_then(Value::as_str)
        .unwrap_or(&event.actor_alias)
        .to_string()
}

/// A suspend-session ingress or its scheduler-delivered resume completion.
fn is_suspend_boundary(event: &BoundaryEvent) -> bool {
    (event.kind == BoundaryKind::Ingress && event.payload.get("suspend_kind").is_some())
        || event
            .payload
            .get("suspend_resume")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

/// Project the abstract observed for a suspend boundary, matching the
/// generated-world observed so cross-backend replay stays equal without
/// modelling the suspend session as a real abstract session.
fn project_suspend_boundary(event: &BoundaryEvent) -> Option<Value> {
    if event.kind == BoundaryKind::Ingress && event.payload.get("suspend_kind").is_some() {
        return Some(json!({
            "session": event.actor_alias,
            "opened": true,
            "ingress_count": 1,
            "runtime_suspend": {
                "suspend_kind": event.payload.get("suspend_kind").cloned().unwrap_or(Value::Null),
                "spawned": true,
            },
        }));
    }
    if event
        .payload
        .get("suspend_resume")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let output = event
            .payload
            .get("output")
            .cloned()
            .unwrap_or_else(|| json!(""));
        let tool_name = event
            .payload
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or("await_tool");
        return Some(json!({
            "session": event.actor_alias,
            "tool_output": output,
            "tool_name": tool_name,
            "tool_call_id": event.boundary_id,
            "execution_count": 1,
            "runtime_tool_output": lash_core::ToolCallOutput::success(output.clone()),
        }));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::{BoundaryEvent, BoundaryKind};

    #[test]
    fn model_store_keeps_cross_session_outputs_isolated() {
        let mut store = ModelStore::default();
        store.apply_boundary(&BoundaryEvent::new(
            "open-1",
            "session-001",
            BoundaryKind::Ingress,
            0,
            "session.open",
            json!({}),
        ));
        store.apply_boundary(&BoundaryEvent::new(
            "open-2",
            "session-002",
            BoundaryKind::Ingress,
            0,
            "session.open",
            json!({}),
        ));
        store.apply_boundary(&BoundaryEvent::new(
            "p1",
            "session-001",
            BoundaryKind::Provider,
            1,
            "provider",
            json!({"text": "one"}),
        ));
        store.apply_boundary(&BoundaryEvent::new(
            "p2",
            "session-002",
            BoundaryKind::Provider,
            1,
            "provider",
            json!({"text": "two"}),
        ));

        let summary = store.summary();
        assert_eq!(summary.session_count, 2);
        assert_eq!(summary.sessions[0].provider_outputs, vec!["one"]);
        assert_eq!(summary.sessions[1].provider_outputs, vec!["two"]);
        assert_ne!(
            summary.sessions[0].provider_outputs,
            summary.sessions[1].provider_outputs
        );
    }

    #[test]
    fn model_store_projects_semantic_boundary_summaries() {
        let mut store = ModelStore::default();
        store.apply_boundary(&BoundaryEvent::new(
            "open-1",
            "session-001",
            BoundaryKind::Ingress,
            0,
            "session.open",
            json!({}),
        ));
        store.apply_boundary(&BoundaryEvent::new(
            "provider-1",
            "session-001",
            BoundaryKind::Provider,
            1,
            "provider.chat.stream",
            json!({"text": "answer for session-001"}),
        ));
        store.apply_boundary(&BoundaryEvent::new(
            "observer-1",
            "session-001",
            BoundaryKind::Observer,
            2,
            "observer.snapshot",
            json!({}),
        ));
        store.apply_boundary(&BoundaryEvent::new(
            "effect-1",
            "session-001",
            BoundaryKind::DurableEffect,
            3,
            "durable.sleep.complete",
            json!({"durable_key": "sleep/session-001", "result": {"done": true}}),
        ));
        store.apply_boundary(&BoundaryEvent::new(
            "effect-1-replay",
            "session-001",
            BoundaryKind::DurableEffect,
            4,
            "durable.sleep.replay",
            json!({"durable_key": "sleep/session-001", "result": {"done": false}}),
        ));
        // Worker fencing is NOT abstractly projected (the abstract arm reports
        // identity only); the model reads the REAL reclaim/fence facts produced by
        // the live lease store, threaded in via `apply_observed_boundary`.
        store.apply_observed_boundary(
            &BoundaryEvent::new(
                "worker-1",
                "worker-001",
                BoundaryKind::Worker,
                5,
                "worker.stale-completion-rejected",
                json!({"session": "session-001"}),
            ),
            &json!({
                "worker_alias": "worker-001",
                "session": "session-001",
                "active_owner": { "incarnation_id": "worker-001:incarnation-002" },
                "active_fencing_token": 2,
                "lease_owner_changed": true,
                "stale_completion_rejected": true,
            }),
        );

        let summary = store.summary();
        assert_eq!(summary.sessions[0].observer_turn_indices, vec![1]);
        assert_eq!(summary.durable_effects[0].execution_count, 1);
        assert_eq!(summary.durable_effects[0].replay_count, 1);
        assert_eq!(summary.workers[0].stale_completion_rejections, 1);
        assert_eq!(summary.workers[0].lease_owner_changes, 1);
        assert_eq!(summary.workers[0].active_fencing_token, 2);
    }

    #[test]
    fn abstract_worker_projection_fabricates_no_fencing() {
        // The abstract worker projection must NOT fabricate fencing: if the real
        // lease facts are never threaded in, the worker summary shows no fence
        // change and the worker oracle cannot pass.
        let mut store = ModelStore::default();
        let observed = store.project_boundary_observation(&BoundaryEvent::new(
            "worker-1",
            "worker-001",
            BoundaryKind::Worker,
            0,
            "worker.stale-completion-rejected",
            json!({"session": "session-001"}),
        ));
        assert!(observed.get("stale_completion_rejected").is_none());
        assert!(observed.get("lease_owner_changed").is_none());
        assert!(observed.get("active_fencing_token").is_none());
    }
}
