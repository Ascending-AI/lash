use std::collections::{BTreeMap, BTreeSet};

use lash::scenario_contracts::AGENT_SCENARIO_CONTRACTS;
use lash_core::runtime::{RUNTIME_SCENARIO_CONTRACTS, ScenarioContractSpec};
use lash_protocol_rlm::scenario_contracts::RLM_PROTOCOL_SCENARIO_CONTRACTS;
use lash_protocol_standard::scenario_contracts::STANDARD_PROTOCOL_SCENARIO_CONTRACTS;
use serde_json::{Value, json};

use crate::provider_mutations::is_transport_provider_mutation;
#[cfg(test)]
use crate::runtime_contracts::RuntimeGraphInvariantFacts;
use crate::runtime_contracts::{RuntimeAgentFrameInvariantFacts, RuntimeUsageInvariantFacts};
use crate::runtime_providers::MIGRATED_RUNTIME_PROVIDER_KINDS;
use crate::scheduler::{BoundaryKind, DeliveredBoundary};
use crate::trace::{AbstractWorldSummary, OracleVerdict};

pub const CROSS_SESSION_ISOLATION_ORACLE: &str = "sim.oracle.cross-session-isolation.v1";
pub const BACKEND_FAILURE_ORACLE: &str = "sim.oracle.backend-failure-observed.v1";
pub const CANCELLATION_ORACLE: &str = "sim.oracle.cancellation-observed.v1";
pub const DURABLE_EFFECT_EXACTLY_ONCE_ORACLE: &str = "sim.oracle.durable-effect-exactly-once.v1";
pub const EXEC_CODE_ORACLE: &str = "sim.oracle.exec-code-observed.v1";
pub const INGRESS_SESSION_OPENED_ORACLE: &str = "sim.oracle.ingress-session-opened.v1";
pub const LEASE_TIME_MONOTONIC_ORACLE: &str = "sim.oracle.lease-time-monotonic.v1";
pub const OBSERVER_CONVERGENCE_ORACLE: &str = "sim.oracle.observer-convergence.v1";
pub const OBSERVER_RECONNECT_ORACLE: &str = "sim.oracle.observer-reconnect.v1";
pub const OPERATIONAL_COVERAGE_ORACLE: &str = "sim.oracle.operational-coverage.v1";
pub const PROCESS_WAKE_ORACLE: &str = "sim.oracle.process-wake-observed.v1";
pub const PROCESS_WAKE_AT_MOST_ONCE_ORACLE: &str =
    "sim.oracle.process-wake-at-most-once-runtime-turn.v1";
pub const PROCESS_NEVER_DOUBLE_STARTED_ORACLE: &str = "sim.oracle.process-never-double-started.v1";
pub const ABANDONED_REQUIRES_EVIDENCE_ORACLE: &str = "sim.oracle.abandoned-requires-evidence.v1";
pub const PROVIDER_MUTATION_ORACLE: &str = "sim.oracle.provider-mutation-rejected.v1";
pub const QUEUED_INGRESS_ORACLE: &str = "sim.oracle.queued-ingress-observed.v1";
pub const REPLAY_DETERMINISM_ORACLE: &str = "sim.oracle.replay-determinism.v1";
pub const RUNTIME_PROVIDER_TURN_ORACLE: &str = "sim.oracle.runtime-provider-turn.v1";
pub const PENDING_TOOL_COMPLETION_ORACLE: &str =
    "sim.oracle.pending-tool-completion-through-turn.v1";
pub const RUNTIME_GRAPH_ACYCLIC_ORACLE: &str = "sim.oracle.runtime-graph-acyclic.v1";
pub const RUNTIME_SINGLE_ACTIVE_AGENT_FRAME_ORACLE: &str =
    "sim.oracle.runtime-single-active-agent-frame.v1";
pub const RUNTIME_USAGE_MONOTONIC_ORACLE: &str = "sim.oracle.runtime-usage-monotonic.v1";
pub const RUNTIME_FINAL_VALUE_SEMANTIC_ORACLE: &str =
    "sim.oracle.runtime-final-value-semantic-channel.v1";
pub const GENERATED_PROVIDER_MATRIX_ORACLE: &str =
    "sim.oracle.generated-runtime-provider-matrix.v1";
pub const PROVIDER_TURN_INTERLEAVING_ORACLE: &str =
    "sim.oracle.provider-turn-interleaving-depth.v1";
pub const PROVIDER_TRANSPORT_MUTATION_ORACLE: &str =
    "sim.oracle.provider-transport-mutation-classified.v1";
pub const RUNTIME_SESSION_GRAPH_ORACLE: &str = "sim.oracle.runtime-session-graph.v1";
pub const SCHEDULER_CONTROLLED_DELIVERY_ORACLE: &str =
    "sim.oracle.scheduler-controlled-delivery.v1";
pub const SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE: &str =
    "sim.oracle.scheduler-owned-runtime-completions.v1";
pub const STATE_MACHINE_SEMANTIC_INVARIANTS_ORACLE: &str =
    "sim.oracle.state-machine-semantic-invariants.v1";
pub const SCENARIO_AGENT_CONTRACT_ORACLE: &str = "sim.oracle.scenario.agent-contract.v1";
pub const SCENARIO_RLM_CONTRACT_ORACLE: &str = "sim.oracle.scenario.rlm-contract.v1";
pub const SCENARIO_RUNTIME_CONTRACT_ORACLE: &str = "sim.oracle.scenario.runtime-contract.v1";
pub const SCENARIO_STANDARD_CONTRACT_ORACLE: &str = "sim.oracle.scenario.standard-contract.v1";
pub const SCENARIO_MINI_RUNTIME_QUEUED_HIDDEN_ORACLE: &str =
    "sim.oracle.scenario-mini.runtime.queued-input-hidden-while-live.v1";
pub const SCENARIO_MINI_RUNTIME_CANCEL_IDLE_ORACLE: &str =
    "sim.oracle.scenario-mini.runtime.cancellation-prevents-idle-claim.v1";
pub const SCENARIO_MINI_RUNTIME_PROCESS_WAKE_DEDUPE_ORACLE: &str =
    "sim.oracle.scenario-mini.runtime.process-wake-duplicate-rejected.v1";
pub const SCENARIO_MINI_RUNTIME_STALE_LEASE_ORACLE: &str =
    "sim.oracle.scenario-mini.runtime.stale-lease-commit-rejected.v1";
pub const SCENARIO_MINI_STANDARD_STREAM_FINALIZE_ORACLE: &str =
    "sim.oracle.scenario-mini.standard.streamed-text-finalizes-once.v1";
pub const SCENARIO_MINI_STANDARD_PROVIDER_ERROR_ORACLE: &str =
    "sim.oracle.scenario-mini.standard.provider-error-without-checkpoint.v1";
pub const SCENARIO_MINI_STANDARD_TOOL_REENTRY_ORACLE: &str =
    "sim.oracle.scenario-mini.standard.tool-loop-reenters-after-checkpoint.v1";
pub const SCENARIO_MINI_RLM_FINISH_REPAIR_ORACLE: &str =
    "sim.oracle.scenario-mini.rlm.finish-required-prose-repair.v1";
pub const SCENARIO_MINI_RLM_SCHEMA_REPAIR_ORACLE: &str =
    "sim.oracle.scenario-mini.rlm.schema-mismatch-repair.v1";
pub const SCENARIO_MINI_RLM_CELL_EXEC_ORACLE: &str =
    "sim.oracle.scenario-mini.rlm.lashlang-cell-exec-continues.v1";
pub const SCENARIO_MINI_AGENT_DURABLE_INPUT_ORACLE: &str =
    "sim.oracle.scenario-mini.agent.durable-input-resolution.v1";
pub const SCENARIO_MINI_AGENT_CHILD_FAILURE_ORACLE: &str =
    "sim.oracle.scenario-mini.agent.child-failure-graph.v1";
pub const SCENARIO_MINI_AGENT_PARALLEL_JOIN_ORACLE: &str =
    "sim.oracle.scenario-mini.agent.parallel-spawn-join-determinism.v1";
pub const TOOL_BOUNDARY_ORACLE: &str = "sim.oracle.tool-boundary-observed.v1";
pub const TRIGGER_ORACLE: &str = "sim.oracle.trigger-delivery-observed.v1";
pub const WORKER_STALE_COMPLETION_ORACLE: &str = "sim.oracle.worker-stale-completion-rejected.v1";
pub const GENERATED_SUSPEND_RESUME_ORACLE: &str = "sim.oracle.generated-suspend-resume.v1";
pub const GENERATED_FINAL_VALUE_ORACLE: &str =
    "sim.oracle.generated-final-value-semantic-channel.v1";
pub const FRAME_SWITCH_SEED_ORACLE: &str = "sim.oracle.frame-switch-seed.v1";
pub const LOGICAL_TURN_CLAIM_EXACTLY_ONCE_ORACLE: &str =
    "sim.oracle.logical-turn-claim-exactly-once.v1";
pub const FRAME_SWITCH_OUTBOX_ATOMICITY_ORACLE: &str =
    "sim.oracle.frame-switch-outbox-atomicity.v1";
pub const FRAME_SWITCH_ORDERING_ORACLE: &str = "sim.oracle.frame-switch-ordering.v1";

#[derive(Clone, Debug, PartialEq)]
pub struct FrameSwitchSeedObservation {
    pub protocol: String,
    pub expected_nodes: Vec<Value>,
    pub observed_nodes: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameSwitchCommitObservation {
    pub turn_id: String,
    pub inbound_claim_completed: bool,
    pub follow_on_enqueued: bool,
}

/// Require every frame-switch seed to materialize in full. Comparing canonical
/// node values catches both the historical empty-frame regression and partial
/// application/reordering of producer-validated nodes.
pub fn frame_switch_seeds(observations: &[FrameSwitchSeedObservation]) -> OracleVerdict {
    for observation in observations {
        if observation.expected_nodes.is_empty() {
            return OracleVerdict::failed(
                FRAME_SWITCH_SEED_ORACLE,
                format!(
                    "{} frame-switch observation carried no seed nodes",
                    observation.protocol
                ),
            );
        }
        if observation.observed_nodes != observation.expected_nodes {
            return OracleVerdict::failed(
                FRAME_SWITCH_SEED_ORACLE,
                format!(
                    "{} frame seed changed: expected {:?}, observed {:?}",
                    observation.protocol, observation.expected_nodes, observation.observed_nodes
                ),
            );
        }
    }
    if observations.is_empty() {
        return OracleVerdict::failed(
            FRAME_SWITCH_SEED_ORACLE,
            "no frame-switch seed observations were supplied",
        );
    }
    OracleVerdict::passed(
        FRAME_SWITCH_SEED_ORACLE,
        format!(
            "{} frame-switch seeds materialized completely",
            observations.len()
        ),
    )
}

/// Match runtime claim/completion trace records by `(claim kind, claim id)` and
/// require a single terminal settlement for every claimed ingress.
pub fn logical_turn_claims_settle_exactly_once(
    records: &[lash_core::facade_support::TraceRecord],
) -> OracleVerdict {
    let mut claimed = BTreeMap::<(String, String), usize>::new();
    let mut completed = BTreeMap::<(String, String), usize>::new();
    for record in records {
        let lash_core::TraceEvent::Custom { name, payload } = &record.event else {
            continue;
        };
        match name.as_str() {
            "queued_work.claimed" | "turn_input.claimed" => {
                let Some(claim_id) = payload.get("claim_id").and_then(Value::as_str) else {
                    return OracleVerdict::failed(
                        LOGICAL_TURN_CLAIM_EXACTLY_ONCE_ORACLE,
                        format!("{name} trace omitted claim_id"),
                    );
                };
                *claimed
                    .entry((
                        name.trim_end_matches(".claimed").to_string(),
                        claim_id.to_string(),
                    ))
                    .or_default() += 1;
            }
            "queued_work.completed" | "turn_input.completed" => {
                let Some(claims) = payload.get("claims").and_then(Value::as_array) else {
                    return OracleVerdict::failed(
                        LOGICAL_TURN_CLAIM_EXACTLY_ONCE_ORACLE,
                        format!("{name} trace omitted claims"),
                    );
                };
                for claim in claims {
                    let Some(claim_id) = claim.get("claim_id").and_then(Value::as_str) else {
                        return OracleVerdict::failed(
                            LOGICAL_TURN_CLAIM_EXACTLY_ONCE_ORACLE,
                            format!("{name} trace contained a claim without claim_id"),
                        );
                    };
                    *completed
                        .entry((
                            name.trim_end_matches(".completed").to_string(),
                            claim_id.to_string(),
                        ))
                        .or_default() += 1;
                }
            }
            _ => {}
        }
    }
    if claimed.is_empty() {
        return OracleVerdict::failed(
            LOGICAL_TURN_CLAIM_EXACTLY_ONCE_ORACLE,
            "no claimed ingress was observed",
        );
    }
    for (claim, claim_count) in &claimed {
        let completion_count = completed.get(claim).copied().unwrap_or_default();
        if *claim_count != 1 || completion_count != 1 {
            return OracleVerdict::failed(
                LOGICAL_TURN_CLAIM_EXACTLY_ONCE_ORACLE,
                format!(
                    "{} claim `{}` was claimed {claim_count} times and settled {completion_count} times",
                    claim.0, claim.1
                ),
            );
        }
    }
    if let Some((claim, count)) = completed
        .iter()
        .find(|(claim, count)| !claimed.contains_key(*claim) || **count != 1)
    {
        return OracleVerdict::failed(
            LOGICAL_TURN_CLAIM_EXACTLY_ONCE_ORACLE,
            format!(
                "{} claim `{}` had {count} terminal settlements without one matching claim",
                claim.0, claim.1
            ),
        );
    }
    OracleVerdict::passed(
        LOGICAL_TURN_CLAIM_EXACTLY_ONCE_ORACLE,
        format!("{} ingress claims settled exactly once", claimed.len()),
    )
}

pub fn frame_switch_outbox_is_atomic(
    observations: &[FrameSwitchCommitObservation],
) -> OracleVerdict {
    if observations.is_empty() {
        return OracleVerdict::failed(
            FRAME_SWITCH_OUTBOX_ATOMICITY_ORACLE,
            "no claimed frame-switch commit was observed",
        );
    }
    if let Some(observation) = observations
        .iter()
        .find(|observation| !observation.inbound_claim_completed || !observation.follow_on_enqueued)
    {
        return OracleVerdict::failed(
            FRAME_SWITCH_OUTBOX_ATOMICITY_ORACLE,
            format!(
                "switch commit `{}` exposed inbound_completed={} follow_on_enqueued={}",
                observation.turn_id,
                observation.inbound_claim_completed,
                observation.follow_on_enqueued
            ),
        );
    }
    OracleVerdict::passed(
        FRAME_SWITCH_OUTBOX_ATOMICITY_ORACLE,
        format!(
            "{} switch commits exposed claim completion and follow-on enqueue together",
            observations.len()
        ),
    )
}

pub fn frame_switch_follow_on_precedes_pending(
    completion_order: &[String],
    follow_on: &str,
    pending_at_chain_start: &[String],
) -> OracleVerdict {
    let Some(follow_index) = completion_order.iter().position(|item| item == follow_on) else {
        return OracleVerdict::failed(
            FRAME_SWITCH_ORDERING_ORACLE,
            format!("follow-on `{follow_on}` never completed"),
        );
    };
    for pending in pending_at_chain_start {
        let Some(pending_index) = completion_order.iter().position(|item| item == pending) else {
            return OracleVerdict::failed(
                FRAME_SWITCH_ORDERING_ORACLE,
                format!("pending item `{pending}` never completed"),
            );
        };
        if pending_index < follow_index {
            return OracleVerdict::failed(
                FRAME_SWITCH_ORDERING_ORACLE,
                format!("pending item `{pending}` completed before follow-on `{follow_on}`"),
            );
        }
    }
    OracleVerdict::passed(
        FRAME_SWITCH_ORDERING_ORACLE,
        format!(
            "follow-on `{follow_on}` completed before {} pre-existing queued items",
            pending_at_chain_start.len()
        ),
    )
}

fn is_suspend_resume(event: &DeliveredBoundary) -> bool {
    event
        .payload
        .get("suspend_resume")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Assert that every generated suspend turn genuinely parked mid-flight and was
/// resumed only by the scheduler-delivered completion boundary — never ran
/// synchronously. This generalizes the fixed pending-tool proof into the live
/// generated search. Workloads with no suspend boundary (e.g. minimized
/// fixtures) pass vacuously.
pub fn generated_suspend_resume(events: &[DeliveredBoundary]) -> OracleVerdict {
    let mut checked = 0usize;
    for event in events.iter().filter(|event| is_suspend_resume(event)) {
        let Some(suspend) = event.observed.get("runtime_suspend") else {
            return OracleVerdict::failed(
                GENERATED_SUSPEND_RESUME_ORACLE,
                format!(
                    "suspend resume `{}` recorded no runtime_suspend evidence",
                    event.boundary_id
                ),
            );
        };
        let suspended_before = suspend
            .get("turn_suspended_before_completion")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let scheduler_delivered = suspend
            .get("scheduler_delivered_completion")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let resolve_accepted = suspend
            .get("resolve_accepted")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let resumed_after = suspend
            .get("resumed_after_completion")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let before = suspend
            .get("completed_event_count_before_resolution")
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX);
        let after = suspend
            .get("completed_event_count_after_resolution")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if !(suspended_before
            && scheduler_delivered
            && resolve_accepted
            && resumed_after
            && before == 0
            && after > before)
        {
            return OracleVerdict::failed(
                GENERATED_SUSPEND_RESUME_ORACLE,
                format!(
                    "suspend `{}` ({}) did not park-then-resume: suspended_before={suspended_before} scheduler_delivered={scheduler_delivered} resolve_accepted={resolve_accepted} resumed_after={resumed_after} completed_before={before} completed_after={after}",
                    event.boundary_id,
                    suspend
                        .get("suspend_kind")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                ),
            );
        }
        checked += 1;
    }
    if checked == 0 {
        // Anti-vacuity: every generated workload plants suspend sessions, so a run
        // with zero suspend-resume boundaries means the class was dropped — the
        // oracle must fail rather than pass on an absent class.
        return OracleVerdict::failed(
            GENERATED_SUSPEND_RESUME_ORACLE,
            "no suspend-resume boundary was observed; the suspend/resume class is absent",
        );
    }
    OracleVerdict::passed(
        GENERATED_SUSPEND_RESUME_ORACLE,
        format!(
            "{checked} generated suspend turn(s) parked mid-flight and resumed only after a scheduler-delivered completion"
        ),
    )
}

/// Run the final-value (semantic-channel / distinct-from-transcript) invariant
/// on every generated provider turn, not just the fixed RLM proof. Generated
/// turns are assistant-message turns, so the invariant asserts the dual of the
/// proof: the assistant prose was NOT mis-projected as a semantic final value —
/// `outcome_kind` is `assistant_message`, no `semantic_value` leaked, and no
/// terminal FinalValue/ToolValue event was emitted. A turn that smuggled a
/// final value through transcript inference would fail.
pub fn generated_final_value_semantic_channel(events: &[DeliveredBoundary]) -> OracleVerdict {
    let mut checked = 0usize;
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Provider)
    {
        let Some(facts) = event.observed.get("runtime_final_value_facts") else {
            return OracleVerdict::failed(
                GENERATED_FINAL_VALUE_ORACLE,
                format!(
                    "provider turn `{}` recorded no runtime_final_value_facts",
                    event.boundary_id
                ),
            );
        };
        let outcome_kind = facts
            .get("outcome_kind")
            .and_then(Value::as_str)
            .unwrap_or("");
        let has_semantic_value = facts
            .get("semantic_value")
            .map(|value| !value.is_null())
            .unwrap_or(false);
        let terminal_event_count = facts
            .get("terminal_event_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let semantic_channel_observed = facts
            .get("semantic_channel_observed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if outcome_kind != "assistant_message"
            || has_semantic_value
            || terminal_event_count != 0
            || semantic_channel_observed
        {
            return OracleVerdict::failed(
                GENERATED_FINAL_VALUE_ORACLE,
                format!(
                    "provider turn `{}` leaked a semantic final value: outcome_kind={outcome_kind} has_semantic_value={has_semantic_value} terminal_events={terminal_event_count} semantic_channel_observed={semantic_channel_observed}",
                    event.boundary_id
                ),
            );
        }
        checked += 1;
    }
    OracleVerdict::passed(
        GENERATED_FINAL_VALUE_ORACLE,
        format!(
            "{checked} generated assistant-message turn(s) kept the semantic final-value channel empty (no transcript-inferred final values)"
        ),
    )
}

pub fn cross_session_isolation(summary: &AbstractWorldSummary) -> OracleVerdict {
    if summary.session_count < 2 {
        return OracleVerdict::failed(
            CROSS_SESSION_ISOLATION_ORACLE,
            "workload did not contain at least two sessions",
        );
    }
    let aliases = summary
        .sessions
        .iter()
        .map(|session| session.alias.as_str())
        .collect::<BTreeSet<_>>();
    for session in &summary.sessions {
        if !session.opened {
            return OracleVerdict::failed(
                CROSS_SESSION_ISOLATION_ORACLE,
                format!("session `{}` was never opened", session.alias),
            );
        }
        for other_alias in aliases
            .iter()
            .copied()
            .filter(|alias| *alias != session.alias)
        {
            let leaked_provider = session
                .provider_outputs
                .iter()
                .any(|output| output.contains(other_alias));
            let leaked_tool = session
                .tool_outputs
                .iter()
                .any(|output| output.contains(other_alias));
            if leaked_provider || leaked_tool {
                return OracleVerdict::failed(
                    CROSS_SESSION_ISOLATION_ORACLE,
                    format!(
                        "session `{}` observed output from `{other_alias}`",
                        session.alias
                    ),
                );
            }
        }
    }
    OracleVerdict::passed(
        CROSS_SESSION_ISOLATION_ORACLE,
        "all generated session outputs stayed scoped to their session alias",
    )
}

pub fn ingress_sessions_opened(summary: &AbstractWorldSummary) -> OracleVerdict {
    for session in &summary.sessions {
        if !session.opened || session.ingress_count != 1 {
            return OracleVerdict::failed(
                INGRESS_SESSION_OPENED_ORACLE,
                format!(
                    "session `{}` expected exactly one ingress opening, got opened={} ingress_count={}",
                    session.alias, session.opened, session.ingress_count
                ),
            );
        }
    }
    OracleVerdict::passed(
        INGRESS_SESSION_OPENED_ORACLE,
        "each generated session opened through an ingress boundary exactly once",
    )
}

pub fn observer_convergence(summary: &AbstractWorldSummary) -> OracleVerdict {
    for session in &summary.sessions {
        let expected_turns = session.provider_outputs.len();
        let Some(last_observed_turn) = session.observer_turn_indices.last().copied() else {
            return OracleVerdict::failed(
                OBSERVER_CONVERGENCE_ORACLE,
                format!("session `{}` had no observer snapshot", session.alias),
            );
        };
        if last_observed_turn != expected_turns {
            return OracleVerdict::failed(
                OBSERVER_CONVERGENCE_ORACLE,
                format!(
                    "session `{}` observer saw turn {}, expected {}",
                    session.alias, last_observed_turn, expected_turns
                ),
            );
        }
    }
    OracleVerdict::passed(
        OBSERVER_CONVERGENCE_ORACLE,
        "observer snapshots converged to the generated runtime turn count",
    )
}

/// Build a failing-capable coverage verdict: the boundary kind must be present
/// AND the runtime invariant its reason claims must actually hold in the events.
/// A present-but-broken boundary fails loudly instead of passing on presence.
fn coverage_invariant_verdict(
    oracle_id: &'static str,
    present: bool,
    missing_reason: &'static str,
    invariant_holds: bool,
    invariant_failed_reason: &'static str,
    passed_reason: &'static str,
) -> OracleVerdict {
    if !present {
        OracleVerdict::failed(oracle_id, missing_reason)
    } else if !invariant_holds {
        OracleVerdict::failed(oracle_id, invariant_failed_reason)
    } else {
        OracleVerdict::passed(oracle_id, passed_reason)
    }
}

pub fn queued_ingress_observed(
    summary: &AbstractWorldSummary,
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    coverage_invariant_verdict(
        QUEUED_INGRESS_ORACLE,
        summary
            .sessions
            .iter()
            .any(|session| session.queued_ingress_count > 0),
        "no queued ingress boundary was observed",
        queued_ingress_has_source_keys(events),
        "queued ingress boundary was observed but it lacked a stable payload/observed source key",
        "generated workload queued turn input through an explicit ingress boundary carrying a stable source key",
    )
}

pub fn cancellation_observed(
    summary: &AbstractWorldSummary,
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    coverage_invariant_verdict(
        CANCELLATION_ORACLE,
        summary
            .sessions
            .iter()
            .any(|session| session.cancellation_count > 0),
        "no cancellation boundary was observed",
        cancellation_terminalizes_pending_input(events),
        "cancellation boundary was observed but it did not terminalize the pending queued input",
        "generated workload cancelled a pending boundary and terminalized its queued input",
    )
}

pub fn trigger_delivery_observed(
    summary: &AbstractWorldSummary,
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    coverage_invariant_verdict(
        TRIGGER_ORACLE,
        summary
            .sessions
            .iter()
            .any(|session| session.trigger_count > 0),
        "no trigger boundary was observed",
        trigger_delivery_runtime_observed(events),
        "trigger boundary was observed but it did not route through a runtime trigger DTO with a stable source identity",
        "generated workload delivered a trigger boundary routed through a runtime trigger DTO with stable source identity",
    )
}

pub fn observer_reconnect_observed(
    summary: &AbstractWorldSummary,
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    coverage_invariant_verdict(
        OBSERVER_RECONNECT_ORACLE,
        summary
            .sessions
            .iter()
            .any(|session| session.observer_reconnects > 0),
        "no observer reconnect boundary was observed",
        observer_reconnect_has_matching_turn(events, summary),
        "observer reconnect boundary was observed but its replayed snapshot did not converge to the session's final provider turn",
        "observer reconnect boundary converged on the same session state (final provider turn)",
    )
}

pub fn backend_failure_observed(
    summary: &AbstractWorldSummary,
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    coverage_invariant_verdict(
        BACKEND_FAILURE_ORACLE,
        summary
            .sessions
            .iter()
            .any(|session| session.backend_failure_count > 0),
        "no backend failure boundary was observed",
        backend_retry_terminalization_semantics(events),
        "backend failure boundary was observed but it did not terminalize through the retry/terminalization path",
        "generated workload injected a backend failure boundary that terminalized through the retry path",
    )
}

pub fn provider_mutation_rejected(
    summary: &AbstractWorldSummary,
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    coverage_invariant_verdict(
        PROVIDER_MUTATION_ORACLE,
        summary
            .sessions
            .iter()
            .any(|session| session.provider_mutation_count > 0),
        "no provider/script mutation boundary was observed",
        provider_mutation_parser_matrix_observed(events),
        "provider/script mutation boundary was observed but it did not run through the real provider parser matrix",
        "provider/script mutation boundary was rejected through the real provider parser matrix without changing runtime state",
    )
}

pub fn generated_runtime_provider_matrix(events: &[DeliveredBoundary]) -> OracleVerdict {
    let expected = MIGRATED_RUNTIME_PROVIDER_KINDS
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let mut observed = BTreeSet::new();
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Provider)
    {
        let payload_kind = event
            .payload
            .get("provider_kind")
            .and_then(Value::as_str)
            .unwrap_or("");
        let observed_kind = event
            .observed
            .get("provider_kind")
            .and_then(Value::as_str)
            .unwrap_or("");
        if payload_kind.is_empty() || observed_kind.is_empty() || payload_kind != observed_kind {
            return OracleVerdict::failed(
                GENERATED_PROVIDER_MATRIX_ORACLE,
                format!(
                    "provider event `{}` payload kind `{payload_kind}` did not match observed kind `{observed_kind}`",
                    event.boundary_id
                ),
            );
        }
        observed.insert(observed_kind);
    }
    if observed != expected {
        return OracleVerdict::failed(
            GENERATED_PROVIDER_MATRIX_ORACLE,
            format!(
                "generated runtime provider kinds {:?} did not cover {:?}",
                observed, expected
            ),
        );
    }
    OracleVerdict::passed(
        GENERATED_PROVIDER_MATRIX_ORACLE,
        "generated runtime turns covered OpenAI-compatible, direct OpenAI, Anthropic, and Google provider crates through ScriptedLlmHttpTransport",
    )
}

/// Peak number of provider turns that were simultaneously live during the run,
/// derived purely from the delivered boundary stream so the value is identical
/// on the generation path and on cross-backend replay.
///
/// A provider turn is "live" in the delivered event stream from the first
/// `ProviderEvent` released for its turn (the turn future is parked on the
/// scripted transport gate at that point) until its `Provider` completion
/// boundary is delivered. The number of distinct turns simultaneously in that
/// window is the interleaving depth; its maximum is the highwater.
pub fn peak_concurrent_live_turns(events: &[DeliveredBoundary]) -> usize {
    let mut live = BTreeSet::<String>::new();
    let mut peak = 0usize;
    for event in events {
        match event.kind {
            BoundaryKind::ProviderEvent => {
                if let Some(turn_boundary_id) = event
                    .payload
                    .get("turn_boundary_id")
                    .and_then(Value::as_str)
                {
                    live.insert(turn_boundary_id.to_string());
                    peak = peak.max(live.len());
                }
            }
            BoundaryKind::Provider => {
                live.remove(&event.boundary_id);
            }
            _ => {}
        }
    }
    peak
}

/// Count of distinct sessions that ran at least one provider turn. Interleaving
/// is structurally impossible below two such sessions, so the interleaving
/// oracle treats those workloads as vacuously satisfied.
fn provider_turn_session_count(events: &[DeliveredBoundary]) -> usize {
    events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Provider)
        .map(|event| event.actor_alias.as_str())
        .collect::<BTreeSet<_>>()
        .len()
}

/// Make interleaving load-bearing: whenever a workload runs provider turns in at
/// least two sessions, the scheduler must actually drive at least two of those
/// turns concurrently. A multi-session workload that never interleaves is a real
/// scheduling regression and fails this oracle; single-session workloads (and
/// minimized fixtures that collapse to one session) pass vacuously.
pub fn provider_turn_interleaving_depth(events: &[DeliveredBoundary]) -> OracleVerdict {
    let peak = peak_concurrent_live_turns(events);
    let sessions = provider_turn_session_count(events);
    if sessions < 2 {
        return OracleVerdict::passed(
            PROVIDER_TURN_INTERLEAVING_ORACLE,
            format!(
                "interleaving not required: only {sessions} session(s) ran provider turns (peak concurrent live turns {peak})"
            ),
        );
    }
    if peak >= 2 {
        OracleVerdict::passed(
            PROVIDER_TURN_INTERLEAVING_ORACLE,
            format!(
                "scheduler drove {peak} provider turns concurrently across {sessions} sessions"
            ),
        )
    } else {
        OracleVerdict::failed(
            PROVIDER_TURN_INTERLEAVING_ORACLE,
            format!(
                "{sessions} sessions ran provider turns but peak concurrent live turns was {peak}; the scheduler never interleaved live provider turns"
            ),
        )
    }
}

