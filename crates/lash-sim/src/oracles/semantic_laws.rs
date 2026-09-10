use super::*;

pub(super) fn streamed_text_finalizes_once_fact(
    events: &[DeliveredBoundary],
) -> Result<ScenarioContractGeneratedFact, String> {
    let Some((provider, release)) =
        successful_provider_events(events)
            .into_iter()
            .find_map(|provider| {
                provider_event_for_turn(events, &provider.boundary_id)
                    .map(|release| (provider, release))
            })
    else {
        return Err(
            "streamed-text finalization did not find a successful provider boundary with scheduler-owned provider-event release".to_string(),
        );
    };
    let expected = provider
        .payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("");
    let output = provider
        .observed
        .get("provider_output")
        .and_then(Value::as_str)
        .unwrap_or("");
    let occurrences = if expected.is_empty() {
        0
    } else {
        output.matches(expected).count()
    };
    if occurrences != 1 {
        return Err(format!(
            "streamed-text finalization expected exactly one `{expected}` projection, found {occurrences}"
        ));
    }
    generated_fact(
        "standard_streamed_text_finalizes_once",
        "scheduler-owned streamed provider release produces exactly one final assistant projection",
        vec![provider, release],
        json!({
            "provider_boundary": provider.boundary_id,
            "provider_event_boundary": release.boundary_id,
            "projected_text": expected,
            "occurrences": occurrences,
        }),
    )
}

pub(super) fn parallel_tool_results_checkpoint_once_fact(
    events: &[DeliveredBoundary],
) -> Result<ScenarioContractGeneratedFact, String> {
    let Some((tool, provider)) = tool_then_same_actor_provider(events) else {
        return Err(
            "parallel tool checkpoint proof did not find tool result followed by same-actor provider continuation".to_string(),
        );
    };
    let releases = provider_events_for_turn(events, &provider.boundary_id);
    if releases.is_empty() {
        return Err(format!(
            "parallel tool checkpoint proof did not find scheduler-owned provider-event release evidence for `{}`",
            provider.boundary_id
        ));
    }
    let mut fact_events = vec![tool, provider];
    fact_events.extend(releases.iter().copied());
    generated_fact(
        "standard_parallel_tool_checkpoint_once",
        "parallel tool results execute once, checkpoint through scheduler-owned provider-event release, and re-enter the same actor",
        fact_events,
        json!({
            "tool_boundary": tool.boundary_id,
            "tool_sequence": tool.sequence,
            "continuation_provider_boundary": provider.boundary_id,
            "continuation_provider_sequence": provider.sequence,
            "provider_event_release_count": releases.len(),
            "actor": tool.actor_alias,
            "execution_count": 1,
        }),
    )
}

pub(super) fn standard_max_turns_after_tool_result_fact(
    events: &[DeliveredBoundary],
) -> Result<ScenarioContractGeneratedFact, String> {
    let proof_event = contract_execution_event(events, "standard.max_turns_after_tool_result")?;
    let execution = contract_execution_payload_matches_observed(
        proof_event,
        "standard.max_turns_after_tool_result",
        "standard_protocol_scenario_max_turns_terminates_after_tool_result",
    )?;
    let result = execution
        .get("result")
        .ok_or_else(|| "standard max-turn execution missing result".to_string())?;
    let anchor = execution
        .get("generated_anchor")
        .ok_or_else(|| "standard max-turn execution missing generated anchor".to_string())?;
    let tool_id = anchor
        .get("tool_boundary")
        .and_then(Value::as_str)
        .ok_or_else(|| "standard max-turn execution missing tool boundary id".to_string())?;
    let provider_id = anchor
        .get("continuation_provider_boundary")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            "standard max-turn execution missing continuation provider boundary id".to_string()
        })?;
    let tool = event_by_boundary_id(events, tool_id)
        .filter(|event| event.kind == BoundaryKind::Tool)
        .ok_or_else(|| {
            format!("standard max-turn execution referenced missing tool `{tool_id}`")
        })?;
    let provider = event_by_boundary_id(events, provider_id)
        .filter(|event| event.kind == BoundaryKind::Provider)
        .ok_or_else(|| {
            format!("standard max-turn execution referenced missing provider `{provider_id}`")
        })?;
    if tool.actor_alias != provider.actor_alias || provider.sequence <= tool.sequence {
        return Err(format!(
            "standard max-turn execution did not preserve tool -> same-actor provider continuation: tool={} provider={}",
            tool.boundary_id, provider.boundary_id
        ));
    }
    let stopped_at_max_turns = result.get("done").and_then(Value::as_bool) == Some(true)
        && result.get("max_turns").and_then(Value::as_u64) == Some(1)
        && result
            .get("turn_outcomes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|outcome| {
                outcome.get("kind").and_then(Value::as_str) == Some("stopped")
                    && outcome.get("stop_reason").and_then(Value::as_str) == Some("max_turns")
            })
        && result
            .get("execution_api")
            .and_then(Value::as_str)
            .is_some_and(|api| api.contains("TurnMachine"))
        && result.get("driver").and_then(Value::as_str)
            == Some("lash_protocol_standard::StandardDriver");
    if !stopped_at_max_turns {
        return Err(
            "standard max-turn execution did not record done=true and TurnStop::MaxTurns after a real tool result".to_string(),
        );
    }
    generated_fact(
        "standard_max_turns_after_tool_result",
        "tool result executes once, same-actor provider continuation is observed, and explicit max-turn stopped/done evidence terminates the contract",
        vec![tool, provider, proof_event],
        json!({
            "tool_boundary": tool.boundary_id,
            "continuation_provider_boundary": provider.boundary_id,
            "contract_execution_boundary": proof_event.boundary_id,
            "actor": tool.actor_alias,
            "tool_sequence": tool.sequence,
            "continuation_provider_sequence": provider.sequence,
            "done": true,
            "turn_outcomes": result.get("turn_outcomes").cloned().unwrap_or(Value::Null),
            "max_turns": result.get("max_turns").cloned().unwrap_or(Value::Null),
            "source": execution.get("source").cloned().unwrap_or(Value::Null),
        }),
    )
}

