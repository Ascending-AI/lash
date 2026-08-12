use super::*;
use lash_sansio::sync::MutexExt;

pub(super) async fn prove_runtime_facade_turn() -> Result<RuntimeFacadeProof, FixedScriptRunnerError>
{
    let script = runtime_script_for_text(OPENAI_COMPATIBLE, "Runtime scripted answer.")
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let transport = Arc::new(ScriptedLlmHttpTransport::from_scripts([script]));
    let (provider_handle, model, provider_kind) =
        runtime_provider_components(OPENAI_COMPATIBLE, &transport)
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let core = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .effect_host(Arc::new(
            lash::durability::InlineEffectHost::default().allow_process_lifetime_completion_keys(),
        ))
        .lease_timings(crate::lease::sim_runtime_lease_timings())
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .provider(provider_handle)
        .model(model)
        .build(crate::sim_process_owner())
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let session = core
        .session("sim-runtime-session")
        .open()
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let output = session
        .turn(lash::TurnInput::text("Run the scripted runtime proof."))
        .run()
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let assistant_message = output.assistant_message().unwrap_or_default().to_string();
    let runtime_ok = output.is_success()
        && output.result.state.session_id == "sim-runtime-session"
        && output.result.state.turn_index == 1;
    let provider_ok = assistant_message == "Runtime scripted answer.";
    let provider_exchange_count = transport_exchanges(transport.as_ref())?.len();
    require(
        runtime_ok,
        "runtime facade turn did not finish with the expected session state",
    )?;
    require(
        provider_ok && provider_exchange_count == 1,
        "runtime facade turn did not consume the expected scripted provider output",
    )?;
    let pending_tool_completion = prove_pending_tool_completion_through_turn().await?;
    let final_value_semantic_channel = prove_final_value_semantic_channel().await?;
    Ok(RuntimeFacadeProof {
        schema: "lash.sim.runtime-facade-proof.v1",
        name: "standard-facade-openai-compatible-scripted-turn",
        provider_kind,
        session_id: output.result.state.session_id,
        turn_index: output.result.state.turn_index,
        assistant_message,
        provider_exchange_count,
        runtime_invariant: runtime_provider_turn(
            runtime_ok,
            "turn finished once and advanced the expected session state",
        ),
        provider_output_invariant: runtime_provider_turn(
            provider_ok,
            "provider output matched the scripted OpenAI-compatible stream",
        ),
        pending_tool_completion,
        final_value_semantic_channel,
    })
}

/// The (provider kind, valid-prose-deltas-before-fault) combos exercised for the
/// live-provider-failure oracle EVERY seed. Covers more than one provider kind
/// and more than one fault position so `live_provider_failure_coverage` cannot
/// pass vacuously on a single degenerate case.
const LIVE_PROVIDER_FAILURE_COMBOS: &[(&str, usize)] = &[
    (OPENAI_COMPATIBLE, 1),
    (OPENAI_COMPATIBLE, 2),
    (ANTHROPIC, 1),
];

/// Drive every live-provider-failure combo for a seed, collecting the observed
/// facts for the per-seed coverage oracle.
pub(super) async fn drive_live_provider_failure_turns(
    seed: u64,
) -> Result<Vec<LiveProviderFailureFacts>, FixedScriptRunnerError> {
    let mut facts = Vec::with_capacity(LIVE_PROVIDER_FAILURE_COMBOS.len());
    for (provider_kind, prose_deltas) in LIVE_PROVIDER_FAILURE_COMBOS.iter().copied() {
        let script = live_failure_script(provider_kind, prose_deltas)
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        facts.push(
            run_live_turn_facts(
                seed,
                provider_kind,
                script,
                "malformed_sse_chunk",
                prose_deltas,
            )
            .await?,
        );
    }
    Ok(facts)
}