/// Each generator-driven transport/HTTP mutation must land on its own named
/// failure path through the real provider: a mid-stream disconnect classifies
/// as a retryable stream fault, response-start and chunk timeouts as retryable
/// timeouts, and a 5xx as a retryable HTTP status carrying its status code.
/// This proves the new mutation classes drive distinct, executable behaviors
/// rather than collapsing into a single generic parser error. Workloads that
/// happen to contain no transport mutation pass vacuously (the class is still
/// covered by the seeded anchor in every full run).
pub fn provider_transport_mutation_classified(events: &[DeliveredBoundary]) -> OracleVerdict {
    let mut observed_classes = BTreeSet::new();
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::ProviderMutation)
    {
        let Some(mutation) = event
            .observed
            .get("mutation")
            .or_else(|| event.payload.get("mutation"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        if !is_transport_provider_mutation(mutation) {
            continue;
        }
        let Some(proof) = event
            .observed
            .pointer("/provider_parser_matrix/matrix/proofs")
            .and_then(Value::as_array)
            .and_then(|proofs| proofs.first())
        else {
            return OracleVerdict::failed(
                PROVIDER_TRANSPORT_MUTATION_ORACLE,
                format!(
                    "transport mutation `{mutation}` on `{}` recorded no real provider parser proof",
                    event.boundary_id
                ),
            );
        };
        let raw_kind = proof.get("kind").and_then(Value::as_str).unwrap_or("");
        let retryable = proof
            .pointer("/classification/retryable")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let status = proof.get("status").and_then(Value::as_u64);
        let classified = match mutation {
            "mid_stream_disconnect" => raw_kind == "Stream" && retryable,
            "response_start_timeout" | "stream_chunk_timeout" => raw_kind == "Timeout" && retryable,
            "retryable_server_error_sequence" => status == Some(503) && retryable,
            _ => false,
        };
        if !classified {
            return OracleVerdict::failed(
                PROVIDER_TRANSPORT_MUTATION_ORACLE,
                format!(
                    "transport mutation `{mutation}` on `{}` classified incorrectly: raw_kind={raw_kind} retryable={retryable} status={status:?}",
                    event.boundary_id
                ),
            );
        }
        observed_classes.insert(mutation.to_string());
    }
    OracleVerdict::passed(
        PROVIDER_TRANSPORT_MUTATION_ORACLE,
        format!(
            "transport mutation classes classified on distinct failure paths: {:?}",
            observed_classes
        ),
    )
}

pub fn process_wake_observed(
    summary: &AbstractWorldSummary,
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    coverage_invariant_verdict(
        PROCESS_WAKE_ORACLE,
        summary
            .sessions
            .iter()
            .any(|session| session.process_wake_count > 0),
        "no process wake boundary was observed",
        process_wake_runtime_dto_observed(events),
        "process wake boundary was observed but it did not materialize a runtime DTO",
        "process wake boundary materialized a runtime DTO",
    )
}

/// Assert across the whole generated schedule that one logical process wake
/// materializes into at most one runtime turn. The harness records a stable
/// `runtime_turn_id` when the real queued-work claim materializes a wake;
/// grouping those ids by durable source key makes duplicate materialization
/// visible even when the duplicate boundary is later settled or deduplicated.
pub fn process_wake_at_most_once(events: &[DeliveredBoundary]) -> OracleVerdict {
    let mut turns_by_wake = BTreeMap::<String, BTreeSet<String>>::new();
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::ProcessWake)
    {
        let Some(queued_work) = event.observed.get("runtime_queued_work") else {
            continue;
        };
        if queued_work.get("enqueued").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let Some(source_key) = queued_work.get("source_key").and_then(Value::as_str) else {
            return OracleVerdict::failed(
                PROCESS_WAKE_AT_MOST_ONCE_ORACLE,
                format!(
                    "enqueued process wake `{}` recorded no queued-work source key",
                    event.boundary_id
                ),
            );
        };
        let turns = turns_by_wake.entry(source_key.to_string()).or_default();
        if queued_work.get("claimed").and_then(Value::as_bool) == Some(true) {
            let Some(turn_id) = queued_work.get("runtime_turn_id").and_then(Value::as_str) else {
                return OracleVerdict::failed(
                    PROCESS_WAKE_AT_MOST_ONCE_ORACLE,
                    format!(
                        "claimed process wake `{}` recorded no runtime-turn materialization id",
                        event.boundary_id
                    ),
                );
            };
            turns.insert(turn_id.to_string());
            if turns.len() > 1 {
                return OracleVerdict::failed(
                    PROCESS_WAKE_AT_MOST_ONCE_ORACLE,
                    format!(
                        "process wake `{source_key}` materialized into {} runtime turns: {:?}",
                        turns.len(),
                        turns
                    ),
                );
            }
        }
    }
    if turns_by_wake.is_empty() {
        return OracleVerdict::failed(
            PROCESS_WAKE_AT_MOST_ONCE_ORACLE,
            "generated schedule recorded no enqueued process wakes",
        );
    }
    let materialized = turns_by_wake
        .values()
        .filter(|turns| !turns.is_empty())
        .count();
    OracleVerdict::passed(
        PROCESS_WAKE_AT_MOST_ONCE_ORACLE,
        format!(
            "{} enqueued logical process wake(s) materialized into at most one runtime turn each ({materialized} materialized)",
            turns_by_wake.len()
        ),
    )
}

pub fn exec_code_observed(
    summary: &AbstractWorldSummary,
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    coverage_invariant_verdict(
        EXEC_CODE_ORACLE,
        summary
            .sessions
            .iter()
            .any(|session| !session.exec_code_outputs.is_empty()),
        "no exec-code boundary was observed",
        exec_runtime_outcome_observed(events),
        "exec-code boundary was observed but it did not produce a captured RuntimeEffectOutcome",
        "exec-code boundary produced a captured RuntimeEffectOutcome result",
    )
}

pub fn tool_boundary_observed(
    summary: &AbstractWorldSummary,
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    coverage_invariant_verdict(
        TOOL_BOUNDARY_ORACLE,
        summary
            .sessions
            .iter()
            .any(|session| !session.tool_outputs.is_empty()),
        "no tool boundary was observed",
        tool_runtime_output_observed(events),
        "tool boundary was observed but it did not produce a captured runtime tool-output DTO",
        "tool boundary produced a captured runtime tool-output DTO",
    )
}

pub fn runtime_session_graph_contract(summary: &AbstractWorldSummary) -> OracleVerdict {
    for session in &summary.sessions {
        if session.provider_outputs.len() < 2 {
            return OracleVerdict::failed(
                RUNTIME_SESSION_GRAPH_ORACLE,
                format!(
                    "session `{}` ran only {} provider turns",
                    session.alias,
                    session.provider_outputs.len()
                ),
            );
        }
        for (index, output) in session.provider_outputs.iter().enumerate() {
            let turn_index = index + 1;
            let expected_exchange_count = turn_index;
            if session.provider_exchange_counts.get(index).copied() != Some(expected_exchange_count)
            {
                return OracleVerdict::failed(
                    RUNTIME_SESSION_GRAPH_ORACLE,
                    format!(
                        "session `{}` turn {turn_index} provider exchanges did not converge to {expected_exchange_count}",
                        session.alias
                    ),
                );
            }
            let expected_graph_min_count = turn_index * 2 + 1;
            if session.graph_node_counts.get(index).copied().unwrap_or(0) < expected_graph_min_count
            {
                return OracleVerdict::failed(
                    RUNTIME_SESSION_GRAPH_ORACLE,
                    format!(
                        "session `{}` turn {turn_index} graph had fewer than {expected_graph_min_count} nodes",
                        session.alias
                    ),
                );
            }
            let expected_transcript_min_count = turn_index * 2;
            if session
                .transcript_message_counts
                .get(index)
                .copied()
                .unwrap_or(0)
                < expected_transcript_min_count
            {
                return OracleVerdict::failed(
                    RUNTIME_SESSION_GRAPH_ORACLE,
                    format!(
                        "session `{}` turn {turn_index} transcript had fewer than {expected_transcript_min_count} messages",
                        session.alias
                    ),
                );
            }
            if !output.contains(&session.alias) {
                return OracleVerdict::failed(
                    RUNTIME_SESSION_GRAPH_ORACLE,
                    format!(
                        "session `{}` turn {turn_index} provider output did not identify its session",
                        session.alias
                    ),
                );
            }
        }
    }
    OracleVerdict::passed(
        RUNTIME_SESSION_GRAPH_ORACLE,
        "runtime-backed generated sessions advanced provider exchanges, graph nodes, and transcript messages across multiple turns",
    )
}

#[cfg(test)]
fn runtime_graph_projection_acyclic(events: &[DeliveredBoundary]) -> OracleVerdict {
    let mut checked = 0;
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Provider)
    {
        let Some(facts) = runtime_observed_fact::<RuntimeGraphInvariantFacts>(event, "graph")
        else {
            return OracleVerdict::failed(
                RUNTIME_GRAPH_ACYCLIC_ORACLE,
                format!(
                    "provider boundary `{}` did not expose real graph invariant facts",
                    event.boundary_id
                ),
            );
        };
        checked += 1;
        if !facts.passed {
            return OracleVerdict::failed(
                RUNTIME_GRAPH_ACYCLIC_ORACLE,
                format!(
                    "provider boundary `{}` observed graph duplicates={:?} missing_parents={:?} cycles={:?} leaf_exists={}",
                    event.boundary_id,
                    facts.duplicate_node_ids,
                    facts.missing_parent_links,
                    facts.cycle_node_ids,
                    facts.leaf_exists
                ),
            );
        }
    }
    if checked == 0 {
        return OracleVerdict::failed(
            RUNTIME_GRAPH_ACYCLIC_ORACLE,
            "no provider turn exposed runtime graph invariant facts",
        );
    }
    OracleVerdict::passed(
        RUNTIME_GRAPH_ACYCLIC_ORACLE,
        format!(
            "{checked} real provider turn graphs had unique nodes, valid parents, and no cycles"
        ),
    )
}

/// Validate graph integrity from accepted checkpoint raw rows. This deliberately
/// bypasses `SessionGraph` and its read-model projection so duplicate durable
/// rows cannot disappear before the oracle observes them.
pub fn runtime_graph_acyclic(writes: &[crate::store::CheckpointWriteEvent]) -> OracleVerdict {
    let mut checked = 0usize;
    for write in writes
        .iter()
        .filter(|write| write.cause_boundary_id.is_none())
    {
        let Some(state) = &write.state else {
            // Promoted v1/v2 fixtures predate accepted raw-row observations.
            continue;
        };
        let session_id = write.attributed_session();
        let Some(raw) = state.accepted_raw_rows.as_ref() else {
            return OracleVerdict::failed(
                RUNTIME_GRAPH_ACYCLIC_ORACLE,
                format!(
                    "session `{session_id}` commit {} exposed no accepted raw rows",
                    write.commit_index
                ),
            );
        };
        let Some(rows) = raw.get("graph_nodes").and_then(Value::as_array) else {
            return OracleVerdict::failed(
                RUNTIME_GRAPH_ACYCLIC_ORACLE,
                format!(
                    "session `{session_id}` commit {} exposed no raw graph rows",
                    write.commit_index
                ),
            );
        };
        if let Err(message) = validate_raw_graph_rows(
            rows,
            raw.get("graph_leaf_node_id").and_then(Value::as_str),
            session_id,
            write.commit_index,
        ) {
            return OracleVerdict::failed(RUNTIME_GRAPH_ACYCLIC_ORACLE, message);
        }
        checked += 1;
    }
    if checked == 0 {
        return OracleVerdict::failed(
            RUNTIME_GRAPH_ACYCLIC_ORACLE,
            "no checkpoint commit exposed accepted raw graph rows",
        );
    }
    OracleVerdict::passed(
        RUNTIME_GRAPH_ACYCLIC_ORACLE,
        format!(
            "{checked} accepted raw graph snapshots had unique rows, valid parents, and no cycles"
        ),
    )
}

fn validate_raw_graph_rows(
    rows: &[Value],
    leaf_node_id: Option<&str>,
    session_id: &str,
    commit_index: usize,
) -> Result<(), String> {
    let context = format!("session `{session_id}` commit {commit_index}");
    let mut seen = BTreeSet::new();
    let mut parents = BTreeMap::<String, Option<String>>::new();
    for row in rows {
        let node_id = row
            .get("node_id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{context} raw graph row has no node id"))?;
        if !seen.insert(node_id.to_string()) {
            return Err(format!(
                "{context} accepted raw graph contains duplicate row `{node_id}`"
            ));
        }
        parents.insert(
            node_id.to_string(),
            row.get("parent_node_id")
                .and_then(Value::as_str)
                .map(ToString::to_string),
        );
    }
    for (node_id, parent) in &parents {
        if let Some(parent) = parent
            && !parents.contains_key(parent)
        {
            return Err(format!(
                "{context} raw graph row `{node_id}` references missing parent `{parent}`"
            ));
        }
    }
    for start in parents.keys() {
        let mut path = BTreeSet::new();
        let mut current = Some(start.as_str());
        while let Some(node_id) = current {
            if !path.insert(node_id.to_string()) {
                return Err(format!(
                    "{context} raw graph contains a cycle through `{node_id}`"
                ));
            }
            current = parents.get(node_id).and_then(Option::as_deref);
        }
    }
    if leaf_node_id.is_some_and(|leaf| !parents.contains_key(leaf)) {
        return Err(format!(
            "{context} raw graph leaf `{}` has no row",
            leaf_node_id.unwrap_or_default()
        ));
    }
    Ok(())
}

pub fn runtime_single_active_agent_frame(events: &[DeliveredBoundary]) -> OracleVerdict {
    let mut checked = 0;
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Provider)
    {
        let Some(facts) =
            runtime_observed_fact::<RuntimeAgentFrameInvariantFacts>(event, "agent_frame")
        else {
            return OracleVerdict::failed(
                RUNTIME_SINGLE_ACTIVE_AGENT_FRAME_ORACLE,
                format!(
                    "provider boundary `{}` did not expose real Agent Frame facts",
                    event.boundary_id
                ),
            );
        };
        checked += 1;
        if !facts.passed {
            return OracleVerdict::failed(
                RUNTIME_SINGLE_ACTIVE_AGENT_FRAME_ORACLE,
                format!(
                    "provider boundary `{}` observed current_frame=`{}` active={:?} current_exists={} current_active={} unknown_node_frames={:?}",
                    event.boundary_id,
                    facts.current_frame_node_id,
                    facts.active_frame_ids,
                    facts.current_frame_exists,
                    facts.current_frame_active,
                    facts.node_agent_frame_ids_without_record
                ),
            );
        }
    }
    if checked == 0 {
        return OracleVerdict::failed(
            RUNTIME_SINGLE_ACTIVE_AGENT_FRAME_ORACLE,
            "no provider turn exposed runtime Agent Frame facts",
        );
    }
    OracleVerdict::passed(
        RUNTIME_SINGLE_ACTIVE_AGENT_FRAME_ORACLE,
        format!(
            "{checked} real provider turn snapshots had exactly one active current Agent Frame"
        ),
    )
}

pub fn runtime_usage_monotonic(events: &[DeliveredBoundary]) -> OracleVerdict {
    let mut checked = 0;
    let mut last_total_by_session = BTreeMap::<String, i64>::new();
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Provider)
    {
        let Some(facts) = runtime_observed_fact::<RuntimeUsageInvariantFacts>(event, "usage")
        else {
            return OracleVerdict::failed(
                RUNTIME_USAGE_MONOTONIC_ORACLE,
                format!(
                    "provider boundary `{}` did not expose real usage facts",
                    event.boundary_id
                ),
            );
        };
        checked += 1;
        if !facts.non_negative {
            return OracleVerdict::failed(
                RUNTIME_USAGE_MONOTONIC_ORACLE,
                format!(
                    "provider boundary `{}` observed negative token fields {:?}",
                    event.boundary_id, facts.negative_fields
                ),
            );
        }
        if !facts.usage_events_monotonic {
            return OracleVerdict::failed(
                RUNTIME_USAGE_MONOTONIC_ORACLE,
                format!(
                    "provider boundary `{}` emitted non-monotonic usage activity cumulatives",
                    event.boundary_id
                ),
            );
        }
        let current = facts.token_ledger_total.total_tokens;
        if let Some(previous) = last_total_by_session.insert(event.actor_alias.clone(), current)
            && current < previous
        {
            return OracleVerdict::failed(
                RUNTIME_USAGE_MONOTONIC_ORACLE,
                format!(
                    "session `{}` cumulative token ledger total regressed from {previous} to {current} at `{}`",
                    event.actor_alias, event.boundary_id
                ),
            );
        }
    }
    if checked == 0 {
        return OracleVerdict::failed(
            RUNTIME_USAGE_MONOTONIC_ORACLE,
            "no provider turn exposed runtime usage facts",
        );
    }
    OracleVerdict::passed(
        RUNTIME_USAGE_MONOTONIC_ORACLE,
        format!(
            "{checked} real provider turns had non-negative usage and non-decreasing session ledger totals"
        ),
    )
}

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
fn lifecycle_processes(events: &[DeliveredBoundary]) -> Vec<&Value> {
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

fn process_field_str<'a>(process: &'a Value, key: &str) -> Option<&'a str> {
    process.get(key).and_then(Value::as_str)
}

fn process_field_bool(process: &Value, key: &str) -> Option<bool> {
    process.get(key).and_then(Value::as_bool)
}

fn is_started_owner_bound(process: &Value) -> bool {
    process_field_str(process, "disposition") == Some("owner_bound")
        && process_field_bool(process, "started") == Some(true)
}

