use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
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
use crate::store::CheckpointWriteEvent;
use crate::trace::{AbstractWorldSummary, OracleVerdict, WorkloadExpectations};

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
    pub turn_id: TurnId,
    pub inbound_claim_completed: bool,
    pub follow_on_enqueued: bool,
}

mod agent_contracts;
mod frame_switch;
mod live_provider;
mod mini_scenarios;
mod recovery_and_scheduling;
mod rlm_contracts;
mod runtime_observation;
mod semantic_laws;
mod standard_contracts;

#[cfg(test)]
mod tests;

use agent_contracts::*;
use frame_switch::*;
pub use frame_switch::{
    frame_switch_follow_on_precedes_pending, frame_switch_outbox_is_atomic, frame_switch_seeds,
    generated_final_value_semantic_channel, generated_suspend_resume,
    logical_turn_claims_settle_exactly_once,
};
pub use live_provider::{
    GENERATED_WORKLOAD_BATTERY_ORACLE, LIVE_PROVIDER_FAILURE_COVERAGE_ORACLE,
    LIVE_PROVIDER_FAILURE_ORACLE, LiveProviderFailureFacts, combine_oracles,
    live_provider_failure_coverage, live_provider_failure_terminalizes, passed_battery_verdict,
};
use mini_scenarios::*;
pub use mini_scenarios::{
    ScenarioContractGeneratedFact, ScenarioFactMemo, scenario_contract_generated_facts,
    scenario_contract_generated_facts_for_semantic, scenario_contract_generated_facts_with_memo,
    scenario_contract_oracles,
};
use recovery_and_scheduling::*;
pub use recovery_and_scheduling::{
    HEALTHY_LONG_TURN_LIVENESS_ORACLE, SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE_KINDS,
    WORKER_FAILOVER_CONTINUATION_ORACLE, abandoned_requires_evidence, durable_effect_exactly_once,
    healthy_long_turn_liveness, lease_time_monotonic, operational_coverage,
    process_never_double_started, scheduler_controlled_delivery,
    scheduler_owned_runtime_completions, state_machine_semantic_invariants,
    worker_failover_continues_work, worker_stale_completion_rejected,
};
use rlm_contracts::*;
use runtime_observation::*;
pub use runtime_observation::{
    backend_failure_observed, cancellation_observed, cross_session_isolation, exec_code_observed,
    generated_runtime_provider_matrix, ingress_sessions_opened, observer_convergence,
    observer_reconnect_observed, peak_concurrent_live_turns, process_wake_at_most_once,
    process_wake_observed, provider_mutation_rejected, provider_transport_mutation_classified,
    provider_turn_interleaving_depth, queued_ingress_observed, runtime_graph_acyclic,
    runtime_session_graph_contract, runtime_single_active_agent_frame, runtime_usage_monotonic,
    tool_boundary_observed, trigger_delivery_observed,
};
use semantic_laws::*;
pub use semantic_laws::{
    pending_tool_completion, replay_determinism, runtime_final_value_semantic,
    runtime_provider_turn,
};
use standard_contracts::*;

/// Evaluate every generated-workload oracle whose evidence is carried by a
/// [`SimulationTrace`](crate::trace::SimulationTrace).
///
/// The live-provider failure coverage oracle is intentionally absent: its live
/// turn facts are not serialized into the trace, so minimization can carry its
/// recorded verdict but cannot re-evaluate it after a shrink. Keeping the
/// remaining battery here makes the runner and minimizer share one ordering and
/// one definition instead of maintaining parallel lists.
pub fn generated_trace_oracles(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
    durable_writes: &[CheckpointWriteEvent],
    expectations: &WorkloadExpectations,
) -> Vec<OracleVerdict> {
    let mut oracles = Vec::new();
    visit_generated_trace_oracles(events, summary, durable_writes, expectations, |verdict| {
        oracles.push(verdict);
        true
    });
    oracles
}

/// Identity of a generated-workload oracle that is known BEFORE the oracle is
/// evaluated. Scenario-contract oracles are keyed by their static contract, so
/// a caller hunting one named verdict can decline to evaluate the rest; every
/// other oracle only becomes identifiable once it has been evaluated.
pub enum OracleSlot<'a> {
    Battery(&'static str),
    ScenarioContract(&'a ScenarioContractSpec),
}

impl OracleSlot<'_> {
    /// Whether this slot reports under `oracle_id`. Every generated-workload
    /// oracle has a fixed id: a battery entry declares the constant it reports
    /// under, and a scenario-contract entry derives its id from the static
    /// contract. Answering the question directly keeps a caller that asks it
    /// once per slot per candidate — the minimizer — from allocating the id.
    pub fn declares_oracle_id(&self, oracle_id: &str) -> bool {
        match self {
            Self::Battery(declared) => *declared == oracle_id,
            Self::ScenarioContract(contract) => oracle_id
                .strip_prefix(contract.oracle_id)
                .and_then(|rest| rest.strip_prefix(':'))
                .is_some_and(|test_name| test_name == contract.test_name),
        }
    }

    /// The verdict id this slot produces. The walk debug-asserts the evaluated
    /// verdict against this, so a slot whose oracle grows a second id fails the
    /// crate's own tests rather than silently making a skip decision wrong.
    pub fn declared_oracle_id(&self) -> String {
        match self {
            Self::Battery(oracle_id) => (*oracle_id).to_string(),
            Self::ScenarioContract(contract) => scenario_contract_oracle_id(contract),
        }
    }
}