/// Run a real `session.turn().run()` against `script`, releasing its
/// scripted-transport SSE events through a REAL `BoundaryScheduler` (the same
/// provider-event release path generated turns use — NOT an ad-hoc index loop),
/// and record whether the turn terminalized without committing any output.
/// Shared by the failure driver and by the end-to-end negative test (which feeds
/// a SUCCESS script to prove the committed-output assertion bites).
pub(super) async fn run_live_turn_facts(
    seed: u64,
    provider_kind: &str,
    script: ProviderWireScript,
    fault_kind: &str,
    offered_prose_deltas: usize,
) -> Result<LiveProviderFailureFacts, FixedScriptRunnerError> {
    let schedule = ScriptedTransportSchedule::new();
    let transport = Arc::new(
        ScriptedLlmHttpTransport::from_scripts([script.clone()])
            .with_event_schedule(schedule.clone()),
    );
    let (provider_handle, model, provider_kind) =
        runtime_provider_components(provider_kind, &transport)
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let core = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .effect_host(Arc::new(
            lash::durability::InlineEffectHost::default().allow_process_lifetime_completion_keys(),
        ))
        .lease_timings(crate::lease::sim_runtime_lease_timings())
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .store_factory(Arc::new(
            lash::persistence::InMemorySessionStoreFactory::new(),
        ))
        .provider(provider_handle)
        .model(model)
        .build(crate::sim_process_owner())
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let session_id = format!("sim-live-failure-{provider_kind}-{offered_prose_deltas}");
    let session = core
        .session(session_id.clone())
        .open()
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;

    let events = Arc::new(RuntimeProofRecordingEvents::default());
    let turn_session = session.clone();
    let turn_events = Arc::clone(&events);
    let turn = tokio::spawn(async move {
        turn_session
            .turn(lash::TurnInput::text("Run the live provider failure turn."))
            .stream_to(turn_events.as_ref())
            .await
    });

    // Schedule the provider-event releases as real boundaries and deliver them
    // through a REAL BoundaryScheduler (seeded), exactly as generated provider
    // turns do.
    let turn_event = BoundaryEvent::new(
        format!("{session_id}:provider:001"),
        session_id.clone(),
        BoundaryKind::Provider,
        0,
        "provider.chat.stream.live-failure",
        json!({ "provider_kind": provider_kind, "turn_index": 1 }),
    );
    let release_boundaries = script
        .timeline
        .iter()
        .enumerate()
        .map(|(event_index, wire_event)| {
            provider_release_boundary(
                &turn_event,
                &script,
                0,
                event_index,
                wire_event,
                wire_event.at(),
            )
        })
        .collect::<Vec<_>>();
    let mut scheduler = BoundaryScheduler::with_events(seed, release_boundaries);

    // The first release gate is an explicit schedule boundary. Once the turn
    // reports that it is blocked there, it cannot finish before delivery.
    schedule.wait_until_blocked(0, 0).await;
    let turn_was_live_parked = !turn.is_finished();

    // Deliver each release boundary through the BoundaryScheduler and release
    // the gate it names.
    loop {
        if turn.is_finished() {
            break;
        }
        let Some(delivered) = scheduler.deliver_next(Value::Null) else {
            break;
        };
        let event = delivered.as_event();
        let exchange_index = event
            .payload
            .get("exchange_index")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let event_index = event
            .payload
            .get("event_index")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let event_name = event
            .payload
            .get("event_name")
            .and_then(Value::as_str)
            .unwrap_or("provider_event")
            .to_string();
        if !turn.is_finished() {
            schedule.release(exchange_index, event_index, &event_name, event.at);
        }
    }

    let result = turn.await.map_err(|err| {
        FixedScriptRunnerError::Runtime(format!(
            "live provider failure turn task failed to join: {err}"
        ))
    })?;

    let streamed_prose_deltas = events.assistant_prose_delta_count().await;
    // COMMITTED output is the durable turn result + session transcript, NOT
    // transient stream deltas: a correct runtime may STREAM partial prose and then
    // DISCARD it on terminal failure. We require it commit none of that prose.
    let (terminalized_failure, committed_assistant_message_nonempty, committed_final_values) =
        match &result {
            Ok(turn_result) => (
                !turn_result.is_success(),
                turn_result
                    .assistant_message()
                    .is_some_and(|message| !message.is_empty()),
                usize::from(turn_result.final_value().is_some()),
            ),
            Err(_) => (true, false, 0),
        };
    let committed_prose_in_transcript =
        committed_transcript_contains(&session, LIVE_FAILURE_LEAK_PROSE);
    Ok(LiveProviderFailureFacts {
        provider_kind,
        fault_kind: fault_kind.to_string(),
        offered_prose_deltas,
        streamed_prose_deltas,
        turn_was_live_parked,
        terminalized_failure,
        committed_assistant_message_nonempty,
        committed_final_values,
        committed_prose_in_transcript,
    })
}