/// An Abandoned terminal is licensed by a live owner's inline drain or an
/// operator-authorized request reconciled after the lease lapsed. Elapsed time
/// or a missing/unknown writer is never licensing evidence.
fn abandoned_evidence_is_licensed(process: &Value) -> bool {
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
fn started_owner_bound_recovery_is_safe(events: &[DeliveredBoundary]) -> bool {
    lifecycle_processes(events).iter().all(|process| {
        let running = process_field_str(process, "terminal_status") == Some("running");
        let abandoned = process_field_str(process, "terminal_status") == Some("abandoned");
        let owner_bound_ok = !is_started_owner_bound(process)
            || (process_field_bool(process, "reran") == Some(false) && (running || abandoned));
        let abandoned_ok = !abandoned || abandoned_evidence_is_licensed(process);
        owner_bound_ok && abandoned_ok
    })
}

fn worker_owned_work_continued_by_successor(event: &DeliveredBoundary) -> bool {
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
pub fn lease_time_monotonic(events: &[DeliveredBoundary]) -> OracleVerdict {
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
    OracleVerdict::passed(
        LEASE_TIME_MONOTONIC_ORACLE,
        format!(
            "{grounded} lease-time boundaries advanced a real session-execution-lease fencing token monotonically per session"
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

pub fn scheduler_owned_runtime_completions(events: &[DeliveredBoundary]) -> OracleVerdict {
    let mut missing = Vec::new();
    for kind in [
        BoundaryKind::Provider,
        BoundaryKind::Cancellation,
        BoundaryKind::BackendFailure,
        BoundaryKind::ProviderMutation,
        BoundaryKind::Tool,
        BoundaryKind::ExecCode,
        BoundaryKind::DurableEffect,
        BoundaryKind::Worker,
    ] {
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
        "provider chunks/retries, cancellation, tool returns, exec results, durable completions, provider mutations, and worker completions were registered as pending runtime boundaries and delivered by the scheduler",
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

/// Scenario-contract oracle vector. Every contract emits its own named,
/// failing-capable verdict; suite-level coverage manifests are deliberately not
/// used as backing oracles because they let generated packages look
/// per-contract while sharing the same semantic proof.
pub fn scenario_contract_oracles(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> Vec<OracleVerdict> {
    all_scenario_contracts()
        .map(|contract| scenario_contract_oracle(contract, events, summary))
        .collect()
}

fn all_scenario_contracts() -> impl Iterator<Item = &'static ScenarioContractSpec> {
    [
        RUNTIME_SCENARIO_CONTRACTS,
        STANDARD_PROTOCOL_SCENARIO_CONTRACTS,
        RLM_PROTOCOL_SCENARIO_CONTRACTS,
        AGENT_SCENARIO_CONTRACTS,
    ]
    .into_iter()
    .flat_map(|contracts| contracts.iter())
}

pub fn scenario_contract_mini_oracles(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> Vec<OracleVerdict> {
    vec![
        mini_runtime_queued_input_hidden(events),
        mini_runtime_cancellation_prevents_idle_claim(events),
        mini_runtime_process_wake_duplicate_rejected(events),
        mini_runtime_stale_lease_commit_rejected(events, summary),
        mini_standard_streamed_text_finalizes_once(events),
        mini_standard_provider_error_without_checkpoint(events),
        mini_standard_tool_loop_reenters(events),
        mini_rlm_finish_required_prose_repair(events, summary),
        mini_rlm_schema_mismatch_repair(events),
        mini_rlm_lashlang_cell_exec_continues(events),
        mini_agent_durable_input_resolution(events),
        mini_agent_child_failure_graph(events, summary),
        mini_agent_parallel_spawn_join(events, summary),
    ]
}

fn mini_runtime_queued_input_hidden(events: &[DeliveredBoundary]) -> OracleVerdict {
    let Some(queued) = first_event(events, BoundaryKind::QueuedIngress) else {
        return OracleVerdict::failed(
            SCENARIO_MINI_RUNTIME_QUEUED_HIDDEN_ORACLE,
            "no queued input boundary was generated",
        );
    };
    let queued_text = queued
        .payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("");
    let leaked = events.iter().any(|event| {
        event.kind == BoundaryKind::Provider
            && event.actor_alias == queued.actor_alias
            && event
                .observed
                .get("provider_output")
                .and_then(Value::as_str)
                .is_some_and(|output| !queued_text.is_empty() && output.contains(queued_text))
    });
    if leaked {
        OracleVerdict::failed(
            SCENARIO_MINI_RUNTIME_QUEUED_HIDDEN_ORACLE,
            format!(
                "queued input `{}` leaked into a provider turn before explicit claim",
                queued.boundary_id
            ),
        )
    } else {
        OracleVerdict::passed(
            SCENARIO_MINI_RUNTIME_QUEUED_HIDDEN_ORACLE,
            "queued input stayed hidden from provider turns while the generated live turn continued",
        )
    }
}

fn mini_runtime_cancellation_prevents_idle_claim(events: &[DeliveredBoundary]) -> OracleVerdict {
    let queued = events
        .iter()
        .filter(|event| event.kind == BoundaryKind::QueuedIngress)
        .map(|event| event.boundary_id.as_str())
        .collect::<BTreeSet<_>>();
    let Some(cancel) = events.iter().find(|event| {
        event.kind == BoundaryKind::Cancellation
            && event
                .observed
                .get("cancelled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
    }) else {
        let outcomes = events
            .iter()
            .filter(|event| event.kind == BoundaryKind::Cancellation)
            .map(|event| {
                format!(
                    "{}={}",
                    event.boundary_id,
                    event
                        .observed
                        .get("cancel_outcome")
                        .and_then(Value::as_str)
                        .unwrap_or("missing")
                )
            })
            .collect::<Vec<_>>();
        return OracleVerdict::failed(
            SCENARIO_MINI_RUNTIME_CANCEL_IDLE_ORACLE,
            format!(
                "no cancellation boundary reported a real runtime cancellation; outcomes={}",
                outcomes.join(",")
            ),
        );
    };
    let target = cancel
        .observed
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("");
    if queued.contains(target) {
        OracleVerdict::passed(
            SCENARIO_MINI_RUNTIME_CANCEL_IDLE_ORACLE,
            "cancelled queued input targets a generated queued boundary and prevents later idle claim",
        )
    } else {
        OracleVerdict::failed(
            SCENARIO_MINI_RUNTIME_CANCEL_IDLE_ORACLE,
            format!("cancellation target `{target}` was not a generated queued boundary"),
        )
    }
}

fn mini_runtime_process_wake_duplicate_rejected(events: &[DeliveredBoundary]) -> OracleVerdict {
    let mut source_events: BTreeMap<String, Vec<&DeliveredBoundary>> = BTreeMap::new();
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::ProcessWake)
    {
        if let Some(source_key) = process_wake_source_key(event) {
            source_events.entry(source_key).or_default().push(event);
        }
    }
    if source_events.values().any(|events| {
        let claims = events
            .iter()
            .filter_map(|event| event.observed.get("claimed_once").and_then(Value::as_bool))
            .collect::<Vec<_>>();
        let strict_claim_dedupe =
            claims.iter().filter(|claimed| **claimed).count() == 1 && claims.contains(&false);
        let in_flight_rejection = events.iter().any(|event| {
            event
                .observed
                .get("lease_busy")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && event
                    .observed
                    .pointer("/runtime_queued_work/enqueued")
                    .and_then(Value::as_bool)
                    == Some(false)
        }) && claims.contains(&false);
        strict_claim_dedupe || in_flight_rejection
    }) {
        OracleVerdict::passed(
            SCENARIO_MINI_RUNTIME_PROCESS_WAKE_DEDUPE_ORACLE,
            "duplicate process wake used the same structural process/event source key and was claimed at most once",
        )
    } else {
        OracleVerdict::failed(
            SCENARIO_MINI_RUNTIME_PROCESS_WAKE_DEDUPE_ORACLE,
            "no structural process/event source key showed a claim/rejection pair or in-flight lease rejection",
        )
    }
}

fn mini_runtime_stale_lease_commit_rejected(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> OracleVerdict {
    let stale_rejected = worker_runtime_lease_dto_observed(events)
        && summary.workers.iter().any(|worker| {
            worker.active_fencing_token > 1 && worker.stale_completion_rejections > 0
        });
    verdict_from_bool(
        SCENARIO_MINI_RUNTIME_STALE_LEASE_ORACLE,
        stale_rejected,
        "stale worker lease completion was rejected while the reclaimed live lease stayed renewable",
        "worker lease evidence did not prove stale completion rejection",
    )
}

fn mini_standard_streamed_text_finalizes_once(events: &[DeliveredBoundary]) -> OracleVerdict {
    let Some(provider) = events.iter().find(|event| {
        event.kind == BoundaryKind::Provider
            && provider_completion_units(event)
                .iter()
                .filter(|unit| unit.contains("sse"))
                .count()
                >= 2
    }) else {
        return OracleVerdict::failed(
            SCENARIO_MINI_STANDARD_STREAM_FINALIZE_ORACLE,
            "no streamed provider completion with multiple scheduler-owned SSE units was observed",
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
    verdict_from_bool(
        SCENARIO_MINI_STANDARD_STREAM_FINALIZE_ORACLE,
        occurrences == 1,
        "streamed provider output contains exactly one final assistant text projection",
        "streamed provider output did not finalize exactly once",
    )
}

fn mini_standard_provider_error_without_checkpoint(events: &[DeliveredBoundary]) -> OracleVerdict {
    let failure_before_provider = events.iter().any(|event| {
        matches!(
            event.kind,
            BoundaryKind::ProviderMutation | BoundaryKind::BackendFailure
        ) && event.payload.get("runtime_completion").is_some()
            && event.sequence
                < next_provider_sequence(events, &event.actor_alias).unwrap_or(usize::MAX)
    });
    verdict_from_bool(
        SCENARIO_MINI_STANDARD_PROVIDER_ERROR_ORACLE,
        failure_before_provider && provider_mutation_parser_matrix_observed(events),
        "provider failure/mutation completed through scheduler before any later successful checkpointed provider turn",
        "no scheduler-owned provider failure before checkpoint/next provider turn was observed",
    )
}

fn mini_standard_tool_loop_reenters(events: &[DeliveredBoundary]) -> OracleVerdict {
    let Some(tool) = first_event(events, BoundaryKind::Tool) else {
        return OracleVerdict::failed(
            SCENARIO_MINI_STANDARD_TOOL_REENTRY_ORACLE,
            "no tool boundary was generated",
        );
    };
    let reentered = events.iter().any(|event| {
        event.kind == BoundaryKind::Provider
            && event.actor_alias == tool.actor_alias
            && event.sequence > tool.sequence
    });
    verdict_from_bool(
        SCENARIO_MINI_STANDARD_TOOL_REENTRY_ORACLE,
        reentered && tool_runtime_output_observed(events),
        "tool result was captured and a later provider turn re-entered the model loop for the same session",
        "tool result did not have a later provider re-entry",
    )
}

fn mini_rlm_finish_required_prose_repair(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> OracleVerdict {
    verdict_from_bool(
        SCENARIO_MINI_RLM_FINISH_REPAIR_ORACLE,
        provider_exchange_counts_are_turn_indexed(summary)
            && observer_reconnect_has_matching_turn(events, summary)
            && events
                .iter()
                .filter(|event| event.kind == BoundaryKind::Provider)
                .any(|event| event.payload.get("runtime_completion").is_some()),
        "finish-required repair mini-replay observed repeated provider completions and observer convergence",
        "finish-required repair mini-replay lacked repeated provider/observer convergence",
    )
}

fn mini_rlm_schema_mismatch_repair(events: &[DeliveredBoundary]) -> OracleVerdict {
    let saw_schema_mutation = events.iter().any(|event| {
        event.kind == BoundaryKind::ProviderMutation
            && event
                .observed
                .pointer("/provider_parser_matrix/matrix/real_provider_parser_execution")
                .and_then(Value::as_bool)
                .unwrap_or(false)
    });
    verdict_from_bool(
        SCENARIO_MINI_RLM_SCHEMA_REPAIR_ORACLE,
        saw_schema_mutation && provider_mutation_classes_observed(events),
        "schema mismatch repair mini-replay used mutated provider scripts through real provider parsers",
        "schema mismatch repair mini-replay lacked provider parser mutation evidence",
    )
}

fn mini_rlm_lashlang_cell_exec_continues(events: &[DeliveredBoundary]) -> OracleVerdict {
    let Some(exec) = first_event(events, BoundaryKind::ExecCode) else {
        return OracleVerdict::failed(
            SCENARIO_MINI_RLM_CELL_EXEC_ORACLE,
            "no exec-code boundary was generated",
        );
    };
    let continued = events.iter().any(|event| {
        event.kind == BoundaryKind::Provider
            && event.actor_alias == exec.actor_alias
            && event.sequence > exec.sequence
    });
    verdict_from_bool(
        SCENARIO_MINI_RLM_CELL_EXEC_ORACLE,
        continued && exec_runtime_outcome_observed(events),
        "lashlang cell exec mini-replay produced an exec outcome and continued to a later provider turn",
        "lashlang cell exec mini-replay did not continue after exec",
    )
}

fn mini_agent_durable_input_resolution(events: &[DeliveredBoundary]) -> OracleVerdict {
    let durable = events.iter().any(|event| {
        event.kind == BoundaryKind::DurableEffect
            && event
                .observed
                .get("replayed")
                .and_then(Value::as_bool)
                .unwrap_or(false)
    });
    verdict_from_bool(
        SCENARIO_MINI_AGENT_DURABLE_INPUT_ORACLE,
        durable && process_wake_runtime_dto_observed(events) && observer_reconnect_has_any(events),
        "durable input mini-replay observed durable replay, process wake, and observer reconnect resolution",
        "durable input mini-replay lacked durable replay/process wake/observer reconnect evidence",
    )
}

fn mini_agent_child_failure_graph(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> OracleVerdict {
    verdict_from_bool(
        SCENARIO_MINI_AGENT_CHILD_FAILURE_ORACLE,
        summary.session_count >= 2
            && worker_runtime_lease_dto_observed(events)
            && events
                .iter()
                .any(|event| event.kind == BoundaryKind::BackendFailure),
        "child failure mini-replay kept multi-session graph evidence while worker/backend failure boundaries executed",
        "child failure mini-replay lacked multi-session worker/backend failure evidence",
    )
}

fn mini_agent_parallel_spawn_join(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> OracleVerdict {
    let wake_sessions = events
        .iter()
        .filter(|event| event.kind == BoundaryKind::ProcessWake)
        .filter_map(|event| event.observed.get("session").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();
    let ordered_sequences = events
        .iter()
        .filter(|event| matches!(event.kind, BoundaryKind::ProcessWake | BoundaryKind::Worker))
        .map(|event| event.sequence)
        .collect::<Vec<_>>();
    let deterministic_order = ordered_sequences.windows(2).all(|pair| pair[0] < pair[1]);
    verdict_from_bool(
        SCENARIO_MINI_AGENT_PARALLEL_JOIN_ORACLE,
        summary.session_count >= 2 && !wake_sessions.is_empty() && deterministic_order,
        "parallel spawn/join mini-replay recorded deterministic process/worker sequence ordering",
        "parallel spawn/join mini-replay did not record deterministic process/worker ordering",
    )
}

fn verdict_from_bool(
    oracle_id: &'static str,
    condition: bool,
    passed: &'static str,
    failed: &'static str,
) -> OracleVerdict {
    if condition {
        OracleVerdict::passed(oracle_id, passed)
    } else {
        OracleVerdict::failed(oracle_id, failed)
    }
}

fn first_event(events: &[DeliveredBoundary], kind: BoundaryKind) -> Option<&DeliveredBoundary> {
    events.iter().find(|event| event.kind == kind)
}

fn provider_completion_units(event: &DeliveredBoundary) -> Vec<String> {
    event
        .payload
        .pointer("/runtime_completion/completion_units")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|unit| unit.get("unit").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

fn next_provider_sequence(events: &[DeliveredBoundary], actor_alias: &str) -> Option<usize> {
    events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Provider && event.actor_alias == actor_alias)
        .map(|event| event.sequence)
        .min()
}

fn observer_reconnect_has_any(events: &[DeliveredBoundary]) -> bool {
    events.iter().any(|event| {
        event.kind == BoundaryKind::Observer
            && event
                .observed
                .get("reconnected")
                .and_then(Value::as_bool)
                .unwrap_or(false)
    })
}

fn scenario_contract_oracle(
    contract: &ScenarioContractSpec,
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> OracleVerdict {
    let missing = contract
        .required_sim_evidence
        .iter()
        .copied()
        .filter(|evidence| !scenario_evidence_satisfied(evidence, events, summary))
        .collect::<Vec<_>>();
    let oracle_id = scenario_contract_oracle_id(contract);
    let semantic = scenario_contract_semantics(contract, events, summary);
    if missing.is_empty() && semantic.passed {
        OracleVerdict::passed(
            oracle_id,
            format!(
                "{} contract `{}` passed semantic `{}`: {}. Evidence: {}; adapter: {}",
                contract.suite,
                contract.test_name,
                contract.semantic_oracle,
                contract.owned_invariant,
                contract.required_sim_evidence.join(", "),
                semantic.reason
            ),
        )
    } else {
        let mut failures = Vec::new();
        if !missing.is_empty() {
            failures.push(format!("missing evidence [{}]", missing.join(", ")));
        }
        if !semantic.passed {
            failures.push(format!("semantic adapter failed: {}", semantic.reason));
        }
        OracleVerdict::failed(
            oracle_id,
            format!(
                "{} contract `{}` failed {} for invariant: {}",
                contract.suite,
                contract.test_name,
                failures.join("; "),
                contract.owned_invariant
            ),
        )
    }
}

fn scenario_contract_oracle_id(contract: &ScenarioContractSpec) -> String {
    format!("{}:{}", contract.oracle_id, contract.test_name)
}

fn scenario_evidence_satisfied(
    evidence: &str,
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> bool {
    match evidence {
        "queued_ingress" => summary
            .sessions
            .iter()
            .any(|session| session.queued_ingress_count > 0),
        "cancellation" => summary
            .sessions
            .iter()
            .any(|session| session.cancellation_count > 0),
        "process_wake" => {
            summary
                .sessions
                .iter()
                .any(|session| session.process_wake_count > 0)
                && process_wake_runtime_dto_observed(events)
        }
        "worker_stale_completion" => {
            summary.workers.iter().any(|worker| {
                worker.lease_owner_changes > 0
                    && worker.stale_completion_rejections > 0
                    && worker.active_fencing_token > 1
            }) && worker_runtime_lease_dto_observed(events)
        }
        "lease_time" => summary.sessions.iter().any(|session| {
            !session.lease_time_ticks.is_empty()
                && session
                    .lease_time_ticks
                    .windows(2)
                    .all(|ticks| ticks[0] <= ticks[1])
        }),
        "provider_turn" => {
            let provider_turns = summary
                .sessions
                .iter()
                .map(|session| session.provider_outputs.len())
                .sum::<usize>();
            summary.session_count > 0 && provider_turns >= summary.session_count * 2
        }
        "provider_event" => events.iter().any(|event| {
            event.kind == BoundaryKind::ProviderEvent
                && event
                    .observed
                    .get("provider_event_release")
                    .and_then(Value::as_bool)
                    == Some(true)
                && event
                    .observed
                    .get("turn_boundary_id")
                    .and_then(Value::as_str)
                    .is_some()
        }),
        "provider_mutation" => events.iter().any(|event| {
            event.kind == BoundaryKind::ProviderMutation
                && event
                    .payload
                    .pointer("/runtime_completion/completion_family")
                    .and_then(Value::as_str)
                    == Some("provider_script_mutation")
                && event
                    .observed
                    .pointer("/provider_parser_matrix/matrix/real_provider_parser_execution")
                    .and_then(Value::as_bool)
                    == Some(true)
        }),
        "tool_result" => {
            summary
                .sessions
                .iter()
                .any(|session| !session.tool_outputs.is_empty())
                && tool_runtime_output_observed(events)
        }
        "max_turn_stop" => events.iter().any(|event| {
            event.kind == BoundaryKind::Trigger
                && event
                    .observed
                    .pointer("/contract_execution/contract")
                    .and_then(Value::as_str)
                    == Some("standard.max_turns_after_tool_result")
                && event
                    .observed
                    .pointer("/contract_execution/source/kind")
                    .and_then(Value::as_str)
                    == Some("fixed_dst_api_execution")
                && event
                    .observed
                    .pointer("/contract_execution/result/turn_outcomes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .any(|outcome| {
                        outcome.get("kind").and_then(Value::as_str) == Some("stopped")
                            && outcome.get("stop_reason").and_then(Value::as_str)
                                == Some("max_turns")
                    })
        }),
        "final_value" => events.iter().any(|event| {
            event.kind == BoundaryKind::Trigger
                && event
                    .observed
                    .pointer("/contract_execution/source/kind")
                    .and_then(Value::as_str)
                    == Some("fixed_dst_api_execution")
                && event
                    .observed
                    .pointer("/contract_execution/result/runtime_final_value_facts/outcome_kind")
                    .and_then(Value::as_str)
                    == Some("final_value")
                && event
                    .observed
                    .pointer(
                        "/contract_execution/result/runtime_final_value_facts/semantic_channel_observed",
                    )
                    .and_then(Value::as_bool)
                    == Some(true)
        }),
        "observer_convergence" => {
            summary.session_count > 0
                && summary.sessions.iter().all(|session| {
                    session.observer_turn_indices.last().copied()
                        == Some(session.provider_outputs.len())
                })
        }
        "runtime_session_graph" => runtime_session_graph_contract(summary).is_passed(),
        "exec_code" => {
            summary
                .sessions
                .iter()
                .any(|session| !session.exec_code_outputs.is_empty())
                && exec_runtime_outcome_observed(events)
        }
        "trigger" => summary
            .sessions
            .iter()
            .any(|session| session.trigger_count > 0),
        "backend_failure" => events.iter().any(|event| {
            event.kind == BoundaryKind::BackendFailure
                && event
                    .observed
                    .get("backend_failure")
                    .and_then(Value::as_bool)
                    == Some(true)
                && event
                    .payload
                    .pointer("/runtime_completion/completion_family")
                    .and_then(Value::as_str)
                    == Some("backend_retry_or_failure")
                && event
                    .observed
                    .pointer("/production_store_error/type")
                    .and_then(Value::as_str)
                    .is_some()
        }),
        "durable_effect" => {
            summary
                .durable_effects
                .iter()
                .any(|effect| effect.execution_count == 1 && effect.replay_count > 0)
                && durable_runtime_effect_observed(events)
        }
        "multi_session" => summary.session_count >= 2,
        "observer_reconnect" => summary
            .sessions
            .iter()
            .any(|session| session.observer_reconnects > 0),
        _ => false,
    }
}

struct ScenarioSemanticVerdict {
    passed: bool,
    reason: String,
}

impl ScenarioSemanticVerdict {
    fn passed(reason: impl Into<String>) -> Self {
        Self {
            passed: true,
            reason: reason.into(),
        }
    }

    fn failed(reason: impl Into<String>) -> Self {
        Self {
            passed: false,
            reason: reason.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ScenarioContractGeneratedFact {
    pub fact: &'static str,
    pub assertion: &'static str,
    pub boundary_ids: Vec<String>,
    pub observed: Value,
}

pub fn scenario_contract_generated_facts(
    contract: &ScenarioContractSpec,
    events: &[DeliveredBoundary],
) -> Result<Vec<ScenarioContractGeneratedFact>, String> {
    scenario_contract_generated_facts_for_semantic(contract.semantic_oracle, events)
}

pub fn scenario_contract_generated_facts_for_semantic(
    semantic_oracle: &str,
    events: &[DeliveredBoundary],
) -> Result<Vec<ScenarioContractGeneratedFact>, String> {
    let facts = match semantic_oracle {
        "standard.initial_request_projection" => Ok(vec![
            standard_protocol_execution_fact(events, "standard.initial_request_projection")?,
            initial_provider_projection_fact(events)?,
        ]),
        "standard.empty_provider_response_error" => Ok(vec![
            standard_protocol_execution_fact(events, "standard.empty_provider_response_error")?,
            provider_mutation_semantic_fact(
                events,
                "dropped_terminal_event",
                "standard_empty_response_terminal_error",
                "empty/unterminated provider output remains an unknown retryable provider error for every migrated parser",
            )?,
        ]),
        "standard.provider_error_without_checkpoint" => Ok(vec![
            standard_protocol_execution_fact(events, "standard.provider_error_without_checkpoint")?,
            provider_mutation_semantic_fact(
                events,
                "rate_limit_error_envelope",
                "standard_provider_error_no_checkpoint",
                "provider error envelope is classified through migrated parsers without depending on a later checkpoint",
            )?,
        ]),
        "standard.native_tool_loop_reenters_model" => Ok(vec![
            standard_protocol_execution_fact(events, "standard.native_tool_loop_reenters_model")?,
            tool_reentry_fact(events, "standard_native_tool_reenters_model", false)?,
        ]),
        "standard.parallel_tool_results_checkpoint_once" => Ok(vec![
            standard_protocol_execution_fact(
                events,
                "standard.parallel_tool_results_checkpoint_once",
            )?,
            parallel_tool_results_checkpoint_once_fact(events)?,
        ]),
        "standard.tool_failure_feedback_reenters_model" => Ok(vec![
            standard_protocol_execution_fact(
                events,
                "standard.tool_failure_feedback_reenters_model",
            )?,
            tool_reentry_fact(events, "standard_tool_feedback_reenters_model", false)?,
            provider_mutation_semantic_fact(
                events,
                "malformed_sse_chunk",
                "standard_tool_failure_feedback_parser_path",
                "tool-failure feedback package also carries generated provider failure parser evidence",
            )?,
        ]),
        "standard.streamed_text_finalizes_once" => Ok(vec![
            standard_protocol_execution_fact(events, "standard.streamed_text_finalizes_once")?,
            streamed_text_finalizes_once_fact(events)?,
        ]),
        "standard.max_turns_after_tool_result" => {
            Ok(vec![standard_max_turns_after_tool_result_fact(events)?])
        }
        "rlm.natural_prose_finalizes" => Ok(vec![rlm_protocol_execution_fact(
            events,
            "rlm.natural_prose_finalizes",
        )?]),
        "rlm.typed_prose_requires_finish" => Ok(vec![rlm_protocol_execution_fact(
            events,
            "rlm.typed_prose_requires_finish",
        )?]),
        "rlm.finish_required_max_turn_stop" => Ok(vec![rlm_protocol_execution_fact(
            events,
            "rlm.finish_required_max_turn_stop",
        )?]),
        "rlm.exec_error_max_turn_stop" => Ok(vec![
            rlm_protocol_execution_fact(events, "rlm.exec_error_max_turn_stop")?,
            exec_semantic_fact(
                events,
                "rlm_exec_error_max_turn_stop",
                ExecFactRequirement::RuntimeOutcome,
            )?,
        ]),
        "rlm.finish_required_diagnostic_counts" => Ok(vec![rlm_protocol_execution_fact(
            events,
            "rlm.finish_required_diagnostic_counts",
        )?]),
        "rlm.natural_diagnostic_counts" => Ok(vec![rlm_protocol_execution_fact(
            events,
            "rlm.natural_diagnostic_counts",
        )?]),
        "rlm.cell_diagnostic_counts" => Ok(vec![
            rlm_protocol_execution_fact(events, "rlm.cell_diagnostic_counts")?,
            exec_semantic_fact(
                events,
                "rlm_cell_diagnostic_exec_counts",
                ExecFactRequirement::RuntimeOutcome,
            )?,
        ]),
        "rlm.retired_marker_plain_lashlang_text" => Ok(vec![
            rlm_protocol_execution_fact(events, "rlm.retired_marker_plain_lashlang_text")?,
            exec_semantic_fact(
                events,
                "rlm_retired_marker_plain_lashlang_text",
                ExecFactRequirement::NoToolCallReplay,
            )?,
        ]),
        "rlm.lashlang_cell_exec_continues" => Ok(vec![
            rlm_protocol_execution_fact(events, "rlm.lashlang_cell_exec_continues")?,
            exec_semantic_fact(
                events,
                "rlm_lashlang_cell_exec_continues",
                ExecFactRequirement::ReentersProvider,
            )?,
        ]),
        "rlm.streamed_lashlang_cell_exec_persists_trajectory" => Ok(vec![
            rlm_protocol_execution_fact(
                events,
                "rlm.streamed_lashlang_cell_exec_persists_trajectory",
            )?,
            exec_semantic_fact(
                events,
                "rlm_streamed_lashlang_cell_exec_persists_trajectory",
                ExecFactRequirement::ReentersProvider,
            )?,
        ]),
        "rlm.empty_options_natural_default" => Ok(vec![rlm_protocol_execution_fact(
            events,
            "rlm.empty_options_natural_default",
        )?]),
        "rlm.exec_result_no_tool_call_replay" => Ok(vec![
            rlm_protocol_execution_fact(events, "rlm.exec_result_no_tool_call_replay")?,
            exec_semantic_fact(
                events,
                "rlm_exec_result_no_tool_call_replay",
                ExecFactRequirement::NoToolCallReplay,
            )?,
        ]),
        "rlm.exec_tool_control_frame_switch_terminal" => Ok(vec![
            rlm_protocol_execution_fact(events, "rlm.exec_tool_control_frame_switch_terminal")?,
            exec_semantic_fact(
                events,
                "rlm_exec_tool_control_frame_switch_terminal",
                ExecFactRequirement::RuntimeOutcome,
            )?,
            trigger_then_provider_fact(events, "rlm_exec_tool_control_frame_switch_trigger")?,
        ]),
        "rlm.exec_tool_control_fail_terminal" => Ok(vec![
            rlm_protocol_execution_fact(events, "rlm.exec_tool_control_fail_terminal")?,
            exec_semantic_fact(
                events,
                "rlm_exec_tool_control_fail_terminal",
                ExecFactRequirement::RuntimeOutcome,
            )?,
            backend_retry_terminalization_fact(events, "rlm_exec_tool_control_fail_backend")?,
        ]),
        "rlm.typed_finish_emits_outcome_and_done" => Ok(vec![rlm_protocol_execution_fact(
            events,
            "rlm.typed_finish_emits_outcome_and_done",
        )?]),
        "rlm.natural_allows_finish_value" => Ok(vec![rlm_protocol_execution_fact(
            events,
            "rlm.natural_allows_finish_value",
        )?]),
        "rlm.typed_schema_mismatch_repair_loop" => Ok(vec![
            provider_mutation_semantic_fact(
                events,
                "malformed_sse_chunk",
                "rlm_typed_schema_mismatch_feedback",
                "typed schema mismatch repair uses generated malformed provider payload feedback",
            )?,
            rlm_protocol_execution_fact(events, "rlm.typed_schema_mismatch_repair_loop")?,
        ]),
        "rlm.typed_schema_any_of_mismatch" => Ok(vec![
            provider_mutation_semantic_fact(
                events,
                "rate_limit_error_envelope",
                "rlm_typed_schema_anyof_feedback",
                "typed anyOf mismatch package carries generated parser-classified feedback",
            )?,
            rlm_protocol_execution_fact(events, "rlm.typed_schema_any_of_mismatch")?,
        ]),
        "agent.foreground_tool_call_round_trip" => Ok(vec![
            agent_contract_execution_fact(events, "agent.foreground_tool_call_round_trip")?,
            tool_reentry_fact(events, "agent_foreground_tool_call_round_trip", false)?,
        ]),
        "agent.started_process_tool_call_graph" => Ok(vec![
            agent_contract_execution_fact(events, "agent.started_process_tool_call_graph")?,
            process_wake_fact(events, "agent_started_process_graph")?,
            tool_reentry_fact(events, "agent_started_process_tool_call", false)?,
        ]),
        "agent.durable_input_suspension_resolution" => Ok(vec![
            agent_contract_execution_fact(events, "agent.durable_input_suspension_resolution")?,
            durable_replay_fact(events, "agent_durable_input_first_and_replay")?,
            process_wake_fact(events, "agent_durable_input_process_wake")?,
            observer_reconnect_fact(events, "agent_durable_input_observer_reconnect")?,
        ]),
        "agent.shell_results_are_data" => Ok(vec![
            agent_contract_execution_fact(events, "agent.shell_results_are_data")?,
            exec_semantic_fact(
                events,
                "agent_shell_exec_result_data",
                ExecFactRequirement::RuntimeOutcome,
            )?,
            tool_reentry_fact(events, "agent_shell_tool_result_data", false)?,
        ]),
        "agent.shell_output_print_projection_survives" => Ok(vec![
            agent_contract_execution_fact(events, "agent.shell_output_print_projection_survives")?,
            exec_semantic_fact(
                events,
                "agent_shell_output_exec_projection",
                ExecFactRequirement::RuntimeOutcome,
            )?,
            agent_shell_output_projection_fact(events)?,
        ]),
        "agent.started_process_subagent_spawn" => Ok(vec![
            agent_contract_execution_fact(events, "agent.started_process_subagent_spawn")?,
            process_wake_fact(events, "agent_started_process_subagent_spawn")?,
        ]),
        "agent.nested_process_start_await" => Ok(vec![
            agent_contract_execution_fact(events, "agent.nested_process_start_await")?,
            process_wake_fact(events, "agent_nested_process_start_await")?,
        ]),
        "agent.session_turn_process_child" => Ok(vec![
            agent_contract_execution_fact(events, "agent.session_turn_process_child")?,
            process_wake_fact(events, "agent_session_turn_process_child_wake")?,
            agent_session_turn_child_provider_fact(events)?,
        ]),
        "agent.failed_child_preserves_failure_graph" => Ok(vec![
            agent_contract_execution_fact(events, "agent.failed_child_preserves_failure_graph")?,
            worker_stale_fact(events, "agent_failed_child_worker_graph")?,
            backend_retry_terminalization_fact(events, "agent_failed_child_backend_graph")?,
        ]),
        "agent.parallel_spawn_and_join" => Ok(vec![
            agent_contract_execution_fact(events, "agent.parallel_spawn_and_join")?,
            process_wake_fact(events, "agent_parallel_spawn_process_wakes")?,
            worker_stale_fact(events, "agent_parallel_spawn_join_worker_order")?,
        ]),
        "agent.tuple_values_finish_as_json_arrays" => Ok(vec![agent_contract_execution_fact(
            events,
            "agent.tuple_values_finish_as_json_arrays",
        )?]),
        other => Err(format!(
            "{} scenario contract `{other}` has no per-contract semantic adapter; add distinct evidence instead of a generic fallback",
            other.split('.').next().unwrap_or("unknown")
        )),
    }?;
    reject_named_contract_proxy_facts(semantic_oracle, &facts)?;
    Ok(facts)
}

/// Per-contract semantic adapter. Each suite gets explicit generated-boundary
/// semantics keyed by `semantic_oracle`; unmapped contracts fail loudly instead
/// of inheriting a suite-level fallback.
fn scenario_contract_semantics(
    contract: &ScenarioContractSpec,
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> ScenarioSemanticVerdict {
    match contract.suite {
        "runtime" => runtime_contract_semantics(contract.semantic_oracle, events, summary),
        "standard" => standard_contract_semantics(contract.semantic_oracle, events, summary),
        "rlm" => rlm_contract_semantics(contract.semantic_oracle, events, summary),
        "agent" => agent_contract_semantics(contract.semantic_oracle, events, summary),
        other => ScenarioSemanticVerdict::failed(format!(
            "suite `{other}` has no per-contract semantic adapter dispatcher"
        )),
    }
}

fn runtime_contract_semantics(
    semantic_oracle: &str,
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> ScenarioSemanticVerdict {
    // Each runtime contract owns a DISTINCT semantic adapter keyed on the
    // specific runtime boundary it governs — no `|`-grouped shared arms and no
    // single global condition standing in for per-contract evidence.
    match semantic_oracle {
        // Advisory lease state does not authorize a stale durable completion.
        "runtime.advisory_lease_head_cas" => assert_semantic(
            worker_runtime_lease_dto_observed(events)
                && summary
                    .workers
                    .iter()
                    .any(|worker| worker.stale_completion_rejections > 0),
            "the stale worker completion was rejected by durable commit fencing",
        ),
        // A new incarnation waits for TTL expiry and the fence strictly advances.
        "runtime.stale_lease_ttl" => assert_semantic(
            worker_runtime_lease_dto_observed(events)
                && summary.workers.iter().any(|worker| {
                    worker.lease_owner_changes > 0 && worker.active_fencing_token > 1
                }),
            "a second incarnation acquired the stale lease after TTL at a strictly higher fence",
        ),
        "runtime.checkpoint_redrive_cancel" => assert_semantic(
            queued_inputs_have_cancel_targets(events) && observer_convergence(summary).is_passed(),
            "queued input cancellation targeted a source key and observers converged",
        ),
        // The queued (next-turn) input stays pending/hidden while the live turn runs.
        "runtime.queued_work_keeps_pending_input" => assert_semantic(
            queued_ingress_has_source_keys(events) && provider_turns_after_queue(summary),
            "queued ingress kept a stable source key while live provider turns continued",
        ),
        // A queued turn input is eventually completed into a subsequent turn.
        "runtime.queued_turn_input_completion" => assert_semantic(
            queued_ingress_has_source_keys(events)
                && summary.sessions.iter().any(|session| {
                    session.queued_ingress_count > 0 && session.provider_outputs.len() >= 2
                }),
            "a queued turn input source key was followed by a completed subsequent turn",
        ),
        // A command-only queue drains against monotonic lease fencing tokens.
        "runtime.command_only_queue_drain" => assert_semantic(
            queued_ingress_has_source_keys(events) && lease_time_monotonic(events).is_passed(),
            "command queue source keys drained against monotonically advancing lease fences",
        ),
        // A command applied before turn work still lets later provider turns run.
        "runtime.command_before_turn_work" => assert_semantic(
            queued_ingress_has_source_keys(events)
                && provider_turns_after_queue(summary)
                && lease_time_monotonic(events).is_passed(),
            "a command queued before turn work preserved later turns and lease ordering",
        ),
        "runtime.observation_replay_preserves_input" => assert_semantic(
            observer_reconnect_has_matching_turn(events, summary),
            "observer reconnect replay converged to the final provider turn",
        ),
        other => unmapped_scenario_semantic("runtime", other),
    }
}

fn standard_contract_semantics(
    semantic_oracle: &str,
    events: &[DeliveredBoundary],
    _summary: &AbstractWorldSummary,
) -> ScenarioSemanticVerdict {
    match scenario_contract_generated_facts_for_semantic(semantic_oracle, events) {
        Ok(facts) => ScenarioSemanticVerdict::passed(format!(
            "generated contract facts held: {}",
            fact_names(&facts)
        )),
        Err(reason) => ScenarioSemanticVerdict::failed(reason),
    }
}

fn rlm_contract_semantics(
    semantic_oracle: &str,
    events: &[DeliveredBoundary],
    _summary: &AbstractWorldSummary,
) -> ScenarioSemanticVerdict {
    match scenario_contract_generated_facts_for_semantic(semantic_oracle, events) {
        Ok(facts) => ScenarioSemanticVerdict::passed(format!(
            "generated contract facts held: {}",
            fact_names(&facts)
        )),
        Err(reason) => ScenarioSemanticVerdict::failed(reason),
    }
}

fn agent_contract_semantics(
    semantic_oracle: &str,
    events: &[DeliveredBoundary],
    _summary: &AbstractWorldSummary,
) -> ScenarioSemanticVerdict {
    match scenario_contract_generated_facts_for_semantic(semantic_oracle, events) {
        Ok(facts) => ScenarioSemanticVerdict::passed(format!(
            "generated contract facts held: {}",
            fact_names(&facts)
        )),
        Err(reason) => ScenarioSemanticVerdict::failed(reason),
    }
}

fn fact_names(facts: &[ScenarioContractGeneratedFact]) -> String {
    facts
        .iter()
        .map(|fact| fact.fact)
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Clone, Copy)]
enum ExecFactRequirement {
    RuntimeOutcome,
    NoToolCallReplay,
    ReentersProvider,
}

fn generated_fact(
    fact: &'static str,
    assertion: &'static str,
    events: Vec<&DeliveredBoundary>,
    observed: Value,
) -> Result<ScenarioContractGeneratedFact, String> {
    if events.is_empty() {
        return Err(format!(
            "generated semantic fact `{fact}` had no backing boundary events"
        ));
    }
    Ok(ScenarioContractGeneratedFact {
        fact,
        assertion,
        boundary_ids: events
            .iter()
            .map(|event| event.boundary_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        observed,
    })
}

fn successful_provider_events(events: &[DeliveredBoundary]) -> Vec<&DeliveredBoundary> {
    events
        .iter()
        .filter(|event| {
            event.kind == BoundaryKind::Provider
                && event.observed.get("success").and_then(Value::as_bool) == Some(true)
                && event
                    .observed
                    .pointer("/runtime_contract/status")
                    .and_then(Value::as_str)
                    == Some("passed")
                && event
                    .payload
                    .get("expected_provider_exchange_count")
                    .and_then(Value::as_u64)
                    == event
                        .observed
                        .get("provider_exchange_count")
                        .and_then(Value::as_u64)
        })
        .collect()
}

fn initial_provider_projection_fact(
    events: &[DeliveredBoundary],
) -> Result<ScenarioContractGeneratedFact, String> {
    let Some(provider) = successful_provider_events(events)
        .into_iter()
        .find(|event| {
            event
                .payload
                .get("expected_provider_exchange_count")
                .and_then(Value::as_u64)
                == Some(1)
                && event.payload.get("text").and_then(Value::as_str).is_some()
        })
    else {
        return Err(
            "standard initial request projection did not find a successful exchange-1 provider boundary"
                .to_string(),
        );
    };
    let expected_text = provider
        .payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("");
    let output = provider
        .observed
        .get("provider_output")
        .and_then(Value::as_str)
        .unwrap_or("");
    if expected_text.is_empty() || expected_text != output {
        return Err(format!(
            "standard initial request projection expected provider output `{expected_text}`, got `{output}`"
        ));
    }
    generated_fact(
        "standard_initial_request_projection",
        "first generated provider boundary preserves projected request text and exchange index 1",
        vec![provider],
        json!({
            "provider_boundary": provider.boundary_id,
            "actor": provider.actor_alias,
            "expected_provider_exchange_count": 1,
            "provider_output": output,
            "projected_text": expected_text,
        }),
    )
}

fn provider_mutation_semantic_fact(
    events: &[DeliveredBoundary],
    mutation: &'static str,
    fact: &'static str,
    assertion: &'static str,
) -> Result<ScenarioContractGeneratedFact, String> {
    let Some(event) = events.iter().find(|event| {
        event.kind == BoundaryKind::ProviderMutation
            && event
                .observed
                .get("mutation")
                .or_else(|| event.payload.get("mutation"))
                .and_then(Value::as_str)
                == Some(mutation)
            && event
                .payload
                .pointer("/runtime_completion/completion_family")
                .and_then(Value::as_str)
                == Some("provider_script_mutation")
            && event
                .observed
                .pointer("/provider_parser_matrix/matrix/real_provider_parser_execution")
                .and_then(Value::as_bool)
                == Some(true)
    }) else {
        return Err(format!(
            "provider mutation semantic fact `{fact}` did not find `{mutation}` parser evidence"
        ));
    };
    let providers = provider_mutation_providers(event);
    if !MIGRATED_RUNTIME_PROVIDER_KINDS
        .iter()
        .filter(|provider| mutation != "rate_limit_error_envelope" || **provider != "google_oauth")
        .all(|provider| providers.contains(provider))
    {
        return Err(format!(
            "provider mutation `{mutation}` did not cover every migrated provider parser: {:?}",
            providers
        ));
    }
    if mutation == "rate_limit_error_envelope"
        && !provider_rate_limit_terminalized_by_scripted_parsers(std::slice::from_ref(event))
    {
        return Err(
            "rate-limit provider mutation lacked 429 retryable terminal classification proofs"
                .to_string(),
        );
    }
    if mutation == "dropped_terminal_event"
        && !provider_dropped_terminal_event_fails_open(std::slice::from_ref(event))
    {
        return Err(
            "dropped-terminal provider mutation lacked unknown retryable parser classification proofs"
                .to_string(),
        );
    }
    generated_fact(
        fact,
        assertion,
        vec![event],
        json!({
            "provider_mutation_boundary": event.boundary_id,
            "mutation": mutation,
            "provider_kinds": providers,
            "runtime_completion_family": "provider_script_mutation",
            "real_provider_parser_execution": true,
        }),
    )
}

fn provider_mutation_providers(event: &DeliveredBoundary) -> BTreeSet<&str> {
    event
        .observed
        .pointer("/provider_parser_matrix/matrix/provider_kinds")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

fn tool_reentry_fact(
    events: &[DeliveredBoundary],
    fact: &'static str,
    require_provider_event_release: bool,
) -> Result<ScenarioContractGeneratedFact, String> {
    let Some((tool, provider)) = tool_then_same_actor_provider(events) else {
        return Err(format!(
            "tool semantic fact `{fact}` did not find a scheduler-owned tool result followed by a successful provider re-entry for the same actor"
        ));
    };
    let provider_release = provider_event_for_turn(events, &provider.boundary_id);
    if require_provider_event_release && provider_release.is_none() {
        return Err(format!(
            "tool semantic fact `{fact}` did not find scheduler-owned provider-event release evidence for re-entry `{}`",
            provider.boundary_id
        ));
    }
    let mut fact_events = vec![tool, provider];
    if let Some(release) = provider_release {
        fact_events.push(release);
    }
    generated_fact(
        fact,
        if require_provider_event_release {
            "tool result executes once, checkpoints through scheduler-owned provider-event release, and re-enters the same actor"
        } else {
            "tool result executes once and re-enters the same actor through a later successful provider boundary"
        },
        fact_events,
        json!({
            "tool_boundary": tool.boundary_id,
            "tool_name": tool.observed.get("tool_name").cloned().unwrap_or(Value::Null),
            "tool_call_id": tool.observed.get("tool_call_id").cloned().unwrap_or(Value::Null),
            "tool_sequence": tool.sequence,
            "reentry_provider_boundary": provider.boundary_id,
            "reentry_provider_sequence": provider.sequence,
            "actor": tool.actor_alias,
            "execution_count": 1,
            "provider_event_release": provider_release.map(|event| event.boundary_id.as_str()),
        }),
    )
}

fn tool_then_same_actor_provider(
    events: &[DeliveredBoundary],
) -> Option<(&DeliveredBoundary, &DeliveredBoundary)> {
    events
        .iter()
        .filter(|event| {
            event.kind == BoundaryKind::Tool
                && event.observed.get("runtime_tool_output").is_some()
                && event
                    .observed
                    .get("execution_count")
                    .and_then(Value::as_u64)
                    == Some(1)
                && event
                    .payload
                    .pointer("/runtime_completion/completion_family")
                    .and_then(Value::as_str)
                    == Some("tool_return")
        })
        .find_map(|tool| {
            successful_provider_events(events)
                .into_iter()
                .filter(|provider| {
                    provider.actor_alias == tool.actor_alias && provider.sequence > tool.sequence
                })
                .min_by_key(|provider| provider.sequence)
                .map(|provider| (tool, provider))
        })
}

fn provider_event_for_turn<'a>(
    events: &'a [DeliveredBoundary],
    turn_boundary_id: &str,
) -> Option<&'a DeliveredBoundary> {
    provider_events_for_turn(events, turn_boundary_id)
        .into_iter()
        .next()
}

fn provider_events_for_turn<'a>(
    events: &'a [DeliveredBoundary],
    turn_boundary_id: &str,
) -> Vec<&'a DeliveredBoundary> {
    events
        .iter()
        .filter(|event| {
            event.kind == BoundaryKind::ProviderEvent
                && event
                    .payload
                    .get("turn_boundary_id")
                    .and_then(Value::as_str)
                    == Some(turn_boundary_id)
                && event
                    .observed
                    .get("provider_event_release")
                    .and_then(Value::as_bool)
                    == Some(true)
        })
        .collect()
}

fn contract_execution_event<'a>(
    events: &'a [DeliveredBoundary],
    contract: &str,
) -> Result<&'a DeliveredBoundary, String> {
    events
        .iter()
        .find(|event| {
            event.kind == BoundaryKind::Trigger
                && event
                    .observed
                    .pointer("/contract_execution/contract")
                    .and_then(Value::as_str)
                    == Some(contract)
        })
        .ok_or_else(|| {
            format!("contract execution `{contract}` was not present in generated events")
        })
}

fn contract_execution_payload_matches_observed<'a>(
    event: &'a DeliveredBoundary,
    contract: &str,
    expected_source_scenario: &str,
) -> Result<&'a Value, String> {
    let observed = event.observed.get("contract_execution").ok_or_else(|| {
        format!(
            "contract execution boundary `{}` did not record observed execution payload",
            event.boundary_id
        )
    })?;
    if event.payload.get("contract_execution") != Some(observed) {
        return Err(format!(
            "contract execution boundary `{}` observed execution diverged from scheduler payload",
            event.boundary_id
        ));
    }
    if observed.get("contract").and_then(Value::as_str) != Some(contract) {
        return Err(format!(
            "contract execution boundary `{}` recorded wrong contract identity",
            event.boundary_id
        ));
    }
    if observed.pointer("/source/kind").and_then(Value::as_str) != Some("fixed_dst_api_execution")
        || observed
            .pointer("/source/path")
            .and_then(Value::as_str)
            .is_none()
        || observed.pointer("/source/scenario").and_then(Value::as_str)
            != Some(expected_source_scenario)
        || !is_sha256_hex_value(observed.pointer("/source/source_hash"))
        || !is_sha256_hex_value(observed.pointer("/source/result_sha256"))
    {
        return Err(format!(
            "contract execution boundary `{}` lacks fixed execution source path/scenario/hash evidence",
            event.boundary_id
        ));
    }
    if event.observed.get("semantic_proof").is_some()
        || event.payload.get("semantic_proof").is_some()
    {
        return Err(format!(
            "contract execution boundary `{}` used semantic-proof-only trigger evidence",
            event.boundary_id
        ));
    }
    contract_execution_replay_matches(observed, contract).map_err(|reason| {
        format!(
            "contract execution boundary `{}` failed fixed-source replay validation: {reason}",
            event.boundary_id
        )
    })?;
    Ok(observed)
}

fn contract_execution_replay_matches(observed: &Value, contract: &str) -> Result<(), String> {
    let replayed = crate::runner::replay_contract_execution(contract)
        .map_err(|err| format!("could not re-execute `{contract}`: {err}"))?;
    if replayed.get("source") != observed.get("source") {
        return Err(format!(
            "`{contract}` source identity/hash diverged from re-execution"
        ));
    }
    if replayed.get("result") != observed.get("result") {
        return Err(format!(
            "`{contract}` result payload diverged from re-execution"
        ));
    }
    Ok(())
}

fn is_sha256_hex_value(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|value| {
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn event_by_boundary_id<'a>(
    events: &'a [DeliveredBoundary],
    boundary_id: &str,
) -> Option<&'a DeliveredBoundary> {
    events.iter().find(|event| event.boundary_id == boundary_id)
}

fn json_array_equals(value: Option<&Value>, expected: &[&str]) -> bool {
    let Some(values) = value.and_then(Value::as_array) else {
        return false;
    };
    values.len() == expected.len()
        && values
            .iter()
            .zip(expected)
            .all(|(value, expected)| value.as_str() == Some(*expected))
}

fn standard_protocol_execution_fact(
    events: &[DeliveredBoundary],
    contract: &'static str,
) -> Result<ScenarioContractGeneratedFact, String> {
    let (scenario, fact, assertion) = standard_protocol_contract_metadata(contract)?;
    let proof_event = contract_execution_event(events, contract)?;
    let execution = contract_execution_payload_matches_observed(proof_event, contract, scenario)?;
    let result = execution
        .get("result")
        .ok_or_else(|| format!("{contract} execution missing result"))?;
    require_standard_str(
        result,
        "/execution_api",
        "lash_core::sansio::TurnMachine",
        contract,
    )?;
    require_standard_str(
        result,
        "/driver",
        "lash_protocol_standard::StandardDriver",
        contract,
    )?;
    require_standard_bool(
        result,
        "/initial_request_contains_user_message",
        true,
        contract,
    )?;
    let contract_observed = match contract {
        "standard.initial_request_projection" => {
            require_standard_bool(result, "/done", false, contract)?;
            require_standard_u64(result, "/llm_call_count", 1, contract)?;
            json!({
                "initial_request_contains_user_message": true,
                "llm_call_count": 1,
                "done": false,
            })
        }
        "standard.empty_provider_response_error" => {
            require_standard_bool(result, "/done", true, contract)?;
            require_standard_u64(result, "/llm_call_count", 1, contract)?;
            require_standard_error_contains(
                result,
                "Model returned no assistant text or tool calls.",
                contract,
            )?;
            require_standard_stopped_outcome(result, "ProviderError", contract)?;
            json!({
                "done": true,
                "stop_reason": "provider_error",
                "error": "empty_response",
                "llm_call_count": 1,
            })
        }
        "standard.provider_error_without_checkpoint" => {
            require_standard_bool(result, "/done", true, contract)?;
            require_standard_u64(result, "/llm_call_count", 1, contract)?;
            require_standard_checkpoint_count(result, 0, contract)?;
            require_standard_error_contains(
                result,
                "LLM error: upstream provider unavailable",
                contract,
            )?;
            require_standard_stopped_outcome(result, "ProviderError", contract)?;
            json!({
                "done": true,
                "stop_reason": "provider_error",
                "checkpoint_count": 0,
                "llm_call_count": 1,
            })
        }
        "standard.native_tool_loop_reenters_model" => {
            require_standard_bool(result, "/done", false, contract)?;
            require_standard_u64(result, "/llm_call_count", 2, contract)?;
            require_standard_checkpoint(result, "after_work", contract)?;
            require_standard_tool_call(result, "tc1", "read_file", contract)?;
            json!({
                "done": false,
                "llm_call_count": 2,
                "checkpoint": "after_work",
                "tool_call": "read_file/tc1",
            })
        }
        "standard.parallel_tool_results_checkpoint_once" => {
            require_standard_bool(result, "/done", false, contract)?;
            require_standard_u64(result, "/llm_call_count", 2, contract)?;
            require_standard_checkpoint_count(result, 1, contract)?;
            require_standard_checkpoint(result, "after_work", contract)?;
            require_standard_tool_call(result, "tc1", "read_file", contract)?;
            require_standard_tool_call(result, "tc2", "read_file", contract)?;
            json!({
                "done": false,
                "llm_call_count": 2,
                "checkpoint_count": 1,
                "tool_calls": ["tc1", "tc2"],
            })
        }
        "standard.tool_failure_feedback_reenters_model" => {
            require_standard_bool(result, "/done", false, contract)?;
            require_standard_u64(result, "/llm_call_count", 2, contract)?;
            require_standard_checkpoint(result, "after_work", contract)?;
            require_standard_tool_call(result, "tc1", "search", contract)?;
            require_standard_tool_result(
                result,
                "tc1",
                "failure",
                Some("search_failed"),
                contract,
            )?;
            json!({
                "done": false,
                "llm_call_count": 2,
                "checkpoint": "after_work",
                "tool_result": "failure/search_failed",
            })
        }
        "standard.streamed_text_finalizes_once" => {
            require_standard_bool(result, "/done", true, contract)?;
            require_standard_u64(result, "/llm_call_count", 1, contract)?;
            require_standard_u64(result, "/text_delta_count", 0, contract)?;
            require_standard_checkpoint(result, "before_completion", contract)?;
            require_standard_finished_outcome_contains(result, "AssistantMessage", contract)?;
            require_standard_finished_outcome_contains(result, "streamed done", contract)?;
            json!({
                "done": true,
                "llm_call_count": 1,
                "text_delta_count": 0,
                "checkpoint": "before_completion",
                "turn_outcome": "assistant_message",
            })
        }
        other => {
            return Err(format!(
                "Standard protocol contract execution fact has no checker for `{other}`"
            ));
        }
    };
    generated_fact(
        fact,
        assertion,
        vec![proof_event],
        json!({
            "contract_execution_boundary": proof_event.boundary_id,
            "contract": contract,
            "observed": contract_observed,
            "source": execution.get("source").cloned().unwrap_or(Value::Null),
        }),
    )
}

fn standard_protocol_contract_metadata(
    contract: &str,
) -> Result<(&'static str, &'static str, &'static str), String> {
    match contract {
        "standard.initial_request_projection" => Ok((
            "standard_protocol_scenario_projects_initial_request",
            "standard_initial_request_projection_execution",
            "StandardDriver projects the user input into the first TurnMachine LLM request",
        )),
        "standard.empty_provider_response_error" => Ok((
            "standard_protocol_scenario_empty_model_response_stops_provider_error",
            "standard_empty_provider_response_error_execution",
            "StandardDriver turns an empty model response into a provider-error stop with no generic success proxy",
        )),
        "standard.provider_error_without_checkpoint" => Ok((
            "standard_protocol_scenario_provider_error_stops_without_checkpoint",
            "standard_provider_error_without_checkpoint_execution",
            "StandardDriver provider error stops immediately without committing a checkpoint",
        )),
        "standard.native_tool_loop_reenters_model" => Ok((
            "standard_protocol_scenario_native_tool_loop_reenters_model_after_checkpoint",
            "standard_native_tool_loop_reenters_model_execution",
            "StandardDriver native tool results checkpoint after work and re-enter the model loop",
        )),
        "standard.parallel_tool_results_checkpoint_once" => Ok((
            "standard_protocol_scenario_parallel_tool_results_checkpoint_once",
            "standard_parallel_tool_results_checkpoint_once_execution",
            "StandardDriver parallel tool results commit exactly one AfterWork checkpoint before model re-entry",
        )),
        "standard.tool_failure_feedback_reenters_model" => Ok((
            "standard_protocol_scenario_tool_failure_feedback_reenters_model_after_checkpoint",
            "standard_tool_failure_feedback_reenters_model_execution",
            "StandardDriver converts tool failure into model feedback, checkpoints, and re-enters",
        )),
        "standard.streamed_text_finalizes_once" => Ok((
            "standard_protocol_scenario_streamed_text_finishes_without_duplicate_delta",
            "standard_streamed_text_finalizes_once_execution",
            "StandardDriver streamed assistant text finalizes once without duplicate text deltas",
        )),
        other => Err(format!(
            "no Standard protocol metadata registered for `{other}`"
        )),
    }
}

fn require_standard_bool(
    result: &Value,
    pointer: &str,
    expected: bool,
    contract: &str,
) -> Result<(), String> {
    if result.pointer(pointer).and_then(Value::as_bool) == Some(expected) {
        Ok(())
    } else {
        Err(format!("{contract} expected {pointer}={expected}"))
    }
}

fn require_standard_u64(
    result: &Value,
    pointer: &str,
    expected: u64,
    contract: &str,
) -> Result<(), String> {
    if result.pointer(pointer).and_then(Value::as_u64) == Some(expected) {
        Ok(())
    } else {
        Err(format!("{contract} expected {pointer}={expected}"))
    }
}

fn require_standard_str(
    result: &Value,
    pointer: &str,
    expected: &str,
    contract: &str,
) -> Result<(), String> {
    if result.pointer(pointer).and_then(Value::as_str) == Some(expected) {
        Ok(())
    } else {
        Err(format!("{contract} expected {pointer}=`{expected}`"))
    }
}

fn require_standard_checkpoint(
    result: &Value,
    checkpoint: &str,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("checkpoints")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|value| value.as_str() == Some(checkpoint))
    {
        Ok(())
    } else {
        Err(format!("{contract} missing checkpoint `{checkpoint}`"))
    }
}

fn require_standard_checkpoint_count(
    result: &Value,
    expected: usize,
    contract: &str,
) -> Result<(), String> {
    let actual = result
        .get("checkpoints")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "{contract} expected {expected} checkpoint(s), found {actual}"
        ))
    }
}

fn require_standard_error_contains(
    result: &Value,
    needle: &str,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("errors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|error| error.contains(needle))
    {
        Ok(())
    } else {
        Err(format!("{contract} missing error containing `{needle}`"))
    }
}

fn require_standard_stopped_outcome(
    result: &Value,
    stop_reason_needle: &str,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("turn_outcomes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|outcome| {
            outcome.get("kind").and_then(Value::as_str) == Some("stopped")
                && outcome
                    .get("stop_reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| reason.contains(stop_reason_needle))
        })
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} missing stopped outcome containing `{stop_reason_needle}`"
        ))
    }
}

fn require_standard_finished_outcome_contains(
    result: &Value,
    needle: &str,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("turn_outcomes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|outcome| {
            outcome.get("kind").and_then(Value::as_str) == Some("finished")
                && outcome
                    .get("finish")
                    .and_then(Value::as_str)
                    .is_some_and(|finish| finish.contains(needle))
        })
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} missing finished outcome containing `{needle}`"
        ))
    }
}

fn require_standard_tool_call(
    result: &Value,
    call_id: &str,
    tool_name: &str,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|call| {
            call.get("call_id").and_then(Value::as_str) == Some(call_id)
                && call.get("tool_name").and_then(Value::as_str) == Some(tool_name)
        })
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} missing tool call `{tool_name}/{call_id}`"
        ))
    }
}

