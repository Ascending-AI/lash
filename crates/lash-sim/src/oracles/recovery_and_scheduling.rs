use super::*;

pub fn durable_effect_exactly_once(summary: &AbstractWorldSummary) -> OracleVerdict {
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
        "durable effect replay reused the first semantic result for each durable key",
    )
}

pub fn worker_stale_completion_rejected(summary: &AbstractWorldSummary) -> OracleVerdict {
    if summary.workers.iter().any(|worker| {
        worker.lease_owner_changes > 0
            && worker.stale_completion_rejections > 0
            && !worker.active_incarnation_id.is_empty()
            && worker.active_fencing_token > 1
            && worker.process_stale_completion_rejected
            && worker.process_stale_output_absent
            && worker.process_terminal_writer == "successor"
            && worker.process_terminal_event_count == 1
    }) {
        return OracleVerdict::passed(
            WORKER_STALE_COMPLETION_ORACLE,
            "worker topology rejected stale process terminal output and persisted the successor terminal exactly once after an incarnation change",
        );
    }
    OracleVerdict::failed(
        WORKER_STALE_COMPLETION_ORACLE,
        "no worker boundary proved stale process-terminal rejection, stale-output absence, and exactly one successor terminal after lease owner change",
    )
}

pub const WORKER_FAILOVER_CONTINUATION_ORACLE: &str =
    "sim.oracle.worker-failover-continues-work.v1";
pub const HEALTHY_LONG_TURN_LIVENESS_ORACLE: &str = "sim.oracle.healthy-long-turn-liveness.v1";

/// Real worker FAILOVER CONTINUATION: a second worker incarnation reclaimed the
/// crashed first owner's session-execution lease at a strictly higher fencing
/// token and COMMITTED (continued) the queued work the dead owner could not,
/// while the dead owner's stale work completion was rejected. These facts are
/// produced by the real lease/queued-work store in
/// `runtime_boundaries::run_worker_stale_completion` (start_worker_owned_work +
/// resume_crashed_worker_work), so this oracle can only pass when a real
/// successor actually continued real work — not merely when stale-completion
/// evidence was observed.
pub fn worker_failover_continues_work(events: &[DeliveredBoundary]) -> OracleVerdict {
    if events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Worker)
        .any(worker_owned_work_continued_by_successor)
    {
        return OracleVerdict::passed(
            WORKER_FAILOVER_CONTINUATION_ORACLE,
            "a second worker incarnation reclaimed the dead owner's lease at a strictly higher fence and committed (continued) the queued work the crashed first owner could not, rejecting its stale completion",
        );
    }
    OracleVerdict::failed(
        WORKER_FAILOVER_CONTINUATION_ORACLE,
        "no worker boundary proved second-owner failover CONTINUATION of the first owner's in-flight work (claimed -> reclaimed at higher fence -> continued -> stale rejected)",
    )
}

/// A generated provider turn remained live and committed while its delivered
/// schedule crossed several production-sized lease TTL windows.
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
                "provider turn `{}` committed after its delivered schedule crossed at least three production lease TTL windows",
                event.boundary_id
            ),
        );
    }
    OracleVerdict::failed(
        HEALTHY_LONG_TURN_LIVENESS_ORACLE,
        "no successful generated provider turn crossed three production lease TTL windows in the delivered schedule",
    )
}

/// The per-process recovery facts recorded by every ProcessLifecycle boundary's
/// real `DurableProcessWorker` sweep (`runtime_boundaries::run_process_lifecycle`).
pub(super) fn lifecycle_processes(events: &[DeliveredBoundary]) -> Vec<&Value> {
    events
        .iter()
        .filter(|event| event.kind == BoundaryKind::ProcessLifecycle)
        .filter_map(|event| {
            event
                .observed
                .pointer("/runtime_process_lifecycle/processes")
                .and_then(Value::as_array)
        })
        .flatten()
        .collect()
}

pub(super) fn process_field_str<'a>(process: &'a Value, key: &str) -> Option<&'a str> {
    process.get(key).and_then(Value::as_str)
}

pub(super) fn process_field_bool(process: &Value, key: &str) -> Option<bool> {
    process.get(key).and_then(Value::as_bool)
}

pub(super) fn is_started_owner_bound(process: &Value) -> bool {
    process_field_str(process, "disposition") == Some("owner_bound")
        && process_field_bool(process, "started") == Some(true)
}