pub(super) fn trigger_then_provider_fact(
    events: &[DeliveredBoundary],
    fact: &'static str,
) -> Result<ScenarioContractGeneratedFact, String> {
    let Some((trigger, provider)) = events
        .iter()
        .filter(|event| {
            event.kind == BoundaryKind::Trigger
                && event
                    .observed
                    .get("trigger_delivered")
                    .and_then(Value::as_bool)
                    == Some(true)
                && event
                    .observed
                    .get("started_process")
                    .and_then(Value::as_bool)
                    == Some(true)
                && event
                    .observed
                    .get("occurrence_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id.starts_with("trigger:"))
        })
        .find_map(|trigger| {
            successful_provider_events(events)
                .into_iter()
                .filter(|provider| {
                    provider.actor_alias == trigger.actor_alias
                        && provider.sequence > trigger.sequence
                })
                .min_by_key(|provider| provider.sequence)
                .map(|provider| (trigger, provider))
        })
    else {
        return Err(format!(
            "trigger/provider semantic fact `{fact}` did not find a generated trigger followed by same-actor provider completion"
        ));
    };
    generated_fact(
        fact,
        "trigger delivery records occurrence/reservation data and later same-actor provider completion",
        vec![trigger, provider],
        json!({
            "trigger_boundary": trigger.boundary_id,
            "provider_boundary": provider.boundary_id,
            "actor": trigger.actor_alias,
            "occurrence_id": trigger.observed.get("occurrence_id").cloned().unwrap_or(Value::Null),
            "reservation_count": trigger.observed.get("reservation_count").cloned().unwrap_or(Value::Null),
        }),
    )
}

pub(super) fn exec_semantic_fact(
    events: &[DeliveredBoundary],
    fact: &'static str,
    requirement: ExecFactRequirement,
) -> Result<ScenarioContractGeneratedFact, String> {
    let Some(exec) = events.iter().find(|event| {
        event.kind == BoundaryKind::ExecCode
            && event
                .payload
                .pointer("/runtime_completion/completion_family")
                .and_then(Value::as_str)
                == Some("exec_result")
            && event.observed.get("runtime_effect_outcome").is_some()
            && event
                .observed
                .get("execution_count")
                .and_then(Value::as_u64)
                == Some(1)
    }) else {
        return Err(format!(
            "exec semantic fact `{fact}` did not find a scheduler-owned exec result with runtime effect outcome"
        ));
    };
    if matches!(
        requirement,
        ExecFactRequirement::NoToolCallReplay | ExecFactRequirement::ReentersProvider
    ) && !exec_outcome_has_no_tool_call_replay(std::slice::from_ref(exec))
    {
        return Err(format!(
            "exec semantic fact `{fact}` found replayed tool-call ids in exec runtime outcome"
        ));
    }
    let provider = if matches!(requirement, ExecFactRequirement::ReentersProvider) {
        let Some(provider) = successful_provider_events(events)
            .into_iter()
            .filter(|provider| {
                provider.actor_alias == exec.actor_alias && provider.sequence > exec.sequence
            })
            .min_by_key(|provider| provider.sequence)
        else {
            return Err(format!(
                "exec semantic fact `{fact}` did not find a later same-actor provider continuation"
            ));
        };
        Some(provider)
    } else {
        None
    };
    let mut fact_events = vec![exec];
    if let Some(provider) = provider {
        fact_events.push(provider);
    }
    generated_fact(
        fact,
        "exec-code boundary completed once through runtime effect outcome and preserved protocol-owned result channel",
        fact_events,
        json!({
            "exec_boundary": exec.boundary_id,
            "actor": exec.actor_alias,
            "runtime_effect_outcome_type": exec.observed.pointer("/runtime_effect_outcome/type").cloned().unwrap_or(Value::Null),
            "execution_count": 1,
            "tool_call_replay_absent": exec_outcome_has_no_tool_call_replay(std::slice::from_ref(exec)),
            "continuation_provider_boundary": provider.map(|provider| provider.boundary_id.as_str()),
        }),
    )
}

pub(super) fn backend_retry_terminalization_fact(
    events: &[DeliveredBoundary],
    fact: &'static str,
) -> Result<ScenarioContractGeneratedFact, String> {
    let mut by_session_operation: BTreeMap<(String, String), Vec<&DeliveredBoundary>> =
        BTreeMap::new();
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::BackendFailure)
    {
        let operation = event
            .observed
            .get("operation")
            .or_else(|| event.payload.get("operation"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let session = event
            .observed
            .get("session")
            .and_then(Value::as_str)
            .unwrap_or(event.actor_alias.as_str())
            .to_string();
        by_session_operation
            .entry((session, operation))
            .or_default()
            .push(event);
    }
    let Some(mut operation_events) = by_session_operation.into_values().find(|events| {
        let retryable = events.iter().any(|event| {
            event.observed.get("retryable").and_then(Value::as_bool) == Some(true)
                && event
                    .observed
                    .pointer("/production_store_error/retryable_class")
                    .and_then(Value::as_bool)
                    == Some(true)
                && event
                    .observed
                    .pointer("/fault_injector/point")
                    .and_then(Value::as_str)
                    == Some("after_begin")
        });
        let terminal = events.iter().any(|event| {
            event.observed.get("retryable").and_then(Value::as_bool) == Some(false)
                && event
                    .observed
                    .get("store_error_class")
                    .and_then(Value::as_str)
                    == Some("terminal_backend_error")
                && event
                    .observed
                    .pointer("/fault_injector/point")
                    .and_then(Value::as_str)
                    == Some("commit_io")
        });
        retryable && terminal
    }) else {
        return Err(format!(
            "backend semantic fact `{fact}` did not find retryable-to-terminal backend failure sequence"
        ));
    };
    operation_events.sort_by_key(|event| event.sequence);
    let observed_events = operation_events
        .iter()
        .map(|event| {
            json!({
                "boundary_id": event.boundary_id,
                "attempt": event.observed.get("attempt").cloned().unwrap_or(Value::Null),
                "retryable": event.observed.get("retryable").cloned().unwrap_or(Value::Null),
                "store_error_class": event.observed.get("store_error_class").cloned().unwrap_or(Value::Null),
            })
        })
        .collect::<Vec<_>>();
    generated_fact(
        fact,
        "backend failure evidence advances from retryable production StoreError to terminal StoreError",
        operation_events,
        json!({
            "backend_failures": observed_events,
        }),
    )
}

pub(super) fn durable_replay_fact(
    events: &[DeliveredBoundary],
    fact: &'static str,
) -> Result<ScenarioContractGeneratedFact, String> {
    let mut by_key: BTreeMap<String, Vec<&DeliveredBoundary>> = BTreeMap::new();
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::DurableEffect)
    {
        if let Some(key) = event.observed.get("durable_key").and_then(Value::as_str) {
            by_key.entry(key.to_string()).or_default().push(event);
        }
    }
    let Some((key, mut durable_events)) = by_key.into_iter().find(|(_key, events)| {
        let first = events.iter().any(|event| {
            event.observed.get("replayed").and_then(Value::as_bool) == Some(false)
                && event
                    .observed
                    .pointer("/runtime_effect/local_executor_called")
                    .and_then(Value::as_bool)
                    == Some(true)
        });
        let replay = events.iter().any(|event| {
            event.observed.get("replayed").and_then(Value::as_bool) == Some(true)
                && event
                    .observed
                    .pointer("/runtime_effect/local_executor_called")
                    .and_then(Value::as_bool)
                    == Some(false)
                && event
                    .observed
                    .get("execution_count")
                    .and_then(Value::as_u64)
                    == Some(1)
        });
        first && replay
    }) else {
        return Err(format!(
            "durable semantic fact `{fact}` did not find first-execution plus replay evidence for one durable key"
        ));
    };
    durable_events.sort_by_key(|event| event.sequence);
    generated_fact(
        fact,
        "durable effect executes locally once and replay returns stored history without local execution",
        durable_events,
        json!({
            "durable_key": key,
            "first_execution": true,
            "replay": true,
        }),
    )
}

pub(super) fn process_wake_fact(
    events: &[DeliveredBoundary],
    fact: &'static str,
) -> Result<ScenarioContractGeneratedFact, String> {
    let mut by_source_key: BTreeMap<String, Vec<&DeliveredBoundary>> = BTreeMap::new();
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::ProcessWake)
    {
        if let Some(source_key) = process_wake_source_key(event) {
            by_source_key.entry(source_key).or_default().push(event);
        }
    }
    let Some((source_key, mut wake_events)) = by_source_key.into_iter().find(|(_key, events)| {
        events.iter().any(|event| {
            event
                .observed
                .pointer("/runtime_process_wake/event_invocation/subject/process_id")
                .and_then(Value::as_str)
                .is_some()
                && event
                    .observed
                    .get("session")
                    .and_then(Value::as_str)
                    .is_some()
        }) && events
            .iter()
            .filter_map(|event| event.observed.get("claimed_once").and_then(Value::as_bool))
            .collect::<Vec<_>>()
            .contains(&false)
    }) else {
        return Err(format!(
            "process wake semantic fact `{fact}` did not find structural source-key evidence with a rejected duplicate"
        ));
    };
    wake_events.sort_by_key(|event| event.sequence);
    let sessions = wake_events
        .iter()
        .filter_map(|event| event.observed.get("session").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();
    let claimed_once_values = wake_events
        .iter()
        .filter_map(|event| event.observed.get("claimed_once").and_then(Value::as_bool))
        .collect::<Vec<_>>();
    generated_fact(
        fact,
        "process wake carries runtime DTO process id, session, structural source key, and at-most-once claim evidence",
        wake_events,
        json!({
            "source_key": source_key,
            "sessions": sessions,
            "claimed_once_values": claimed_once_values,
        }),
    )
}

pub(super) fn agent_shell_output_projection_fact(
    events: &[DeliveredBoundary],
) -> Result<ScenarioContractGeneratedFact, String> {
    let Some((exec, provider)) = events
        .iter()
        .filter(|event| {
            event.kind == BoundaryKind::ExecCode
                && event
                    .payload
                    .pointer("/runtime_completion/completion_family")
                    .and_then(Value::as_str)
                    == Some("exec_result")
                && event
                    .observed
                    .pointer("/runtime_effect_outcome/result/Ok/calls")
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty)
                && event
                    .observed
                    .get("execution_count")
                    .and_then(Value::as_u64)
                    == Some(1)
        })
        .find_map(|exec| {
            successful_provider_events(events)
                .into_iter()
                .filter(|provider| {
                    provider.actor_alias == exec.actor_alias && provider.sequence > exec.sequence
                })
                .min_by_key(|provider| provider.sequence)
                .map(|provider| (exec, provider))
        })
    else {
        return Err(
            "agent shell output projection did not find an exec data result followed by same-actor provider projection"
                .to_string(),
        );
    };
    generated_fact(
        "agent_shell_output_projection_survives",
        "shell exec output is scheduler-owned data and a later same-actor provider turn projects it without replaying tool calls",
        vec![exec, provider],
        json!({
            "exec_boundary": exec.boundary_id,
            "projection_provider_boundary": provider.boundary_id,
            "actor": exec.actor_alias,
            "exec_sequence": exec.sequence,
            "provider_sequence": provider.sequence,
            "exec_result_channel": "runtime_effect_outcome.result.Ok",
            "tool_calls_replayed": false,
        }),
    )
}