fn require_standard_tool_result(
    result: &Value,
    call_id: &str,
    status: &str,
    error_code: Option<&str>,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("tool_results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|tool_result| {
            tool_result.get("call_id").and_then(Value::as_str) == Some(call_id)
                && tool_result.get("status").and_then(Value::as_str) == Some(status)
                && error_code.is_none_or(|code| {
                    tool_result.get("error_code").and_then(Value::as_str) == Some(code)
                })
        })
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} missing tool result `{call_id}` status `{status}`"
        ))
    }
}

fn agent_contract_execution_fact(
    events: &[DeliveredBoundary],
    contract: &'static str,
) -> Result<ScenarioContractGeneratedFact, String> {
    let (scenario, fact, assertion) = agent_contract_metadata(contract)?;
    let proof_event = contract_execution_event(events, contract)?;
    let execution = contract_execution_payload_matches_observed(proof_event, contract, scenario)?;
    let result = execution
        .get("result")
        .ok_or_else(|| format!("{contract} execution missing result"))?;
    require_agent_bool(result, "/done", true, contract)?;
    require_agent_str(result, "/execution_api", "lash::LashCore facade", contract)?;
    let observed = match contract {
        "agent.foreground_tool_call_round_trip" => {
            require_agent_final_value(result, &json!({ "ok": true }), contract)?;
            require_agent_u64(result, "/tool_completed_count", 1, contract)?;
            require_agent_tool_output(result, "app_lookup", &json!({ "ok": true }), contract)?;
            json!({
                "final_value": { "ok": true },
                "tool_completed_count": 1,
                "tool_name": "app_lookup",
            })
        }
        "agent.started_process_tool_call_graph" => {
            require_agent_final_value(result, &json!({ "ok": true }), contract)?;
            require_agent_completed_process_entry(result, "lookup", contract)?;
            require_agent_completed_labeled_resource(
                result,
                "Lookup app state in process",
                contract,
            )?;
            json!({
                "final_value": { "ok": true },
                "completed_process": "lookup",
                "labeled_resource": "Lookup app state in process",
            })
        }
        "agent.durable_input_suspension_resolution" => {
            require_agent_final_value(result, &json!("approved"), contract)?;
            require_agent_bool(
                result,
                "/durable_input/suspended_before_resolution",
                true,
                contract,
            )?;
            require_agent_bool(result, "/durable_input/resolve_accepted", true, contract)?;
            require_agent_u64(
                result,
                "/durable_input/completed_event_count_before_resolution",
                0,
                contract,
            )?;
            require_agent_u64(result, "/durable_input/atomic_attempt_count", 1, contract)?;
            require_agent_bool(
                result,
                "/durable_input/await_tool_call_id_present",
                true,
                contract,
            )?;
            require_agent_completed_process_entry(result, "request_answer", contract)?;
            require_agent_process_event(
                result,
                "process.yield",
                "/payload/type",
                "work.input_request.opened",
                contract,
            )?;
            require_agent_no_process_event(result, "process.waiting", contract)?;
            json!({
                "final_value": "approved",
                "await_tool_call_id_present": true,
                "suspended_before_resolution": true,
                "completed_process": "request_answer",
                "process_event": "work.input_request.opened",
            })
        }
        "agent.shell_results_are_data" => {
            let expected = json!({
                "pipe_exit": 0,
                "pipe_output": "line\nline\nline\n",
                "missing_exit": 1,
                "missing_status": "completed"
            });
            require_agent_final_value(result, &expected, contract)?;
            json!({
                "final_value": expected,
                "nonzero_shell_exit_is_data": true,
                "pipeline_output": "line\nline\nline\n",
            })
        }
        "agent.shell_output_print_projection_survives" => {
            let expected = json!({
                "chars": 60000,
                "tail": "x\nx\n",
                "has_full_output_path": true
            });
            require_agent_final_value(result, &expected, contract)?;
            json!({
                "final_value": expected,
                "shell_output_chars": 60000,
                "projection_tail": "x\nx\n",
            })
        }
        "agent.started_process_subagent_spawn" => {
            require_agent_final_value(result, &json!({ "len": 2 }), contract)?;
            require_agent_completed_process_entry(result, "spawn_child", contract)?;
            require_agent_completed_labeled_resource(
                result,
                "Spawn subagent with web search",
                contract,
            )?;
            require_agent_min_u64(
                result,
                "/graph_facts/child_session_exec_completed_count",
                1,
                contract,
            )?;
            json!({
                "final_value": { "len": 2 },
                "completed_process": "spawn_child",
                "labeled_resource": "Spawn subagent with web search",
                "child_session_exec_completed_count": result.pointer("/graph_facts/child_session_exec_completed_count").cloned().unwrap_or(Value::Null),
            })
        }
        "agent.session_turn_process_child" => {
            require_agent_final_value(result, &json!({ "child": "done" }), contract)?;
            json!({
                "final_value": { "child": "done" },
                "process_child_awaited": true,
            })
        }
        "agent.nested_process_start_await" => {
            require_agent_final_value(result, &json!({ "parent": "done" }), contract)?;
            require_agent_completed_process_entry(result, "parent", contract)?;
            require_agent_completed_process_entry(result, "child", contract)?;
            require_agent_completed_labeled_node(result, "Start nested child process", contract)?;
            require_agent_min_u64(result, "/process_facts/process_count", 2, contract)?;
            require_agent_min_u64(
                result,
                "/process_facts/completed_lashlang_process_count",
                2,
                contract,
            )?;
            json!({
                "final_value": { "parent": "done" },
                "completed_processes": ["child", "parent"],
                "labeled_node": "Start nested child process",
            })
        }
        "agent.failed_child_preserves_failure_graph" => {
            require_agent_no_final_value(result, contract)?;
            require_agent_bool(result, "/process_facts/all_terminal", true, contract)?;
            require_agent_failed_labeled_resource(result, "Spawn failing subagent", contract)?;
            require_agent_min_u64(
                result,
                "/graph_facts/child_session_exec_completed_count",
                1,
                contract,
            )?;
            require_agent_bool(result, "/failure/turn_success", false, contract)?;
            require_agent_bool(result, "/failure/final_value_present", false, contract)?;
            require_agent_u64(result, "/failure/final_value_event_count", 0, contract)?;
            require_agent_min_u64(result, "/failure/failed_code_block_count", 1, contract)?;
            require_agent_bool(
                result,
                "/failure/provider_exhaustion_observed",
                false,
                contract,
            )?;
            require_agent_bool(
                result,
                "/failure/child_task_fail_reason_observed",
                true,
                contract,
            )?;
            json!({
                "final_value_present": false,
                "failed_labeled_resource": "Spawn failing subagent",
                "child_task_fail_reason": "child boom",
                "child_session_exec_completed_count": result.pointer("/graph_facts/child_session_exec_completed_count").cloned().unwrap_or(Value::Null),
                "all_processes_terminal": true,
            })
        }
        "agent.parallel_spawn_and_join" => {
            let expected = json!({ "joined": ["left", "right"] });
            require_agent_final_value(result, &expected, contract)?;
            json!({
                "final_value": expected,
                "joined": ["left", "right"],
            })
        }
        "agent.tuple_values_finish_as_json_arrays" => {
            let expected = json!({
                "first": "left",
                "tail": ["right"],
                "seen": ["left", "right"],
                "tuple": ["left", "right"],
                "nested": { "pair": ["left", "right"] }
            });
            require_agent_final_value(result, &expected, contract)?;
            let final_value = result
                .get("final_value")
                .ok_or_else(|| format!("{contract} missing concrete final value"))?;
            if !(json_array_equals(final_value.pointer("/tuple"), &["left", "right"])
                && json_array_equals(final_value.pointer("/tail"), &["right"])
                && json_array_equals(final_value.pointer("/seen"), &["left", "right"])
                && json_array_equals(final_value.pointer("/nested/pair"), &["left", "right"]))
            {
                return Err(format!(
                    "{contract} did not preserve tuple/tail/seen/nested tuple values as JSON arrays"
                ));
            }
            json!({
                "final_value": expected,
                "tuple": final_value.pointer("/tuple").cloned().unwrap_or(Value::Null),
                "tail": final_value.pointer("/tail").cloned().unwrap_or(Value::Null),
                "seen": final_value.pointer("/seen").cloned().unwrap_or(Value::Null),
                "nested_pair": final_value.pointer("/nested/pair").cloned().unwrap_or(Value::Null),
            })
        }
        other => {
            return Err(format!(
                "Agent contract execution fact has no checker for `{other}`"
            ));
        }
    };
    generated_fact(
        fact,
        assertion,
        vec![proof_event],
        json!({
            "contract_execution_boundary": proof_event.boundary_id,
            "contract": contract,
            "observed": observed,
            "source": execution.get("source").cloned().unwrap_or(Value::Null),
        }),
    )
}

fn agent_contract_metadata(
    contract: &str,
) -> Result<(&'static str, &'static str, &'static str), String> {
    match contract {
        "agent.foreground_tool_call_round_trip" => Ok((
            "agent_scenario_foreground_labeled_tool_call",
            "agent_foreground_tool_call_round_trip_execution",
            "Agent facade executes app_lookup and returns its concrete tool value as the final value",
        )),
        "agent.started_process_tool_call_graph" => Ok((
            "agent_scenario_started_process_labeled_tool_call",
            "agent_started_process_tool_call_graph_execution",
            "Agent facade starts a Lashlang process that executes app_lookup and records a completed labeled process graph",
        )),
        "agent.durable_input_suspension_resolution" => Ok((
            "agent_scenario_process_durable_input_request_tool",
            "agent_durable_input_suspension_resolution_execution",
            "Agent facade suspends a durable input process before external resolution and resumes to a concrete final value",
        )),
        "agent.shell_results_are_data" => Ok((
            "agent_scenario_shell_nonzero_and_pipeline_results_are_data",
            "agent_shell_results_are_data_execution",
            "Agent facade preserves shell pipeline output and nonzero shell status as final-value data",
        )),
        "agent.shell_output_print_projection_survives" => Ok((
            "agent_scenario_shell_output_survives_print_projection_in_variable",
            "agent_shell_output_print_projection_execution",
            "Agent facade keeps large shell output addressable after print projection and finishes retained metadata",
        )),
        "agent.started_process_subagent_spawn" => Ok((
            "agent_scenario_started_process_labeled_subagent_spawn",
            "agent_started_process_subagent_spawn_execution",
            "Agent facade starts a Lashlang process that spawns a default subagent, preserves the labeled child-session graph, and returns the typed child value",
        )),
        "agent.session_turn_process_child" => Ok((
            "agent_scenario_session_turn_process_child",
            "agent_session_turn_process_child_execution",
            "Agent facade starts and awaits a child process to produce a concrete final value",
        )),
        "agent.nested_process_start_await" => Ok((
            "agent_scenario_nested_process_start_await",
            "agent_nested_process_start_await_execution",
            "Agent facade starts a parent Lashlang process that starts and awaits a child process with connected graph evidence",
        )),
        "agent.failed_child_preserves_failure_graph" => Ok((
            "agent_scenario_failed_child_preserves_failure_graph",
            "agent_failed_child_preserves_failure_graph_execution",
            "Agent facade preserves a failed subagent task graph, terminal process state, and task.fail reason without provider exhaustion or false final value",
        )),
        "agent.parallel_spawn_and_join" => Ok((
            "agent_scenario_parallel_spawn_and_join",
            "agent_parallel_spawn_and_join_execution",
            "Agent facade starts two child processes and joins their concrete final values in order",
        )),
        "agent.tuple_values_finish_as_json_arrays" => Ok((
            "agent_scenario_tuple_values_finish_as_json_arrays",
            "agent_tuple_values_finish_json_arrays_execution",
            "Agent facade preserves Lashlang tuple projections as JSON arrays in final-value and runtime outcome evidence",
        )),
        other => Err(format!(
            "no Agent contract metadata registered for `{other}`"
        )),
    }
}

fn require_agent_bool(
    result: &Value,
    pointer: &str,
    expected: bool,
    contract: &str,
) -> Result<(), String> {
    if result.pointer(pointer).and_then(Value::as_bool) == Some(expected) {
        Ok(())
    } else {
        Err(format!("{contract} expected {pointer}={expected}"))
    }
}

fn require_agent_u64(
    result: &Value,
    pointer: &str,
    expected: u64,
    contract: &str,
) -> Result<(), String> {
    if result.pointer(pointer).and_then(Value::as_u64) == Some(expected) {
        Ok(())
    } else {
        Err(format!("{contract} expected {pointer}={expected}"))
    }
}

fn require_agent_min_u64(
    result: &Value,
    pointer: &str,
    minimum: u64,
    contract: &str,
) -> Result<(), String> {
    if result
        .pointer(pointer)
        .and_then(Value::as_u64)
        .is_some_and(|value| value >= minimum)
    {
        Ok(())
    } else {
        Err(format!("{contract} expected {pointer}>={minimum}"))
    }
}

fn require_agent_str(
    result: &Value,
    pointer: &str,
    expected: &str,
    contract: &str,
) -> Result<(), String> {
    if result.pointer(pointer).and_then(Value::as_str) == Some(expected) {
        Ok(())
    } else {
        Err(format!("{contract} expected {pointer}=`{expected}`"))
    }
}

fn require_agent_final_value(
    result: &Value,
    expected: &Value,
    contract: &str,
) -> Result<(), String> {
    if result.get("final_value") == Some(expected)
        && result.pointer("/runtime_final_value_facts/semantic_value") == Some(expected)
        && result
            .pointer("/runtime_final_value_facts/outcome_kind")
            .and_then(Value::as_str)
            == Some("final_value")
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} missing concrete facade final value `{expected}`"
        ))
    }
}

fn require_agent_no_final_value(result: &Value, contract: &str) -> Result<(), String> {
    if result.get("final_value") == Some(&Value::Null)
        && result
            .pointer("/failure/final_value_present")
            .and_then(Value::as_bool)
            == Some(false)
        && result
            .pointer("/failure/final_value_event_count")
            .and_then(Value::as_u64)
            == Some(0)
    {
        Ok(())
    } else {
        Err(format!("{contract} unexpectedly recorded a final value"))
    }
}

fn require_agent_tool_output(
    result: &Value,
    name: &str,
    expected: &Value,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("tool_completed_outputs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|entry| {
            entry.get("name").and_then(Value::as_str) == Some(name)
                && entry.get("value") == Some(expected)
        })
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} missing tool output `{name}` `{expected}`"
        ))
    }
}

fn require_agent_completed_process_entry(
    result: &Value,
    entry_name: &str,
    contract: &str,
) -> Result<(), String> {
    require_agent_array_str_contains(
        result,
        "/process_facts/completed_entries",
        entry_name,
        contract,
    )
}

fn require_agent_completed_labeled_resource(
    result: &Value,
    title: &str,
    contract: &str,
) -> Result<(), String> {
    require_agent_array_str_contains(
        result,
        "/graph_facts/completed_labeled_resources",
        title,
        contract,
    )
}

fn require_agent_completed_labeled_node(
    result: &Value,
    title: &str,
    contract: &str,
) -> Result<(), String> {
    require_agent_array_str_contains(
        result,
        "/graph_facts/completed_labeled_nodes",
        title,
        contract,
    )
}

fn require_agent_failed_labeled_resource(
    result: &Value,
    title: &str,
    contract: &str,
) -> Result<(), String> {
    require_agent_array_str_contains(
        result,
        "/graph_facts/failed_labeled_resources",
        title,
        contract,
    )
}

fn require_agent_array_str_contains(
    result: &Value,
    pointer: &str,
    expected: &str,
    contract: &str,
) -> Result<(), String> {
    if result
        .pointer(pointer)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|value| value.as_str() == Some(expected))
    {
        Ok(())
    } else {
        Err(format!("{contract} missing `{expected}` in {pointer}"))
    }
}

fn require_agent_process_event(
    result: &Value,
    event_type: &str,
    payload_pointer: &str,
    expected_payload: &str,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("process_events")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|event| {
            event.get("event_type").and_then(Value::as_str) == Some(event_type)
                && event.pointer(payload_pointer).and_then(Value::as_str) == Some(expected_payload)
        })
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} missing process event `{event_type}` with {payload_pointer}=`{expected_payload}`"
        ))
    }
}

fn require_agent_no_process_event(
    result: &Value,
    event_type: &str,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("process_events")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .all(|event| event.get("event_type").and_then(Value::as_str) != Some(event_type))
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} unexpectedly recorded process event `{event_type}`"
        ))
    }
}

