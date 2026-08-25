use lash_core::runtime::ScenarioContractSpec;

/// Canonical agent scenario contracts used by facade consumers.
pub const AGENT_SCENARIO_CONTRACTS: &[ScenarioContractSpec] = &[
    ScenarioContractSpec {
        suite: "agent",
        test_name: "agent_scenario_foreground_labeled_tool_call",
        owned_invariant: "Facade root turn, app tool execution, label graph, final value, and remote DTO round trip.",
        semantic_oracle: "agent.foreground_tool_call_round_trip",
        required_sim_evidence: &["tool_result", "provider_turn"],
        oracle_id: "sim.oracle.scenario.agent-contract.v1",
    },
    ScenarioContractSpec {
        suite: "agent",
        test_name: "agent_scenario_started_process_labeled_tool_call",
        owned_invariant: "Started Lashlang process calling an app tool with process graph completion.",
        semantic_oracle: "agent.started_process_tool_call_graph",
        required_sim_evidence: &["process_wake", "tool_result", "provider_turn"],
        oracle_id: "sim.oracle.scenario.agent-contract.v1",
    },
    ScenarioContractSpec {
        suite: "agent",
        test_name: "agent_scenario_process_durable_input_request_tool",
        owned_invariant: "Live durable input suspension, external resolution, process event, and final value.",
        semantic_oracle: "agent.durable_input_suspension_resolution",
        required_sim_evidence: &["durable_effect", "process_wake", "observer_reconnect"],
        oracle_id: "sim.oracle.scenario.agent-contract.v1",
    },
    ScenarioContractSpec {
        suite: "agent",
        test_name: "agent_scenario_shell_nonzero_and_pipeline_results_are_data",
        owned_invariant: "Shell failures and pipelines remain data at the facade boundary.",
        semantic_oracle: "agent.shell_results_are_data",
        required_sim_evidence: &["exec_code", "tool_result"],
        oracle_id: "sim.oracle.scenario.agent-contract.v1",
    },
    ScenarioContractSpec {
        suite: "agent",
        test_name: "agent_scenario_shell_output_survives_print_projection_in_variable",
        owned_invariant: "Large shell output survives print projection and remains addressable.",
        semantic_oracle: "agent.shell_output_print_projection_survives",
        required_sim_evidence: &["exec_code", "provider_turn"],
        oracle_id: "sim.oracle.scenario.agent-contract.v1",
    },
    ScenarioContractSpec {
        suite: "agent",
        test_name: "agent_scenario_started_process_labeled_subagent_spawn",
        owned_invariant: "Started process spawns a subagent and records child session execution graphs.",
        semantic_oracle: "agent.started_process_subagent_spawn",
        required_sim_evidence: &["process_wake", "multi_session"],
        oracle_id: "sim.oracle.scenario.agent-contract.v1",
    },
    ScenarioContractSpec {
        suite: "agent",
        test_name: "agent_scenario_nested_process_start_await",
        owned_invariant: "Nested process start/await produces deterministic process ids and graph lineage.",
        semantic_oracle: "agent.nested_process_start_await",
        required_sim_evidence: &["process_wake", "multi_session"],
        oracle_id: "sim.oracle.scenario.agent-contract.v1",
    },
    ScenarioContractSpec {
        suite: "agent",
        test_name: "agent_scenario_session_turn_process_child",
        owned_invariant: "Host session-turn process API creates and awaits a child session turn.",
        semantic_oracle: "agent.session_turn_process_child",
        required_sim_evidence: &["process_wake", "provider_turn", "multi_session"],
        oracle_id: "sim.oracle.scenario.agent-contract.v1",
    },
    ScenarioContractSpec {
        suite: "agent",
        test_name: "agent_scenario_failed_child_preserves_failure_graph",
        owned_invariant: "Child failure path preserves failure graph and avoids provider-exhaustion false failures.",
        semantic_oracle: "agent.failed_child_preserves_failure_graph",
        required_sim_evidence: &[
            "worker_stale_completion",
            "backend_failure",
            "multi_session",
        ],
        oracle_id: "sim.oracle.scenario.agent-contract.v1",
    },
    ScenarioContractSpec {
        suite: "agent",
        test_name: "agent_scenario_parallel_spawn_and_join",
        owned_invariant: "Parallel process starts join deterministically with unique process ids.",
        semantic_oracle: "agent.parallel_spawn_and_join",
        required_sim_evidence: &["process_wake", "worker_stale_completion", "multi_session"],
        oracle_id: "sim.oracle.scenario.agent-contract.v1",
    },
    ScenarioContractSpec {
        suite: "agent",
        test_name: "agent_scenario_tuple_values_finish_as_json_arrays",
        owned_invariant: "Facade final values preserve tuple-to-JSON array projection.",
        semantic_oracle: "agent.tuple_values_finish_as_json_arrays",
        required_sim_evidence: &["final_value"],
        oracle_id: "sim.oracle.scenario.agent-contract.v1",
    },
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn agent_scenario_contract_metadata_is_unique_and_complete() {
        assert_eq!(AGENT_SCENARIO_CONTRACTS.len(), 11);
        let mut names = BTreeSet::new();
        for contract in AGENT_SCENARIO_CONTRACTS {
            assert_eq!(contract.suite, "agent");
            assert!(contract.test_name.starts_with("agent_scenario_"));
            assert!(!contract.owned_invariant.trim().is_empty());
            assert!(contract.semantic_oracle.starts_with("agent."));
            assert!(!contract.required_sim_evidence.is_empty());
            assert_eq!(contract.oracle_id, "sim.oracle.scenario.agent-contract.v1");
            assert!(names.insert(contract.test_name));
        }
    }
}
