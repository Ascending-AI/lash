use super::*;

pub fn durable_effect_exactly_once(summary: &AbstractWorldView) -> OracleVerdict {
    if summary.durable_effects.is_empty() {
        return OracleVerdict::failed(
            DURABLE_EFFECT_EXACTLY_ONCE_ORACLE,
            "workload did not execute a durable effect boundary",
        );
    }
    for effect in &summary.durable_effects {
        if effect.execution_count != 1 {
            return OracleVerdict::failed(
                DURABLE_EFFECT_EXACTLY_ONCE_ORACLE,
                format!(
                    "durable key `{}` executed {} times",
                    effect.durable_key, effect.execution_count
                ),
            );
        }
        if effect.replay_count == 0 {
            return OracleVerdict::failed(
                DURABLE_EFFECT_EXACTLY_ONCE_ORACLE,
                format!("durable key `{}` was never replayed", effect.durable_key),
            );
        }
    }
    OracleVerdict::passed(
        DURABLE_EFFECT_EXACTLY_ONCE_ORACLE,
        "each durable effect ran once, and its redrive was served the recorded result",
    )
}

pub const HEALTHY_LONG_TURN_LIVENESS_ORACLE: crate::trace::OracleId<'static> =
    crate::trace::OracleId::real("sim.oracle.healthy-long-turn-liveness.v1");

/// A generated provider turn remained live and committed while its delivered
/// schedule crossed several production-sized failover windows.
pub fn healthy_long_turn_liveness(events: &[DeliveredBoundary]) -> OracleVerdict {
    let healthy = events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Provider)
        .find(|event| {
            event.observed.get("success").and_then(Value::as_bool) == Some(true)
                && event
                    .observed
                    .pointer("/sim_clock/scheduled_elapsed_ms")
                    .and_then(Value::as_u64)
                    .is_some_and(|elapsed| elapsed >= 3 * 30_000)
        });
    if let Some(event) = healthy {
        return OracleVerdict::passed(
            HEALTHY_LONG_TURN_LIVENESS_ORACLE,
            format!(
                "provider turn `{}` committed after its delivered schedule crossed at least three production failover windows",
                event.boundary_id
            ),
        );
    }
    OracleVerdict::failed(
        HEALTHY_LONG_TURN_LIVENESS_ORACLE,
        "no successful generated provider turn crossed three production failover windows in the delivered schedule",
    )
}

pub fn scheduler_controlled_delivery(events: &[DeliveredBoundary]) -> OracleVerdict {
    if events.is_empty() {
        return OracleVerdict::failed(
            SCHEDULER_CONTROLLED_DELIVERY_ORACLE,
            "generated workload delivered no scheduler boundaries",
        );
    }
    for (sequence, event) in events.iter().enumerate() {
        if event.sequence != sequence {
            return OracleVerdict::failed(
                SCHEDULER_CONTROLLED_DELIVERY_ORACLE,
                format!(
                    "boundary `{}` had sequence {}, expected {}",
                    event.boundary_id, event.sequence, sequence
                ),
            );
        }
        if !event.scheduler.scheduler_controlled {
            return OracleVerdict::failed(
                SCHEDULER_CONTROLLED_DELIVERY_ORACLE,
                format!(
                    "boundary `{}` was delivered outside scheduler control",
                    event.boundary_id
                ),
            );
        }
        if event.scheduler.delivered_at != event.at {
            return OracleVerdict::failed(
                SCHEDULER_CONTROLLED_DELIVERY_ORACLE,
                format!(
                    "boundary `{}` delivery tick {} diverged from scheduled tick {}",
                    event.boundary_id, event.scheduler.delivered_at, event.at
                ),
            );
        }
        if event.scheduler.min_scheduled_at > event.scheduler.delivered_at {
            return OracleVerdict::failed(
                SCHEDULER_CONTROLLED_DELIVERY_ORACLE,
                format!(
                    "boundary `{}` was delivered before the scheduler's minimum tick",
                    event.boundary_id
                ),
            );
        }
        if event.scheduler.candidate_count_at_tick == 0 {
            return OracleVerdict::failed(
                SCHEDULER_CONTROLLED_DELIVERY_ORACLE,
                format!(
                    "boundary `{}` recorded zero scheduler candidates",
                    event.boundary_id
                ),
            );
        }
        if event.scheduler.selected_candidate_index >= event.scheduler.candidate_count_at_tick {
            return OracleVerdict::failed(
                SCHEDULER_CONTROLLED_DELIVERY_ORACLE,
                format!(
                    "boundary `{}` selected candidate {} out of {}",
                    event.boundary_id,
                    event.scheduler.selected_candidate_index,
                    event.scheduler.candidate_count_at_tick
                ),
            );
        }
    }
    OracleVerdict::passed(
        SCHEDULER_CONTROLLED_DELIVERY_ORACLE,
        "generated trace records scheduler-owned delivery order, timing, and tie-break evidence",
    )
}