/// An Abandoned terminal is licensed by a live owner's native drain or an
/// operator-authorized request reconciled after the lease lapsed. Elapsed time
/// or a missing/unknown writer is never licensing evidence.
pub(super) fn abandoned_evidence_is_licensed(process: &Value) -> bool {
    match process_field_str(process, "abandon_writer") {
        Some("owner_drain") => process_field_str(process, "abandon_evidence_owner").is_some(),
        Some("reconciled_request") => {
            process_field_bool(process, "abandon_requested") == Some(true)
                && process_field_bool(process, "lease_lapsed") == Some(true)
        }
        _ => false,
    }
}

/// ADR 0019: a started OwnerBound process is NEVER re-executed by recovery —
/// it remains non-terminal until explicitly abandoned and never reaches a run
/// terminal (detected through the recovery execution journal: a re-run would
/// land a `completed`/`failed` terminal instead). The contrast row
/// proves the sweep is capable of re-execution: a Rerunnable sibling IS re-run.
/// Without both, an all-abandoned outcome could pass vacuously.
pub fn process_never_double_started(events: &[DeliveredBoundary]) -> OracleVerdict {
    let processes = lifecycle_processes(events);
    let started_owner_bound: Vec<&Value> = processes
        .iter()
        .copied()
        .filter(|process| is_started_owner_bound(process))
        .collect();
    let owner_bound_never_reran = started_owner_bound
        .iter()
        .all(|process| process_field_bool(process, "reran") == Some(false));
    let rerunnable_reran = processes.iter().any(|process| {
        process_field_str(process, "disposition") == Some("rerunnable")
            && process_field_bool(process, "reran") == Some(true)
    });
    coverage_invariant_verdict(
        PROCESS_NEVER_DOUBLE_STARTED_ORACLE,
        !started_owner_bound.is_empty(),
        "no started OwnerBound process was recovered",
        owner_bound_never_reran && rerunnable_reran,
        "a started OwnerBound process reached a run terminal (double-started), or no Rerunnable sibling was re-run to prove the sweep can re-execute",
        "every started OwnerBound process stayed non-rerun while a Rerunnable sibling was re-run — the sweep can re-execute but never re-runs started OwnerBound work",
    )
}

/// ADR 0019: every Abandoned terminal carries the evidence that licensed it — a
/// live owner drain or a reconciled request with a lapsed lease. Elapsed time
/// alone never terminalizes.
pub fn abandoned_requires_evidence(events: &[DeliveredBoundary]) -> OracleVerdict {
    let processes = lifecycle_processes(events);
    let abandoned: Vec<&Value> = processes
        .iter()
        .copied()
        .filter(|process| process_field_str(process, "terminal_status") == Some("abandoned"))
        .collect();
    coverage_invariant_verdict(
        ABANDONED_REQUIRES_EVIDENCE_ORACLE,
        !abandoned.is_empty(),
        "no Abandoned terminal was observed",
        abandoned
            .iter()
            .all(|process| abandoned_evidence_is_licensed(process)),
        "an Abandoned terminal lacked licensing evidence (owner drain or reconciled request with a lapsed lease) — elapsed time alone never terminalizes",
        "every Abandoned terminal carried its licensing evidence (owner drain or reconciled request with a lapsed lease)",
    )
}

/// Composite OwnerBound recovery invariant for the state-machine oracle: a
/// started row is never re-run; without licensed abandonment it stays running.
/// Absent a recovery scenario this holds vacuously.
pub(super) fn started_owner_bound_recovery_is_safe(events: &[DeliveredBoundary]) -> bool {
    lifecycle_processes(events).iter().all(|process| {
        let running = process_field_str(process, "terminal_status") == Some("running");
        let abandoned = process_field_str(process, "terminal_status") == Some("abandoned");
        let owner_bound_ok = !is_started_owner_bound(process)
            || (process_field_bool(process, "reran") == Some(false) && (running || abandoned));
        let abandoned_ok = !abandoned || abandoned_evidence_is_licensed(process);
        owner_bound_ok && abandoned_ok
    })
}

pub(super) fn worker_owned_work_continued_by_successor(event: &DeliveredBoundary) -> bool {
    let Some(work) = event
        .observed
        .get("runtime_worker_store")
        .and_then(|store| store.get("worker_owned_work"))
    else {
        return false;
    };
    let flag = |key: &str| work.get(key).and_then(Value::as_bool).unwrap_or(false);
    let process = event
        .observed
        .pointer("/runtime_worker_store/process_completion");
    let process_is_fenced = process.is_some_and(|process| {
        process
            .get("stale_completion_rejected")
            .and_then(Value::as_bool)
            == Some(true)
            && process.get("stale_output_absent").and_then(Value::as_bool) == Some(true)
            && process.get("terminal_writer").and_then(Value::as_str) == Some("successor")
            && process.get("terminal_event_count").and_then(Value::as_u64) == Some(1)
    });
    process_is_fenced
        && event
            .observed
            .get("expired_owner_commit_rejected")
            .and_then(Value::as_bool)
            == Some(true)
        && event
            .observed
            .pointer("/runtime_worker_store/takeover_after_ttl_expiry")
            .and_then(Value::as_bool)
            == Some(true)
        && flag("first_owner_claimed_work")
        && flag("second_owner_resumed_work")
        && flag("second_owner_outranks_first")
        && flag("stale_work_completion_rejected")
}