/// Walk the generated-workload oracle battery in evaluation order, handing each
/// verdict to `visit` and stopping as soon as it returns `false`.
///
/// [`generated_trace_oracles`] is this walk collected into a vector, so the two
/// cannot drift: one ordering, one membership. `skip` is consulted BEFORE a
/// slot is evaluated and a skipped slot costs nothing — the minimizer uses it
/// to avoid re-deriving scenario-contract verdicts it has already proven cannot
/// change its answer (see `minimize::candidate_preserves_target`).
pub fn walk_generated_trace_oracles<S, V>(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
    durable_writes: &[CheckpointWriteEvent],
    expectations: &WorkloadExpectations,
    memo: &ScenarioFactMemo,
    mut skip: S,
    mut visit: V,
) where
    S: FnMut(&OracleSlot<'_>) -> bool,
    V: FnMut(OracleVerdict) -> bool,
{
    macro_rules! battery {
        ($oracle_id:expr, $verdict:expr) => {
            if !skip(&OracleSlot::Battery($oracle_id)) {
                let verdict = $verdict;
                debug_assert_eq!(
                    verdict.oracle_id, $oracle_id,
                    "battery slot declared an oracle id its verdict does not use"
                );
                if !visit(verdict) {
                    return;
                }
            }
        };
    }
    battery!(
        SCHEDULER_CONTROLLED_DELIVERY_ORACLE,
        scheduler_controlled_delivery(events)
    );
    battery!(
        SCHEDULER_OWNED_RUNTIME_COMPLETION_ORACLE,
        scheduler_owned_runtime_completions(events)
    );
    battery!(
        STATE_MACHINE_SEMANTIC_INVARIANTS_ORACLE,
        state_machine_semantic_invariants(events, summary)
    );
    battery!(
        OPERATIONAL_COVERAGE_ORACLE,
        operational_coverage(events, summary)
    );
    battery!(
        INGRESS_SESSION_OPENED_ORACLE,
        ingress_sessions_opened(summary, expectations)
    );
    battery!(
        QUEUED_INGRESS_ORACLE,
        queued_ingress_observed(summary, events)
    );
    battery!(CANCELLATION_ORACLE, cancellation_observed(summary, events));
    battery!(TRIGGER_ORACLE, trigger_delivery_observed(summary, events));
    battery!(
        OBSERVER_RECONNECT_ORACLE,
        observer_reconnect_observed(summary, events)
    );
    battery!(
        BACKEND_FAILURE_ORACLE,
        backend_failure_observed(summary, events)
    );
    battery!(
        PROVIDER_MUTATION_ORACLE,
        provider_mutation_rejected(summary, events)
    );
    battery!(
        PROVIDER_TRANSPORT_MUTATION_ORACLE,
        provider_transport_mutation_classified(events, expectations)
    );
    battery!(
        GENERATED_PROVIDER_MATRIX_ORACLE,
        generated_runtime_provider_matrix(events)
    );
    battery!(
        PROVIDER_TURN_INTERLEAVING_ORACLE,
        provider_turn_interleaving_depth(events, expectations)
    );
    battery!(PROCESS_WAKE_ORACLE, process_wake_observed(summary, events));
    battery!(
        PROCESS_WAKE_AT_MOST_ONCE_ORACLE,
        process_wake_at_most_once(events)
    );
    battery!(
        PROCESS_NEVER_DOUBLE_STARTED_ORACLE,
        process_never_double_started(events)
    );
    battery!(
        ABANDONED_REQUIRES_EVIDENCE_ORACLE,
        abandoned_requires_evidence(events)
    );
    battery!(
        TOOL_BOUNDARY_ORACLE,
        tool_boundary_observed(summary, events)
    );
    battery!(EXEC_CODE_ORACLE, exec_code_observed(summary, events));
    battery!(
        CROSS_SESSION_ISOLATION_ORACLE,
        cross_session_isolation(summary)
    );
    battery!(
        OBSERVER_CONVERGENCE_ORACLE,
        observer_convergence(summary, expectations)
    );
    battery!(
        RUNTIME_SESSION_GRAPH_ORACLE,
        runtime_session_graph_contract(summary, expectations)
    );
    battery!(
        RUNTIME_GRAPH_ACYCLIC_ORACLE,
        runtime_graph_acyclic(durable_writes)
    );
    battery!(
        RUNTIME_SINGLE_ACTIVE_AGENT_FRAME_ORACLE,
        runtime_single_active_agent_frame(events)
    );
    battery!(
        RUNTIME_USAGE_MONOTONIC_ORACLE,
        runtime_usage_monotonic(events)
    );
    battery!(
        crate::usage_oracle::RUNTIME_USAGE_CONSERVATION_ORACLE,
        crate::usage_oracle::checkpoint_usage_conservation(durable_writes)
    );
    battery!(
        DURABLE_EFFECT_EXACTLY_ONCE_ORACLE,
        durable_effect_exactly_once(summary)
    );
    battery!(
        WORKER_STALE_COMPLETION_ORACLE,
        worker_stale_completion_rejected(summary)
    );
    battery!(
        WORKER_FAILOVER_CONTINUATION_ORACLE,
        worker_failover_continues_work(events)
    );
    battery!(
        HEALTHY_LONG_TURN_LIVENESS_ORACLE,
        healthy_long_turn_liveness(events)
    );
    battery!(
        LEASE_TIME_MONOTONIC_ORACLE,
        lease_time_monotonic(events, expectations)
    );
    battery!(
        GENERATED_SUSPEND_RESUME_ORACLE,
        generated_suspend_resume(events)
    );
    battery!(
        GENERATED_FINAL_VALUE_ORACLE,
        generated_final_value_semantic_channel(events, expectations)
    );
    battery!(
        crate::state_checker::INDEPENDENT_CHECKPOINT_STATE_ORACLE,
        crate::state_checker::checkpoint_state_consistency(events, durable_writes, expectations)
    );
    // The mini scenario-contract oracles, one slot at a time so each declares
    // the id it reports under. This walk is their only list: evaluation order
    // decides which failure `combine_oracles` reports, so a second list would
    // be a second answer.
    battery!(
        SCENARIO_MINI_RUNTIME_QUEUED_HIDDEN_ORACLE,
        mini_runtime_queued_input_hidden(events)
    );
    battery!(
        SCENARIO_MINI_RUNTIME_CANCEL_IDLE_ORACLE,
        mini_runtime_cancellation_prevents_idle_claim(events)
    );
    battery!(
        SCENARIO_MINI_RUNTIME_PROCESS_WAKE_DEDUPE_ORACLE,
        mini_runtime_process_wake_duplicate_rejected(events)
    );
    battery!(
        SCENARIO_MINI_RUNTIME_STALE_LEASE_ORACLE,
        mini_runtime_stale_lease_commit_rejected(events, summary)
    );
    battery!(
        SCENARIO_MINI_STANDARD_STREAM_FINALIZE_ORACLE,
        mini_standard_streamed_text_finalizes_once(events)
    );
    battery!(
        SCENARIO_MINI_STANDARD_PROVIDER_ERROR_ORACLE,
        mini_standard_provider_error_without_checkpoint(events)
    );
    battery!(
        SCENARIO_MINI_STANDARD_TOOL_REENTRY_ORACLE,
        mini_standard_tool_loop_reenters(events)
    );
    battery!(
        SCENARIO_MINI_RLM_FINISH_REPAIR_ORACLE,
        mini_rlm_finish_required_prose_repair(events, summary)
    );
    battery!(
        SCENARIO_MINI_RLM_SCHEMA_REPAIR_ORACLE,
        mini_rlm_schema_mismatch_repair(events)
    );
    battery!(
        SCENARIO_MINI_RLM_CELL_EXEC_ORACLE,
        mini_rlm_lashlang_cell_exec_continues(events)
    );
    battery!(
        SCENARIO_MINI_AGENT_DURABLE_INPUT_ORACLE,
        mini_agent_durable_input_resolution(events)
    );
    battery!(
        SCENARIO_MINI_AGENT_CHILD_FAILURE_ORACLE,
        mini_agent_child_failure_graph(events, summary)
    );
    battery!(
        SCENARIO_MINI_AGENT_PARALLEL_JOIN_ORACLE,
        mini_agent_parallel_spawn_join(events, summary)
    );
    for contract in all_scenario_contracts() {
        if skip(&OracleSlot::ScenarioContract(contract)) {
            continue;
        }
        let verdict = scenario_contract_oracle(contract, events, summary, memo);
        debug_assert_eq!(
            verdict.oracle_id,
            scenario_contract_oracle_id(contract),
            "scenario-contract slot declared an oracle id its verdict does not use"
        );
        if !visit(verdict) {
            return;
        }
    }
}

/// [`walk_generated_trace_oracles`] with nothing skipped, against a fresh fact
/// memo. A caller that walks one trace repeatedly should hold its own memo and
/// call the walk directly.
pub fn visit_generated_trace_oracles<V>(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
    durable_writes: &[CheckpointWriteEvent],
    expectations: &WorkloadExpectations,
    visit: V,
) where
    V: FnMut(OracleVerdict) -> bool,
{
    walk_generated_trace_oracles(
        events,
        summary,
        durable_writes,
        expectations,
        &ScenarioFactMemo::default(),
        |_| false,
        visit,
    )
}