pub(super) fn agent_session_turn_child_provider_fact(
    events: &[DeliveredBoundary],
) -> Result<ScenarioContractGeneratedFact, String> {
    let Some((wake, provider)) = events
        .iter()
        .filter(|event| {
            event.kind == BoundaryKind::ProcessWake
                && event
                    .observed
                    .pointer("/runtime_process_wake/event_invocation/subject/process_id")
                    .and_then(Value::as_str)
                    .is_some()
                && event
                    .observed
                    .get("session")
                    .and_then(Value::as_str)
                    .is_some()
                && event
                    .observed
                    .pointer("/runtime_queued_work/claimed")
                    .and_then(Value::as_bool)
                    == Some(true)
        })
        .find_map(|wake| {
            let provider = successful_provider_events(events)
                .into_iter()
                .min_by_key(|provider| provider.sequence)?;
            Some((wake, provider))
        })
    else {
        return Err(
            "agent session-turn child did not find a claimed process wake and scheduler-owned provider completion in the same generated trace"
                .to_string(),
        );
    };
    generated_fact(
        "agent_session_turn_process_child_provider",
        "session-turn process child wake is claimed through runtime queued work while the same generated trace carries scheduler-owned provider completion evidence",
        vec![wake, provider],
        json!({
            "process_wake_boundary": wake.boundary_id,
            "child_provider_boundary": provider.boundary_id,
            "child_session": wake.observed.get("session").cloned().unwrap_or(Value::Null),
            "process_id": wake.observed.pointer("/runtime_process_wake/event_invocation/subject/process_id").cloned().unwrap_or(Value::Null),
            "runtime_queued_work_claimed": true,
            "wake_sequence": wake.sequence,
            "provider_sequence": provider.sequence,
        }),
    )
}