/// Whether the session's COMMITTED transcript contains `needle` — used to detect
/// partial prose leaked into durable state on a terminal failure.
fn committed_transcript_contains(session: &lash::LashSession, needle: &str) -> bool {
    let observation = session.observe().current_observation();
    observation.read_view.messages().iter().any(|message| {
        message
            .parts
            .iter()
            .any(|part| part.content.contains(needle))
    })
}

pub(super) async fn prove_pending_tool_completion_through_turn()
-> Result<PendingToolCompletionProof, FixedScriptRunnerError> {
    let (key_tx, key_rx) = tokio::sync::oneshot::channel();
    let events = Arc::new(RuntimeProofRecordingEvents::default());
    let core = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .effect_host(Arc::new(
            lash::durability::InlineEffectHost::default().allow_process_lifetime_completion_keys(),
        ))
        .lease_timings(crate::lease::sim_runtime_lease_timings())
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .store_factory(Arc::new(
            lash::persistence::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(lash_core::TestLocalProcessRegistry::default())
            as Arc<dyn lash_core::ProcessRegistry>)
        .provider(pending_tool_roundtrip_provider())
        .model(
            lash_core::ModelSpec::builder("mock-model")
                .context_window_tokens(200_000)
                .build()
                .map_err(|error| FixedScriptRunnerError::Assertion(error.to_string()))?,
        )
        .tools(Arc::new(PendingToolProvider::new(key_tx)) as Arc<dyn lash_core::ToolProvider>)
        .build(crate::sim_process_owner())
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let session = core
        .session("sim-pending-tool-session")
        .open()
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let turn_session = session.clone();
    let turn_events = Arc::clone(&events);
    let turn = tokio::spawn(async move {
        turn_session
            .turn(lash::TurnInput::text("use async tool"))
            .stream_to(turn_events.as_ref())
            .await
    });

    let key = key_rx.await.map_err(|_| {
        FixedScriptRunnerError::Runtime("pending tool did not send completion key".to_string())
    })?;
    let completed_before = events.tool_completed_count().await;
    let suspended_before_completion = !turn.is_finished() && completed_before == 0;
    require(
        suspended_before_completion,
        "pending tool turn completed before the scheduler-delivered completion boundary",
    )?;

    let resolved_payload = json!({
        "ok": true,
        "async": true,
        "resolved_by": "lash-sim-boundary-scheduler",
    });
    let completion_boundary_id = "sim-pending-tool-session:tool-completion:001";
    let mut scheduler = BoundaryScheduler::with_events(
        0x5eed_7001,
        [BoundaryEvent::new(
            completion_boundary_id,
            "sim-pending-tool-session",
            BoundaryKind::Tool,
            1,
            "tool.pending-completion.resolve",
            json!({
                "tool": "app_lookup",
                "resolution": resolved_payload,
                "completion_key_observed": true,
            }),
        )],
    );
    let mut delivered = scheduler.deliver_next(Value::Null).ok_or_else(|| {
        FixedScriptRunnerError::Assertion(
            "pending tool completion boundary was not scheduled".to_string(),
        )
    })?;
    let event = delivered.as_event();
    let resolution = event.payload.get("resolution").cloned().ok_or_else(|| {
        FixedScriptRunnerError::Assertion(
            "pending tool boundary missing resolution payload".to_string(),
        )
    })?;
    let accepted = core
        .completions()
        .resolve(key.clone(), lash_core::Resolution::Ok(resolution.clone()))
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    delivered.observed = json!({
        "session": event.actor_alias,
        "tool": event.payload.get("tool").cloned().unwrap_or(Value::Null),
        "scheduler_delivered_tool_completion": true,
        "completion_key_observed": event.payload.get("completion_key_observed").cloned().unwrap_or(Value::Bool(false)),
        "resolve_outcome": accepted.clone(),
    });

    let result = turn
        .await
        .map_err(|err| {
            FixedScriptRunnerError::Runtime(format!("pending tool turn task failed: {err}"))
        })?
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let completed_after = events.tool_completed_count().await;
    let completed_outputs = events.tool_completed_outputs().await;
    let assistant_message = result.assistant_message().unwrap_or_default().to_string();
    let final_ok = matches!(
        &result.outcome,
        lash_core::facade_support::TurnOutcome::Finished(
            lash_core::facade_support::TurnFinish::AssistantMessage { .. }
        )
    ) && assistant_message == "done"
        && completed_after > completed_before
        && completed_outputs
            .iter()
            .any(|(name, output)| name == "app_lookup" && output == &resolution);
    require(
        final_ok,
        "pending tool completion did not resume the turn to the scripted final answer",
    )?;
    let duplicate = core
        .completions()
        .resolve(
            key,
            lash_core::Resolution::Ok(json!({"ok": false, "duplicate": true})),
        )
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let session_id = result.state.session_id.clone();
    let turn_index = result.state.turn_index;

    Ok(PendingToolCompletionProof {
        schema: "lash.sim.pending-tool-completion-proof.v1",
        name: "standard-turn-pending-tool-scheduler-resolution",
        session_id,
        turn_index,
        assistant_message,
        tool_name: "app_lookup".to_string(),
        completion_boundary_id: delivered.boundary_id.clone(),
        scheduler_controlled: delivered.scheduler.scheduler_controlled,
        scheduler_sequence: delivered.sequence,
        turn_suspended_before_completion: suspended_before_completion,
        completed_event_count_before_resolution: completed_before,
        completed_event_count_after_resolution: completed_after,
        resolved_payload: resolution.clone(),
        completion_outcome: accepted.clone(),
        duplicate_completion_outcome: duplicate,
        turn_suspension_invariant: pending_tool_completion(
            suspended_before_completion,
            "pending ToolResult parked the live turn before external resolution",
        ),
        scheduler_resolution_invariant: pending_tool_completion(
            delivered.kind == BoundaryKind::Tool
                && delivered.scheduler.scheduler_controlled
                && matches!(accepted, lash_core::ResolveOutcome::Accepted),
            "BoundaryScheduler delivered the Tool boundary that resolved the await key",
        ),
        final_result_invariant: pending_tool_completion(
            final_ok,
            "tool completion produced ToolCallCompleted evidence and the second provider response finalized the turn",
        ),
    })
}

#[derive(Default)]
pub(super) struct RuntimeProofRecordingEvents {
    events: tokio::sync::Mutex<Vec<lash::TurnActivity>>,
}

impl RuntimeProofRecordingEvents {
    pub(super) async fn snapshot(&self) -> Vec<lash::TurnActivity> {
        self.events.lock().await.clone()
    }

    pub(super) async fn tool_completed_count(&self) -> usize {
        self.events
            .lock()
            .await
            .iter()
            .filter(|activity| matches!(activity.event, lash::TurnEvent::ToolCallCompleted { .. }))
            .count()
    }

    pub(super) async fn tool_completed_outputs(&self) -> Vec<(String, Value)> {
        self.events
            .lock()
            .await
            .iter()
            .filter_map(|activity| match &activity.event {
                lash::TurnEvent::ToolCallCompleted { name, output, .. } => {
                    Some((name.clone(), output.value_for_projection()))
                }
                _ => None,
            })
            .collect()
    }

    pub(super) async fn final_value_events(&self) -> Vec<Value> {
        self.events
            .lock()
            .await
            .iter()
            .filter_map(|activity| match &activity.event {
                lash::TurnEvent::FinalValue { value } => Some(value.clone()),
                _ => None,
            })
            .collect()
    }

    pub(super) async fn assistant_prose_delta_count(&self) -> usize {
        self.events
            .lock()
            .await
            .iter()
            .filter(|activity| {
                matches!(activity.event, lash::TurnEvent::AssistantProseDelta { .. })
            })
            .count()
    }
}

#[async_trait::async_trait]
impl lash::TurnActivitySink for RuntimeProofRecordingEvents {
    async fn emit(&self, activity: lash::TurnActivity) {
        self.events.lock().await.push(activity);
    }
}

pub(super) async fn prove_final_value_semantic_channel()
-> Result<FinalValueSemanticProof, FixedScriptRunnerError> {
    let events = Arc::new(RuntimeProofRecordingEvents::default());
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::new(
            lash_protocol_rlm::ExecutionBound::instructions(1_000_000),
            lash_protocol_rlm::ExecutionBound::secs(30),
            lash_protocol_rlm::ExecutionBound::instructions(64 * 1024 * 1024),
        ),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    let core = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .effect_host(Arc::new(
            lash::durability::InlineEffectHost::default().allow_process_lifetime_completion_keys(),
        ))
        .lease_timings(crate::lease::sim_runtime_lease_timings())
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .store_factory(Arc::new(
            lash::persistence::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(lash_core::TestLocalProcessRegistry::default())
            as Arc<dyn lash_core::ProcessRegistry>)
        .provider(rlm_final_value_provider())
        .model(
            lash_core::ModelSpec::builder("mock-rlm-final-value")
                .context_window_tokens(200_000)
                .build()
                .map_err(|error| FixedScriptRunnerError::Assertion(error.to_string()))?,
        )
        .build(crate::sim_process_owner())
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let session = core
        .session("sim-final-value-session")
        .open()
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let result = session
        .turn(lash::TurnInput::text("produce a semantic final value"))
        .require_finish()
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?
        .stream_to(events.as_ref())
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let final_value = result.final_value().cloned().ok_or_else(|| {
        FixedScriptRunnerError::Assertion(format!(
            "final-value proof finished without TurnFinish::FinalValue: {:?}",
            result.outcome
        ))
    })?;
    let recorded = events.snapshot().await;
    let final_value_events = events.final_value_events().await;
    let assistant_prose_delta_count = events.assistant_prose_delta_count().await;
    let facts = runtime_final_value_invariant_facts(&result, &recorded);
    let transcript_text = result
        .state
        .read_view()
        .messages()
        .iter()
        .flat_map(|message| message.parts.iter())
        .map(|part| part.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let transcript_contains_final_value =
        transcript_text.contains("semantic-channel") || transcript_text.contains("\"count\"");
    let semantic_ok = facts.passed
        && facts.outcome_kind == "final_value"
        && facts.semantic_value.as_ref() == Some(&final_value)
        && final_value_events.iter().any(|value| value == &final_value)
        && !facts.transcript_inference_required
        && result.assistant_message().is_none()
        && !transcript_contains_final_value;
    require(
        semantic_ok,
        "final-value proof did not observe a semantic TurnOutcome and FinalValue event",
    )?;
    Ok(FinalValueSemanticProof {
        schema: "lash.sim.final-value-semantic-proof.v1",
        name: "rlm-turn-final-value-semantic-channel",
        session_id: result.state.session_id,
        turn_index: result.state.turn_index,
        final_value,
        assistant_output_text: result.assistant_output.safe_text,
        final_value_event_count: final_value_events.len(),
        assistant_prose_delta_count,
        facts,
        semantic_channel_invariant: runtime_final_value_semantic(
            semantic_ok,
            "final value was read from TurnFinish::FinalValue and TurnEvent::FinalValue, not transcript prose",
        ),
    })
}

pub(super) fn rlm_final_value_provider() -> ProviderHandle {
    const RAW_FINAL: &str = "Visible prose before semantic value.\n<lashlang>\nfinish { source: \"semantic-channel\", ok: true, count: 3 }\n</lashlang>";
    const CHUNKS: &[&str] = &[
        "Visible prose",
        " before semantic value.\n<lash",
        "lang>\nfinish { source: ",
        "\"semantic-channel\", ok: true, count: 3 }",
        "\n</lashlang>",
    ];
    lash_core::testing::TestProvider::builder()
        .kind("lash-sim-rlm-final-value")
        .requires_streaming(true)
        .complete(|request| async move {
            let stream = request.stream_events.ok_or_else(|| {
                LlmTransportError::new("rlm final-value proof requires provider streaming")
            })?;
            for chunk in CHUNKS {
                stream.send(LlmStreamEvent::Delta((*chunk).to_string()));
            }
            let response = text_llm_response(RAW_FINAL);
            if response.full_text != RAW_FINAL || response_text_part(&response) != Some(RAW_FINAL)
            {
                return Err(LlmTransportError::new(format!(
                    "rlm final-value fixed response shape changed: expected {:?}, got full_text {:?} parts {:?}",
                    RAW_FINAL, response.full_text, response.parts
                )));
            }
            Ok(response)
        })
        .build()
        .into_handle()
}

struct PendingToolProvider {
    key_tx: Mutex<Option<tokio::sync::oneshot::Sender<lash_core::AwaitEventKey>>>,
}

impl PendingToolProvider {
    fn new(key_tx: tokio::sync::oneshot::Sender<lash_core::AwaitEventKey>) -> Self {
        Self {
            key_tx: Mutex::new(Some(key_tx)),
        }
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for PendingToolProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![pending_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(pending_tool_definition().contract()))
    }

    fn attempt_may_defer(&self, tool_id: &lash_core::ToolId) -> bool {
        tool_id == pending_tool_definition().id()
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
        if call.name != "app_lookup" {
            return lash_core::ToolResult::err_fmt(format_args!("unknown tool {}", call.name));
        }
        let key = match call.context.completion_key().await {
            Ok(key) => key,
            Err(err) => return lash_core::ToolResult::err_fmt(err),
        };
        if let Some(tx) = self.key_tx.lock_recover().take() {
            let _ = tx.send(key);
        }
        lash_core::ToolResult::pending(lash_core::PendingCompletion::new())
    }
}

fn pending_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:app_lookup",
        "app_lookup",
        "Look up app state.",
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        json!({ "type": "object" }),
    )
}

pub(super) fn pending_tool_roundtrip_provider() -> ProviderHandle {
    let responses = Arc::new(tokio::sync::Mutex::new(VecDeque::from([
        tool_call_llm_response("call-1", "app_lookup", "{}"),
        text_llm_response("done"),
    ])));
    lash_core::testing::TestProvider::builder()
        .kind("lash-sim-pending-tool")
        .complete(move |_request| {
            let responses = Arc::clone(&responses);
            async move {
                responses.lock().await.pop_front().ok_or_else(|| {
                    LlmTransportError::new("pending tool roundtrip provider exhausted")
                })
            }
        })
        .build()
        .into_handle()
}

/// A sim tool that registers its await key in a shared slot the generated world
/// can read, then returns `ToolResult::pending` so the calling turn parks until
/// the scheduler resolves the key. Generalizes `PendingToolProvider` for the
/// generated suspend sessions (Tool / DurableEffect / ExecCode).
pub(super) struct SuspendToolProvider {
    tool_name: String,
    key_slot: Arc<tokio::sync::Mutex<Option<lash_core::AwaitEventKey>>>,
}

impl SuspendToolProvider {
    pub(super) fn new(
        tool_name: String,
        key_slot: Arc<tokio::sync::Mutex<Option<lash_core::AwaitEventKey>>>,
    ) -> Self {
        Self {
            tool_name,
            key_slot,
        }
    }

    fn definition(&self) -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            format!("tool:{}", self.tool_name),
            self.tool_name.clone(),
            "Await an externally-resolved completion.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            json!({ "type": "object" }),
        )
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for SuspendToolProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![self.definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == self.tool_name).then(|| Arc::new(self.definition().contract()))
    }

    fn attempt_may_defer(&self, tool_id: &lash_core::ToolId) -> bool {
        tool_id == self.definition().id()
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
        if call.name != self.tool_name {
            return lash_core::ToolResult::err_fmt(format_args!("unknown tool {}", call.name));
        }
        let key = match call.context.completion_key().await {
            Ok(key) => key,
            Err(err) => return lash_core::ToolResult::err_fmt(err),
        };
        *self.key_slot.lock().await = Some(key);
        lash_core::ToolResult::pending(lash_core::PendingCompletion::new())
    }
}