/// Assert that the real session-execution-lease fencing tokens recorded by the
/// lease-time boundaries strictly increase per session. Unlike the old
/// generator-fed tick (which was monotonic by construction and could never
/// fail), this reads the ground-truth fencing token the in-memory/SQLite lease
/// store handed back, so a broken fencing implementation that reissued or
/// regressed a token would fail this oracle.
pub fn lease_time_monotonic(
    events: &[DeliveredBoundary],
    expectations: &WorkloadExpectations,
) -> OracleVerdict {
    lease_time_monotonic_law(events, Some(expectations))
}

/// The fencing-token monotonicity law without the declared-coverage floor, for
/// scenario evidence predicates that only judge the boundaries they observed.
pub(super) fn lease_time_monotonic_law(
    events: &[DeliveredBoundary],
    expectations: Option<&WorkloadExpectations>,
) -> OracleVerdict {
    let mut last_by_session: BTreeMap<&str, u64> = BTreeMap::new();
    let mut grounded = 0usize;
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::LeaseTime)
    {
        let Some(fencing_token) = event
            .observed
            .pointer("/runtime_lease_probe/session_execution_lease_fencing_token")
            .and_then(Value::as_u64)
        else {
            return OracleVerdict::failed(
                LEASE_TIME_MONOTONIC_ORACLE,
                format!(
                    "lease-time boundary `{}` recorded no real lease fencing token",
                    event.boundary_id
                ),
            );
        };
        grounded += 1;
        if let Some(previous) = last_by_session.insert(event.actor_alias.as_str(), fencing_token)
            && fencing_token <= previous
        {
            return OracleVerdict::failed(
                LEASE_TIME_MONOTONIC_ORACLE,
                format!(
                    "session `{}` lease fencing token did not advance: {previous} -> {fencing_token}",
                    event.actor_alias
                ),
            );
        }
    }
    let declared = expectations.map_or(0, |declared| declared.lease_time_boundary_count);
    if let Some(shortfall) = declared_coverage_shortfall(
        LEASE_TIME_MONOTONIC_ORACLE,
        "lease-time boundary(ies)",
        declared,
        grounded,
    ) {
        return shortfall;
    }
    OracleVerdict::passed(
        LEASE_TIME_MONOTONIC_ORACLE,
        format!(
            "{grounded} lease-time boundaries (workload declared {declared}) advanced a real session-execution-lease fencing token monotonically per session"
        ),
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
    BoundaryKind::Worker,
    BoundaryKind::ProcessWake,
    BoundaryKind::Observer,
];

