use lash_core::runtime::ScenarioContractSpec;

pub const STANDARD_PROTOCOL_SCENARIO_CONTRACTS: &[ScenarioContractSpec] = &[
    ScenarioContractSpec {
        suite: "standard",
        test_name: "standard_protocol_scenario_projects_initial_request",
        owned_invariant: "Standard protocol projects user/system input into the first model request.",
        semantic_oracle: "standard.initial_request_projection",
        required_sim_evidence: &["provider_turn", "runtime_session_graph"],
        oracle_id: "sim.oracle.scenario.standard-contract.v1",
    },
    ScenarioContractSpec {
        suite: "standard",
        test_name: "standard_protocol_scenario_empty_model_response_stops_provider_error",
        owned_invariant: "Empty provider response terminates through the protocol error boundary.",
        semantic_oracle: "standard.empty_provider_response_error",
        required_sim_evidence: &["provider_mutation"],
        oracle_id: "sim.oracle.scenario.standard-contract.v1",
    },
    ScenarioContractSpec {
        suite: "standard",
        test_name: "standard_protocol_scenario_provider_error_stops_without_checkpoint",
        owned_invariant: "Provider errors stop without committing a protocol checkpoint.",
        semantic_oracle: "standard.provider_error_without_checkpoint",
        required_sim_evidence: &["provider_mutation"],
        oracle_id: "sim.oracle.scenario.standard-contract.v1",
    },
    ScenarioContractSpec {
        suite: "standard",
        test_name: "standard_protocol_scenario_native_tool_loop_reenters_model_after_checkpoint",
        owned_invariant: "Native tool calls checkpoint and re-enter the model loop.",
        semantic_oracle: "standard.native_tool_loop_reenters_model",
        required_sim_evidence: &["tool_result", "provider_turn"],
        oracle_id: "sim.oracle.scenario.standard-contract.v1",
    },
    ScenarioContractSpec {
        suite: "standard",
        test_name: "standard_protocol_scenario_parallel_tool_results_checkpoint_once",
        owned_invariant: "Parallel native tool results checkpoint once before protocol re-entry.",
        semantic_oracle: "standard.parallel_tool_results_checkpoint_once",
        required_sim_evidence: &["tool_result", "provider_event", "provider_turn"],
        oracle_id: "sim.oracle.scenario.standard-contract.v1",
    },
    ScenarioContractSpec {
        suite: "standard",
        test_name: "standard_protocol_scenario_tool_failure_feedback_reenters_model_after_checkpoint",
        owned_invariant: "Tool failure is converted into model feedback after checkpoint.",
        semantic_oracle: "standard.tool_failure_feedback_reenters_model",
        required_sim_evidence: &["tool_result", "provider_mutation", "provider_turn"],
        oracle_id: "sim.oracle.scenario.standard-contract.v1",
    },
    ScenarioContractSpec {
        suite: "standard",
        test_name: "standard_protocol_scenario_streamed_text_finishes_without_duplicate_delta",
        owned_invariant: "Streaming text projection emits a clean final response without duplicate deltas.",
        semantic_oracle: "standard.streamed_text_finalizes_once",
        required_sim_evidence: &["provider_event", "provider_turn", "observer_convergence"],
        oracle_id: "sim.oracle.scenario.standard-contract.v1",
    },
    ScenarioContractSpec {
        suite: "standard",
        test_name: "standard_protocol_scenario_max_turns_terminates_after_tool_result",
        owned_invariant: "Tool-result continuation terminates at max-turns with the expected final message.",
        semantic_oracle: "standard.max_turns_after_tool_result",
        required_sim_evidence: &["tool_result", "provider_turn", "max_turn_stop"],
        oracle_id: "sim.oracle.scenario.standard-contract.v1",
    },
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn standard_scenario_contract_metadata_is_unique_and_complete() {
        assert_eq!(STANDARD_PROTOCOL_SCENARIO_CONTRACTS.len(), 8);
        let mut names = BTreeSet::new();
        for contract in STANDARD_PROTOCOL_SCENARIO_CONTRACTS {
            assert_eq!(contract.suite, "standard");
            assert!(
                contract
                    .test_name
                    .starts_with("standard_protocol_scenario_")
            );
            assert!(!contract.owned_invariant.trim().is_empty());
            assert!(contract.semantic_oracle.starts_with("standard."));
            assert!(!contract.required_sim_evidence.is_empty());
            assert_eq!(
                contract.oracle_id,
                "sim.oracle.scenario.standard-contract.v1"
            );
            assert!(names.insert(contract.test_name));
        }
    }
}