pub(super) fn worker_stale_fact(
    events: &[DeliveredBoundary],
    fact: &'static str,
) -> Result<ScenarioContractGeneratedFact, String> {
    let Some(worker) = events.iter().find(|event| {
        event.kind == BoundaryKind::Worker
            && event.observed.get("runtime_active_lease").is_some()
            && event.observed.get("runtime_stale_completion").is_some()
            && event
                .observed
                .get("stale_completion_rejected")
                .and_then(Value::as_bool)
                == Some(true)
    }) else {
        return Err(format!(
            "worker semantic fact `{fact}` did not find stale completion rejection evidence"
        ));
    };
    generated_fact(
        fact,
        "worker evidence rejects stale completion while preserving active lease data",
        vec![worker],
        json!({
            "worker_boundary": worker.boundary_id,
            "session": worker.observed.get("session").cloned().unwrap_or(Value::Null),
            "stale_completion_rejected": true,
        }),
    )
}

pub(super) fn observer_reconnect_fact(
    events: &[DeliveredBoundary],
    fact: &'static str,
) -> Result<ScenarioContractGeneratedFact, String> {
    let Some(observer) = events.iter().find(|event| {
        event.kind == BoundaryKind::Observer
            && event.observed.get("reconnected").and_then(Value::as_bool) == Some(true)
            && event
                .observed
                .get("turn_index")
                .and_then(Value::as_u64)
                .is_some()
    }) else {
        return Err(format!(
            "observer semantic fact `{fact}` did not find reconnected observer turn-index evidence"
        ));
    };
    generated_fact(
        fact,
        "observer reconnect boundary records the converged turn index",
        vec![observer],
        json!({
            "observer_boundary": observer.boundary_id,
            "turn_index": observer.observed.get("turn_index").cloned().unwrap_or(Value::Null),
        }),
    )
}

/// An unmapped scenario-contract semantic oracle. Each contract must own a
/// distinct semantic adapter; a new or renamed contract that reaches here fails
/// loudly rather than passing through a decorative shared fallback.
pub(super) fn unmapped_scenario_semantic(
    suite: &str,
    semantic_oracle: &str,
) -> ScenarioSemanticVerdict {
    ScenarioSemanticVerdict::failed(format!(
        "{suite} scenario contract `{semantic_oracle}` has no per-contract semantic adapter; add distinct evidence instead of a generic fallback"
    ))
}

pub(super) fn assert_semantic(condition: bool, reason: &'static str) -> ScenarioSemanticVerdict {
    if condition {
        ScenarioSemanticVerdict::passed(reason)
    } else {
        ScenarioSemanticVerdict::failed(reason)
    }
}