pub fn scheduler_owned_runtime_completions(events: &[DeliveredBoundary]) -> OracleVerdict {
    let mut missing = Vec::new();
    for &kind in SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE_KINDS {
        let mut saw_kind = false;
        for event in events
            .iter()
            .filter(|event| event.kind == kind && !is_suspend_resume(event))
        {
            saw_kind = true;
            let Some(completion) = event.payload.get("runtime_completion") else {
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
            let family = completion
                .get("completion_family")
                .and_then(Value::as_str)
                .unwrap_or("");
            let units = completion
                .get("completion_units")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let ready_at = completion
                .get("ready_at")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let registered_after = completion
                .get("registered_after")
                .and_then(Value::as_str)
                .unwrap_or("");
            if family.is_empty()
                || units == 0
                || ready_at != event.at
                || registered_after.is_empty()
            {
                return OracleVerdict::failed(
                    SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE,
                    format!(
                        "runtime completion `{}` for {kind:?} had incomplete pending evidence: family=`{family}` units={units} ready_at={ready_at} registered_after=`{registered_after}`",
                        event.boundary_id
                    ),
                );
            }
        }
        if !saw_kind {
            missing.push(format!("{kind:?}"));
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
        "provider chunks/retries, cancellation, tool returns, exec results, durable completions, provider mutations, worker completions, process wakes, and observer reconnects were registered as pending runtime boundaries and delivered by the scheduler",
    )
}

pub fn operational_coverage(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> OracleVerdict {
    let mut missing = Vec::new();
    if !summary
        .sessions
        .iter()
        .any(|session| session.queued_ingress_count > 0)
    {
        missing.push("queueing inputs");
    }
    if !summary
        .sessions
        .iter()
        .any(|session| session.trigger_count > 0)
    {
        missing.push("triggers");
    }
    if !summary
        .sessions
        .iter()
        .any(|session| session.cancellation_count > 0)
    {
        missing.push("cancellation");
    }
    if !summary
        .sessions
        .iter()
        .any(|session| session.observer_reconnects > 0)
    {
        missing.push("observer reconnects");
    }
    let provider_mutations = events
        .iter()
        .filter(|event| event.kind == BoundaryKind::ProviderMutation)
        .filter_map(|event| {
            event
                .observed
                .get("mutation")
                .or_else(|| event.payload.get("mutation"))
                .and_then(serde_json::Value::as_str)
        })
        .collect::<BTreeSet<_>>();
    if !provider_mutations.contains("malformed_sse_chunk")
        || !provider_mutations.contains("rate_limit_error_envelope")
    {
        missing.push("provider failures/mutations");
    }
    if !summary
        .sessions
        .iter()
        .any(|session| session.process_wake_count > 0)
    {
        missing.push("process wakes");
    }
    if !summary
        .sessions
        .iter()
        .any(|session| !session.tool_outputs.is_empty())
    {
        missing.push("tool results");
    }
    if !summary
        .sessions
        .iter()
        .any(|session| !session.exec_code_outputs.is_empty())
    {
        missing.push("exec-code results");
    }
    if !summary
        .durable_effects
        .iter()
        .any(|effect| effect.execution_count == 1 && effect.replay_count > 0)
    {
        missing.push("durable effects");
    }
    if !summary
        .workers
        .iter()
        .any(|worker| worker.lease_owner_changes > 0 && worker.stale_completion_rejections > 0)
    {
        missing.push("worker lease/failover");
    }
    let backend_retryable = events.iter().any(|event| {
        event.kind == BoundaryKind::BackendFailure
            && event
                .observed
                .get("retryable")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
    });
    let backend_terminal = events.iter().any(|event| {
        event.kind == BoundaryKind::BackendFailure
            && !event
                .observed
                .get("retryable")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true)
    });
    if !backend_retryable || !backend_terminal {
        missing.push("backend choices");
    }
    let backend_retry_attempt = events.iter().any(|event| {
        event.kind == BoundaryKind::BackendFailure
            && event
                .observed
                .get("attempt")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
                > 1
    });
    if !backend_retry_attempt || !structural_process_wake_identity_semantics(events) {
        missing.push("retries/duplicates");
    }

    if missing.is_empty() {
        OracleVerdict::passed(
            OPERATIONAL_COVERAGE_ORACLE,
            "generated DST trace hit queueing, triggers, cancellation, observer reconnects, provider failure/mutation, process wake, tool/exec, durable effect, worker failover, backend choice, retry, and duplicate cases",
        )
    } else {
        OracleVerdict::failed(
            OPERATIONAL_COVERAGE_ORACLE,
            format!(
                "generated DST trace missed operational cases: {}",
                missing.join(", ")
            ),
        )
    }
}

pub fn state_machine_semantic_invariants(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> OracleVerdict {
    let mut missing = Vec::new();
    if !queued_active_turn_input_hidden_semantics(events) {
        missing.push("queued active-turn input hidden from live provider turns");
    }
    if !cancellation_terminalizes_pending_input(events) {
        missing.push("cancellation terminalizes a pending queued input");
    }
    if !trigger_wakeup_route_semantics(events) {
        missing.push("trigger wakeup routes through TriggerStore reservation");
    }
    if !backend_retry_terminalization_semantics(events) {
        missing.push("backend retry terminalization");
    }
    if !duplicate_delivery_semantics(events, summary) {
        missing.push("duplicate delivery/replay semantics");
    }
    if !protocol_terminal_state_semantics(events, summary) {
        missing.push("provider/protocol terminal state semantics");
    }
    if !started_owner_bound_recovery_is_safe(events) {
        missing.push(
            "started OwnerBound recovery never re-runs and only abandons with licensed evidence",
        );
    }

    if missing.is_empty() {
        OracleVerdict::passed(
            STATE_MACHINE_SEMANTIC_INVARIANTS_ORACLE,
            "queued input, cancellation, trigger wakeup, retry terminalization, duplicate delivery/replay, protocol terminal-state, and disposition-driven recovery-abandonment invariants held",
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