pub const SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE_KINDS: &[BoundaryKind] = &[
    BoundaryKind::Provider,
    BoundaryKind::Cancellation,
    BoundaryKind::BackendFailure,
    BoundaryKind::ProviderMutation,
    BoundaryKind::Tool,
    BoundaryKind::ExecCode,
    BoundaryKind::DurableEffect,
    BoundaryKind::Observer,
];

pub fn scheduler_owned_runtime_completions(
    events: &[DeliveredBoundary],
    expectations: &WorkloadExpectations,
) -> OracleVerdict {
    let mut missing = Vec::new();
    for &kind in SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE_KINDS {
        let mut observed = 0usize;
        for event in events
            .iter()
            .filter(|event| event.kind == kind && !is_suspend_resume(event))
        {
            observed += 1;
            let Some(completion) = event.payload.get(PendingRuntimeBoundary::PAYLOAD_KEY) else {
                return OracleVerdict::failed(
                    SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE,
                    format!(
                        "runtime completion `{}` for {kind:?} was delivered without pending runtime boundary evidence",
                        event.boundary_id
                    ),
                );
            };
            if !event.scheduler.scheduler_controlled {
                return OracleVerdict::failed(
                    SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE,
                    format!(
                        "runtime completion `{}` for {kind:?} bypassed scheduler control",
                        event.boundary_id
                    ),
                );
            }
            let detail = match serde_json::from_value::<PendingRuntimeBoundary>(completion.clone())
            {
                Ok(pending)
                    if !pending.completion_units.is_empty()
                        && pending.ready_at == event.at
                        && !pending.registered_after.is_empty() =>
                {
                    continue;
                }
                Ok(pending) => format!(
                    "family=`{:?}` units={} ready_at={} registered_after=`{}`",
                    pending.completion_family,
                    pending.completion_units.len(),
                    pending.ready_at,
                    pending.registered_after
                ),
                Err(error) => {
                    format!("evidence did not deserialize as PendingRuntimeBoundary: {error}")
                }
            };
            return OracleVerdict::failed(
                SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE,
                format!(
                    "runtime completion `{}` for {kind:?} had incomplete pending evidence: {detail}",
                    event.boundary_id
                ),
            );
        }
        match expectations.declared_completion_count(kind) {
            Some(declared) if observed < declared => {
                return OracleVerdict::failed(
                    SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE,
                    format!(
                        "generated trace delivered {observed} scheduler-owned {kind:?} runtime completions; the workload declared {declared}"
                    ),
                );
            }
            Some(_) => {}
            None if observed == 0 => missing.push(format!("{kind:?}")),
            None => {}
        }
    }
    if !missing.is_empty() {
        return OracleVerdict::failed(
            SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE,
            format!(
                "generated trace did not include scheduler-owned runtime completion kinds: {}",
                missing.join(", ")
            ),
        );
    }
    OracleVerdict::passed(
        SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE,
        "provider chunks/retries, cancellation, tool returns, exec results, durable completions, provider mutations, and observer reconnects were registered as pending runtime boundaries and delivered by the scheduler",
    )
}

pub fn state_machine_semantic_invariants(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldView,
    expectations: &WorkloadExpectations,
) -> OracleVerdict {
    let required = |kind| expectations.declared_completion_count(kind) != Some(0);
    let mut missing = Vec::new();
    if required(BoundaryKind::QueuedIngress) && !queued_active_turn_input_hidden_semantics(events) {
        missing.push("queued active-turn input hidden from live provider turns");
    }
    if required(BoundaryKind::Cancellation) && !cancellation_terminalizes_pending_input(events) {
        missing.push("cancellation terminalizes a pending queued input");
    }

    if required(BoundaryKind::BackendFailure) && !backend_fault_classification_semantics(events) {
        missing.push("production backend fault classification");
    }
    if required(BoundaryKind::DurableEffect) && !duplicate_delivery_semantics(events, summary) {
        missing.push("duplicate delivery/replay semantics");
    }
    if !duplicate_free_stream_finalization(events, summary)
        || (required(BoundaryKind::ProviderMutation)
            && !protocol_terminal_state_semantics(events, summary))
    {
        missing.push("provider/protocol terminal state semantics");
    }

    if missing.is_empty() {
        OracleVerdict::passed(
            STATE_MACHINE_SEMANTIC_INVARIANTS_ORACLE,
            "queued input, cancellation, retry terminalization, duplicate delivery/replay, and protocol terminal-state invariants held",
        )
    } else {
        OracleVerdict::failed(
            STATE_MACHINE_SEMANTIC_INVARIANTS_ORACLE,
            format!(
                "generated DST trace violated semantic state-machine invariants: {}",
                missing.join(", ")
            ),
        )
    }
}