pub(super) fn queued_active_turn_input_hidden_semantics(events: &[DeliveredBoundary]) -> bool {
    events
        .iter()
        .filter(|event| event.kind == BoundaryKind::QueuedIngress)
        .any(|queued| {
            let source_key = queued.payload.get("source_key").and_then(Value::as_str);
            let observed_source_key = queued.observed.get("source_key").and_then(Value::as_str);
            let text = queued
                .payload
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("");
            let active_turn_queued = queued.payload.get("ingress_mode").and_then(Value::as_str)
                == Some("active_turn")
                && queued.observed.get("ingress_mode").and_then(Value::as_str)
                    == Some("active_turn")
                && queued
                    .observed
                    .get("input_state")
                    .and_then(Value::as_str)
                    .is_some_and(|state| state.starts_with("pending"))
                && source_key.is_some()
                && source_key == observed_source_key
                && queued
                    .observed
                    .get("active_turn_id")
                    .or_else(|| queued.payload.get("active_turn_id"))
                    .and_then(Value::as_str)
                    .is_some_and(|turn_id| !turn_id.is_empty());
            let active_turn_id = queued
                .observed
                .get("active_turn_id")
                .or_else(|| queued.payload.get("active_turn_id"))
                .and_then(Value::as_str);
            if !active_turn_queued {
                return false;
            }
            let live_turn_in_flight = active_turn_id.is_some_and(|turn_id| {
                let provider_completed_after_queue = events.iter().any(|event| {
                    event.kind == BoundaryKind::Provider
                        && event.boundary_id == turn_id
                        && event.actor_alias == queued.actor_alias
                        && event.sequence > queued.sequence
                        && event
                            .observed
                            .get("success")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                });
                let provider_release_after_queue = events.iter().any(|event| {
                    event.kind == BoundaryKind::ProviderEvent
                        && event.actor_alias == queued.actor_alias
                        && event.sequence > queued.sequence
                        && event
                            .payload
                            .get("turn_boundary_id")
                            .and_then(Value::as_str)
                            == Some(turn_id)
                        && event
                            .observed
                            .get("released_while_turn_pending")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                });
                provider_completed_after_queue && provider_release_after_queue
            });
            let leaked = events.iter().any(|event| {
                event.kind == BoundaryKind::Provider
                    && event.actor_alias == queued.actor_alias
                    && event.sequence > queued.sequence
                    && event
                        .observed
                        .get("provider_output")
                        .and_then(Value::as_str)
                        .is_some_and(|output| !text.is_empty() && output.contains(text))
            });
            live_turn_in_flight && !leaked
        })
}

pub(super) fn cancellation_terminalizes_pending_input(events: &[DeliveredBoundary]) -> bool {
    let queued = events
        .iter()
        .filter(|event| event.kind == BoundaryKind::QueuedIngress)
        .map(|event| (event.boundary_id.as_str(), event))
        .collect::<BTreeMap<_, _>>();
    events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Cancellation)
        .any(|cancel| {
            let Some(target) = cancel.observed.get("target").and_then(Value::as_str) else {
                return false;
            };
            let Some(queued) = queued.get(target).copied() else {
                return false;
            };
            let queued_text = queued
                .payload
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("");
            let registered_after = cancel
                .payload
                .pointer("/runtime_completion/registered_after")
                .and_then(Value::as_str);
            let cancelled = cancel
                .observed
                .get("cancelled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && cancel
                    .observed
                    .get("cancel_outcome")
                    .and_then(Value::as_str)
                    == Some("cancelled")
                && cancel.sequence > queued.sequence
                && registered_after == Some(target);
            let leaked_after_cancel = events.iter().any(|event| {
                event.kind == BoundaryKind::Provider
                    && event.actor_alias == cancel.actor_alias
                    && event.sequence > cancel.sequence
                    && event
                        .observed
                        .get("provider_output")
                        .and_then(Value::as_str)
                        .is_some_and(|output| {
                            !queued_text.is_empty() && output.contains(queued_text)
                        })
            });
            cancelled && !leaked_after_cancel
        })
}

pub(super) fn trigger_wakeup_route_semantics(events: &[DeliveredBoundary]) -> bool {
    events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Trigger)
        .any(|event| {
            let payload_source_key = event.payload.get("source_key").and_then(Value::as_str);
            event
                .observed
                .get("trigger_delivered")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && event
                    .observed
                    .get("started_process")
                    .and_then(Value::as_bool)
                    == Some(true)
                && event.observed.get("session").and_then(Value::as_str)
                    == Some(event.actor_alias.as_str())
                && event
                    .observed
                    .get("occurrence_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id.starts_with("trigger:"))
                && event
                    .observed
                    .get("reservation_count")
                    .and_then(Value::as_u64)
                    .is_some_and(|count| count > 0)
                && payload_source_key.is_some()
                && event.observed.get("source_key").and_then(Value::as_str) == payload_source_key
        })
}