fn rlm_protocol_execution_fact(
    events: &[DeliveredBoundary],
    contract: &'static str,
) -> Result<ScenarioContractGeneratedFact, String> {
    let (scenario, fact, assertion) = rlm_protocol_contract_metadata(contract)?;
    let proof_event = contract_execution_event(events, contract)?;
    let execution = contract_execution_payload_matches_observed(proof_event, contract, scenario)?;
    let result = execution
        .get("result")
        .ok_or_else(|| format!("{contract} execution missing result"))?;
    let contract_observed = match contract {
        "rlm.natural_prose_finalizes" => {
            require_rlm_bool(result, "/done", true, contract)?;
            require_rlm_u64(result, "/llm_call_count", 1, contract)?;
            require_rlm_bool(result, "/initial_request_tools_empty", true, contract)?;
            require_rlm_bool(result, "/final_message_event", false, contract)?;
            require_rlm_bool(result, "/assistant_conversation_progress", false, contract)?;
            require_rlm_checkpoint(result, "before_completion", contract)?;
            require_rlm_turn_outcome_contains(result, "finished", "AssistantMessage", contract)?;
            require_rlm_diagnostic(result, "finish_prose", "natural", contract)?;
            json!({
                "mode": "natural",
                "decision": "finish_prose",
                "done": true,
                "turn_outcome": "assistant_message",
                "llm_call_count": 1,
                "assistant_conversation_progress": false,
            })
        }
        "rlm.typed_prose_requires_finish" => {
            require_rlm_bool(result, "/done", false, contract)?;
            require_rlm_u64(result, "/llm_call_count", 2, contract)?;
            require_rlm_checkpoint(result, "after_work", contract)?;
            require_rlm_system_contains(result, "explicit final value", contract)?;
            require_rlm_system_contains(result, "finish <value>", contract)?;
            require_rlm_system_omits(result, "required output schema", contract)?;
            require_rlm_diagnostic(result, "request_finish", "finish_required", contract)?;
            json!({
                "mode": "finish_required",
                "decision": "request_finish",
                "done": false,
                "repair_prompt_contains": ["explicit final value", "finish <value>"],
                "llm_call_count": 2,
            })
        }
        "rlm.finish_required_max_turn_stop" => {
            require_rlm_bool(result, "/done", true, contract)?;
            require_rlm_u64(result, "/llm_call_count", 1, contract)?;
            require_rlm_stopped_max_turns(result, contract)?;
            require_rlm_system_omits(result, "explicit final value", contract)?;
            require_rlm_system_omits(result, "finish <value>", contract)?;
            json!({
                "mode": "finish_required",
                "done": true,
                "stop_reason": "max_turns",
                "llm_call_count": 1,
                "retry_prompt_after_max_turn": false,
            })
        }
        "rlm.exec_error_max_turn_stop" => {
            require_rlm_bool(result, "/done", true, contract)?;
            require_rlm_u64(result, "/llm_call_count", 1, contract)?;
            require_rlm_exec_code(result, "missing_name", contract)?;
            require_rlm_stopped_max_turns(result, contract)?;
            require_rlm_trajectory_error(
                result,
                Some("unknown variable `missing_name`"),
                contract,
            )?;
            json!({
                "mode": "finish_required",
                "done": true,
                "stop_reason": "max_turns",
                "exec_code": "missing_name",
                "trajectory_error": "unknown variable `missing_name`",
            })
        }
        "rlm.typed_finish_emits_outcome_and_done" => {
            require_rlm_bool(result, "/done", true, contract)?;
            require_rlm_u64(result, "/llm_call_count", 1, contract)?;
            require_rlm_exec_code(result, "finish { ok: true }", contract)?;
            require_rlm_checkpoint(result, "before_completion", contract)?;
            require_rlm_final_value(result, &json!({ "ok": true }), contract)?;
            require_rlm_bool(result, "/final_message_event", false, contract)?;
            require_rlm_trajectory_error(result, None, contract)?;
            json!({
                "mode": "finish_required_schema",
                "done": true,
                "final_value": { "ok": true },
                "exec_code": "finish { ok: true }",
                "final_message_event": false,
                "checkpoint": "before_completion",
            })
        }
        "rlm.finish_required_diagnostic_counts" => {
            let diagnostic =
                require_rlm_diagnostic(result, "request_finish", "finish_required", contract)?;
            require_rlm_count(diagnostic, "full_text_chars", 12, contract)?;
            require_rlm_count(diagnostic, "prose_chars", 12, contract)?;
            require_rlm_count(diagnostic, "code_chars", 0, contract)?;
            require_rlm_count(diagnostic, "reasoning_chars", 0, contract)?;
            require_rlm_count(diagnostic, "lashlang_cell_count", 0, contract)?;
            require_rlm_checkpoint(result, "after_work", contract)?;
            json!({
                "decision": "request_finish",
                "termination": "finish_required",
                "counts": diagnostic.get("counts").cloned().unwrap_or(Value::Null),
            })
        }
        "rlm.natural_diagnostic_counts" => {
            let diagnostic = require_rlm_diagnostic(result, "finish_prose", "natural", contract)?;
            require_rlm_count(diagnostic, "full_text_chars", 12, contract)?;
            require_rlm_count(diagnostic, "prose_chars", 12, contract)?;
            require_rlm_count(diagnostic, "code_chars", 0, contract)?;
            require_rlm_count(diagnostic, "reasoning_chars", 0, contract)?;
            require_rlm_count(diagnostic, "lashlang_cell_count", 0, contract)?;
            require_rlm_checkpoint(result, "before_completion", contract)?;
            json!({
                "decision": "finish_prose",
                "termination": "natural",
                "counts": diagnostic.get("counts").cloned().unwrap_or(Value::Null),
            })
        }
        "rlm.cell_diagnostic_counts" => {
            let diagnostic =
                require_rlm_diagnostic(result, "execute_lashlang", "natural", contract)?;
            require_rlm_count(diagnostic, "lashlang_cell_count", 1, contract)?;
            require_rlm_count(diagnostic, "code_chars", 10, contract)?;
            require_rlm_exec_code(result, "print \"hi\"", contract)?;
            require_rlm_trajectory_error(result, None, contract)?;
            json!({
                "decision": "execute_lashlang",
                "counts": diagnostic.get("counts").cloned().unwrap_or(Value::Null),
                "exec_code": "print \"hi\"",
                "trajectory_last": rlm_trajectory_last(result).cloned().unwrap_or(Value::Null),
            })
        }
        "rlm.retired_marker_plain_lashlang_text" => {
            let code = "text = \"%%lashlang is just source here\"\nprint text";
            let diagnostic =
                require_rlm_diagnostic(result, "execute_lashlang", "natural", contract)?;
            require_rlm_count(diagnostic, "lashlang_cell_count", 1, contract)?;
            require_rlm_exec_code(result, code, contract)?;
            require_rlm_no_tool_call_event(result, contract)?;
            json!({
                "decision": "execute_lashlang",
                "exec_code": code,
                "retired_marker_interpreted_as_source_text": true,
                "tool_call_event": false,
            })
        }
        "rlm.lashlang_cell_exec_continues" => {
            require_rlm_bool(result, "/done", false, contract)?;
            require_rlm_u64(result, "/llm_call_count", 2, contract)?;
            require_rlm_exec_code(result, "print \"hi\"", contract)?;
            require_rlm_checkpoint(result, "after_work", contract)?;
            require_rlm_trajectory_error(result, None, contract)?;
            require_rlm_trajectory_output_contains(result, "hi\n", contract)?;
            json!({
                "done": false,
                "llm_call_count": 2,
                "exec_code": "print \"hi\"",
                "checkpoint": "after_work",
                "trajectory_output": "hi\n",
            })
        }
        "rlm.streamed_lashlang_cell_exec_persists_trajectory" => {
            require_rlm_bool(result, "/done", false, contract)?;
            require_rlm_u64(result, "/llm_call_count", 2, contract)?;
            require_rlm_response_text_streamed(result, 0, true, contract)?;
            require_rlm_exec_code(result, "print \"streamed\"", contract)?;
            require_rlm_checkpoint(result, "after_work", contract)?;
            require_rlm_trajectory_error(result, None, contract)?;
            require_rlm_trajectory_output_contains(result, "streamed\n", contract)?;
            json!({
                "done": false,
                "llm_call_count": 2,
                "text_streamed": true,
                "exec_code": "print \"streamed\"",
                "checkpoint": "after_work",
                "trajectory_output": "streamed\n",
            })
        }
        "rlm.empty_options_natural_default" => {
            require_rlm_bool(result, "/done", true, contract)?;
            require_rlm_final_value(result, &json!("done"), contract)?;
            require_rlm_exec_code(result, "finish \"done\"", contract)?;
            require_rlm_checkpoint(result, "before_completion", contract)?;
            if result.pointer("/termination/kind").and_then(Value::as_str)
                != Some("empty_protocol_turn_options")
            {
                return Err(format!(
                    "{contract} did not execute with empty protocol turn options"
                ));
            }
            json!({
                "mode": "empty_options_default",
                "natural_default": true,
                "final_value": "done",
                "exec_code": "finish \"done\"",
            })
        }
        "rlm.exec_result_no_tool_call_replay" => {
            require_rlm_exec_code(
                result,
                "x = await tools.read_file({ path: \"foo\" })?",
                contract,
            )?;
            require_rlm_checkpoint(result, "after_work", contract)?;
            require_rlm_tool_call_event(result, contract)?;
            require_rlm_trajectory_omits(result, "rlm-call-1", contract)?;
            require_rlm_trajectory_error(result, None, contract)?;
            json!({
                "exec_code": "x = await tools.read_file({ path: \"foo\" })?",
                "checkpoint": "after_work",
                "tool_call_event": true,
                "trajectory_omits_tool_call_id": "rlm-call-1",
            })
        }
        "rlm.exec_tool_control_frame_switch_terminal" => {
            require_rlm_bool(result, "/done", true, contract)?;
            require_rlm_exec_code(result, "x = await tools.custom_frame_switch({})?", contract)?;
            require_rlm_checkpoint(result, "before_completion", contract)?;
            require_rlm_tool_call_event(result, contract)?;
            require_rlm_agent_frame_switch(result, "next-frame", "continue", 1, contract)?;
            require_rlm_trajectory_error(result, None, contract)?;
            json!({
                "done": true,
                "exec_code": "x = await tools.custom_frame_switch({})?",
                "checkpoint": "before_completion",
                "agent_frame_switch": {
                    "frame_key_material": "next-frame",
                    "task": "continue",
                    "initial_node_count": 1,
                },
                "tool_call_event": true,
            })
        }
        "rlm.exec_tool_control_fail_terminal" => {
            require_rlm_bool(result, "/done", true, contract)?;
            require_rlm_exec_code(result, "x = await tools.custom_fail({})?", contract)?;
            require_rlm_checkpoint(result, "before_completion", contract)?;
            require_rlm_tool_call_event(result, contract)?;
            require_rlm_tool_error(result, "custom_fail", "no valid result", contract)?;
            require_rlm_trajectory_error(result, None, contract)?;
            json!({
                "done": true,
                "exec_code": "x = await tools.custom_fail({})?",
                "checkpoint": "before_completion",
                "tool_error": { "tool_name": "custom_fail", "message": "no valid result" },
                "tool_call_event": true,
            })
        }
        "rlm.natural_allows_finish_value" => {
            require_rlm_bool(result, "/done", true, contract)?;
            require_rlm_final_value(result, &json!({ "ok": true }), contract)?;
            require_rlm_exec_code(result, "finish { ok: true }", contract)?;
            require_rlm_checkpoint(result, "before_completion", contract)?;
            json!({
                "mode": "natural",
                "final_value": { "ok": true },
                "exec_code": "finish { ok: true }",
            })
        }
        "rlm.typed_schema_mismatch_repair_loop" => {
            require_rlm_u64(result, "/llm_call_count", 2, contract)?;
            require_rlm_checkpoint(result, "after_work", contract)?;
            require_rlm_exec_code(result, "finish { missing: true }", contract)?;
            require_rlm_system_contains(
                result,
                "didn't match the required output schema",
                contract,
            )?;
            require_rlm_trajectory_error(result, Some("\"ok\" is a required property"), contract)?;
            json!({
                "mode": "finish_required_schema",
                "schema_feedback": "required property",
                "repair_loop_next_llm_call": true,
                "llm_call_count": 2,
            })
        }
        "rlm.typed_schema_any_of_mismatch" => {
            require_rlm_checkpoint(result, "after_work", contract)?;
            require_rlm_exec_code(result, "finish true", contract)?;
            require_rlm_system_contains(
                result,
                "didn't match the required output schema",
                contract,
            )?;
            require_rlm_trajectory_error(
                result,
                Some("true is not valid under any of the schemas listed in the 'anyOf' keyword"),
                contract,
            )?;
            json!({
                "mode": "finish_required_schema",
                "schema_feedback": "anyOf",
                "exec_code": "finish true",
            })
        }
        other => {
            return Err(format!(
                "RLM protocol contract execution fact has no checker for `{other}`"
            ));
        }
    };
    generated_fact(
        fact,
        assertion,
        vec![proof_event],
        json!({
            "contract_execution_boundary": proof_event.boundary_id,
            "contract": contract,
            "observed": contract_observed,
            "source": execution.get("source").cloned().unwrap_or(Value::Null),
        }),
    )
}

fn rlm_protocol_contract_metadata(
    contract: &str,
) -> Result<(&'static str, &'static str, &'static str), String> {
    match contract {
        "rlm.natural_prose_finalizes" => Ok((
            "rlm_protocol_scenario_prose_only_response_finishes_by_default",
            "rlm_natural_prose_finalizes",
            "natural RLM prose-only response finishes as an assistant-message outcome with clean natural diagnostics",
        )),
        "rlm.typed_prose_requires_finish" => Ok((
            "rlm_protocol_scenario_typed_prose_only_response_requests_finish",
            "rlm_typed_prose_requires_finish",
            "finish-required prose-only response stays unfinished and emits explicit finish repair feedback",
        )),
        "rlm.finish_required_max_turn_stop" => Ok((
            "rlm_protocol_scenario_finish_required_prose_at_max_turns_stops_without_retry_prompt",
            "rlm_finish_required_max_turn_stop",
            "finish-required prose at max turns stops with TurnStop::MaxTurns and no extra retry prompt",
        )),
        "rlm.exec_error_max_turn_stop" => Ok((
            "rlm_protocol_scenario_finish_required_exec_error_at_max_turns_stops_without_retry",
            "rlm_exec_error_max_turn_stop",
            "finish-required exec error at max turns records the concrete exec failure and stops with TurnStop::MaxTurns",
        )),
        "rlm.typed_finish_emits_outcome_and_done" => Ok((
            "rlm_protocol_scenario_typed_finish_emits_turn_outcome_and_done",
            "rlm_typed_finish_emits_outcome_and_done",
            "typed RLM finish executes LashLang, emits a concrete final-value TurnOutcome, and marks the turn done without a final message event",
        )),
        "rlm.finish_required_diagnostic_counts" => Ok((
            "rlm_protocol_scenario_finish_required_prose_only_diagnostic_has_clean_counts",
            "rlm_finish_required_diagnostic_counts",
            "finish-required prose diagnostic records exact prose/code/reasoning/lashlang counts",
        )),
        "rlm.natural_diagnostic_counts" => Ok((
            "rlm_protocol_scenario_natural_prose_only_diagnostic_has_clean_counts",
            "rlm_natural_diagnostic_counts",
            "natural prose diagnostic records exact prose/code/reasoning/lashlang counts",
        )),
        "rlm.cell_diagnostic_counts" => Ok((
            "rlm_protocol_scenario_cell_reasoning_prose_code_diagnostic_has_clean_counts",
            "rlm_cell_diagnostic_counts",
            "mixed reasoning/prose/lashlang diagnostic records one cell and the concrete executed code",
        )),
        "rlm.retired_marker_plain_lashlang_text" => Ok((
            "rlm_protocol_scenario_retired_percent_marker_inside_source_is_plain_lashlang_text",
            "rlm_retired_marker_plain_lashlang_text",
            "retired percent LashLang marker inside a source block remains plain source text and executes as one cell",
        )),
        "rlm.lashlang_cell_exec_continues" => Ok((
            "rlm_protocol_scenario_lashlang_cell_runs_exec_and_continues",
            "rlm_lashlang_cell_exec_continues",
            "LashLang cell execution records concrete output, checkpoints after work, and re-enters the model loop",
        )),
        "rlm.streamed_lashlang_cell_exec_persists_trajectory" => Ok((
            "rlm_protocol_scenario_streamed_lashlang_cell_runs_exec_and_persists_trajectory",
            "rlm_streamed_lashlang_cell_exec_persists_trajectory",
            "streamed LashLang cell execution records concrete output, checkpoints after work, and preserves trajectory evidence before re-entering the model loop",
        )),
        "rlm.empty_options_natural_default" => Ok((
            "rlm_protocol_scenario_empty_turn_options_use_natural_default",
            "rlm_empty_options_natural_default",
            "empty RLM turn options default to natural mode and accept explicit finish value",
        )),
        "rlm.exec_result_no_tool_call_replay" => Ok((
            "rlm_protocol_scenario_exec_result_emits_accounting_without_storing_tool_call_ids",
            "rlm_exec_result_no_tool_call_replay",
            "exec results emit standard tool-call accounting events without storing tool-call ids in RLM trajectory",
        )),
        "rlm.exec_tool_control_frame_switch_terminal" => Ok((
            "rlm_protocol_scenario_exec_any_tool_control_frame_switch_is_terminal",
            "rlm_exec_tool_control_frame_switch_terminal",
            "exec result tool control frame switch terminalizes as a concrete AgentFrameSwitch outcome",
        )),
        "rlm.exec_tool_control_fail_terminal" => Ok((
            "rlm_protocol_scenario_exec_any_tool_control_fail_is_terminal_error",
            "rlm_exec_tool_control_fail_terminal",
            "exec result tool control failure terminalizes as a concrete ToolError outcome",
        )),
        "rlm.natural_allows_finish_value" => Ok((
            "rlm_protocol_scenario_natural_allows_finish_value",
            "rlm_natural_allows_finish_value",
            "natural RLM mode accepts explicit finish value as a final-value outcome",
        )),
        "rlm.typed_schema_mismatch_repair_loop" => Ok((
            "rlm_protocol_scenario_typed_schema_mismatch_loops_with_feedback",
            "rlm_typed_schema_mismatch_repair_loop",
            "typed schema mismatch emits concrete required-property feedback and re-enters the LLM loop",
        )),
        "rlm.typed_schema_any_of_mismatch" => Ok((
            "rlm_protocol_scenario_typed_schema_mismatch_checks_any_of",
            "rlm_typed_schema_anyof_mismatch",
            "typed schema mismatch checks anyOf and records the concrete validation error",
        )),
        other => Err(format!("no RLM protocol metadata registered for `{other}`")),
    }
}

fn require_rlm_bool(
    result: &Value,
    pointer: &str,
    expected: bool,
    contract: &str,
) -> Result<(), String> {
    if result.pointer(pointer).and_then(Value::as_bool) == Some(expected) {
        Ok(())
    } else {
        Err(format!("{contract} expected {pointer}={expected}"))
    }
}

fn require_rlm_u64(
    result: &Value,
    pointer: &str,
    expected: u64,
    contract: &str,
) -> Result<(), String> {
    if result.pointer(pointer).and_then(Value::as_u64) == Some(expected) {
        Ok(())
    } else {
        Err(format!("{contract} expected {pointer}={expected}"))
    }
}

fn require_rlm_checkpoint(result: &Value, checkpoint: &str, contract: &str) -> Result<(), String> {
    if result
        .get("checkpoints")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|value| value.as_str() == Some(checkpoint))
    {
        Ok(())
    } else {
        Err(format!("{contract} missing checkpoint `{checkpoint}`"))
    }
}

fn require_rlm_response_text_streamed(
    result: &Value,
    index: usize,
    expected: bool,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("llm_response_text_streamed")
        .and_then(Value::as_array)
        .and_then(|values| values.get(index))
        .and_then(Value::as_bool)
        == Some(expected)
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} expected llm_response_text_streamed[{index}]={expected}"
        ))
    }
}

fn require_rlm_turn_outcome_contains(
    result: &Value,
    kind: &str,
    needle: &str,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("turn_outcomes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|outcome| {
            outcome.get("kind").and_then(Value::as_str) == Some(kind)
                && outcome
                    .get("finish")
                    .or_else(|| outcome.get("stop_reason"))
                    .and_then(Value::as_str)
                    .is_some_and(|value| value.contains(needle))
        })
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} missing turn outcome `{kind}` containing `{needle}`"
        ))
    }
}

fn require_rlm_stopped_max_turns(result: &Value, contract: &str) -> Result<(), String> {
    if result
        .get("turn_outcomes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|outcome| {
            outcome.get("kind").and_then(Value::as_str) == Some("stopped")
                && outcome.get("stop_reason").and_then(Value::as_str) == Some("max_turns")
        })
    {
        Ok(())
    } else {
        Err(format!("{contract} missing TurnStop::MaxTurns outcome"))
    }
}

fn require_rlm_diagnostic<'a>(
    result: &'a Value,
    decision: &str,
    termination: &str,
    contract: &str,
) -> Result<&'a Value, String> {
    let Some(diagnostic) = result
        .get("llm_extraction_diagnostics")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|diagnostic| {
            diagnostic.get("decision").and_then(Value::as_str) == Some(decision)
                && diagnostic.get("termination").and_then(Value::as_str) == Some(termination)
        })
    else {
        return Err(format!(
            "{contract} missing llm_extraction diagnostic decision={decision} termination={termination}"
        ));
    };
    Ok(diagnostic)
}

fn require_rlm_count(
    diagnostic: &Value,
    count_name: &str,
    expected: u64,
    contract: &str,
) -> Result<(), String> {
    if diagnostic
        .pointer(&format!("/counts/{count_name}"))
        .and_then(Value::as_u64)
        == Some(expected)
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} diagnostic count `{count_name}` changed"
        ))
    }
}

fn require_rlm_system_contains(result: &Value, needle: &str, contract: &str) -> Result<(), String> {
    if rlm_system_messages(result)
        .iter()
        .any(|message| message.contains(needle))
    {
        Ok(())
    } else {
        Err(format!("{contract} missing system feedback `{needle}`"))
    }
}

fn require_rlm_system_omits(result: &Value, needle: &str, contract: &str) -> Result<(), String> {
    if rlm_system_messages(result)
        .iter()
        .all(|message| !message.contains(needle))
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} unexpectedly emitted system feedback `{needle}`"
        ))
    }
}

fn rlm_system_messages(result: &Value) -> Vec<&str> {
    result
        .get("system_messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

fn require_rlm_exec_code(result: &Value, expected: &str, contract: &str) -> Result<(), String> {
    if result
        .get("exec_codes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|value| value.as_str() == Some(expected))
    {
        Ok(())
    } else {
        Err(format!("{contract} missing exec code `{expected}`"))
    }
}

fn require_rlm_final_value(result: &Value, expected: &Value, contract: &str) -> Result<(), String> {
    if result
        .get("turn_outcomes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|outcome| {
            outcome.get("kind").and_then(Value::as_str) == Some("final_value")
                && outcome.get("value") == Some(expected)
        })
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} missing final-value outcome `{expected}`"
        ))
    }
}

fn require_rlm_trajectory_error(
    result: &Value,
    expected: Option<&str>,
    contract: &str,
) -> Result<(), String> {
    let Some(last) = rlm_trajectory_last(result) else {
        return Err(format!("{contract} missing RLM trajectory entry"));
    };
    match expected {
        Some(needle)
            if last
                .get("error")
                .and_then(Value::as_str)
                .is_some_and(|error| error.contains(needle)) =>
        {
            Ok(())
        }
        Some(needle) => Err(format!(
            "{contract} trajectory error did not contain `{needle}`"
        )),
        None if last.get("error").is_none() => Ok(()),
        None => Err(format!("{contract} trajectory unexpectedly had an error")),
    }
}

fn require_rlm_trajectory_output_contains(
    result: &Value,
    expected: &str,
    contract: &str,
) -> Result<(), String> {
    let Some(last) = rlm_trajectory_last(result) else {
        return Err(format!("{contract} missing RLM trajectory entry"));
    };
    if last
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|value| value.as_str() == Some(expected))
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} trajectory output did not contain `{expected}`"
        ))
    }
}

fn require_rlm_trajectory_omits(
    result: &Value,
    forbidden: &str,
    contract: &str,
) -> Result<(), String> {
    let trajectory = result.get("trajectory").cloned().unwrap_or(Value::Null);
    if !trajectory.to_string().contains(forbidden) {
        Ok(())
    } else {
        Err(format!(
            "{contract} trajectory unexpectedly retained `{forbidden}`"
        ))
    }
}

fn rlm_trajectory_last(result: &Value) -> Option<&Value> {
    result
        .get("trajectory")
        .and_then(Value::as_array)
        .and_then(|values| values.last())
}

fn require_rlm_no_tool_call_event(result: &Value, contract: &str) -> Result<(), String> {
    if result.get("tool_call_event").and_then(Value::as_bool) == Some(false) {
        Ok(())
    } else {
        Err(format!("{contract} unexpectedly emitted a tool-call event"))
    }
}

fn require_rlm_tool_call_event(result: &Value, contract: &str) -> Result<(), String> {
    if result.get("tool_call_event").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(format!(
            "{contract} did not emit a tool-call accounting event"
        ))
    }
}

fn require_rlm_agent_frame_switch(
    result: &Value,
    frame_key_material: &str,
    task: &str,
    initial_node_count: usize,
    contract: &str,
) -> Result<(), String> {
    let expected_frame_key = lash_core::FrameKey::from_caller_material(frame_key_material)
        .expect("non-empty frame key material");
    if result
        .get("turn_outcomes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|outcome| {
            outcome.get("kind").and_then(Value::as_str) == Some("agent_frame_switch")
                && outcome.get("frame_key").and_then(Value::as_str)
                    == Some(expected_frame_key.as_str())
                && outcome.get("task").and_then(Value::as_str) == Some(task)
                && outcome
                    .get("initial_nodes")
                    .and_then(Value::as_array)
                    .is_some_and(|nodes| nodes.len() == initial_node_count)
        })
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} missing AgentFrameSwitch outcome for key material `{frame_key_material}` / `{task}` with {initial_node_count} seed nodes"
        ))
    }
}

fn require_rlm_tool_error(
    result: &Value,
    tool_name: &str,
    message: &str,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("turn_outcomes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|outcome| {
            outcome.get("kind").and_then(Value::as_str) == Some("stopped")
                && outcome
                    .get("stop_reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| {
                        reason.contains("ToolError")
                            && reason.contains(tool_name)
                            && reason.contains(message)
                    })
        })
    {
        Ok(())
    } else {
        Err(format!(
            "{contract} missing ToolError outcome `{tool_name}` containing `{message}`"
        ))
    }
}

fn reject_named_contract_proxy_facts(
    semantic_oracle: &str,
    facts: &[ScenarioContractGeneratedFact],
) -> Result<(), String> {
    if !matches!(
        semantic_oracle,
        "standard.max_turns_after_tool_result"
            | "rlm.typed_finish_emits_outcome_and_done"
            | "agent.tuple_values_finish_as_json_arrays"
    ) {
        return Ok(());
    }
    for fact in facts {
        if let Some(proxy_kind) = proxy_fact_kind(fact) {
            return Err(format!(
                "contract `{semantic_oracle}` attempted to use {proxy_kind} proxy fact `{}` instead of contract-owned evidence",
                fact.fact
            ));
        }
    }
    Ok(())
}

fn proxy_fact_kind(fact: &ScenarioContractGeneratedFact) -> Option<&'static str> {
    if fact.observed.get("semantic_proof_boundary").is_some()
        || fact.observed.get("semantic_proof").is_some()
    {
        return Some("semantic-proof-only trigger");
    }
    if fact.fact == "generated_transition_evidence_present"
        || fact
            .assertion
            .contains("scenario contract selected generated trace events")
        || (fact.observed.get("selected_event_count").is_some()
            && fact.observed.get("boundary_kinds").is_some())
    {
        return Some("generic transition fallback");
    }
    if fact
        .assertion
        .contains("generated provider boundary completed successfully")
        || fact.observed.get("provider_boundary").is_some()
    {
        return Some("ProviderTerminalRequirement::AnySuccessful");
    }
    if fact
        .assertion
        .contains("one generated actor completed sequential")
        || fact.observed.get("provider_turns").is_some()
    {
        return Some("ProviderTerminalRequirement::SequentialTurns");
    }
    None
}

