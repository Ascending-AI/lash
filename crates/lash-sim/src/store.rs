use lash_sansio::SessionId;
use std::collections::{BTreeMap, BTreeSet};

use lash_core::StoreError;
use serde_json::{Value, json};

use crate::runtime_boundaries::durable_effect_scope;
use crate::runtime_contracts::{
    RuntimeAgentFrameInvariantFacts, RuntimeFinalValueInvariantFacts, RuntimeGraphInvariantFacts,
    RuntimeTurnObservation, RuntimeUsageInvariantFacts, RuntimeUsageTotals, runtime_turn_contract,
};
use crate::scheduler::{
    ACTIVE_TURN_INPUT_STATE, BoundaryEvent, BoundaryKind, NEXT_TURN_INPUT_STATE, QueuedIngressMode,
};
use crate::trace::{
    AbstractWorldSummary, DurableEffectAbstractSummary, ProviderTurnSummary,
    SessionAbstractSummary, value_digest,
};

mod boundary;

use boundary::{boundary_session_alias, is_suspend_boundary, project_suspend_boundary};

pub use lash_core::testing::checkpoint_observer::{
    CHECKPOINT_WRITE_EVENT_SCHEMA, CheckpointAttribution, CheckpointComponent,
    CheckpointComponentWrite, CheckpointComponentWriteKind, CheckpointStateWrite,
    CheckpointWriteCollector, CheckpointWriteEvent, ObservedSessionStoreFactory,
};

pub fn backend_fault_observation(
    session: Value,
    operation: String,
    attempt: usize,
    retryable: bool,
) -> Value {
    let store_error = backend_fault_store_error(&operation, attempt, retryable);
    let store_error_variant = store_error.variant_name().to_string();
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
            expected: attempt.saturating_sub(1) as u64,
            actual: attempt as u64,
        }
    } else {
        StoreError::Backend(format!(
            "simulated terminal backend failure during {operation}"
        ))
    }
}

#[derive(Clone, Debug)]
enum ModelPendingInputState {
    Queued,
    Claimed(String),
    Completed,
    Cancelled,
}

#[derive(Clone, Debug)]
struct ModelPendingInput {
    session: String,
    mode: QueuedIngressMode,
    state: ModelPendingInputState,
}

#[derive(Clone, Debug, Default)]
pub struct ModelStore {
    sessions: BTreeMap<String, ModelSession>,
    durable_effects: BTreeMap<String, ModelDurableEffect>,
    backend_attempts_by_operation: BTreeMap<String, usize>,
    tool_completions: BTreeMap<String, usize>,
    exec_executions: BTreeMap<String, usize>,
    rejected_provider_mutations: BTreeSet<String>,
    queued_input_boundaries: BTreeMap<String, ModelPendingInput>,
    pending_turn_input_seq_by_session: BTreeMap<String, u64>,
    total_events: usize,
}

impl ModelStore {
    pub(crate) fn queued_next_turn_boundaries(&self, session: &str) -> Vec<String> {
        self.queued_input_boundaries
            .iter()
            .filter(|(_, input)| {
                input.session == session
                    && input.mode == QueuedIngressMode::NextTurn
                    && matches!(input.state, ModelPendingInputState::Queued)
            })
            .map(|(boundary, _)| boundary.clone())
            .collect()
    }