pub(super) fn backend_retry_terminalization_semantics(events: &[DeliveredBoundary]) -> bool {
    let mut by_session_operation: BTreeMap<(String, String), Vec<&DeliveredBoundary>> =
        BTreeMap::new();
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::BackendFailure)
    {
        let operation = event
            .observed
            .get("operation")
            .or_else(|| event.payload.get("operation"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let session = event
            .observed
            .get("session")
            .and_then(Value::as_str)
            .unwrap_or(event.actor_alias.as_str())
            .to_string();
        by_session_operation
            .entry((session, operation))
            .or_default()
            .push(event);
    }
    by_session_operation.values().any(|events| {
        let mut events = events.clone();
        events.sort_by_key(|event| event.sequence);
        let mut saw_retryable = false;
        let mut last_attempt = 0;
        for event in events {
            let attempt = event
                .observed
                .get("attempt")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if attempt == 0 || attempt <= last_attempt {
                return false;
            }
            last_attempt = attempt;
            let Some(retryable) = event.observed.get("retryable").and_then(Value::as_bool) else {
                return false;
            };
            let Some(store_error_retryable) = event
                .observed
                .pointer("/production_store_error/retryable_class")
                .and_then(Value::as_bool)
            else {
                return false;
            };
            let observed_fault_point = event
                .observed
                .pointer("/fault_injector/point")
                .and_then(Value::as_str);
            if retryable && store_error_retryable && observed_fault_point == Some("after_begin") {
                saw_retryable = true;
                continue;
            }
            if saw_retryable
                && !retryable
                && !store_error_retryable
                && observed_fault_point == Some("commit_io")
                && event
                    .observed
                    .get("store_error_class")
                    .and_then(Value::as_str)
                    == Some("terminal_backend_error")
            {
                return true;
            }
        }
        false
    })
}

pub(super) fn duplicate_delivery_semantics(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> bool {
    structural_process_wake_identity_semantics(events)
        && durable_effect_replay_semantics(events, summary)
}

pub(super) fn structural_process_wake_identity_semantics(events: &[DeliveredBoundary]) -> bool {
    let mut by_source_key: BTreeMap<String, Vec<&DeliveredBoundary>> = BTreeMap::new();
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::ProcessWake)
    {
        if let Some(source_key) = process_wake_source_key(event) {
            by_source_key.entry(source_key).or_default().push(event);
        }
    }
    by_source_key.values().any(|events| {
        let claims = events
            .iter()
            .filter_map(|event| event.observed.get("claimed_once").and_then(Value::as_bool))
            .collect::<Vec<_>>();
        let queued_claims = events
            .iter()
            .filter_map(|event| {
                event
                    .observed
                    .pointer("/runtime_queued_work/claimed")
                    .and_then(Value::as_bool)
            })
            .collect::<Vec<_>>();
        let strict_claim_dedupe = claims.iter().filter(|claimed| **claimed).count() == 1
            && claims.contains(&false)
            && queued_claims.iter().filter(|claimed| **claimed).count() == 1
            && queued_claims.contains(&false);
        let in_flight_rejection = events
            .iter()
            .filter(|event| {
                event
                    .observed
                    .get("lease_busy")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    && event.observed.get("runtime_process_wake").is_some()
                    && event
                        .observed
                        .pointer("/runtime_queued_work/enqueued")
                        .and_then(Value::as_bool)
                        == Some(false)
            })
            .count()
            > 0
            && claims.contains(&false);
        strict_claim_dedupe || in_flight_rejection
    })
}

pub(super) fn process_wake_source_key(event: &DeliveredBoundary) -> Option<String> {
    if let Some(source_key) = event
        .observed
        .pointer("/runtime_queued_work/source_key")
        .and_then(Value::as_str)
    {
        return Some(source_key.to_string());
    }
    let process_id = event
        .observed
        .pointer("/runtime_process_wake/process_id")
        .and_then(Value::as_str)?;
    let sequence = event
        .observed
        .pointer("/runtime_process_wake/sequence")
        .and_then(Value::as_u64)?;
    Some(lash_core::facade_support::process_wake_source_key(
        &ProcessId::from(process_id),
        sequence,
    ))
}

pub(super) fn durable_effect_replay_semantics(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> bool {
    summary
        .durable_effects
        .iter()
        .any(|effect| effect.execution_count == 1 && effect.replay_count > 0)
        && {
            let mut by_key: BTreeMap<String, Vec<&DeliveredBoundary>> = BTreeMap::new();
            for event in events
                .iter()
                .filter(|event| event.kind == BoundaryKind::DurableEffect)
            {
                let Some(key) = event.observed.get("durable_key").and_then(Value::as_str) else {
                    continue;
                };
                by_key.entry(key.to_string()).or_default().push(event);
            }
            by_key.values().any(|events| {
                let first = events.iter().any(|event| {
                    event.observed.get("replayed").and_then(Value::as_bool) == Some(false)
                        && event
                            .observed
                            .pointer("/runtime_effect/local_executor_called")
                            .and_then(Value::as_bool)
                            == Some(true)
                });
                let replay = events.iter().any(|event| {
                    event.observed.get("replayed").and_then(Value::as_bool) == Some(true)
                        && event
                            .observed
                            .pointer("/runtime_effect/local_executor_called")
                            .and_then(Value::as_bool)
                            == Some(false)
                        && event
                            .observed
                            .get("execution_count")
                            .and_then(Value::as_u64)
                            == Some(1)
                        && event.observed.get("replay_count").and_then(Value::as_u64) == Some(1)
                });
                first && replay
            })
        }
}

pub(super) fn protocol_terminal_state_semantics(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> bool {
    duplicate_free_stream_finalization(events, summary)
        && provider_rate_limit_terminalized_by_scripted_parsers(events)
        && provider_dropped_terminal_event_classified(events)
}

pub(super) fn queued_ingress_has_source_keys(events: &[DeliveredBoundary]) -> bool {
    events.iter().any(|event| {
        event.kind == BoundaryKind::QueuedIngress
            && event
                .observed
                .get("source_key")
                .and_then(Value::as_str)
                .is_some_and(|source_key| !source_key.is_empty())
    })
}

pub(super) fn queued_inputs_have_cancel_targets(events: &[DeliveredBoundary]) -> bool {
    let queued = events
        .iter()
        .filter(|event| event.kind == BoundaryKind::QueuedIngress)
        .map(|event| event.boundary_id.as_str())
        .collect::<BTreeSet<_>>();
    events.iter().any(|event| {
        event.kind == BoundaryKind::Cancellation
            && event
                .observed
                .get("target")
                .and_then(Value::as_str)
                .is_some_and(|target| queued.contains(target))
    })
}

pub(super) fn provider_turns_after_queue(summary: &AbstractWorldSummary) -> bool {
    summary
        .sessions
        .iter()
        .any(|session| session.queued_ingress_count > 0 && session.provider_turns.len() >= 2)
}

pub(super) fn process_wake_runtime_dto_observed(events: &[DeliveredBoundary]) -> bool {
    events.iter().any(|event| {
        event.kind == BoundaryKind::ProcessWake
            && event.observed.get("runtime_process_wake").is_some()
            && event
                .observed
                .pointer("/runtime_process_wake/event_invocation/subject/process_id")
                .and_then(Value::as_str)
                .is_some()
    })
}

pub(super) fn worker_runtime_lease_dto_observed(events: &[DeliveredBoundary]) -> bool {
    events.iter().any(|event| {
        event.kind == BoundaryKind::Worker
            && event.observed.get("runtime_active_lease").is_some()
            && event.observed.get("runtime_stale_completion").is_some()
            && event
                .observed
                .get("stale_completion_rejected")
                .and_then(Value::as_bool)
                .unwrap_or(false)
    })
}

pub(super) fn durable_runtime_effect_observed(events: &[DeliveredBoundary]) -> bool {
    events.iter().any(|event| {
        event.kind == BoundaryKind::DurableEffect
            && event.observed.get("runtime_effect").is_some()
            && event
                .observed
                .get("execution_count")
                .and_then(Value::as_u64)
                == Some(1)
    })
}

pub(super) fn tool_runtime_output_observed(events: &[DeliveredBoundary]) -> bool {
    events.iter().any(|event| {
        event.kind == BoundaryKind::Tool
            && event.observed.get("runtime_tool_output").is_some()
            && event
                .observed
                .get("execution_count")
                .and_then(Value::as_u64)
                == Some(1)
    })
}

pub(super) fn exec_runtime_outcome_observed(events: &[DeliveredBoundary]) -> bool {
    events.iter().any(|event| {
        event.kind == BoundaryKind::ExecCode
            && event.observed.get("runtime_effect_outcome").is_some()
            && event
                .observed
                .get("execution_count")
                .and_then(Value::as_u64)
                == Some(1)
    })
}

pub(super) fn exec_outcome_has_no_tool_call_replay(events: &[DeliveredBoundary]) -> bool {
    events.iter().any(|event| {
        if event.kind != BoundaryKind::ExecCode {
            return false;
        }
        let Some(outcome) = event.observed.get("runtime_effect_outcome") else {
            return false;
        };
        if outcome.get("type").and_then(Value::as_str) != Some("exec_code") {
            return false;
        }
        outcome
            .pointer("/result/Ok/calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| {
                calls.iter().all(|call| {
                    call.get("host_record")
                        .is_none_or(serde_json::Value::is_null)
                })
            })
    })
}

pub(super) fn duplicate_free_stream_finalization(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> bool {
    let provider_count = events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Provider)
        .count();
    let observed_turns = summary
        .sessions
        .iter()
        .map(|session| session.provider_turns.len())
        .sum::<usize>();
    provider_count == observed_turns
        && provider_turn_exchange_counts_are_indexed(summary)
        && observer_convergence_law(summary, None).is_passed()
}

pub(super) fn provider_mutation_parser_matrix_observed(events: &[DeliveredBoundary]) -> bool {
    events.iter().any(|event| {
        event.kind == BoundaryKind::ProviderMutation
            && event
                .observed
                .pointer("/provider_parser_matrix/matrix/real_provider_parser_execution")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            && event
                .observed
                .pointer("/provider_parser_matrix/matrix/provider_kinds")
                .and_then(Value::as_array)
                .is_some_and(|providers| {
                    let providers = providers
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<BTreeSet<_>>();
                    ["openai-compatible", "openai", "anthropic", "google_oauth"]
                        .into_iter()
                        .all(|provider| providers.contains(provider))
                })
    })
}

pub(super) fn provider_mutation_classes_observed(events: &[DeliveredBoundary]) -> bool {
    let mutations = events
        .iter()
        .filter(|event| event.kind == BoundaryKind::ProviderMutation)
        .filter_map(|event| {
            event
                .observed
                .get("mutation")
                .or_else(|| event.payload.get("mutation"))
                .and_then(Value::as_str)
        })
        .collect::<BTreeSet<_>>();
    mutations.contains("malformed_sse_chunk") && mutations.contains("rate_limit_error_envelope")
}

pub(super) fn provider_rate_limit_terminalized_by_scripted_parsers(
    events: &[DeliveredBoundary],
) -> bool {
    events
        .iter()
        .filter(|event| event.kind == BoundaryKind::ProviderMutation)
        .filter(|event| {
            event
                .observed
                .get("mutation")
                .or_else(|| event.payload.get("mutation"))
                .and_then(Value::as_str)
                == Some("rate_limit_error_envelope")
        })
        .any(|event| {
            let Some(proofs) = event
                .observed
                .pointer("/provider_parser_matrix/matrix/proofs")
                .and_then(Value::as_array)
            else {
                return false;
            };
            let mut providers = BTreeSet::new();
            let mut retryable_provider_observed = false;
            for proof in proofs {
                let Some(provider_kind) = proof.get("provider_kind").and_then(Value::as_str) else {
                    continue;
                };
                let terminal_reason = proof.get("terminal_reason").and_then(Value::as_str);
                let status = proof
                    .get("classification")
                    .and_then(|classification| classification.get("status"))
                    .and_then(Value::as_u64)
                    .or_else(|| proof.get("status").and_then(Value::as_u64));
                let retryable = proof
                    .get("classification")
                    .and_then(|classification| classification.get("retryable"))
                    .and_then(Value::as_bool);
                let kind = proof
                    .get("classification")
                    .and_then(|classification| classification.get("kind"))
                    .and_then(Value::as_str);
                if terminal_reason == Some("provider_error")
                    && status == Some(429)
                    && retryable.is_some()
                    && kind.is_some()
                {
                    retryable_provider_observed |= retryable == Some(true);
                    providers.insert(provider_kind);
                }
            }
            MIGRATED_RUNTIME_PROVIDER_KINDS
                .iter()
                .filter(|provider| **provider != "google_oauth")
                .all(|provider| providers.contains(*provider))
                && retryable_provider_observed
        })
}

pub(super) fn provider_dropped_terminal_event_classified(events: &[DeliveredBoundary]) -> bool {
    events
        .iter()
        .filter(|event| event.kind == BoundaryKind::ProviderMutation)
        .filter(|event| {
            event
                .observed
                .get("mutation")
                .or_else(|| event.payload.get("mutation"))
                .and_then(Value::as_str)
                == Some("dropped_terminal_event")
        })
        .any(|event| {
            let Some(proofs) = event
                .observed
                .pointer("/provider_parser_matrix/matrix/proofs")
                .and_then(Value::as_array)
            else {
                return false;
            };
            let providers = proofs
                .iter()
                .filter(|proof| {
                    proof.get("terminal_reason").and_then(Value::as_str) == Some("provider_error")
                        && proof
                            .get("classification")
                            .and_then(|classification| classification.get("retryable"))
                            .and_then(Value::as_bool)
                            == Some(false)
                })
                .filter_map(|proof| proof.get("provider_kind").and_then(Value::as_str))
                .collect::<BTreeSet<_>>();
            MIGRATED_RUNTIME_PROVIDER_KINDS
                .iter()
                .all(|provider| providers.contains(*provider))
        })
}

pub(super) fn trigger_delivery_runtime_observed(events: &[DeliveredBoundary]) -> bool {
    events.iter().any(|event| {
        event.kind == BoundaryKind::Trigger
            && event
                .observed
                .get("occurrence_id")
                .and_then(Value::as_str)
                .is_some_and(|id| id.starts_with("trigger:"))
            && event
                .observed
                .get("reservation_count")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0
    })
}

pub(super) fn provider_turn_exchange_counts_are_indexed(summary: &AbstractWorldSummary) -> bool {
    summary.sessions.iter().all(|session| {
        !session.provider_turns.is_empty()
            && session
                .provider_turns
                .iter()
                .enumerate()
                .all(|(index, turn)| turn.exchange_count == Some(index as u64 + 1))
    })
}

pub(super) fn observer_reconnect_has_matching_turn(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> bool {
    let reconnect_seen = events.iter().any(|event| {
        event.kind == BoundaryKind::Observer
            && event
                .observed
                .get("reconnected")
                .and_then(Value::as_bool)
                .unwrap_or(false)
    });
    reconnect_seen
        && summary.sessions.iter().all(|session| {
            session.observer_turn_indices.last().copied() == Some(session.provider_turns.len())
        })
}

pub fn replay_determinism(
    expected: &AbstractWorldSummary,
    actual: &AbstractWorldSummary,
) -> OracleVerdict {
    // Fencing tokens are monotonic backend implementation details, not semantic
    // output. A durable backend may consume an extra token while preserving the
    // same ownership transitions and stale-writer rejection. Keep the raw values
    // in artifacts for diagnosis, but compare the behavior they protect.
    let workers_match = expected.workers.len() == actual.workers.len()
        && expected
            .workers
            .iter()
            .zip(&actual.workers)
            .all(|(expected, actual)| {
                expected.worker_alias == actual.worker_alias
                    && expected.session_alias == actual.session_alias
                    && expected.active_incarnation_id == actual.active_incarnation_id
                    && expected.lease_owner_changes == actual.lease_owner_changes
                    && expected.stale_completion_rejections == actual.stale_completion_rejections
                    && expected.process_stale_completion_rejected
                        == actual.process_stale_completion_rejected
                    && expected.process_stale_output_absent == actual.process_stale_output_absent
                    && expected.process_terminal_writer == actual.process_terminal_writer
                    && expected.process_terminal_event_count == actual.process_terminal_event_count
            });
    let semantic_match = expected.session_count == actual.session_count
        && expected.total_events == actual.total_events
        && expected.sessions == actual.sessions
        && expected.durable_effects == actual.durable_effects
        && workers_match;
    if semantic_match {
        OracleVerdict::passed(
            REPLAY_DETERMINISM_ORACLE,
            "replay reproduced the delivered boundary sequence and semantic final abstract summary",
        )
    } else {
        OracleVerdict::failed(
            REPLAY_DETERMINISM_ORACLE,
            format!(
                "replay summary digest diverged: expected {}, actual {}",
                expected.digest, actual.digest
            ),
        )
    }
}

pub fn runtime_provider_turn(ok: bool, message: impl Into<String>) -> OracleVerdict {
    if ok {
        OracleVerdict::passed(RUNTIME_PROVIDER_TURN_ORACLE, message)
    } else {
        OracleVerdict::failed(RUNTIME_PROVIDER_TURN_ORACLE, message)
    }
}

pub fn pending_tool_completion(ok: bool, message: impl Into<String>) -> OracleVerdict {
    if ok {
        OracleVerdict::passed(PENDING_TOOL_COMPLETION_ORACLE, message)
    } else {
        OracleVerdict::failed(PENDING_TOOL_COMPLETION_ORACLE, message)
    }
}

pub fn runtime_final_value_semantic(ok: bool, message: impl Into<String>) -> OracleVerdict {
    if ok {
        OracleVerdict::passed(RUNTIME_FINAL_VALUE_SEMANTIC_ORACLE, message)
    } else {
        OracleVerdict::failed(RUNTIME_FINAL_VALUE_SEMANTIC_ORACLE, message)
    }
}

pub(super) fn runtime_observed_fact<T>(event: &DeliveredBoundary, key: &str) -> Option<T>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(
        event
            .observed
            .get("runtime_invariant_facts")?
            .get(key)?
            .clone(),
    )
    .ok()
}
