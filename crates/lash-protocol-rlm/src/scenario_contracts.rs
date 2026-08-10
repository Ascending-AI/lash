use lash_core::runtime::ScenarioContractSpec;

pub const RLM_PROTOCOL_SCENARIO_CONTRACTS: &[ScenarioContractSpec] = &[
    ScenarioContractSpec {
        suite: "rlm",
        test_name: "rlm_protocol_scenario_prose_only_response_finishes_by_default",
        owned_invariant: "Natural RLM prose-only response finalizes by default.",
        semantic_oracle: "rlm.natural_prose_finalizes",
        required_sim_evidence: &["provider_turn"],
        oracle_id: "sim.oracle.scenario.rlm-contract.v1",
    },
    ScenarioContractSpec {
        suite: "rlm",
        test_name: "rlm_protocol_scenario_typed_prose_only_response_requests_finish",
        owned_invariant: "Typed RLM output requires explicit finish rather than prose-only success.",
        semantic_oracle: "rlm.typed_prose_requires_finish",
        required_sim_evidence: &["trigger", "provider_turn"],
        oracle_id: "sim.oracle.scenario.rlm-contract.v1",
    },
    ScenarioContractSpec {
        suite: "rlm",
        test_name: "rlm_protocol_scenario_finish_required_prose_at_max_turns_stops_without_retry_prompt",
        owned_invariant: "Finish-required prose stops at max turns without an extra retry prompt.",
        semantic_oracle: "rlm.finish_required_max_turn_stop",
        required_sim_evidence: &["provider_turn"],
        oracle_id: "sim.oracle.scenario.rlm-contract.v1",
    },
    ScenarioContractSpec {
        suite: "rlm",
        test_name: "rlm_protocol_scenario_finish_required_exec_error_at_max_turns_stops_without_retry",
        owned_invariant: "Exec errors at max turns stop cleanly without another repair turn.",
        semantic_oracle: "rlm.exec_error_max_turn_stop",
        required_sim_evidence: &["exec_code"],
        oracle_id: "sim.oracle.scenario.rlm-contract.v1",
    },
    ScenarioContractSpec {
        suite: "rlm",
        test_name: "rlm_protocol_scenario_finish_required_prose_only_diagnostic_has_clean_counts",
        owned_invariant: "Finish-required diagnostics classify prose-only responses with clean counts.",
        semantic_oracle: "rlm.finish_required_diagnostic_counts",
        required_sim_evidence: &["provider_turn"],
        oracle_id: "sim.oracle.scenario.rlm-contract.v1",
    },
    ScenarioContractSpec {
        suite: "rlm",
        test_name: "rlm_protocol_scenario_natural_prose_only_diagnostic_has_clean_counts",
        owned_invariant: "Natural diagnostics classify prose-only responses with clean counts.",
        semantic_oracle: "rlm.natural_diagnostic_counts",
        required_sim_evidence: &["provider_turn"],
        oracle_id: "sim.oracle.scenario.rlm-contract.v1",
    },
    ScenarioContractSpec {
        suite: "rlm",
        test_name: "rlm_protocol_scenario_cell_reasoning_prose_code_diagnostic_has_clean_counts",
        owned_invariant: "Mixed reasoning/prose/code diagnostics keep separate counts.",
        semantic_oracle: "rlm.cell_diagnostic_counts",
        required_sim_evidence: &["exec_code", "provider_turn"],
        oracle_id: "sim.oracle.scenario.rlm-contract.v1",
    },
    ScenarioContractSpec {
        suite: "rlm",
        test_name: "rlm_protocol_scenario_retired_percent_marker_inside_source_is_plain_lashlang_text",
        owned_invariant: "Retired percent cell marker remains Lashlang source text, not a control marker.",
        semantic_oracle: "rlm.retired_marker_plain_lashlang_text",
        required_sim_evidence: &["exec_code"],
        oracle_id: "sim.oracle.scenario.rlm-contract.v1",
    },
    ScenarioContractSpec {
        suite: "rlm",
        test_name: "rlm_protocol_scenario_lashlang_cell_runs_exec_and_continues",
        owned_invariant: "Lashlang cell execution feeds the protocol loop and continues.",
        semantic_oracle: "rlm.lashlang_cell_exec_continues",
        required_sim_evidence: &["exec_code", "provider_turn"],
        oracle_id: "sim.oracle.scenario.rlm-contract.v1",
    },
    ScenarioContractSpec {
        suite: "rlm",
        test_name: "rlm_protocol_scenario_streamed_lashlang_cell_runs_exec_and_persists_trajectory",
        owned_invariant: "A complete streamed Lashlang cell executes and persists trajectory events.",
        semantic_oracle: "rlm.streamed_lashlang_cell_exec_persists_trajectory",
        required_sim_evidence: &["exec_code", "provider_event", "provider_turn"],
        oracle_id: "sim.oracle.scenario.rlm-contract.v1",
    },
    ScenarioContractSpec {
        suite: "rlm",
        test_name: "rlm_protocol_scenario_empty_turn_options_use_natural_default",
        owned_invariant: "Empty RLM turn options use the natural default.",
        semantic_oracle: "rlm.empty_options_natural_default",
        required_sim_evidence: &["provider_turn"],
        oracle_id: "sim.oracle.scenario.rlm-contract.v1",
    },
    ScenarioContractSpec {
        suite: "rlm",
        test_name: "rlm_protocol_scenario_exec_result_emits_accounting_without_storing_tool_call_ids",
        owned_invariant: "Exec result feedback avoids tool-call id storage and synthetic tool replay.",
        semantic_oracle: "rlm.exec_result_no_tool_call_replay",
        required_sim_evidence: &["exec_code"],
        oracle_id: "sim.oracle.scenario.rlm-contract.v1",
    },
    ScenarioContractSpec {
        suite: "rlm",
        test_name: "rlm_protocol_scenario_exec_any_tool_control_frame_switch_is_terminal",
        owned_invariant: "Tool control frame-switch from exec is terminal.",
        semantic_oracle: "rlm.exec_tool_control_frame_switch_terminal",
        required_sim_evidence: &["exec_code", "trigger"],
        oracle_id: "sim.oracle.scenario.rlm-contract.v1",
    },
    ScenarioContractSpec {
        suite: "rlm",
        test_name: "rlm_protocol_scenario_exec_any_tool_control_fail_is_terminal_error",
        owned_invariant: "Tool control fail from exec is a terminal error.",
        semantic_oracle: "rlm.exec_tool_control_fail_terminal",
        required_sim_evidence: &["exec_code", "backend_failure"],
        oracle_id: "sim.oracle.scenario.rlm-contract.v1",
    },
    ScenarioContractSpec {
        suite: "rlm",
        test_name: "rlm_protocol_scenario_typed_finish_emits_turn_outcome_and_done",
        owned_invariant: "Typed finish emits both turn outcome and done status.",
        semantic_oracle: "rlm.typed_finish_emits_outcome_and_done",
        required_sim_evidence: &["final_value"],
        oracle_id: "sim.oracle.scenario.rlm-contract.v1",
    },
    ScenarioContractSpec {
        suite: "rlm",
        test_name: "rlm_protocol_scenario_natural_allows_finish_value",
        owned_invariant: "Natural RLM mode accepts an explicit finish value.",
        semantic_oracle: "rlm.natural_allows_finish_value",
        required_sim_evidence: &["provider_turn"],
        oracle_id: "sim.oracle.scenario.rlm-contract.v1",
    },
    ScenarioContractSpec {
        suite: "rlm",
        test_name: "rlm_protocol_scenario_typed_schema_mismatch_loops_with_feedback",
        owned_invariant: "Typed schema mismatch loops with repair feedback.",
        semantic_oracle: "rlm.typed_schema_mismatch_repair_loop",
        required_sim_evidence: &["provider_mutation", "provider_turn"],
        oracle_id: "sim.oracle.scenario.rlm-contract.v1",
    },
    ScenarioContractSpec {
        suite: "rlm",
        test_name: "rlm_protocol_scenario_typed_schema_mismatch_checks_any_of",
        owned_invariant: "Typed schema validation checks anyOf mismatches.",
        semantic_oracle: "rlm.typed_schema_any_of_mismatch",
        required_sim_evidence: &["provider_mutation", "provider_turn"],
        oracle_id: "sim.oracle.scenario.rlm-contract.v1",
    },
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn rlm_scenario_contract_metadata_is_unique_and_complete() {
        assert_eq!(RLM_PROTOCOL_SCENARIO_CONTRACTS.len(), 18);
        let mut names = BTreeSet::new();
        for contract in RLM_PROTOCOL_SCENARIO_CONTRACTS {
            assert_eq!(contract.suite, "rlm");
            assert!(contract.test_name.starts_with("rlm_protocol_scenario_"));
            assert!(!contract.owned_invariant.trim().is_empty());
            assert!(contract.semantic_oracle.starts_with("rlm."));
            assert!(!contract.required_sim_evidence.is_empty());
            assert_eq!(contract.oracle_id, "sim.oracle.scenario.rlm-contract.v1");
            assert!(names.insert(contract.test_name));
        }
    }
}