    /// Admission occurs at provider start, before its completion is delivered.
    /// Only queued next-turn inputs are eligible; cancellation remains a local
    /// lifecycle transition whose outcome is independently projected.
    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    pub(crate) fn apply_provider_admissions(&mut self, admissions: &[Value]) {
        for admission in admissions {
            let session = admission["session"].as_str().expect("admission session");
            let provider = admission["provider_boundary"]
                .as_str()
                .expect("admission provider");
            for input in self.queued_input_boundaries.values_mut() {
                if input.session == session
                    && input.mode == QueuedIngressMode::NextTurn
                    && matches!(input.state, ModelPendingInputState::Queued)
                {
                    input.state = ModelPendingInputState::Claimed(provider.to_string());
                }
            }
        }
    }

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

    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
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
                self.queued_input_boundaries.insert(
                    event.boundary_id.clone(),
                    ModelPendingInput {
                        session: event.actor_alias.clone(),
                        mode: event.queued_ingress_mode().unwrap_or_else(|err| {
                            panic!("queued-ingress boundary `{}`: {err}", event.boundary_id)
                        }),
                        state: ModelPendingInputState::Queued,
                    },
                );
            }
            BoundaryKind::Provider => {
                for input in self.queued_input_boundaries.values_mut() {
                    if matches!(&input.state, ModelPendingInputState::Claimed(provider) if provider == &event.boundary_id)
                    {
                        input.state = ModelPendingInputState::Completed;
                    }
                }
                let session = self.ensure_session(event.actor_alias.clone());
                let provider_kind = event
                    .payload
                    .get("provider_kind")
                    .and_then(Value::as_str)
                    .unwrap_or("openai-compatible");
                if provider_kind != "openai-compatible" {
                    session.usage_ledger_keys.insert(provider_kind.to_string());
                }
                if let Some(usage) =
                    observed.pointer("/runtime_invariant_facts/usage/token_ledger_total")
                {
                    for &field in RuntimeUsageTotals::FIELDS {
                        if let Some(value) = usage.get(field).and_then(Value::as_i64) {
                            let known_field = session.cumulative_usage.set_field(field, value);
                            debug_assert!(known_field);
                        }
                    }
                }
                let text = observed
                    .get("provider_output")
                    .and_then(Value::as_str)
                    .or_else(|| event.payload.get("text").and_then(Value::as_str))
                    .unwrap_or("")
                    .to_string();
                session.provider_turns.push(ProviderTurnSummary {
                    output: text,
                    exchange_count: observed
                        .get("provider_exchange_count")
                        .and_then(Value::as_u64),
                    graph_node_count: observed.get("graph_node_count").and_then(Value::as_u64),
                    transcript_message_count: observed
                        .get("transcript_message_count")
                        .and_then(Value::as_u64),
                });
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
            BoundaryKind::Observer => {
                let session = self.ensure_session(event.actor_alias.clone());
                let turn_index = observed
                    .get("turn_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(session.provider_turns.len() as u64)
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
                if let Some(input) = event
                    .payload
                    .get("target")
                    .and_then(Value::as_str)
                    .and_then(|target| self.queued_input_boundaries.get_mut(target))
                    && matches!(input.state, ModelPendingInputState::Queued)
                {
                    input.state = ModelPendingInputState::Cancelled;
                }
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
        AbstractWorldSummary::with_digest(
            self.sessions.len(),
            self.total_events,
            sessions,
            durable_effects,
        )
    }

    fn apply_checkpoint_writes(&mut self, writes: &[CheckpointWriteEvent]) -> Result<(), String> {
        for write in writes {
            let session_id = write.attributed_session();
            let session = self.sessions.get_mut(session_id).ok_or_else(|| {
                format!(
                    "checkpoint write for unknown session `{session_id}` (store session `{}`)",
                    write.session_id
                )
            })?;
            session.checkpoint_commit_count += 1;
            session.checkpoint_head_revision =
                session.checkpoint_head_revision.max(write.revision_after);
            for component in &write.components {
                match component.kind {
                    CheckpointComponentWriteKind::Stored { .. }
                    | CheckpointComponentWriteKind::PluginState { .. } => {
                        session.checkpoint_component_stored_count += 1;
                    }
                    CheckpointComponentWriteKind::UnchangedRef => {
                        session.checkpoint_component_ref_count += 1;
                    }
                }
            }
        }
        Ok(())
    }

    /// Apply checkpoint evidence and produce the final abstract summary in one
    /// owned step. Model replay/minimization deliberately pass recorded
    /// evidence; backend replay passes commits observed from the replayed store.
    pub fn summarize_with_checkpoint_writes(
        mut self,
        writes: &[CheckpointWriteEvent],
    ) -> Result<AbstractWorldSummary, String> {
        self.apply_checkpoint_writes(writes)?;
        Ok(self.summary())
    }

    /// Summarize checkpoint evidence using the same session-model boundary as
    /// `apply_observed_boundary`: generated suspend fixtures are real runtime turns but
    /// intentionally not abstract sessions.
    pub fn summarize_with_trace_checkpoint_writes(
        self,
        events: &[crate::scheduler::DeliveredBoundary],
        writes: &[CheckpointWriteEvent],
    ) -> Result<AbstractWorldSummary, String> {
        let modeled_sessions = events
            .iter()
            .filter(|event| {
                event.kind == BoundaryKind::Ingress && !is_suspend_boundary(&event.as_event())
            })
            .map(|event| event.actor_alias.as_str())
            .collect::<BTreeSet<_>>();
        let suspend_sessions = events
            .iter()
            .filter(|event| {
                event.kind == BoundaryKind::Ingress && is_suspend_boundary(&event.as_event())
            })
            .map(|event| event.actor_alias.as_str())
            .collect::<BTreeSet<_>>();
        let mut projected = Vec::new();
        for write in writes {
            let session_id = write.attributed_session();
            if modeled_sessions.contains(session_id) {
                projected.push(write.clone());
            } else if !suspend_sessions.contains(session_id) {
                return Err(format!(
                    "checkpoint write for unknown session `{session_id}` (store session `{}`)",
                    write.session_id
                ));
            }
        }
        self.summarize_with_checkpoint_writes(&projected)
    }

    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
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
                let ingress_mode = event.queued_ingress_mode().unwrap_or_else(|err| {
                    panic!("queued-ingress boundary `{}`: {err}", event.boundary_id)
                });
                // Deliberate vocabulary restatement: keep this drift-pin for prelude item 21.
                let input_state = match ingress_mode {
                    QueuedIngressMode::ActiveTurn => ACTIVE_TURN_INPUT_STATE,
                    QueuedIngressMode::NextTurn => NEXT_TURN_INPUT_STATE,
                };
                json!({
                    "session": event.actor_alias,
                    "queued_ingress": true,
                    "source_key": event.payload.get("source_key").cloned().unwrap_or(Value::Null),
                    "input_id": format!("recording-ti-{}", *next_seq),
                    "input_state": input_state,
                    "ingress_mode": ingress_mode.as_str(),
                    "active_turn_id": event.payload.get("active_turn_id").cloned().unwrap_or(Value::Null),
                })
            }
            BoundaryKind::Provider => {
                let turn_index = self
                    .sessions
                    .get(&event.actor_alias)
                    .map_or(1, |session| session.provider_turns.len() + 1);
                let streamed = event
                    .payload
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let text = crate::runtime_contracts::host_assistant_message(streamed);
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
                    .unwrap_or((turn_index * 2 + 1) as u64)
                    as usize;
                let transcript_message_count = event
                    .payload
                    .get("expected_transcript_message_count")
                    .and_then(Value::as_u64)
                    .unwrap_or((turn_index * 2) as u64)
                    as usize;
                let runtime_contract = runtime_turn_contract(
                    &RuntimeTurnObservation {
                        session_id: SessionId::from(event.actor_alias.clone()),
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
                    &SessionId::from(event.actor_alias.clone()),
                    turn_index,
                    streamed,
                    provider_exchange_count,
                );
                let provider_kind = event
                    .payload
                    .get("provider_kind")
                    .and_then(Value::as_str)
                    .unwrap_or("openai-compatible");
                // Generated turns carry the usage their script reports; turns
                // recorded before generated usage ran the canonical scripts'
                // fixed counts.
                let turn_usage =
                    match crate::runtime_providers::scripted_turn_from_provider_boundary(
                        &event.payload,
                    )
                    .ok()
                    .and_then(|turn| turn.usage)
                    {
                        Some(usage) => RuntimeUsageTotals::new(
                            usage.input_tokens,
                            usage.output_tokens,
                            usage.cache_read_input_tokens,
                            0,
                            usage.reasoning_output_tokens,
                        ),
                        None => match provider_kind {
                            "openai" => RuntimeUsageTotals::new(5, 2, 0, 0, 0),
                            "anthropic" => RuntimeUsageTotals::new(7, 4, 0, 0, 0),
                            "google_oauth" => RuntimeUsageTotals::new(6, 4, 0, 0, 1),
                            _ => RuntimeUsageTotals::default(),
                        },
                    };
                let (prior_usage, prior_ledger_keys) =
                    self.sessions.get(&event.actor_alias).map_or_else(
                        || (RuntimeUsageTotals::default(), BTreeSet::new()),
                        |session| {
                            (
                                session.cumulative_usage.clone(),
                                session.usage_ledger_keys.clone(),
                            )
                        },
                    );
                let total_usage = prior_usage.saturating_add(&turn_usage);
                let mut ledger_keys = prior_ledger_keys;
                if turn_usage != RuntimeUsageTotals::default() {
                    ledger_keys.insert(provider_kind.to_string());
                }
                let frame_key = lash_core::FrameKey::from_caller_material("initial-frame")
                    .expect("non-empty initial frame material");
                let frame_node_id = lash_core::facade_support::frame_node_id(
                    &SessionId::from(event.actor_alias.clone()),
                    frame_key.as_str(),
                );
                // The model's runtime invariant facts are the same typed fact
                // sets the real turn path emits, so `passed` and the
                // leaf-existence rule are derived exactly once.
                let graph_facts = RuntimeGraphInvariantFacts {
                    node_count: graph_node_count,
                    edge_count: graph_node_count.saturating_sub(1),
                    duplicate_node_ids: Vec::new(),
                    missing_parent_links: Vec::new(),
                    cycle_node_ids: Vec::new(),
                    leaf_node_id: None,
                    leaf_exists: RuntimeGraphInvariantFacts::leaf_exists(None, &BTreeSet::new()),
                };
                let agent_frame_facts = RuntimeAgentFrameInvariantFacts {
                    current_frame_node_id: frame_node_id.as_str().to_string(),
                    frame_count: 1,
                    active_frame_ids: vec![frame_node_id.as_str().to_string()],
                    current_frame_exists: true,
                    current_frame_active: true,
                    nodes_without_agent_frame: Vec::new(),
                    node_agent_frame_ids_without_record: Vec::new(),
                    observation_limit: None,
                };
                let usage_facts = RuntimeUsageInvariantFacts {
                    turn_usage: turn_usage.clone(),
                    total_usage: turn_usage.clone(),
                    token_ledger_total: total_usage,
                    token_ledger_entry_count: ledger_keys.len(),
                    usage_event_count: 1,
                    usage_event_cumulative_totals: vec![turn_usage],
                    non_negative: true,
                    usage_events_monotonic: true,
                    negative_fields: Vec::new(),
                };
                let final_value_facts = RuntimeFinalValueInvariantFacts {
                    outcome_kind: "assistant_message".to_string(),
                    semantic_value: None,
                    terminal_event_count: 0,
                    assistant_prose_delta_count: 1,
                    assistant_output_text: text.clone(),
                    semantic_channel_observed: false,
                };
                let mut observed = json!({
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
                    "provider_kind": provider_kind,
                    "runtime_invariants": {
                        "session_id": true,
                        "turn_index": true,
                        "graph_non_empty": true,
                        "transcript_contains_provider_output": true,
                        "activity_count_nonzero": true,
                        "graph_acyclic": graph_facts.cycle_node_ids.is_empty(),
                        "single_active_agent_frame": agent_frame_facts.active_frame_ids.len() == 1,
                        "usage_monotonic": usage_facts.usage_events_monotonic,
                    },
                    "runtime_invariant_facts": {
                        "graph": graph_facts,
                        "agent_frame": agent_frame_facts,
                        "usage": usage_facts,
                    },
                    "runtime_final_value_facts": final_value_facts,
                    "runtime_contract": runtime_contract,
                });
                // A fixture that strips the runtime session attribution from a
                // provider completion strips it from the model's projection too.
                if event
                    .payload
                    .get("omit_runtime_session_id")
                    .and_then(Value::as_bool)
                    == Some(true)
                    && let Some(object) = observed.as_object_mut()
                {
                    object.remove("runtime_session_id");
                }
                observed
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
                    "runtime_effect": {
                        "controller": "restate_runtime_effect_controller",
                        "kind": "tool_attempt",
                        "local_executor_called": true,
                    },
                    "runtime_tool_output": tool_output,
                    "runtime_tool_record": {
                        "call_id": event.boundary_id,
                        "tool": tool_name,
                        "args": {
                            "boundary_id": event.boundary_id,
                            "session": event.actor_alias,
                        },
                        "output": tool_output,
                    },
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
                    observations: vec![lash_core::Observation {
                        text: output.clone(),
                        projection: Default::default(),
                    }],
                    calls: Vec::new(),
                    printed_images: Vec::new(),
                    error: (exit_code != 0).then(|| {
                        lash_core::CellFailure::new(
                            lash_core::CellFailureKind::Program,
                            format!("exit code {exit_code}"),
                        )
                    }),
                    degraded_bindings: Vec::new(),
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
                    "runtime_effect": {
                        "controller": "restate_runtime_effect_controller",
                        "kind": "exec_code",
                        "local_executor_called": true,
                    },
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
                Self::project_durable_effect(event, durable_key, result)
            }
            BoundaryKind::Observer => {
                let turn_index = self
                    .sessions
                    .get(&event.actor_alias)
                    .map_or(0, |session| session.provider_turns.len());
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
                        .unwrap_or((turn_index * 2 + 1) as u64),
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
                let outcome =
                    match target.and_then(|target| self.queued_input_boundaries.get(target)) {
                        Some(input) => match input.state {
                            ModelPendingInputState::Queued => "cancelled",
                            ModelPendingInputState::Claimed(_) => "already_claimed",
                            ModelPendingInputState::Completed => "already_completed",
                            ModelPendingInputState::Cancelled => "already_cancelled",
                        },
                        None => "not_found",
                    };
                json!({
                    "session": event.actor_alias,
                    "target": event.payload.get("target").cloned().unwrap_or(Value::Null),
                    "cancelled": outcome == "cancelled",
                    "cancel_outcome": outcome,
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
                let occurrence_id =
                    lash_core::facade_support::deterministic_occurrence_id(&request);
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
        }
    }

    fn ensure_session(&mut self, alias: impl Into<String>) -> &mut ModelSession {
        let alias = alias.into();
        self.sessions
            .entry(alias.clone())
            .or_insert_with(|| ModelSession::new(alias))
    }

    /// COVERAGE-ONLY abstract model projection of a durable effect under
    /// crash and redrive: executed once, and the redrive served the recorded
    /// result.
    ///
    /// This projection makes generated model states comparable; it is not
    /// evidence for the `durable_effect_exactly_once` runtime oracle, which
    /// reads what the engine did in `runtime_boundaries`.
    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    fn project_durable_effect(event: &BoundaryEvent, durable_key: String, result: Value) -> Value {
        let effect_id = event
            .payload
            .get("runtime_effect")
            .and_then(|runtime_effect| runtime_effect.get("effect_id"))
            .and_then(Value::as_str)
            .unwrap_or(&event.boundary_id)
            .to_string();
        let envelope = lash_core::RuntimeEffectEnvelope::new(
            lash_core::RuntimeEffectInvocation::new(
                lash_core::EffectAddress::new(
                    durable_effect_scope(&event.actor_alias, &durable_key),
                    durable_key.clone(),
                )
                .expect("abstract durable effect carries an admitted effect scope"),
                lash_core::RuntimeAttribution::for_session(event.actor_alias.clone()),
                effect_id.clone(),
            ),
            lash_core::RuntimeEffectCommand::ToolAttempt {
                call: lash_core::PreparedToolCall::from_parts(
                    effect_id.clone(),
                    lash_core::ToolId::from("tool:sim_opaque_effect"),
                    "sim_opaque_effect",
                    json!({
                        "durable_key": durable_key,
                        "session": event.actor_alias,
                    }),
                    None,
                    json!({"prepared_by": "lash-sim"}),
                ),
                execution_grant: None,
                attempt: 1,
                max_attempts: 1,
            },
        );
        let envelope_hash = envelope
            .stable_hash()
            .expect("abstract durable-effect envelope is serializable");
        let recorded_intents =
            lash_core::ToolIntents::v3(vec![lash_core::ToolIntent::StartProcess(Box::new(
                lash_core::StartProcessIntent {
                    session_id: SessionId::from(event.actor_alias.clone()),
                    declaration: lash_core::ProcessStartDeclaration::external(
                        lash_core::ProcessOriginator::host_scoped("lash-sim-durable-effect"),
                        json!({"durable_key": durable_key}),
                        lash_core::Lifetime::Detached,
                    ),
                },
            ))]);
        let result_digest = value_digest(&result);
        json!({
            "durable_key": durable_key,
            "result_digest": result_digest,
            "redrive_result_digest": result_digest,
            "redrive_served_recorded_result": true,
            "execution_count": 1,
            "replay_count": 1,
            "replayed": true,
            "runtime_effect": {
                "kind": "tool_attempt",
                "effect_id": effect_id,
                "replay_key": envelope.invocation.replay_key(),
                "envelope_hash": envelope_hash,
                "controller": "restate_runtime_effect_controller",
                "local_executor_called": true,
                "redrive_local_executor_called": false,
            },
            "runtime_effect_outcome": {
                "type": "tool_attempt",
                "launch": {
                    "status": "done",
                    "record": {
                        "call_id": effect_id,
                        "tool": "sim_opaque_effect",
                        "args": null,
                        "output": lash_core::ToolCallOutput::success(result),
                    },
                    "intents": recorded_intents,
                },
            },
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ModelSession {
    alias: String,
    opened: bool,
    ingress_count: usize,
    provider_turns: Vec<ProviderTurnSummary>,
    usage_ledger_keys: BTreeSet<String>,
    cumulative_usage: RuntimeUsageTotals,
    tool_outputs: Vec<String>,
    exec_code_outputs: Vec<String>,
    observer_turn_indices: Vec<usize>,
    observer_reconnects: usize,
    queued_ingress_count: usize,
    cancellation_count: usize,
    trigger_count: usize,
    backend_failure_count: usize,
    provider_mutation_count: usize,
    durable_effect_keys: Vec<String>,
    checkpoint_commit_count: usize,
    checkpoint_component_stored_count: usize,
    checkpoint_component_ref_count: usize,
    checkpoint_head_revision: u64,
}

impl ModelSession {
    fn new(alias: String) -> Self {
        Self {
            alias,
            opened: false,
            ingress_count: 0,
            provider_turns: Vec::new(),
            usage_ledger_keys: BTreeSet::new(),
            cumulative_usage: RuntimeUsageTotals::default(),
            tool_outputs: Vec::new(),
            exec_code_outputs: Vec::new(),
            observer_turn_indices: Vec::new(),
            observer_reconnects: 0,
            queued_ingress_count: 0,
            cancellation_count: 0,
            trigger_count: 0,
            backend_failure_count: 0,
            provider_mutation_count: 0,
            durable_effect_keys: Vec::new(),
            checkpoint_commit_count: 0,
            checkpoint_component_stored_count: 0,
            checkpoint_component_ref_count: 0,
            checkpoint_head_revision: 0,
        }
    }

    fn summary(&self) -> SessionAbstractSummary {
        SessionAbstractSummary {
            alias: self.alias.clone(),
            opened: self.opened,
            ingress_count: self.ingress_count,
            provider_turns: self.provider_turns.clone(),
            tool_outputs: self.tool_outputs.clone(),
            exec_code_outputs: self.exec_code_outputs.clone(),
            observer_turn_indices: self.observer_turn_indices.clone(),
            observer_reconnects: self.observer_reconnects,
            queued_ingress_count: self.queued_ingress_count,
            cancellation_count: self.cancellation_count,
            trigger_count: self.trigger_count,
            backend_failure_count: self.backend_failure_count,
            provider_mutation_count: self.provider_mutation_count,
            durable_effect_keys: self.durable_effect_keys.clone(),
            checkpoint_commit_count: self.checkpoint_commit_count,
            checkpoint_component_stored_count: self.checkpoint_component_stored_count,
            checkpoint_component_ref_count: self.checkpoint_component_ref_count,
            checkpoint_head_revision: self.checkpoint_head_revision,
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

#[cfg(test)]
mod tests;