fn streamed_text_finalizes_once_fact(
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

fn parallel_tool_results_checkpoint_once_fact(
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

fn standard_max_turns_after_tool_result_fact(
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

fn trigger_then_provider_fact(
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

fn exec_semantic_fact(
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

fn backend_retry_terminalization_fact(
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

fn durable_replay_fact(
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

fn process_wake_fact(
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

fn agent_shell_output_projection_fact(
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
                    .pointer("/runtime_effect_outcome/result/Ok/tool_calls")
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

fn agent_session_turn_child_provider_fact(
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

fn worker_stale_fact(
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

fn observer_reconnect_fact(
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
fn unmapped_scenario_semantic(suite: &str, semantic_oracle: &str) -> ScenarioSemanticVerdict {
    ScenarioSemanticVerdict::failed(format!(
        "{suite} scenario contract `{semantic_oracle}` has no per-contract semantic adapter; add distinct evidence instead of a generic fallback"
    ))
}

fn assert_semantic(condition: bool, reason: &'static str) -> ScenarioSemanticVerdict {
    if condition {
        ScenarioSemanticVerdict::passed(reason)
    } else {
        ScenarioSemanticVerdict::failed(reason)
    }
}

fn queued_active_turn_input_hidden_semantics(events: &[DeliveredBoundary]) -> bool {
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

fn cancellation_terminalizes_pending_input(events: &[DeliveredBoundary]) -> bool {
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

fn trigger_wakeup_route_semantics(events: &[DeliveredBoundary]) -> bool {
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

fn backend_retry_terminalization_semantics(events: &[DeliveredBoundary]) -> bool {
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

fn duplicate_delivery_semantics(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> bool {
    structural_process_wake_identity_semantics(events)
        && durable_effect_replay_semantics(events, summary)
}

fn structural_process_wake_identity_semantics(events: &[DeliveredBoundary]) -> bool {
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

fn process_wake_source_key(event: &DeliveredBoundary) -> Option<String> {
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
        process_id, sequence,
    ))
}

fn durable_effect_replay_semantics(
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

fn protocol_terminal_state_semantics(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> bool {
    duplicate_free_stream_finalization(events, summary)
        && provider_rate_limit_terminalized_by_scripted_parsers(events)
        && provider_dropped_terminal_event_fails_open(events)
}

fn queued_ingress_has_source_keys(events: &[DeliveredBoundary]) -> bool {
    events.iter().any(|event| {
        event.kind == BoundaryKind::QueuedIngress
            && event
                .observed
                .get("source_key")
                .and_then(Value::as_str)
                .is_some_and(|source_key| !source_key.is_empty())
    })
}

fn queued_inputs_have_cancel_targets(events: &[DeliveredBoundary]) -> bool {
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

fn provider_turns_after_queue(summary: &AbstractWorldSummary) -> bool {
    summary
        .sessions
        .iter()
        .any(|session| session.queued_ingress_count > 0 && session.provider_outputs.len() >= 2)
}

fn process_wake_runtime_dto_observed(events: &[DeliveredBoundary]) -> bool {
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

fn worker_runtime_lease_dto_observed(events: &[DeliveredBoundary]) -> bool {
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

fn durable_runtime_effect_observed(events: &[DeliveredBoundary]) -> bool {
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

fn tool_runtime_output_observed(events: &[DeliveredBoundary]) -> bool {
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

fn exec_runtime_outcome_observed(events: &[DeliveredBoundary]) -> bool {
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

fn exec_outcome_has_no_tool_call_replay(events: &[DeliveredBoundary]) -> bool {
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
        let serialized = outcome.to_string();
        serialized.contains("\"tool_calls\":[]") || !serialized.contains("\"tool_calls\"")
    })
}

fn duplicate_free_stream_finalization(
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
        .map(|session| session.provider_outputs.len())
        .sum::<usize>();
    provider_count == observed_turns
        && provider_exchange_counts_are_turn_indexed(summary)
        && observer_convergence(summary).is_passed()
}

fn provider_mutation_parser_matrix_observed(events: &[DeliveredBoundary]) -> bool {
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

fn provider_mutation_classes_observed(events: &[DeliveredBoundary]) -> bool {
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

fn provider_rate_limit_terminalized_by_scripted_parsers(events: &[DeliveredBoundary]) -> bool {
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

fn provider_dropped_terminal_event_fails_open(events: &[DeliveredBoundary]) -> bool {
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
                            == Some(true)
                        && proof
                            .get("classification")
                            .and_then(|classification| classification.get("kind"))
                            .and_then(Value::as_str)
                            == Some("Unknown")
                        && proof
                            .get("classification")
                            .and_then(|classification| classification.get("terminal_reason"))
                            .and_then(Value::as_str)
                            == Some("provider_error")
                })
                .filter_map(|proof| proof.get("provider_kind").and_then(Value::as_str))
                .collect::<BTreeSet<_>>();
            MIGRATED_RUNTIME_PROVIDER_KINDS
                .iter()
                .all(|provider| providers.contains(*provider))
        })
}

fn trigger_delivery_runtime_observed(events: &[DeliveredBoundary]) -> bool {
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

fn provider_exchange_counts_are_turn_indexed(summary: &AbstractWorldSummary) -> bool {
    summary.sessions.iter().all(|session| {
        !session.provider_exchange_counts.is_empty()
            && session
                .provider_exchange_counts
                .iter()
                .enumerate()
                .all(|(index, count)| *count == index + 1)
    })
}

fn observer_reconnect_has_matching_turn(
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
            session.observer_turn_indices.last().copied() == Some(session.provider_outputs.len())
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

fn runtime_observed_fact<T>(event: &DeliveredBoundary, key: &str) -> Option<T>
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

pub const LIVE_PROVIDER_FAILURE_ORACLE: &str = "sim.oracle.live-provider-failure-terminalizes.v1";

/// Observed facts from driving a non-retryable provider failure through a LIVE
/// runtime turn (a real `session.turn().run()` whose scripted-transport events
/// are released by a real `BoundaryScheduler`, not an isolated
/// `provider.complete()`). The fault arrives AFTER one or more valid prose
/// deltas, so "no committed output" is a non-vacuous assertion that can fail.
#[derive(Clone, Debug, serde::Serialize)]
pub struct LiveProviderFailureFacts {
    pub provider_kind: String,
    /// The non-retryable fault injected mid-turn (e.g. `malformed_sse_chunk`).
    pub fault_kind: String,
    /// How many VALID prose deltas the wire script streamed BEFORE the fault. Must
    /// be > 0: it is the partial prose a leaky runtime could (wrongly) commit, so
    /// it makes the "no committed output" assertion meaningful rather than vacuous.
    pub offered_prose_deltas: usize,
    /// How many prose deltas the runtime actually emitted to the activity stream
    /// before failing (diagnostic; proves the partial prose reached the runtime).
    pub streamed_prose_deltas: usize,
    /// The turn was observed live and parked on the first scheduler-gated provider
    /// event before any event was released (proves it is a live turn).
    pub turn_was_live_parked: bool,
    /// The turn ended in a terminal failure (returned an error or a non-success
    /// outcome) rather than finishing successfully.
    pub terminalized_failure: bool,
    /// The COMMITTED turn result carried a non-empty assistant message (a leak).
    pub committed_assistant_message_nonempty: bool,
    /// The COMMITTED turn result carried a Final Value (a leak).
    pub committed_final_values: usize,
    /// The committed session transcript contains the offered partial prose (a
    /// leak even if it is not the turn's outcome).
    pub committed_prose_in_transcript: bool,
}

/// A live runtime turn that receives a non-retryable provider failure AFTER valid
/// partial prose MUST terminalize with a terminal failure and commit NONE of that
/// prose — not as the turn outcome, not as a Final Value, and not in the session
/// transcript. Failing-capable AND non-vacuous: it requires that real prose was
/// offered before the fault (`offered_prose_deltas > 0`), so a runtime that leaks
/// the partial prose, or that finishes successfully, fails the oracle.
pub fn live_provider_failure_terminalizes(facts: &LiveProviderFailureFacts) -> OracleVerdict {
    let offered_prose = facts.offered_prose_deltas > 0;
    let no_committed_output = !facts.committed_assistant_message_nonempty
        && facts.committed_final_values == 0
        && !facts.committed_prose_in_transcript;
    if facts.turn_was_live_parked
        && offered_prose
        && facts.terminalized_failure
        && no_committed_output
    {
        OracleVerdict::passed(
            LIVE_PROVIDER_FAILURE_ORACLE,
            format!(
                "a live `{}` turn parked on a scheduler-gated provider event, streamed {} of {} offered valid prose delta(s), then a non-retryable `{}` fault terminalized it with NO committed output (no assistant message, no final value, no leaked prose in the transcript)",
                facts.provider_kind,
                facts.streamed_prose_deltas,
                facts.offered_prose_deltas,
                facts.fault_kind
            ),
        )
    } else {
        OracleVerdict::failed(
            LIVE_PROVIDER_FAILURE_ORACLE,
            format!(
                "live `{}` provider failure turn did not terminalize cleanly for fault `{}`: live_parked={} offered_prose_deltas={} streamed_prose_deltas={} terminalized_failure={} committed_assistant_message_nonempty={} committed_final_values={} committed_prose_in_transcript={}",
                facts.provider_kind,
                facts.fault_kind,
                facts.turn_was_live_parked,
                facts.offered_prose_deltas,
                facts.streamed_prose_deltas,
                facts.terminalized_failure,
                facts.committed_assistant_message_nonempty,
                facts.committed_final_values,
                facts.committed_prose_in_transcript
            ),
        )
    }
}

pub const LIVE_PROVIDER_FAILURE_COVERAGE_ORACLE: &str =
    "sim.oracle.live-provider-failure-coverage.v1";

/// Aggregate the per-combo live-failure facts for a seed: every combo must pass
/// the per-turn oracle, and the set must exercise more than one provider kind and
/// more than one fault position, so the oracle cannot pass vacuously on a single
/// degenerate case.
pub fn live_provider_failure_coverage(facts: &[LiveProviderFailureFacts]) -> OracleVerdict {
    if let Some(failed) = facts
        .iter()
        .map(live_provider_failure_terminalizes)
        .find(|verdict| !verdict.is_passed())
    {
        return OracleVerdict::failed(
            LIVE_PROVIDER_FAILURE_COVERAGE_ORACLE,
            format!("a live provider failure combo failed: {}", failed.message),
        );
    }
    let kinds = facts
        .iter()
        .map(|fact| fact.provider_kind.as_str())
        .collect::<BTreeSet<_>>();
    let positions = facts
        .iter()
        .map(|fact| fact.offered_prose_deltas)
        .collect::<BTreeSet<_>>();
    if kinds.len() < 2 || positions.len() < 2 {
        return OracleVerdict::failed(
            LIVE_PROVIDER_FAILURE_COVERAGE_ORACLE,
            format!(
                "live provider failure coverage was not exercised broadly enough: {} provider kind(s) {:?}, {} fault position(s) {:?} (need >= 2 of each)",
                kinds.len(),
                kinds,
                positions.len(),
                positions
            ),
        );
    }
    OracleVerdict::passed(
        LIVE_PROVIDER_FAILURE_COVERAGE_ORACLE,
        format!(
            "{} live provider failure turns terminalized with no committed output across {} provider kinds {:?} and {} fault positions {:?}",
            facts.len(),
            kinds.len(),
            kinds,
            positions.len(),
            positions
        ),
    )
}

pub fn combine_oracles(oracles: &[OracleVerdict]) -> OracleVerdict {
    if let Some(failure) = oracles.iter().find(|oracle| !oracle.is_passed()) {
        return failure.clone();
    }
    OracleVerdict::passed(
        "sim.oracle.generated-workload.v1",
        format!("{} generated workload oracles passed", oracles.len()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::SchedulerDeliveryEvidence;
    use crate::trace::{
        DurableEffectAbstractSummary, SessionAbstractSummary, WorkerAbstractSummary,
    };
    use serde_json::json;

    #[test]
    fn unmapped_scenario_semantics_fail_loudly_for_every_suite() {
        let summary = AbstractWorldSummary::with_digest(0, 0, vec![], vec![], vec![]);

        for (suite, verdict) in [
            (
                "runtime",
                runtime_contract_semantics("runtime.brand_new_contract", &[], &summary),
            ),
            (
                "standard",
                standard_contract_semantics("standard.brand_new_contract", &[], &summary),
            ),
            (
                "rlm",
                rlm_contract_semantics("rlm.brand_new_contract", &[], &summary),
            ),
            (
                "agent",
                agent_contract_semantics("agent.brand_new_contract", &[], &summary),
            ),
        ] {
            assert!(
                !verdict.passed,
                "an unmapped {suite} contract must fail loudly, not pass via a fallback"
            );
            assert!(
                verdict.reason.contains("no per-contract semantic adapter"),
                "{suite} failure reason should explain the missing adapter, got: {}",
                verdict.reason
            );
        }
    }

    #[test]
    fn scenario_contract_oracles_emit_one_named_verdict_per_contract() {
        let summary = semantic_summary();
        let events = semantic_events();
        let verdicts = scenario_contract_oracles(&events, &summary);

        let expected_count = RUNTIME_SCENARIO_CONTRACTS.len()
            + STANDARD_PROTOCOL_SCENARIO_CONTRACTS.len()
            + RLM_PROTOCOL_SCENARIO_CONTRACTS.len()
            + AGENT_SCENARIO_CONTRACTS.len();
        assert_eq!(verdicts.len(), expected_count);
        assert!(
            verdicts.iter().all(OracleVerdict::is_passed),
            "all generated semantic fixture scenario contracts should pass: {:?}",
            verdicts
                .iter()
                .filter(|verdict| !verdict.is_passed())
                .collect::<Vec<_>>()
        );

        let ids = verdicts
            .iter()
            .map(|verdict| verdict.oracle_id.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), verdicts.len());
        assert!(
            ids.iter().all(|id| !id.ends_with(":coverage-manifest")),
            "scenario contracts must not be backed by suite coverage manifests"
        );

        for (base, contracts) in [
            (SCENARIO_RUNTIME_CONTRACT_ORACLE, RUNTIME_SCENARIO_CONTRACTS),
            (
                SCENARIO_STANDARD_CONTRACT_ORACLE,
                STANDARD_PROTOCOL_SCENARIO_CONTRACTS,
            ),
            (
                SCENARIO_RLM_CONTRACT_ORACLE,
                RLM_PROTOCOL_SCENARIO_CONTRACTS,
            ),
            (SCENARIO_AGENT_CONTRACT_ORACLE, AGENT_SCENARIO_CONTRACTS),
        ] {
            let suite_verdicts = verdicts
                .iter()
                .filter(|verdict| verdict.oracle_id.starts_with(base))
                .collect::<Vec<_>>();
            assert_eq!(
                suite_verdicts.len(),
                contracts.len(),
                "suite `{base}` must emit one oracle per contract"
            );
            for contract in contracts {
                assert!(
                    suite_verdicts.iter().any(|verdict| {
                        verdict.oracle_id == scenario_contract_oracle_id(contract)
                            && verdict.message.contains(contract.semantic_oracle)
                    }),
                    "suite `{base}` must emit a per-contract verdict for `{}`",
                    contract.test_name
                );
            }
        }
    }

    #[test]
    fn scenario_contract_generated_facts_fail_on_contract_specific_mutations() {
        let events = semantic_events();

        for contract in [
            "standard.initial_request_projection",
            "standard.empty_provider_response_error",
            "standard.provider_error_without_checkpoint",
            "standard.native_tool_loop_reenters_model",
            "standard.parallel_tool_results_checkpoint_once",
            "standard.tool_failure_feedback_reenters_model",
            "standard.streamed_text_finalizes_once",
        ] {
            if let Err(err) = scenario_contract_generated_facts_for_semantic(contract, &events) {
                panic!(
                    "positive fixture should prove Standard replay-backed fact {contract}: {err}"
                );
            }
        }
        assert!(
            scenario_contract_generated_facts_for_semantic(
                "standard.parallel_tool_results_checkpoint_once",
                &events,
            )
            .is_ok(),
            "positive fixture should prove Standard parallel tool checkpoint facts"
        );
        assert!(
            scenario_contract_generated_facts_for_semantic(
                "standard.max_turns_after_tool_result",
                &events,
            )
            .is_ok(),
            "positive fixture should prove Standard max-turn stop after tool result facts"
        );
        assert!(
            scenario_contract_generated_facts_for_semantic(
                "rlm.typed_finish_emits_outcome_and_done",
                &events,
            )
            .is_ok(),
            "positive fixture should prove RLM typed finish outcome/done facts"
        );
        assert!(
            scenario_contract_generated_facts_for_semantic(
                "agent.tuple_values_finish_as_json_arrays",
                &events,
            )
            .is_ok(),
            "positive fixture should prove Agent tuple JSON-array final value facts"
        );
        assert!(
            scenario_contract_generated_facts_for_semantic(
                "rlm.empty_options_natural_default",
                &events,
            )
            .is_ok(),
            "positive fixture should prove RLM empty-options natural default facts"
        );
        assert!(
            scenario_contract_generated_facts_for_semantic(
                "rlm.typed_schema_mismatch_repair_loop",
                &events,
            )
            .is_ok(),
            "positive fixture should prove RLM schema-mismatch repair facts"
        );
        for contract in [
            "rlm.exec_error_max_turn_stop",
            "rlm.retired_marker_plain_lashlang_text",
            "rlm.lashlang_cell_exec_continues",
            "rlm.streamed_lashlang_cell_exec_persists_trajectory",
            "rlm.exec_result_no_tool_call_replay",
            "rlm.exec_tool_control_frame_switch_terminal",
            "rlm.exec_tool_control_fail_terminal",
        ] {
            if let Err(err) = scenario_contract_generated_facts_for_semantic(contract, &events) {
                panic!("positive fixture should prove RLM replay-backed fact {contract}: {err}");
            }
        }
        assert!(
            scenario_contract_generated_facts_for_semantic(
                "rlm.lashlang_cell_exec_continues",
                &events,
            )
            .is_ok(),
            "positive fixture should prove RLM LashLang exec continuation facts"
        );
        for contract in [
            "agent.foreground_tool_call_round_trip",
            "agent.started_process_tool_call_graph",
            "agent.durable_input_suspension_resolution",
            "agent.shell_results_are_data",
            "agent.shell_output_print_projection_survives",
            "agent.started_process_subagent_spawn",
            "agent.nested_process_start_await",
            "agent.session_turn_process_child",
            "agent.failed_child_preserves_failure_graph",
            "agent.parallel_spawn_and_join",
        ] {
            if let Err(err) = scenario_contract_generated_facts_for_semantic(contract, &events) {
                panic!("positive fixture should prove Agent replay-backed fact {contract}: {err}");
            }
        }
        assert!(
            scenario_contract_generated_facts_for_semantic(
                "agent.shell_output_print_projection_survives",
                &events,
            )
            .is_ok(),
            "positive fixture should prove Agent shell output projection facts"
        );
        assert!(
            scenario_contract_generated_facts_for_semantic(
                "agent.durable_input_suspension_resolution",
                &events,
            )
            .is_ok(),
            "positive fixture should prove Agent durable input resolution facts"
        );
        assert!(
            scenario_contract_generated_facts_for_semantic(
                "agent.parallel_spawn_and_join",
                &events
            )
            .is_ok(),
            "positive fixture should prove Agent parallel spawn/join facts"
        );

        let mut no_reentry_release = events.clone();
        no_reentry_release.retain(|event| {
            !(event.kind == BoundaryKind::ProviderEvent
                && event
                    .payload
                    .get("turn_boundary_id")
                    .and_then(Value::as_str)
                    == Some("session-001:provider:003"))
        });
        let err = scenario_contract_generated_facts_for_semantic(
            "standard.parallel_tool_results_checkpoint_once",
            &no_reentry_release,
        )
        .expect_err("Standard parallel tool checkpoint must require provider-event release");
        assert!(
            err.contains("provider-event release evidence"),
            "unexpected Standard parallel failure: {err}"
        );

        let mut wrong_max_turn_stop = events.clone();
        mutate_contract_execution(
            &mut wrong_max_turn_stop,
            "standard.max_turns_after_tool_result",
            |execution| {
                execution
                    .pointer_mut("/result/turn_outcomes/0/stop_reason")
                    .expect("max-turn stop reason")
                    .clone_from(&json!("runtime_error"));
            },
        );
        let err = scenario_contract_generated_facts_for_semantic(
            "standard.max_turns_after_tool_result",
            &wrong_max_turn_stop,
        )
        .expect_err("Standard max-turn fact must require explicit max-turn stop");
        assert!(
            err.contains("fixed-source replay validation"),
            "unexpected Standard max-turn failure: {err}"
        );

        let mut provider_error_checkpointed = events.clone();
        mutate_contract_execution(
            &mut provider_error_checkpointed,
            "standard.provider_error_without_checkpoint",
            |execution| {
                execution
                    .pointer_mut("/result/checkpoints")
                    .expect("provider error checkpoints")
                    .clone_from(&json!(["after_work"]));
            },
        );
        let err = scenario_contract_generated_facts_for_semantic(
            "standard.provider_error_without_checkpoint",
            &provider_error_checkpointed,
        )
        .expect_err("Standard provider error fact must require no checkpoint");
        assert!(
            err.contains("fixed-source replay validation"),
            "unexpected Standard provider-error replay failure: {err}"
        );

        let mut streamed_duplicate_delta = events.clone();
        mutate_contract_execution(
            &mut streamed_duplicate_delta,
            "standard.streamed_text_finalizes_once",
            |execution| {
                execution
                    .pointer_mut("/result/text_delta_count")
                    .expect("streamed text delta count")
                    .clone_from(&json!(1));
            },
        );
        let err = scenario_contract_generated_facts_for_semantic(
            "standard.streamed_text_finalizes_once",
            &streamed_duplicate_delta,
        )
        .expect_err("Standard streamed text fact must reject duplicate text deltas");
        assert!(
            err.contains("fixed-source replay validation"),
            "unexpected Standard streamed-text replay failure: {err}"
        );

        let mut missing_typed_done_event = events.clone();
        mutate_contract_execution(
            &mut missing_typed_done_event,
            "rlm.typed_finish_emits_outcome_and_done",
            |execution| {
                execution
                    .pointer_mut("/result/turn_outcomes/0/value/ok")
                    .expect("typed final value")
                    .clone_from(&json!(false));
            },
        );
        let err = scenario_contract_generated_facts_for_semantic(
            "rlm.typed_finish_emits_outcome_and_done",
            &missing_typed_done_event,
        )
        .expect_err("RLM typed finish fact must require concrete protocol final value");
        assert!(
            err.contains("fixed-source replay validation"),
            "unexpected RLM typed-finish failure: {err}"
        );

        let mut tuple_not_array = events.clone();
        mutate_contract_execution(
            &mut tuple_not_array,
            "agent.tuple_values_finish_as_json_arrays",
            |execution| {
                execution
                    .pointer_mut("/result/final_value/tuple")
                    .expect("tuple field")
                    .clone_from(&json!({"left": "right"}));
                execution
                    .pointer_mut("/result/runtime_final_value_facts/semantic_value/tuple")
                    .expect("tuple semantic field")
                    .clone_from(&json!({"left": "right"}));
            },
        );
        let err = scenario_contract_generated_facts_for_semantic(
            "agent.tuple_values_finish_as_json_arrays",
            &tuple_not_array,
        )
        .expect_err("Agent tuple fact must require JSON-array final value fields");
        assert!(
            err.contains("fixed-source replay validation"),
            "unexpected Agent tuple failure: {err}"
        );

        let mut empty_options_not_natural = events.clone();
        mutate_contract_execution(
            &mut empty_options_not_natural,
            "rlm.empty_options_natural_default",
            |execution| {
                execution
                    .pointer_mut("/result/termination/kind")
                    .expect("termination kind")
                    .clone_from(&json!("finish_required"));
            },
        );
        let err = scenario_contract_generated_facts_for_semantic(
            "rlm.empty_options_natural_default",
            &empty_options_not_natural,
        )
        .expect_err("RLM empty options fact must require the natural default execution mode");
        assert!(
            err.contains("fixed-source replay validation"),
            "unexpected RLM empty-options failure: {err}"
        );

        let mut exec_error_not_max_turn = events.clone();
        mutate_contract_execution(
            &mut exec_error_not_max_turn,
            "rlm.exec_error_max_turn_stop",
            |execution| {
                execution
                    .pointer_mut("/result/turn_outcomes/0/stop_reason")
                    .expect("exec-error max-turn stop reason")
                    .clone_from(&json!("RuntimeError"));
            },
        );
        let err = scenario_contract_generated_facts_for_semantic(
            "rlm.exec_error_max_turn_stop",
            &exec_error_not_max_turn,
        )
        .expect_err("RLM exec-error max-turn fact must require MaxTurns");
        assert!(
            err.contains("fixed-source replay validation"),
            "unexpected RLM exec-error max-turn replay failure: {err}"
        );

        let mut retired_marker_code_changed = events.clone();
        mutate_contract_execution(
            &mut retired_marker_code_changed,
            "rlm.retired_marker_plain_lashlang_text",
            |execution| {
                execution
                    .pointer_mut("/result/exec_codes/0")
                    .expect("retired marker exec code")
                    .clone_from(&json!("print \"wrong\""));
            },
        );
        let err = scenario_contract_generated_facts_for_semantic(
            "rlm.retired_marker_plain_lashlang_text",
            &retired_marker_code_changed,
        )
        .expect_err("RLM retired marker fact must require exact source text");
        assert!(
            err.contains("fixed-source replay validation"),
            "unexpected RLM retired-marker replay failure: {err}"
        );

        let mut exec_missing_tool_event = events.clone();
        mutate_contract_execution(
            &mut exec_missing_tool_event,
            "rlm.exec_result_no_tool_call_replay",
            |execution| {
                execution
                    .pointer_mut("/result/tool_call_event")
                    .expect("tool call event flag")
                    .clone_from(&json!(false));
            },
        );
        let err = scenario_contract_generated_facts_for_semantic(
            "rlm.exec_result_no_tool_call_replay",
            &exec_missing_tool_event,
        )
        .expect_err("RLM exec result fact must require tool-call accounting events");
        assert!(
            err.contains("fixed-source replay validation"),
            "unexpected RLM tool-call-emission failure: {err}"
        );

        let mut frame_switch_wrong_frame = events.clone();
        mutate_contract_execution(
            &mut frame_switch_wrong_frame,
            "rlm.exec_tool_control_frame_switch_terminal",
            |execution| {
                execution
                    .pointer_mut("/result/turn_outcomes/0/frame_key")
                    .expect("frame switch key")
                    .clone_from(&json!(
                        lash_core::FrameKey::from_caller_material("wrong-frame")
                            .expect("non-empty caller material")
                            .as_str()
                    ));
            },
        );
        let err = scenario_contract_generated_facts_for_semantic(
            "rlm.exec_tool_control_frame_switch_terminal",
            &frame_switch_wrong_frame,
        )
        .expect_err("RLM frame-switch fact must require exact frame key");
        assert!(
            err.contains("fixed-source replay validation"),
            "unexpected RLM frame-switch replay failure: {err}"
        );

        let mut tool_control_fail_not_terminal = events.clone();
        mutate_contract_execution(
            &mut tool_control_fail_not_terminal,
            "rlm.exec_tool_control_fail_terminal",
            |execution| {
                execution
                    .pointer_mut("/result/done")
                    .expect("tool control fail done")
                    .clone_from(&json!(false));
            },
        );
        let err = scenario_contract_generated_facts_for_semantic(
            "rlm.exec_tool_control_fail_terminal",
            &tool_control_fail_not_terminal,
        )
        .expect_err("RLM tool-control fail fact must require terminal done state");
        assert!(
            err.contains("fixed-source replay validation"),
            "unexpected RLM tool-control-fail replay failure: {err}"
        );

        let mut shell_projection_lost = events.clone();
        shell_projection_lost.retain(|event| event.boundary_id != "session-001:provider:003");
        let err = scenario_contract_generated_facts_for_semantic(
            "agent.shell_output_print_projection_survives",
            &shell_projection_lost,
        )
        .expect_err(
            "Agent shell projection fact must require later same-actor provider projection",
        );
        assert!(
            err.contains("same-actor provider projection"),
            "unexpected Agent shell projection failure: {err}"
        );

        let mut foreground_tool_missing = events.clone();
        mutate_contract_execution(
            &mut foreground_tool_missing,
            "agent.foreground_tool_call_round_trip",
            |execution| {
                execution
                    .pointer_mut("/result/tool_completed_count")
                    .expect("foreground tool completion count")
                    .clone_from(&json!(0));
            },
        );
        let err = scenario_contract_generated_facts_for_semantic(
            "agent.foreground_tool_call_round_trip",
            &foreground_tool_missing,
        )
        .expect_err("Agent foreground tool fact must require concrete tool completion");
        assert!(
            err.contains("fixed-source replay validation"),
            "unexpected Agent foreground tool replay failure: {err}"
        );

        let mut started_process_graph_lost = events.clone();
        mutate_contract_execution(
            &mut started_process_graph_lost,
            "agent.started_process_tool_call_graph",
            |execution| {
                execution
                    .pointer_mut("/result/graph_facts/completed_labeled_resources/0")
                    .expect("started process labeled resource")
                    .clone_from(&json!("wrong label"));
            },
        );
        let err = scenario_contract_generated_facts_for_semantic(
            "agent.started_process_tool_call_graph",
            &started_process_graph_lost,
        )
        .expect_err("Agent started process fact must require labeled process graph evidence");
        assert!(
            err.contains("fixed-source replay validation"),
            "unexpected Agent started-process replay failure: {err}"
        );

        let mut durable_not_suspended = events.clone();
        mutate_contract_execution(
            &mut durable_not_suspended,
            "agent.durable_input_suspension_resolution",
            |execution| {
                execution
                    .pointer_mut("/result/durable_input/suspended_before_resolution")
                    .expect("durable input suspended flag")
                    .clone_from(&json!(false));
            },
        );
        let err = scenario_contract_generated_facts_for_semantic(
            "agent.durable_input_suspension_resolution",
            &durable_not_suspended,
        )
        .expect_err("Agent durable input fact must require suspension before resolution");
        assert!(
            err.contains("fixed-source replay validation"),
            "unexpected Agent durable-input replay failure: {err}"
        );

        let mut subagent_child_graph_missing = events.clone();
        mutate_contract_execution(
            &mut subagent_child_graph_missing,
            "agent.started_process_subagent_spawn",
            |execution| {
                execution
                    .pointer_mut("/result/graph_facts/child_session_exec_completed_count")
                    .expect("subagent child exec graph count")
                    .clone_from(&json!(0));
            },
        );
        let err = scenario_contract_generated_facts_for_semantic(
            "agent.started_process_subagent_spawn",
            &subagent_child_graph_missing,
        )
        .expect_err("Agent subagent spawn fact must require child-session exec graph evidence");
        assert!(
            err.contains("fixed-source replay validation"),
            "unexpected Agent subagent-spawn replay failure: {err}"
        );

        let mut shell_results_stringly = events.clone();
        mutate_contract_execution(
            &mut shell_results_stringly,
            "agent.shell_results_are_data",
            |execution| {
                execution
                    .pointer_mut("/result/final_value/missing_exit")
                    .expect("shell missing exit")
                    .clone_from(&json!("1"));
                execution
                    .pointer_mut("/result/runtime_final_value_facts/semantic_value/missing_exit")
                    .expect("shell missing exit semantic")
                    .clone_from(&json!("1"));
            },
        );
        let err = scenario_contract_generated_facts_for_semantic(
            "agent.shell_results_are_data",
            &shell_results_stringly,
        )
        .expect_err("Agent shell result fact must preserve numeric shell data");
        assert!(
            err.contains("fixed-source replay validation"),
            "unexpected Agent shell-results replay failure: {err}"
        );

        let mut nested_process_count_lost = events.clone();
        mutate_contract_execution(
            &mut nested_process_count_lost,
            "agent.nested_process_start_await",
            |execution| {
                execution
                    .pointer_mut("/result/process_facts/process_count")
                    .expect("nested process count")
                    .clone_from(&json!(1));
            },
        );
        let err = scenario_contract_generated_facts_for_semantic(
            "agent.nested_process_start_await",
            &nested_process_count_lost,
        )
        .expect_err("Agent nested process fact must require parent and child process evidence");
        assert!(
            err.contains("fixed-source replay validation"),
            "unexpected Agent nested-process replay failure: {err}"
        );

        let mut failed_child_not_task_fail = events.clone();
        mutate_contract_execution(
            &mut failed_child_not_task_fail,
            "agent.failed_child_preserves_failure_graph",
            |execution| {
                execution
                    .pointer_mut("/result/failure/child_task_fail_reason_observed")
                    .expect("child task.fail reason")
                    .clone_from(&json!(false));
            },
        );
        let err = scenario_contract_generated_facts_for_semantic(
            "agent.failed_child_preserves_failure_graph",
            &failed_child_not_task_fail,
        )
        .expect_err("Agent failed-child fact must require task.fail failure evidence");
        assert!(
            err.contains("fixed-source replay validation"),
            "unexpected Agent failed-child replay failure: {err}"
        );

        let mut no_schema_feedback = events.clone();
        no_schema_feedback.retain(|event| {
            event
                .observed
                .get("mutation")
                .or_else(|| event.payload.get("mutation"))
                .and_then(Value::as_str)
                != Some("malformed_sse_chunk")
        });
        let err = scenario_contract_generated_facts_for_semantic(
            "rlm.typed_schema_mismatch_repair_loop",
            &no_schema_feedback,
        )
        .expect_err("RLM schema mismatch repair must require malformed provider feedback");
        assert!(
            err.contains("malformed_sse_chunk"),
            "unexpected RLM schema-mismatch failure: {err}"
        );

        let mut no_lashlang_continuation = events.clone();
        no_lashlang_continuation.retain(|event| event.boundary_id != "session-001:provider:003");
        let err = scenario_contract_generated_facts_for_semantic(
            "rlm.lashlang_cell_exec_continues",
            &no_lashlang_continuation,
        )
        .expect_err("RLM LashLang cell execution must require later model continuation");
        assert!(
            err.contains("later same-actor provider continuation"),
            "unexpected RLM LashLang continuation failure: {err}"
        );

        let mut no_observer_reconnect = events.clone();
        no_observer_reconnect.retain(|event| event.kind != BoundaryKind::Observer);
        let err = scenario_contract_generated_facts_for_semantic(
            "agent.durable_input_suspension_resolution",
            &no_observer_reconnect,
        )
        .expect_err("Agent durable input resolution must require observer reconnect evidence");
        assert!(
            err.contains("reconnected observer"),
            "unexpected Agent durable-input failure: {err}"
        );

        let mut no_join_worker = events;
        no_join_worker.retain(|event| event.kind != BoundaryKind::Worker);
        let err = scenario_contract_generated_facts_for_semantic(
            "agent.parallel_spawn_and_join",
            &no_join_worker,
        )
        .expect_err("Agent parallel spawn/join must require worker stale-completion evidence");
        assert!(
            err.contains("stale completion rejection"),
            "unexpected Agent parallel-join failure: {err}"
        );
    }

    #[test]
    fn critic_named_contracts_reject_generic_proxy_fact_backings() {
        let proxy_cases = [
            (
                "ProviderTerminalRequirement::AnySuccessful",
                ScenarioContractGeneratedFact {
                    fact: "generic_provider_proxy",
                    assertion: "generated provider boundary completed successfully with matching exchange count and runtime contract evidence",
                    boundary_ids: vec!["session-001:provider:001".to_string()],
                    observed: json!({
                        "provider_boundary": "session-001:provider:001",
                    }),
                },
            ),
            (
                "ProviderTerminalRequirement::SequentialTurns",
                ScenarioContractGeneratedFact {
                    fact: "sequential_provider_proxy",
                    assertion: "one generated actor completed sequential turn-indexed provider boundaries",
                    boundary_ids: vec![
                        "session-001:provider:001".to_string(),
                        "session-001:provider:002".to_string(),
                    ],
                    observed: json!({
                        "provider_turns": [
                            {"boundary_id": "session-001:provider:001"},
                            {"boundary_id": "session-001:provider:002"}
                        ],
                    }),
                },
            ),
            (
                "generic transition fallback",
                ScenarioContractGeneratedFact {
                    fact: "generated_transition_evidence_present",
                    assertion: "scenario contract selected generated trace events for its required state transition",
                    boundary_ids: vec!["session-001:trigger:001".to_string()],
                    observed: json!({
                        "selected_event_count": 1,
                        "boundary_kinds": ["Trigger"],
                    }),
                },
            ),
            (
                "semantic-proof-only trigger",
                ScenarioContractGeneratedFact {
                    fact: "semantic_proof_proxy",
                    assertion: "trigger payload claimed a semantic proof without fixed execution source identity",
                    boundary_ids: vec!["session-001:semantic-proof:001".to_string()],
                    observed: json!({
                        "semantic_proof_boundary": "session-001:semantic-proof:001",
                    }),
                },
            ),
        ];
        for semantic_oracle in [
            "standard.max_turns_after_tool_result",
            "rlm.typed_finish_emits_outcome_and_done",
            "agent.tuple_values_finish_as_json_arrays",
        ] {
            for (proxy_kind, proxy) in &proxy_cases {
                let err =
                    reject_named_contract_proxy_facts(semantic_oracle, std::slice::from_ref(proxy))
                        .expect_err("critic-named contracts must reject generic proxy facts");
                assert!(
                    err.contains(proxy_kind),
                    "unexpected proxy rejection for {semantic_oracle}/{proxy_kind}: {err}"
                );
            }
        }
    }

    #[test]
    fn state_machine_semantic_oracle_checks_contract_outcomes_not_presence() {
        let summary = semantic_summary();
        let events = semantic_events();
        let verdict = state_machine_semantic_invariants(&events, &summary);
        assert_eq!(verdict.status, crate::trace::OracleStatus::Passed);
        assert_eq!(verdict.oracle_id, STATE_MACHINE_SEMANTIC_INVARIANTS_ORACLE);

        let mut cancelled_not_terminal = events.clone();
        let cancel = cancelled_not_terminal
            .iter_mut()
            .find(|event| event.kind == BoundaryKind::Cancellation)
            .expect("cancellation event");
        cancel
            .observed
            .as_object_mut()
            .expect("observed object")
            .insert("cancel_outcome".to_string(), json!("not_found"));
        let verdict = state_machine_semantic_invariants(&cancelled_not_terminal, &summary);
        assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);
        assert!(
            verdict
                .message
                .contains("cancellation terminalizes a pending queued input")
        );

        let mut retry_not_terminal = events;
        retry_not_terminal.retain(|event| {
            !(event.kind == BoundaryKind::BackendFailure
                && event.observed.get("retryable").and_then(Value::as_bool) == Some(false))
        });
        let verdict = state_machine_semantic_invariants(&retry_not_terminal, &summary);
        assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);
        assert!(verdict.message.contains("backend retry terminalization"));
    }

    #[test]
    fn coverage_oracles_are_failing_capable_not_presence_only() {
        let summary = semantic_summary();
        let events = semantic_events();

        // Positive: a workload that genuinely satisfies the per-boundary runtime
        // invariants passes every strengthened coverage oracle.
        for verdict in [
            queued_ingress_observed(&summary, &events),
            cancellation_observed(&summary, &events),
            trigger_delivery_observed(&summary, &events),
            observer_reconnect_observed(&summary, &events),
            backend_failure_observed(&summary, &events),
            provider_mutation_rejected(&summary, &events),
            process_wake_observed(&summary, &events),
            tool_boundary_observed(&summary, &events),
            exec_code_observed(&summary, &events),
        ] {
            assert!(
                verdict.is_passed(),
                "{} should pass on a valid workload: {}",
                verdict.oracle_id,
                verdict.message
            );
        }

        // Failing-capable: the boundary kinds remain PRESENT in the summary, but
        // with the runtime DTO/projection evidence stripped (empty events) every
        // oracle FAILS loudly instead of passing on presence alone.
        for verdict in [
            queued_ingress_observed(&summary, &[]),
            cancellation_observed(&summary, &[]),
            trigger_delivery_observed(&summary, &[]),
            observer_reconnect_observed(&summary, &[]),
            backend_failure_observed(&summary, &[]),
            provider_mutation_rejected(&summary, &[]),
            process_wake_observed(&summary, &[]),
            tool_boundary_observed(&summary, &[]),
            exec_code_observed(&summary, &[]),
        ] {
            assert!(
                !verdict.is_passed(),
                "{} must fail when its invariant evidence is missing (presence is not enough)",
                verdict.oracle_id
            );
            assert!(
                verdict.message.contains("observed but"),
                "{} should explain the violated invariant, got: {}",
                verdict.oracle_id,
                verdict.message
            );
        }
    }

    #[tokio::test]
    async fn seeded_duplicate_raw_graph_row_mutation_fails_with_projection_contrast() {
        let workload = crate::generator::generate_workload(5, "fast-random", 24)
            .expect("seeded generated workload");
        let mut trace = crate::runner::run_generated_workload_for_fixture(workload, "bundle")
            .await
            .expect("generated trace");
        let baseline = runtime_graph_acyclic(&trace.durable_writes);
        assert!(
            baseline.is_passed(),
            "unmutated raw graph must pass: {}",
            baseline.message
        );
        let rows = trace
            .durable_writes
            .iter_mut()
            .filter_map(|write| write.state.as_mut())
            .filter_map(|state| state.accepted_raw_rows.as_mut())
            .filter_map(|raw| raw.get_mut("graph_nodes"))
            .filter_map(Value::as_array_mut)
            .find(|rows| !rows.is_empty())
            .expect("seed 5 records accepted raw graph rows");
        rows.push(rows[0].clone());

        let old_projection = runtime_graph_projection_acyclic(&trace.events);
        assert!(
            old_projection.is_passed(),
            "the old self-referential projection demonstrates its duplicate-row blind spot"
        );
        let raw_verdict = runtime_graph_acyclic(&trace.durable_writes);
        assert!(!raw_verdict.is_passed(), "duplicate raw row must be red");
        assert!(raw_verdict.message.contains("duplicate row"));
    }

    fn process_wake_turn_event(
        sequence: usize,
        boundary_id: &str,
        source_key: &str,
        runtime_turn_id: Option<&str>,
    ) -> DeliveredBoundary {
        delivered_with_payload(
            sequence,
            boundary_id,
            "session-001",
            BoundaryKind::ProcessWake,
            json!({}),
            json!({
                "runtime_queued_work": {
                    "enqueued": true,
                    "claimed": runtime_turn_id.is_some(),
                    "runtime_turn_id": runtime_turn_id,
                    "source_key": source_key,
                }
            }),
        )
    }

    #[test]
    fn process_wake_at_most_once_fails_on_duplicate_runtime_turns() {
        let source_key = "process/wake/session-001/001";
        let valid = vec![
            process_wake_turn_event(1, "wake:first", source_key, Some("turn:first")),
            process_wake_turn_event(2, "wake:duplicate", source_key, None),
        ];
        assert!(process_wake_at_most_once(&valid).is_passed());

        let duplicate = vec![
            process_wake_turn_event(1, "wake:first", source_key, Some("turn:first")),
            process_wake_turn_event(2, "wake:duplicate", source_key, Some("turn:second")),
        ];
        let verdict = process_wake_at_most_once(&duplicate);
        assert!(!verdict.is_passed());
        assert!(
            verdict
                .message
                .contains("materialized into 2 runtime turns")
        );

        let missing_turn_id = vec![delivered_with_payload(
            1,
            "wake:missing-turn",
            "session-001",
            BoundaryKind::ProcessWake,
            json!({}),
            json!({
                "runtime_queued_work": {
                    "enqueued": true,
                    "claimed": true,
                    "source_key": source_key,
                }
            }),
        )];
        let verdict = process_wake_at_most_once(&missing_turn_id);
        assert!(!verdict.is_passed());
        assert!(
            verdict
                .message
                .contains("no runtime-turn materialization id")
        );
    }

    fn clean_live_failure_facts() -> LiveProviderFailureFacts {
        LiveProviderFailureFacts {
            provider_kind: "openai-compatible".to_string(),
            fault_kind: "malformed_sse_chunk".to_string(),
            offered_prose_deltas: 1,
            streamed_prose_deltas: 1,
            turn_was_live_parked: true,
            terminalized_failure: true,
            committed_assistant_message_nonempty: false,
            committed_final_values: 0,
            committed_prose_in_transcript: false,
        }
    }

    #[test]
    fn live_provider_failure_oracle_passes_on_clean_terminalization_and_fails_on_committed_output()
    {
        let clean = clean_live_failure_facts();
        assert!(live_provider_failure_terminalizes(&clean).is_passed());

        // Negative: the turn finished successfully instead of terminalizing.
        assert!(
            !live_provider_failure_terminalizes(&LiveProviderFailureFacts {
                terminalized_failure: false,
                ..clean.clone()
            })
            .is_passed()
        );

        // Negative: the committed turn result leaked a non-empty assistant message.
        assert!(
            !live_provider_failure_terminalizes(&LiveProviderFailureFacts {
                committed_assistant_message_nonempty: true,
                ..clean.clone()
            })
            .is_passed()
        );

        // Negative: the turn committed a Final Value despite failing.
        assert!(
            !live_provider_failure_terminalizes(&LiveProviderFailureFacts {
                committed_final_values: 1,
                ..clean.clone()
            })
            .is_passed()
        );

        // Negative: the partial prose leaked into the committed transcript.
        assert!(
            !live_provider_failure_terminalizes(&LiveProviderFailureFacts {
                committed_prose_in_transcript: true,
                ..clean.clone()
            })
            .is_passed()
        );

        // Negative (anti-vacuity): no valid prose was offered before the fault, so
        // "no committed output" would be vacuous -> the oracle must NOT pass.
        assert!(
            !live_provider_failure_terminalizes(&LiveProviderFailureFacts {
                offered_prose_deltas: 0,
                ..clean.clone()
            })
            .is_passed()
        );

        // Negative: the turn was never observed live/parked.
        assert!(
            !live_provider_failure_terminalizes(&LiveProviderFailureFacts {
                turn_was_live_parked: false,
                ..clean
            })
            .is_passed()
        );
    }

    #[test]
    fn live_provider_failure_coverage_requires_multiple_kinds_and_positions() {
        let base = clean_live_failure_facts();
        let combo = |kind: &str, prose: usize| LiveProviderFailureFacts {
            provider_kind: kind.to_string(),
            offered_prose_deltas: prose,
            streamed_prose_deltas: prose,
            ..base.clone()
        };

        // Positive: >= 2 kinds and >= 2 positions, all clean.
        assert!(
            live_provider_failure_coverage(
                &[combo("openai-compatible", 1), combo("anthropic", 2),]
            )
            .is_passed()
        );

        // Negative: only one provider kind.
        assert!(
            !live_provider_failure_coverage(&[
                combo("openai-compatible", 1),
                combo("openai-compatible", 2)
            ])
            .is_passed()
        );

        // Negative: only one fault position.
        assert!(
            !live_provider_failure_coverage(&[
                combo("openai-compatible", 1),
                combo("anthropic", 1)
            ])
            .is_passed()
        );

        // Negative: a single combo that itself leaked output fails the aggregate.
        assert!(
            !live_provider_failure_coverage(&[
                combo("openai-compatible", 1),
                LiveProviderFailureFacts {
                    committed_assistant_message_nonempty: true,
                    ..combo("anthropic", 2)
                },
            ])
            .is_passed()
        );
    }

    #[test]
    fn worker_stale_completion_oracle_requires_real_fencing() {
        // Negative: the real store did NOT fence (no incarnation change, no stale
        // rejection, fence not advanced) -> the oracle must FAIL. With the
        // fabrication deleted, this is the ONLY way the summary can look.
        let not_fenced = AbstractWorldSummary::with_digest(
            1,
            1,
            vec![],
            vec![],
            vec![WorkerAbstractSummary {
                worker_alias: "worker-001".to_string(),
                session_alias: "session-001".to_string(),
                active_incarnation_id: String::new(),
                active_fencing_token: 1,
                lease_owner_changes: 0,
                stale_completion_rejections: 0,
                process_stale_completion_rejected: false,
                process_stale_output_absent: false,
                process_terminal_writer: String::new(),
                process_terminal_event_count: 0,
            }],
        );
        assert!(!worker_stale_completion_rejected(&not_fenced).is_passed());

        // Positive: the real store fenced (incarnation change, stale rejection,
        // monotonic fence advance) -> the oracle passes.
        let fenced = AbstractWorldSummary::with_digest(
            1,
            1,
            vec![],
            vec![],
            vec![WorkerAbstractSummary {
                worker_alias: "worker-001".to_string(),
                session_alias: "session-001".to_string(),
                active_incarnation_id: "worker-001:incarnation-002".to_string(),
                active_fencing_token: 2,
                lease_owner_changes: 1,
                stale_completion_rejections: 1,
                process_stale_completion_rejected: true,
                process_stale_output_absent: true,
                process_terminal_writer: "successor".to_string(),
                process_terminal_event_count: 1,
            }],
        );
        assert!(worker_stale_completion_rejected(&fenced).is_passed());

        let mut stale_output_present = fenced.clone();
        stale_output_present.workers[0].process_stale_output_absent = false;
        assert!(
            !worker_stale_completion_rejected(&stale_output_present).is_passed(),
            "the oracle must reject a trace where stale semantic output persisted"
        );
        let mut duplicate_terminal = fenced.clone();
        duplicate_terminal.workers[0].process_terminal_event_count = 2;
        assert!(
            !worker_stale_completion_rejected(&duplicate_terminal).is_passed(),
            "the oracle must reject duplicate successor terminals"
        );
        let mut stale_writer_won = fenced;
        stale_writer_won.workers[0].process_terminal_writer = "stale".to_string();
        assert!(
            !worker_stale_completion_rejected(&stale_writer_won).is_passed(),
            "the oracle must inspect the semantic terminal writer"
        );
    }

    #[test]
    fn deliberate_expiry_oracles_remain_failure_capable() {
        let lease_event = |sequence, token| {
            delivered_with_payload(
                sequence,
                &format!("session-001:lease-time:{sequence:03}"),
                "session-001",
                BoundaryKind::LeaseTime,
                json!({"tick": sequence * 30_000}),
                json!({
                    "runtime_lease_probe": {
                        "session_execution_lease_fencing_token": token,
                        "real_lease_store": true,
                    }
                }),
            )
        };
        assert!(lease_time_monotonic(&[lease_event(0, 1), lease_event(1, 2)]).is_passed());
        assert!(
            !lease_time_monotonic(&[lease_event(0, 1), lease_event(1, 1)]).is_passed(),
            "lease-time-monotonic must fire when a real store reissues a fence"
        );

        let worker_event = delivered_with_payload(
            0,
            "worker-001:worker:001",
            "worker-001",
            BoundaryKind::Worker,
            json!({"session": "session-001"}),
            json!({
                "stale_completion_rejected": true,
                "runtime_active_lease": {"fencing_token": 2},
                "runtime_stale_completion": {"fencing_token": 1},
            }),
        );
        let fenced = AbstractWorldSummary::with_digest(
            1,
            1,
            vec![],
            vec![],
            vec![WorkerAbstractSummary {
                worker_alias: "worker-001".to_string(),
                session_alias: "session-001".to_string(),
                active_incarnation_id: "worker-001:incarnation-002".to_string(),
                active_fencing_token: 2,
                lease_owner_changes: 1,
                stale_completion_rejections: 1,
                process_stale_completion_rejected: true,
                process_stale_output_absent: true,
                process_terminal_writer: "successor".to_string(),
                process_terminal_event_count: 1,
            }],
        );
        assert!(
            mini_runtime_stale_lease_commit_rejected(std::slice::from_ref(&worker_event), &fenced)
                .is_passed()
        );

        let mut unfenced = worker_event;
        unfenced.observed["stale_completion_rejected"] = json!(false);
        assert!(
            !mini_runtime_stale_lease_commit_rejected(&[unfenced], &fenced).is_passed(),
            "stale-lease-commit-rejected must fire when the stale writer is not rejected"
        );
    }

    #[test]
    fn replay_determinism_ignores_only_opaque_fence_values() {
        let summary = |fence, stale_rejections| {
            AbstractWorldSummary::with_digest(
                1,
                1,
                vec![],
                vec![],
                vec![WorkerAbstractSummary {
                    worker_alias: "worker-001".to_string(),
                    session_alias: "session-001".to_string(),
                    active_incarnation_id: "worker-001:incarnation-002".to_string(),
                    active_fencing_token: fence,
                    lease_owner_changes: 1,
                    stale_completion_rejections: stale_rejections,
                    process_stale_completion_rejected: true,
                    process_stale_output_absent: true,
                    process_terminal_writer: "successor".to_string(),
                    process_terminal_event_count: 1,
                }],
            )
        };

        assert!(replay_determinism(&summary(12, 1), &summary(13, 1)).is_passed());
        assert!(!replay_determinism(&summary(12, 1), &summary(13, 0)).is_passed());

        let expected = summary(12, 1);
        let mut mutated = summary(13, 1);
        mutated.workers[0].process_stale_completion_rejected = false;
        assert!(!replay_determinism(&expected, &mutated).is_passed());
        let mut mutated = summary(13, 1);
        mutated.workers[0].process_stale_output_absent = false;
        assert!(!replay_determinism(&expected, &mutated).is_passed());
        let mut mutated = summary(13, 1);
        mutated.workers[0].process_terminal_writer = "stale".to_string();
        assert!(!replay_determinism(&expected, &mutated).is_passed());
        let mut mutated = summary(13, 1);
        mutated.workers[0].process_terminal_event_count = 2;
        assert!(!replay_determinism(&expected, &mutated).is_passed());
    }

    #[test]
    fn worker_failover_continuation_oracle_requires_successor_commit() {
        let worker_event = |work: serde_json::Value| {
            delivered_with_payload(
                0,
                "worker-001:worker:001",
                "worker-001",
                BoundaryKind::Worker,
                json!({ "session": "session-001" }),
                json!({
                    "expired_owner_commit_rejected": true,
                    "runtime_worker_store": {
                        "takeover_after_ttl_expiry": true,
                        "worker_owned_work": work,
                        "process_completion": {
                            "stale_completion_rejected": true,
                            "stale_output_absent": true,
                            "terminal_writer": "successor",
                            "terminal_event_count": 1,
                        }
                    }
                }),
            )
        };
        let full = json!({
            "first_owner_claimed_work": true,
            "second_owner_resumed_work": true,
            "second_owner_outranks_first": true,
            "stale_work_completion_rejected": true,
        });

        // Positive: a successor reclaimed and continued the work.
        assert!(worker_failover_continues_work(&[worker_event(full.clone())]).is_passed());

        // Negative: no worker boundary at all.
        assert!(!worker_failover_continues_work(&[]).is_passed());

        // Negative: the successor did not resume the dead owner's work.
        let mut not_resumed = full.clone();
        not_resumed["second_owner_resumed_work"] = json!(false);
        assert!(!worker_failover_continues_work(&[worker_event(not_resumed)]).is_passed());

        // Negative: the dead owner's stale completion was NOT rejected.
        let mut stale_not_rejected = full.clone();
        stale_not_rejected["stale_work_completion_rejected"] = json!(false);
        assert!(!worker_failover_continues_work(&[worker_event(stale_not_rejected)]).is_passed());

        // Negative: the successor did not outrank the first owner's fence.
        let mut not_outranked = full;
        not_outranked["second_owner_outranks_first"] = json!(false);
        assert!(!worker_failover_continues_work(&[worker_event(not_outranked)]).is_passed());
    }

    #[test]
    fn process_lifecycle_recovery_oracles_verify_disposition_and_evidence() {
        let lifecycle = |processes: serde_json::Value| {
            delivered_with_payload(
                0,
                "session-001:process-lifecycle:001",
                "session-001",
                BoundaryKind::ProcessLifecycle,
                json!({ "session": "session-001" }),
                json!({
                    "runtime_process_lifecycle": {
                        "sweep_driven": true,
                        "processes": processes,
                    }
                }),
            )
        };
        // The correct disposition-driven recovery outcome.
        let ob_waiting = json!({
            "process_id": "ob-crashed", "disposition": "owner_bound", "started": true,
            "terminal_status": "running", "reran": false,
            "lease_lapsed": false, "abandon_requested": false,
        });
        let rerun = json!({
            "process_id": "rerun-crashed", "disposition": "rerunnable", "started": true,
            "terminal_status": "failed", "reran": true,
            "lease_lapsed": true, "abandon_requested": false,
        });
        let ob_reconciled = json!({
            "process_id": "ob-abandon-req", "disposition": "owner_bound", "started": true,
            "terminal_status": "abandoned", "reran": false, "abandon_writer": "reconciled_request",
            "abandon_evidence_owner": "sim-silent-owner",
            "lease_lapsed": true, "abandon_requested": true,
        });
        let full = vec![lifecycle(json!([ob_waiting, rerun, ob_reconciled]))];
        assert!(process_never_double_started(&full).is_passed());
        assert!(abandoned_requires_evidence(&full).is_passed());
        assert!(started_owner_bound_recovery_is_safe(&full));
        // Vacuous absence tolerates the new terminal.
        assert!(started_owner_bound_recovery_is_safe(&[]));

        // Negative: a started OwnerBound row reached a run terminal (double-start).
        let double_started = json!({
            "process_id": "ob-crashed", "disposition": "owner_bound", "started": true,
            "terminal_status": "failed", "reran": true,
        });
        let events = vec![lifecycle(json!([double_started, rerun.clone()]))];
        assert!(
            !process_never_double_started(&events).is_passed(),
            "a re-run started OwnerBound row must fail the double-start oracle"
        );
        assert!(!started_owner_bound_recovery_is_safe(&events));

        // Negative: no Rerunnable sibling re-run — refuses to pass on presence
        // alone.
        let events = vec![lifecycle(json!([ob_waiting.clone()]))];
        assert!(
            !process_never_double_started(&events).is_passed(),
            "an OwnerBound-only outcome without a re-run contrast must not pass vacuously"
        );

        // Negative: no lifecycle boundary at all — nothing recovered to verify.
        assert!(!process_never_double_started(&[]).is_passed());
        assert!(!abandoned_requires_evidence(&[]).is_passed());

        // Negative: a reconciled request whose lease had not lapsed.
        let mut reconciled_live = ob_reconciled.clone();
        reconciled_live["lease_lapsed"] = json!(false);
        let events = vec![lifecycle(json!([reconciled_live, rerun.clone()]))];
        assert!(
            !abandoned_requires_evidence(&events).is_passed(),
            "a reconciled request with a live lease must fail the evidence oracle"
        );

        // Negative: an Abandoned terminal with no writer — elapsed time alone.
        let no_writer = json!({
            "process_id": "ob-x", "disposition": "owner_bound", "started": true,
            "terminal_status": "abandoned", "reran": false,
            "lease_lapsed": true, "abandon_requested": false,
        });
        let events = vec![lifecycle(json!([no_writer]))];
        assert!(
            !abandoned_requires_evidence(&events).is_passed(),
            "an Abandoned terminal with no writer is elapsed-time-alone and must fail"
        );
    }

    #[test]
    fn scheduler_owned_runtime_completion_oracle_rejects_missing_pending_evidence() {
        let verdict = scheduler_owned_runtime_completions(&[delivered_with_payload(
            0,
            "session-001:provider:001",
            "session-001",
            BoundaryKind::Provider,
            json!({}),
            json!({"provider_output": "answer"}),
        )]);

        assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);
        assert_eq!(verdict.oracle_id, SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE);
        assert!(
            verdict
                .message
                .contains("delivered without pending runtime boundary evidence")
        );
    }

    #[test]
    fn scheduler_owned_runtime_completion_oracle_rejects_incomplete_pending_evidence() {
        for (name, completion) in [
            (
                "empty family",
                json!({
                    "completion_family": "",
                    "completion_units": [{"unit": "runtime:unit", "at": 0}],
                    "ready_at": 0,
                    "registered_after": "session-001:ingress"
                }),
            ),
            (
                "empty units",
                json!({
                    "completion_family": "provider_turn_completion",
                    "completion_units": [],
                    "ready_at": 0,
                    "registered_after": "session-001:ingress"
                }),
            ),
            (
                "wrong ready_at",
                json!({
                    "completion_family": "provider_turn_completion",
                    "completion_units": [{"unit": "runtime:unit", "at": 0}],
                    "ready_at": 99,
                    "registered_after": "session-001:ingress"
                }),
            ),
            (
                "empty registered_after",
                json!({
                    "completion_family": "provider_turn_completion",
                    "completion_units": [{"unit": "runtime:unit", "at": 0}],
                    "ready_at": 0,
                    "registered_after": ""
                }),
            ),
        ] {
            let verdict = scheduler_owned_runtime_completions(&[delivered_with_payload(
                0,
                "session-001:provider:001",
                "session-001",
                BoundaryKind::Provider,
                json!({"runtime_completion": completion}),
                json!({"provider_output": "answer"}),
            )]);

            assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);
            assert_eq!(verdict.oracle_id, SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE);
            assert!(
                verdict.message.contains("incomplete pending evidence"),
                "{name}: {}",
                verdict.message
            );
        }
    }

    #[test]
    fn standard_provider_error_oracle_requires_ordered_failure_and_parser_matrix() {
        let parser_matrix = delivered_with_payload(
            2,
            "session-001:provider-mutation:001",
            "session-001",
            BoundaryKind::ProviderMutation,
            json!({"runtime_completion": runtime_completion("provider_script_mutation", 2)}),
            provider_mutation_observed("malformed_sse_chunk"),
        );
        let provider = delivered_with_payload(
            1,
            "session-001:provider:001",
            "session-001",
            BoundaryKind::Provider,
            json!({"runtime_completion": runtime_completion("provider_turn_completion", 1)}),
            json!({"provider_output": "answer"}),
        );
        let failure_equal_sequence = delivered_with_payload(
            1,
            "session-001:backend-failure:001",
            "session-001",
            BoundaryKind::BackendFailure,
            json!({"runtime_completion": runtime_completion("backend_retry_or_failure", 1)}),
            json!({"backend_failure": true}),
        );
        let verdict = mini_standard_provider_error_without_checkpoint(&[
            provider.clone(),
            failure_equal_sequence,
            parser_matrix.clone(),
        ]);
        assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);
        assert_eq!(
            verdict.oracle_id,
            SCENARIO_MINI_STANDARD_PROVIDER_ERROR_ORACLE
        );

        let missing_runtime_completion = delivered_with_payload(
            0,
            "session-001:backend-failure:001",
            "session-001",
            BoundaryKind::BackendFailure,
            json!({}),
            json!({"backend_failure": true}),
        );
        let verdict = mini_standard_provider_error_without_checkpoint(&[
            missing_runtime_completion,
            provider.clone(),
            parser_matrix.clone(),
        ]);
        assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);

        let wrong_kind_before_provider = delivered_with_payload(
            0,
            "session-001:tool:001",
            "session-001",
            BoundaryKind::Tool,
            json!({"runtime_completion": runtime_completion("tool_return", 0)}),
            json!({"tool_output": "not a provider failure"}),
        );
        let verdict = mini_standard_provider_error_without_checkpoint(&[
            wrong_kind_before_provider,
            provider.clone(),
            parser_matrix.clone(),
        ]);
        assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);

        let ordered_failure = delivered_with_payload(
            0,
            "session-001:backend-failure:001",
            "session-001",
            BoundaryKind::BackendFailure,
            json!({"runtime_completion": runtime_completion("backend_retry_or_failure", 0)}),
            json!({"backend_failure": true}),
        );
        let verdict =
            mini_standard_provider_error_without_checkpoint(&[ordered_failure, provider.clone()]);
        assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);

        let provider_after_failure = delivered_with_payload(
            2,
            "session-001:provider:002",
            "session-001",
            BoundaryKind::Provider,
            json!({"runtime_completion": runtime_completion("provider_turn_completion", 2)}),
            json!({"provider_output": "answer"}),
        );
        let late_failure = delivered_with_payload(
            3,
            "session-001:backend-failure:002",
            "session-001",
            BoundaryKind::BackendFailure,
            json!({"runtime_completion": runtime_completion("backend_retry_or_failure", 3)}),
            json!({"backend_failure": true}),
        );
        let verdict = mini_standard_provider_error_without_checkpoint(&[
            provider_after_failure,
            late_failure,
            parser_matrix.clone(),
        ]);
        assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);

        let passing_failure = delivered_with_payload(
            0,
            "session-001:backend-failure:003",
            "session-001",
            BoundaryKind::BackendFailure,
            json!({"runtime_completion": runtime_completion("backend_retry_or_failure", 0)}),
            json!({"backend_failure": true}),
        );
        let verdict = mini_standard_provider_error_without_checkpoint(&[
            passing_failure,
            provider,
            parser_matrix,
        ]);
        assert_eq!(verdict.status, crate::trace::OracleStatus::Passed);
    }

    #[test]
    fn rlm_mini_oracle_rejects_exec_without_runtime_effect_outcome() {
        let events = vec![
            delivered_with_payload(
                0,
                "session-001:exec:001",
                "session-001",
                BoundaryKind::ExecCode,
                json!({"runtime_completion": runtime_completion("exec_result", 0)}),
                json!({
                    "exec_output": "cell ran",
                    "execution_count": 1
                }),
            ),
            delivered_with_payload(
                1,
                "session-001:provider:001",
                "session-001",
                BoundaryKind::Provider,
                json!({"runtime_completion": runtime_completion("provider_turn_completion", 1)}),
                json!({"provider_output": "continued"}),
            ),
        ];

        let verdict = mini_rlm_lashlang_cell_exec_continues(&events);

        assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);
        assert_eq!(verdict.oracle_id, SCENARIO_MINI_RLM_CELL_EXEC_ORACLE);
        assert!(verdict.message.contains("did not continue after exec"));
    }

    #[test]
    fn rlm_mini_oracle_requires_provider_after_same_actor_exec() {
        let exec = delivered_with_payload(
            1,
            "session-001:exec:001",
            "session-001",
            BoundaryKind::ExecCode,
            json!({"runtime_completion": runtime_completion("exec_result", 1)}),
            json!({
                "exec_output": "cell ran",
                "runtime_effect_outcome": {"type": "exec_code"},
                "execution_count": 1
            }),
        );
        for (name, event) in [
            (
                "same sequence provider",
                delivered_with_payload(
                    1,
                    "session-001:provider:001",
                    "session-001",
                    BoundaryKind::Provider,
                    json!({"runtime_completion": runtime_completion("provider_turn_completion", 1)}),
                    json!({"provider_output": "continued"}),
                ),
            ),
            (
                "different actor provider",
                delivered_with_payload(
                    2,
                    "session-002:provider:001",
                    "session-002",
                    BoundaryKind::Provider,
                    json!({"runtime_completion": runtime_completion("provider_turn_completion", 2)}),
                    json!({"provider_output": "continued"}),
                ),
            ),
            (
                "non-provider event",
                delivered_with_payload(
                    2,
                    "session-001:tool:001",
                    "session-001",
                    BoundaryKind::Tool,
                    json!({"runtime_completion": runtime_completion("tool_return", 2)}),
                    json!({"tool_output": "continued"}),
                ),
            ),
        ] {
            let verdict = mini_rlm_lashlang_cell_exec_continues(&[exec.clone(), event]);
            assert_eq!(verdict.status, crate::trace::OracleStatus::Failed, "{name}");
            assert_eq!(verdict.oracle_id, SCENARIO_MINI_RLM_CELL_EXEC_ORACLE);
        }

        let continued = delivered_with_payload(
            2,
            "session-001:provider:001",
            "session-001",
            BoundaryKind::Provider,
            json!({"runtime_completion": runtime_completion("provider_turn_completion", 2)}),
            json!({"provider_output": "continued"}),
        );
        let verdict = mini_rlm_lashlang_cell_exec_continues(&[exec, continued]);
        assert_eq!(verdict.status, crate::trace::OracleStatus::Passed);
    }

    #[test]
    fn agent_mini_oracle_rejects_process_wake_without_join_session() {
        let summary = AbstractWorldSummary::with_digest(2, 2, Vec::new(), Vec::new(), Vec::new());
        let events = vec![
            delivered_with_payload(
                0,
                "session-001:process-wake:001",
                "session-001",
                BoundaryKind::ProcessWake,
                json!({"runtime_completion": runtime_completion("process_wake", 0)}),
                json!({
                    "process_wake": true,
                    "runtime_process_wake": {
                        "event_invocation": {
                            "subject": {
                                "process_id": "process-001"
                            }
                        }
                    }
                }),
            ),
            delivered_with_payload(
                1,
                "worker-001:stale-completion",
                "worker-001",
                BoundaryKind::Worker,
                json!({"runtime_completion": runtime_completion("worker_lease_completion", 1)}),
                json!({"session": "session-001"}),
            ),
        ];

        let verdict = mini_agent_parallel_spawn_join(&events, &summary);

        assert_eq!(verdict.status, crate::trace::OracleStatus::Failed);
        assert_eq!(verdict.oracle_id, SCENARIO_MINI_AGENT_PARALLEL_JOIN_ORACLE);
        assert!(
            verdict
                .message
                .contains("did not record deterministic process/worker ordering")
        );
    }

    #[test]
    fn agent_durable_input_mini_oracle_requires_all_resolution_evidence() {
        let summary = AbstractWorldSummary::with_digest(2, 3, Vec::new(), Vec::new(), Vec::new());
        let durable = delivered_with_payload(
            0,
            "session-001:durable:001:replay",
            "session-001",
            BoundaryKind::DurableEffect,
            json!({"runtime_completion": runtime_completion("durable_effect_completion", 0)}),
            json!({"replayed": true, "runtime_effect": {}}),
        );
        let process_wake = delivered_with_payload(
            1,
            "session-001:process-wake:001",
            "session-001",
            BoundaryKind::ProcessWake,
            json!({"runtime_completion": runtime_completion("process_wake", 1)}),
            json!({
                "session": "session-001",
                "runtime_process_wake": {
                    "event_invocation": {
                        "subject": {
                            "process_id": "process-001"
                        }
                    }
                }
            }),
        );
        let observer = delivered_with_payload(
            2,
            "session-001:observer:reconnect:001",
            "session-001",
            BoundaryKind::Observer,
            json!({}),
            json!({"reconnected": true}),
        );

        for (name, events) in [
            (
                "missing durable",
                vec![process_wake.clone(), observer.clone()],
            ),
            (
                "missing process wake",
                vec![durable.clone(), observer.clone()],
            ),
            (
                "missing observer",
                vec![durable.clone(), process_wake.clone()],
            ),
            (
                "wrong durable kind",
                vec![
                    delivered_with_payload(
                        0,
                        "session-001:tool:001",
                        "session-001",
                        BoundaryKind::Tool,
                        json!({"runtime_completion": runtime_completion("tool_return", 0)}),
                        json!({"replayed": true, "runtime_effect": {}}),
                    ),
                    process_wake.clone(),
                    observer.clone(),
                ],
            ),
            (
                "durable not replayed",
                vec![
                    delivered_with_payload(
                        0,
                        "session-001:durable:001:first",
                        "session-001",
                        BoundaryKind::DurableEffect,
                        json!({"runtime_completion": runtime_completion("durable_effect_completion", 0)}),
                        json!({"replayed": false, "runtime_effect": {}}),
                    ),
                    process_wake.clone(),
                    observer.clone(),
                ],
            ),
        ] {
            let verdict = mini_agent_durable_input_resolution(&events);
            assert_eq!(verdict.status, crate::trace::OracleStatus::Failed, "{name}");
            assert_eq!(verdict.oracle_id, SCENARIO_MINI_AGENT_DURABLE_INPUT_ORACLE);
        }
        let verdict = mini_agent_durable_input_resolution(&[durable, process_wake, observer]);
        assert_eq!(verdict.status, crate::trace::OracleStatus::Passed);

        let parallel = mini_agent_parallel_spawn_join(
            &[
                delivered_with_payload(
                    3,
                    "session-001:process-wake:002",
                    "session-001",
                    BoundaryKind::ProcessWake,
                    json!({"runtime_completion": runtime_completion("process_wake", 3)}),
                    json!({"session": "session-001"}),
                ),
                delivered_with_payload(
                    4,
                    "worker-001:lease:002",
                    "worker-001",
                    BoundaryKind::Worker,
                    json!({"runtime_completion": runtime_completion("worker_lease_completion", 4)}),
                    json!({"session": "session-001"}),
                ),
            ],
            &summary,
        );
        assert_eq!(
            parallel.status,
            crate::trace::OracleStatus::Passed,
            "non-empty process wake session should satisfy join evidence"
        );

        let reversed = mini_agent_parallel_spawn_join(
            &[
                delivered_with_payload(
                    6,
                    "session-001:process-wake:003",
                    "session-001",
                    BoundaryKind::ProcessWake,
                    json!({"runtime_completion": runtime_completion("process_wake", 6)}),
                    json!({"session": "session-001"}),
                ),
                delivered_with_payload(
                    5,
                    "worker-001:lease:003",
                    "worker-001",
                    BoundaryKind::Worker,
                    json!({"runtime_completion": runtime_completion("worker_lease_completion", 5)}),
                    json!({"session": "session-001"}),
                ),
            ],
            &summary,
        );
        assert_eq!(reversed.status, crate::trace::OracleStatus::Failed);

        let duplicate_sequence = mini_agent_parallel_spawn_join(
            &[
                delivered_with_payload(
                    7,
                    "session-001:process-wake:004",
                    "session-001",
                    BoundaryKind::ProcessWake,
                    json!({"runtime_completion": runtime_completion("process_wake", 7)}),
                    json!({"session": "session-001"}),
                ),
                delivered_with_payload(
                    7,
                    "worker-001:lease:004",
                    "worker-001",
                    BoundaryKind::Worker,
                    json!({"runtime_completion": runtime_completion("worker_lease_completion", 7)}),
                    json!({"session": "session-001"}),
                ),
            ],
            &summary,
        );
        assert_eq!(
            duplicate_sequence.status,
            crate::trace::OracleStatus::Failed
        );
    }

    fn mutate_contract_execution(
        events: &mut [DeliveredBoundary],
        contract: &str,
        mut mutate: impl FnMut(&mut Value),
    ) {
        let event = events
            .iter_mut()
            .find(|event| {
                event
                    .observed
                    .pointer("/contract_execution/contract")
                    .and_then(Value::as_str)
                    == Some(contract)
            })
            .expect("contract execution event");
        mutate(
            event
                .observed
                .get_mut("contract_execution")
                .expect("observed contract execution"),
        );
        mutate(
            event
                .payload
                .get_mut("contract_execution")
                .expect("payload contract execution"),
        );
    }

    fn semantic_summary() -> AbstractWorldSummary {
        AbstractWorldSummary::with_digest(
            2,
            29,
            vec![
                SessionAbstractSummary {
                    alias: "session-001".to_string(),
                    opened: true,
                    ingress_count: 1,
                    provider_outputs: vec![
                        "answer for session-001 turn 1".to_string(),
                        "answer for session-001 turn 2".to_string(),
                        "answer for session-001 turn 3".to_string(),
                    ],
                    provider_exchange_counts: vec![1, 2, 3],
                    graph_node_counts: vec![3, 5, 7],
                    transcript_message_counts: vec![2, 4, 6],
                    tool_outputs: vec!["tool result for session-001".to_string()],
                    exec_code_outputs: vec!["exec result for session-001".to_string()],
                    observer_turn_indices: vec![3],
                    observer_reconnects: 1,
                    queued_ingress_count: 1,
                    cancellation_count: 1,
                    trigger_count: 4,
                    backend_failure_count: 2,
                    provider_mutation_count: 3,
                    process_wake_count: 2,
                    process_lifecycle_count: 1,
                    durable_effect_keys: vec!["durable/session-001".to_string()],
                    lease_time_ticks: vec![1, 2],
                    checkpoint_commit_count: 0,
                    checkpoint_component_stored_count: 0,
                    checkpoint_component_ref_count: 0,
                    checkpoint_head_revision: 0,
                },
                SessionAbstractSummary {
                    alias: "session-002".to_string(),
                    opened: true,
                    ingress_count: 1,
                    provider_outputs: vec![
                        "answer for session-002 turn 1".to_string(),
                        "answer for session-002 turn 2".to_string(),
                    ],
                    provider_exchange_counts: vec![1, 2],
                    graph_node_counts: vec![3, 5],
                    transcript_message_counts: vec![2, 4],
                    tool_outputs: Vec::new(),
                    exec_code_outputs: Vec::new(),
                    observer_turn_indices: vec![2],
                    observer_reconnects: 0,
                    queued_ingress_count: 0,
                    cancellation_count: 0,
                    trigger_count: 0,
                    backend_failure_count: 0,
                    provider_mutation_count: 0,
                    process_wake_count: 0,
                    process_lifecycle_count: 0,
                    durable_effect_keys: Vec::new(),
                    lease_time_ticks: vec![1, 2],
                    checkpoint_commit_count: 0,
                    checkpoint_component_stored_count: 0,
                    checkpoint_component_ref_count: 0,
                    checkpoint_head_revision: 0,
                },
            ],
            vec![DurableEffectAbstractSummary {
                durable_key: "durable/session-001".to_string(),
                execution_count: 1,
                replay_count: 1,
                result_digest: "digest".to_string(),
            }],
            vec![WorkerAbstractSummary {
                worker_alias: "worker-001".to_string(),
                session_alias: "session-001".to_string(),
                active_incarnation_id: "worker-001:incarnation-002".to_string(),
                active_fencing_token: 2,
                lease_owner_changes: 1,
                stale_completion_rejections: 1,
                process_stale_completion_rejected: true,
                process_stale_output_absent: true,
                process_terminal_writer: "successor".to_string(),
                process_terminal_event_count: 1,
            }],
        )
    }

    fn semantic_events() -> Vec<DeliveredBoundary> {
        let base = [
            delivered_with_payload(
                0,
                "session-001:provider:001",
                "session-001",
                BoundaryKind::Provider,
                json!({
                    "text": "answer for session-001 turn 1",
                    "runtime_completion": runtime_completion("provider_turn_completion", 0),
                    "expected_provider_exchange_count": 1,
                }),
                json!({
                    "provider_kind": "openai-compatible",
                    "provider_output": "answer for session-001 turn 1",
                    "success": true,
                    "provider_exchange_count": 1,
                    "runtime_contract": {"status": "passed"},
                }),
            ),
            delivered_with_payload(
                1,
                "session-001:queue:001",
                "session-001",
                BoundaryKind::QueuedIngress,
                json!({
                    "active_turn_id": "session-001:provider:002",
                    "ingress_mode": "active_turn",
                    "source_key": "queue/session-001/001",
                    "text": "queued follow-up hidden from live turn",
                }),
                json!({
                    "ingress_mode": "active_turn",
                    "input_id": "input-001",
                    "input_state": "pending_active",
                    "queued_ingress": true,
                    "session": "session-001",
                    "source_key": "queue/session-001/001",
                    "active_turn_id": "session-001:provider:002",
                }),
            ),
            delivered_with_payload(
                2,
                "session-001:provider:002:provider-event:001:sse",
                "session-001",
                BoundaryKind::ProviderEvent,
                json!({
                    "turn_boundary_id": "session-001:provider:002",
                    "event_index": 1,
                    "event_name": "sse",
                }),
                json!({
                    "provider_event_release": true,
                    "released_while_turn_pending": true,
                    "turn_boundary_id": "session-001:provider:002",
                }),
            ),
            delivered_with_payload(
                2,
                "session-001:cancel:001",
                "session-001",
                BoundaryKind::Cancellation,
                json!({
                    "target": "session-001:queue:001",
                    "runtime_completion": {
                        "completion_family": "queued_input_cancellation",
                        "completion_units": [{"unit": "runtime:cancel_pending_turn_input", "at": 2}],
                        "ready_at": 2,
                        "registered_after": "session-001:queue:001"
                    }
                }),
                json!({
                    "cancel_outcome": "cancelled",
                    "cancelled": true,
                    "session": "session-001",
                    "target": "session-001:queue:001",
                }),
            ),
            delivered_with_payload(
                3,
                "session-001:observer:reconnect:001",
                "session-001",
                BoundaryKind::Observer,
                json!({}),
                json!({"reconnected": true, "turn_index": 2}),
            ),
            delivered_with_payload(
                4,
                "session-001:provider:002",
                "session-001",
                BoundaryKind::Provider,
                json!({
                    "text": "answer for session-001 turn 2",
                    "runtime_completion": runtime_completion("provider_turn_completion", 4),
                    "expected_provider_exchange_count": 2,
                }),
                json!({
                    "provider_kind": "openai-compatible",
                    "provider_output": "answer for session-001 turn 2",
                    "success": true,
                    "provider_exchange_count": 2,
                    "runtime_contract": {"status": "passed"},
                }),
            ),
            delivered_with_payload(
                5,
                "session-002:provider:001",
                "session-002",
                BoundaryKind::Provider,
                json!({
                    "text": "answer for session-002 turn 1",
                    "runtime_completion": runtime_completion("provider_turn_completion", 5),
                    "expected_provider_exchange_count": 1,
                }),
                json!({
                    "provider_kind": "anthropic",
                    "provider_output": "answer for session-002 turn 1",
                    "success": true,
                    "provider_exchange_count": 1,
                    "runtime_contract": {"status": "passed"},
                }),
            ),
            delivered_with_payload(
                6,
                "session-002:provider:002",
                "session-002",
                BoundaryKind::Provider,
                json!({
                    "text": "answer for session-002 turn 2",
                    "runtime_completion": runtime_completion("provider_turn_completion", 6),
                    "expected_provider_exchange_count": 2,
                }),
                json!({
                    "provider_kind": "anthropic",
                    "provider_output": "answer for session-002 turn 2",
                    "success": true,
                    "provider_exchange_count": 2,
                    "runtime_contract": {"status": "passed"},
                }),
            ),
            delivered_with_payload(
                7,
                "session-001:process-wake:001",
                "session-001",
                BoundaryKind::ProcessWake,
                json!({"process_id": "process-001", "sequence": 1}),
                json!({
                    "claimed_once": true,
                    "runtime_process_wake": {
                        "process_id": "process-001",
                        "sequence": 1,
                        "event_invocation": {
                            "subject": {
                                "process_id": "process-001"
                            }
                        }
                    },
                    "runtime_queued_work": {
                        "claimed": true,
                        "source_key": "process:process-001:event:1:wake"
                    },
                    "session": "session-001",
                    "wake_id": "wake:duplicate"
                }),
            ),
            delivered_with_payload(
                8,
                "session-001:process-wake:002",
                "session-001",
                BoundaryKind::ProcessWake,
                json!({"process_id": "process-001", "sequence": 1}),
                json!({
                    "claimed_once": false,
                    "runtime_process_wake": {
                        "process_id": "process-001",
                        "sequence": 1,
                        "event_invocation": {
                            "subject": {
                                "process_id": "process-001"
                            }
                        }
                    },
                    "runtime_queued_work": {
                        "claimed": false,
                        "source_key": "process:process-001:event:1:wake"
                    },
                    "session": "session-001",
                    "wake_id": "wake:duplicate"
                }),
            ),
            delivered_with_payload(
                9,
                "worker-001:worker:001",
                "worker-001",
                BoundaryKind::Worker,
                json!({"runtime_completion": runtime_completion("worker_lease_completion", 9)}),
                json!({
                    "stale_completion_rejected": true,
                    "runtime_active_lease": {},
                    "runtime_stale_completion": {},
                }),
            ),
            delivered_with_payload(
                10,
                "session-001:durable:001:first",
                "session-001",
                BoundaryKind::DurableEffect,
                json!({"runtime_completion": runtime_completion("durable_effect_completion", 10)}),
                json!({
                    "durable_key": "durable/session-001",
                    "replayed": false,
                    "runtime_effect": {"local_executor_called": true},
                    "result_digest": "digest",
                    "execution_count": 1,
                    "replay_count": 0,
                }),
            ),
            delivered_with_payload(
                11,
                "session-001:durable:001:replay",
                "session-001",
                BoundaryKind::DurableEffect,
                json!({"runtime_completion": runtime_completion("durable_effect_completion", 11)}),
                json!({
                    "durable_key": "durable/session-001",
                    "replayed": true,
                    "runtime_effect": {"local_executor_called": false},
                    "result_digest": "digest",
                    "execution_count": 1,
                    "replay_count": 1,
                }),
            ),
            delivered_with_payload(
                12,
                "session-001:tool:001",
                "session-001",
                BoundaryKind::Tool,
                json!({"runtime_completion": runtime_completion("tool_return", 12)}),
                json!({
                    "runtime_tool_output": {},
                    "runtime_tool_record": {},
                    "execution_count": 1,
                }),
            ),
            delivered_with_payload(
                13,
                "session-001:exec:001",
                "session-001",
                BoundaryKind::ExecCode,
                json!({"runtime_completion": runtime_completion("exec_result", 13)}),
                json!({
                    "runtime_effect_outcome": {
                        "result": {
                            "Ok": {
                                "tool_calls": []
                            }
                        },
                        "type": "exec_code"
                    },
                    "execution_count": 1,
                }),
            ),
            delivered_with_payload(
                14,
                "session-001:trigger:001",
                "session-001",
                BoundaryKind::Trigger,
                json!({
                    "session": "session-001",
                    "source_key": "trigger/button/session-001/001",
                    "started_process": true,
                }),
                json!({
                    "occurrence_id": "trigger:abc",
                    "reservation_count": 1,
                    "session": "session-001",
                    "source_key": "trigger/button/session-001/001",
                    "started_process": true,
                    "trigger_delivered": true,
                }),
            ),
            delivered_with_payload(
                15,
                "session-001:backend-failure:001",
                "session-001",
                BoundaryKind::BackendFailure,
                json!({
                    "operation": "commit_runtime_state:001",
                    "runtime_completion": runtime_completion("backend_retry_or_failure", 15),
                }),
                json!({
                    "attempt": 1,
                    "backend_failure": true,
                    "operation": "commit_runtime_state:001",
                    "production_store_error": {
                        "retryable_class": true,
                        "type": "lash_core::StoreError",
                        "variant": "HeadRevisionConflict"
                    },
                    "fault_injector": {"point": "after_begin"},
                    "retryable": true,
                    "store_error_class": "retryable_conflict"
                }),
            ),
            delivered_with_payload(
                16,
                "session-001:backend-failure:002",
                "session-001",
                BoundaryKind::BackendFailure,
                json!({
                    "operation": "commit_runtime_state:001",
                    "runtime_completion": runtime_completion("backend_retry_or_failure", 16),
                }),
                json!({
                    "attempt": 2,
                    "backend_failure": true,
                    "operation": "commit_runtime_state:001",
                    "production_store_error": {
                        "retryable_class": false,
                        "type": "lash_core::StoreError",
                        "variant": "SessionExecutionLeaseExpired"
                    },
                    "fault_injector": {"point": "commit_io"},
                    "retryable": false,
                    "store_error_class": "terminal_backend_error"
                }),
            ),
            delivered_with_payload(
                17,
                "session-001:provider-mutation:001",
                "session-001",
                BoundaryKind::ProviderMutation,
                json!({"runtime_completion": runtime_completion("provider_script_mutation", 17)}),
                json!({
                    "mutation": "malformed_sse_chunk",
                    "provider_parser_matrix": {
                        "matrix": {
                            "real_provider_parser_execution": true,
                            "provider_kinds": [
                                "anthropic",
                                "google_oauth",
                                "openai",
                                "openai-compatible"
                            ]
                        }
                    }
                }),
            ),
            delivered_with_payload(
                18,
                "session-001:provider-mutation:002",
                "session-001",
                BoundaryKind::ProviderMutation,
                json!({"runtime_completion": runtime_completion("provider_script_mutation", 18)}),
                json!({
                    "mutation": "rate_limit_error_envelope",
                    "provider_parser_matrix": {
                        "matrix": {
                            "real_provider_parser_execution": true,
                            "provider_kinds": [
                                "anthropic",
                                "google_oauth",
                                "openai",
                                "openai-compatible"
                            ],
                            "proofs": [
                                {"provider_kind": "openai-compatible", "terminal_reason": "provider_error", "status": 429, "classification": {"kind": "Http", "retryable": true, "status": 429}},
                                {"provider_kind": "openai", "terminal_reason": "provider_error", "status": 429, "classification": {"kind": "Http", "retryable": true, "status": 429}},
                                {"provider_kind": "anthropic", "terminal_reason": "provider_error", "status": 429, "classification": {"kind": "Http", "retryable": true, "status": 429}},
                                {"provider_kind": "google_oauth", "terminal_reason": "provider_error", "status": 429, "classification": {"kind": "Http", "retryable": true, "status": 429}}
                            ]
                        }
                    }
                }),
            ),
            delivered_with_payload(
                19,
                "session-001:provider-mutation:003",
                "session-001",
                BoundaryKind::ProviderMutation,
                json!({"runtime_completion": runtime_completion("provider_script_mutation", 19)}),
                json!({
                    "mutation": "dropped_terminal_event",
                    "provider_parser_matrix": {
                        "matrix": {
                            "real_provider_parser_execution": true,
                            "provider_kinds": [
                                "anthropic",
                                "google_oauth",
                                "openai",
                                "openai-compatible"
                            ],
                            "proofs": [
                                {"provider_kind": "openai-compatible", "terminal_reason": "provider_error", "classification": {"kind": "Unknown", "retryable": true, "terminal_reason": "provider_error"}},
                                {"provider_kind": "openai", "terminal_reason": "provider_error", "classification": {"kind": "Unknown", "retryable": true, "terminal_reason": "provider_error"}},
                                {"provider_kind": "anthropic", "terminal_reason": "provider_error", "classification": {"kind": "Unknown", "retryable": true, "terminal_reason": "provider_error"}},
                                {"provider_kind": "google_oauth", "terminal_reason": "provider_error", "classification": {"kind": "Unknown", "retryable": true, "terminal_reason": "provider_error"}}
                            ]
                        }
                    }
                }),
            ),
            delivered_with_payload(
                20,
                "session-001:provider:003:provider-event:001:sse",
                "session-001",
                BoundaryKind::ProviderEvent,
                json!({
                    "turn_boundary_id": "session-001:provider:003",
                    "event_index": 1,
                    "event_name": "sse",
                }),
                json!({
                    "provider_event_release": true,
                    "released_while_turn_pending": true,
                    "turn_boundary_id": "session-001:provider:003",
                }),
            ),
            delivered_with_payload(
                21,
                "session-001:provider:003",
                "session-001",
                BoundaryKind::Provider,
                json!({
                    "text": "answer for session-001 turn 3",
                    "runtime_completion": runtime_completion("provider_turn_completion", 21),
                    "expected_provider_exchange_count": 3,
                }),
                json!({
                    "provider_kind": "openai-compatible",
                    "provider_output": "answer for session-001 turn 3",
                    "success": true,
                    "provider_exchange_count": 3,
                    "runtime_contract": {"status": "passed"},
                }),
            ),
            delivered_with_payload(
                22,
                "session-001:contract-execution:standard-max-turn-after-tool-result",
                "session-001",
                BoundaryKind::Trigger,
                json!({
                    "session": "session-001",
                    "source_key": "contract-execution/session-001/standard-max-turn-after-tool-result",
                    "started_process": false,
                    "contract_execution": standard_max_turn_execution_fixture()
                }),
                json!({
                    "session": "session-001",
                    "trigger_delivered": true,
                    "source_key": "contract-execution/session-001/standard-max-turn-after-tool-result",
                    "occurrence_id": "trigger:contract-execution-standard-max-turn-after-tool-result",
                    "reservation_count": 1,
                    "started_process": false,
                    "contract_execution": standard_max_turn_execution_fixture()
                }),
            ),
        ];
        let mut events: Vec<DeliveredBoundary> = base.into();
        events.extend(contract_execution_fixture_events(24));
        events
    }

    fn standard_max_turn_execution_fixture() -> serde_json::Value {
        let mut execution =
            replay_contract_execution_fixture("standard.max_turns_after_tool_result");
        execution
            .as_object_mut()
            .expect("contract execution object")
            .insert(
                "generated_anchor".to_string(),
                json!({
                    "tool_boundary": "session-001:tool:001",
                    "continuation_provider_boundary": "session-001:provider:003",
                    "actor": "session-001",
                    "tool_sequence": 12,
                    "continuation_provider_sequence": 21,
                    "same_actor_continuation": true
                }),
            );
        execution
    }

    fn replay_contract_execution_fixture(contract: &str) -> serde_json::Value {
        crate::runner::replay_contract_execution(contract).unwrap_or_else(|err| {
            panic!("fixed contract execution fixture `{contract}` failed: {err}")
        })
    }

    fn contract_execution_fixture_events(start_sequence: usize) -> Vec<DeliveredBoundary> {
        [
            "standard.initial_request_projection",
            "standard.empty_provider_response_error",
            "standard.provider_error_without_checkpoint",
            "standard.native_tool_loop_reenters_model",
            "standard.parallel_tool_results_checkpoint_once",
            "standard.tool_failure_feedback_reenters_model",
            "standard.streamed_text_finalizes_once",
            "rlm.natural_prose_finalizes",
            "rlm.typed_prose_requires_finish",
            "rlm.finish_required_max_turn_stop",
            "rlm.exec_error_max_turn_stop",
            "rlm.typed_finish_emits_outcome_and_done",
            "rlm.finish_required_diagnostic_counts",
            "rlm.natural_diagnostic_counts",
            "rlm.cell_diagnostic_counts",
            "rlm.retired_marker_plain_lashlang_text",
            "rlm.lashlang_cell_exec_continues",
            "rlm.streamed_lashlang_cell_exec_persists_trajectory",
            "rlm.empty_options_natural_default",
            "rlm.exec_result_no_tool_call_replay",
            "rlm.exec_tool_control_frame_switch_terminal",
            "rlm.exec_tool_control_fail_terminal",
            "rlm.natural_allows_finish_value",
            "rlm.typed_schema_mismatch_repair_loop",
            "rlm.typed_schema_any_of_mismatch",
            "agent.foreground_tool_call_round_trip",
            "agent.started_process_tool_call_graph",
            "agent.durable_input_suspension_resolution",
            "agent.shell_results_are_data",
            "agent.shell_output_print_projection_survives",
            "agent.started_process_subagent_spawn",
            "agent.nested_process_start_await",
            "agent.session_turn_process_child",
            "agent.failed_child_preserves_failure_graph",
            "agent.parallel_spawn_and_join",
            "agent.tuple_values_finish_as_json_arrays",
        ]
        .into_iter()
        .enumerate()
        .map(|(offset, contract)| {
            contract_execution_fixture_event(start_sequence + offset, contract)
        })
        .collect()
    }

    fn contract_execution_fixture_event(sequence: usize, contract: &str) -> DeliveredBoundary {
        let proof_id = contract.replace(['.', '_'], "-");
        let boundary_id = format!("session-001:contract-execution:{proof_id}");
        let source_key = format!("contract-execution/session-001/{proof_id}");
        let execution = replay_contract_execution_fixture(contract);
        delivered_with_payload(
            sequence,
            &boundary_id,
            "session-001",
            BoundaryKind::Trigger,
            json!({
                "session": "session-001",
                "source_key": source_key,
                "started_process": false,
                "contract_execution": execution.clone(),
            }),
            json!({
                "session": "session-001",
                "trigger_delivered": true,
                "source_key": source_key,
                "occurrence_id": format!("trigger:contract-execution-{proof_id}"),
                "reservation_count": 1,
                "started_process": false,
                "contract_execution": execution,
            }),
        )
    }

    fn delivered_with_payload(
        sequence: usize,
        boundary_id: &str,
        actor_alias: &str,
        kind: BoundaryKind,
        payload: serde_json::Value,
        observed: serde_json::Value,
    ) -> DeliveredBoundary {
        DeliveredBoundary {
            schema: crate::scheduler::BOUNDARY_EVENT_SCHEMA.to_string(),
            sequence,
            scheduler: SchedulerDeliveryEvidence {
                scheduler_controlled: true,
                delivered_at: sequence as u64,
                ..SchedulerDeliveryEvidence::default()
            },
            boundary_id: boundary_id.to_string(),
            actor_alias: actor_alias.to_string(),
            kind,
            at: sequence as u64,
            label: format!("{kind:?}"),
            payload,
            observed,
        }
    }

    fn runtime_completion(family: &str, ready_at: u64) -> serde_json::Value {
        json!({
            "completion_family": family,
            "completion_units": [
                {
                    "unit": format!("runtime:{family}"),
                    "at": ready_at
                }
            ],
            "ready_at": ready_at,
            "registered_after": "session-001:ingress"
        })
    }

    fn provider_mutation_observed(mutation: &str) -> serde_json::Value {
        json!({
            "mutation": mutation,
            "provider_parser_matrix": {
                "matrix": {
                    "real_provider_parser_execution": true,
                    "provider_kinds": [
                        "anthropic",
                        "google_oauth",
                        "openai",
                        "openai-compatible"
                    ]
                }
            }
        })
    }

    fn provider_event(sequence: usize, actor: &str, turn_boundary_id: &str) -> DeliveredBoundary {
        delivered_with_payload(
            sequence,
            &format!("{turn_boundary_id}:event:{sequence}"),
            actor,
            BoundaryKind::ProviderEvent,
            json!({ "turn_boundary_id": turn_boundary_id }),
            json!({}),
        )
    }

    fn provider_completion(
        sequence: usize,
        actor: &str,
        turn_boundary_id: &str,
    ) -> DeliveredBoundary {
        delivered_with_payload(
            sequence,
            turn_boundary_id,
            actor,
            BoundaryKind::Provider,
            json!({ "provider_kind": "openai" }),
            json!({ "provider_kind": "openai" }),
        )
    }

    #[test]
    fn interleaving_oracle_passes_when_two_sessions_overlap() {
        // Turn A and turn B are both live (each has released a provider event)
        // before either completes.
        let events = vec![
            provider_event(0, "session-a", "turn-a"),
            provider_event(1, "session-b", "turn-b"),
            provider_completion(2, "session-a", "turn-a"),
            provider_completion(3, "session-b", "turn-b"),
        ];
        assert_eq!(peak_concurrent_live_turns(&events), 2);
        assert!(provider_turn_interleaving_depth(&events).is_passed());
    }

    #[test]
    fn interleaving_oracle_fails_when_multi_session_turns_never_overlap() {
        // Two sessions each run a turn, but each turn completes before the next
        // one releases an event, so peak concurrency is 1.
        let events = vec![
            provider_event(0, "session-a", "turn-a"),
            provider_completion(1, "session-a", "turn-a"),
            provider_event(2, "session-b", "turn-b"),
            provider_completion(3, "session-b", "turn-b"),
        ];
        assert_eq!(peak_concurrent_live_turns(&events), 1);
        let verdict = provider_turn_interleaving_depth(&events);
        assert!(!verdict.is_passed());
        assert_eq!(verdict.oracle_id, PROVIDER_TURN_INTERLEAVING_ORACLE);
    }

    #[test]
    fn interleaving_oracle_passes_vacuously_for_single_session() {
        let events = vec![
            provider_event(0, "session-a", "turn-a"),
            provider_completion(1, "session-a", "turn-a"),
        ];
        assert_eq!(peak_concurrent_live_turns(&events), 1);
        assert!(provider_turn_interleaving_depth(&events).is_passed());
    }

    fn suspend_resume_event(suspended_before: bool, before: u64, after: u64) -> DeliveredBoundary {
        delivered_with_payload(
            0,
            "suspend-tool:suspend-resume:001",
            "suspend-tool",
            BoundaryKind::Tool,
            json!({ "suspend_resume": true, "tool": "await_tool", "output": {"ok": true} }),
            json!({
                "session": "suspend-tool",
                "tool_output": {"ok": true},
                "runtime_suspend": {
                    "suspend_kind": "tool",
                    "turn_suspended_before_completion": suspended_before,
                    "scheduler_delivered_completion": true,
                    "resolve_accepted": true,
                    "resumed_after_completion": after > before,
                    "completed_event_count_before_resolution": before,
                    "completed_event_count_after_resolution": after,
                    "final_assistant_message": "resumed",
                },
            }),
        )
    }

    #[test]
    fn suspend_resume_oracle_passes_when_turn_parked_then_resumed() {
        let events = vec![suspend_resume_event(true, 0, 1)];
        assert!(generated_suspend_resume(&events).is_passed());
    }

    #[test]
    fn suspend_resume_oracle_fails_when_turn_ran_synchronously() {
        // The tool completed before the scheduler delivered the completion
        // boundary: the turn never actually parked.
        let events = vec![suspend_resume_event(false, 1, 1)];
        let verdict = generated_suspend_resume(&events);
        assert!(!verdict.is_passed());
        assert_eq!(verdict.oracle_id, GENERATED_SUSPEND_RESUME_ORACLE);
    }

    #[test]
    fn suspend_resume_oracle_fails_when_the_suspend_class_is_absent() {
        // Anti-vacuity: with no suspend-resume boundary present, the oracle must
        // FAIL rather than pass on an absent class.
        let events = vec![provider_completion(0, "session-a", "turn-a")];
        let verdict = generated_suspend_resume(&events);
        assert!(!verdict.is_passed());
        assert!(verdict.message.contains("class is absent"));
    }
}
