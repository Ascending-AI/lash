#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub struct ScenarioContractSpec {
    pub suite: &'static str,
    pub test_name: &'static str,
    pub owned_invariant: &'static str,
    pub semantic_oracle: &'static str,
    pub required_sim_evidence: &'static [&'static str],
    pub oracle_id: &'static str,
}

const RUNTIME_REQUIRED_EVIDENCE: &[&str] = &[
    "queued_ingress",
    "cancellation",
    "process_wake",
    "worker_stale_completion",
    "lease_time",
];

pub const RUNTIME_SCENARIO_CONTRACTS: &[ScenarioContractSpec] = &[
    ScenarioContractSpec {
        suite: "runtime",
        test_name: "runtime_scenario_drains_command_before_turn_work_and_commits_checkpoint",
        owned_invariant: "Session-command gate, checkpoint persistence, stale queue completion rejection, final queue drain.",
        semantic_oracle: "runtime.command_before_turn_work",
        required_sim_evidence: RUNTIME_REQUIRED_EVIDENCE,
        oracle_id: "sim.oracle.scenario.runtime-contract.v1",
    },
    ScenarioContractSpec {
        suite: "runtime",
        test_name: "runtime_scenario_command_only_queue_drain_completes_without_turn_work",
        owned_invariant: "Command-only queued work claims no turn work and explicitly commits.",
        semantic_oracle: "runtime.command_only_queue_drain",
        required_sim_evidence: RUNTIME_REQUIRED_EVIDENCE,
        oracle_id: "sim.oracle.scenario.runtime-contract.v1",
    },
    ScenarioContractSpec {
        suite: "runtime",
        test_name: "runtime_scenario_queued_work_claim_keeps_pending_next_turn_input",
        owned_invariant: "Queued turn work does not consume pending next-turn input.",
        semantic_oracle: "runtime.queued_work_keeps_pending_input",
        required_sim_evidence: RUNTIME_REQUIRED_EVIDENCE,
        oracle_id: "sim.oracle.scenario.runtime-contract.v1",
    },
    ScenarioContractSpec {
        suite: "runtime",
        test_name: "runtime_scenario_claims_queued_turn_input_and_completes_it",
        owned_invariant: "Next-turn pending inputs are claimed, hidden while live, and completed by commit.",
        semantic_oracle: "runtime.queued_turn_input_completion",
        required_sim_evidence: RUNTIME_REQUIRED_EVIDENCE,
        oracle_id: "sim.oracle.scenario.runtime-contract.v1",
    },
    ScenarioContractSpec {
        suite: "runtime",
        test_name: "runtime_scenario_observation_replay_keeps_original_turn_input",
        owned_invariant: "Source-key observation replay preserves the original live input payload and id.",
        semantic_oracle: "runtime.observation_replay_preserves_input",
        required_sim_evidence: RUNTIME_REQUIRED_EVIDENCE,
        oracle_id: "sim.oracle.scenario.runtime-contract.v1",
    },
    ScenarioContractSpec {
        suite: "runtime",
        test_name: "runtime_scenario_defers_checkpoint_turn_input_and_respects_cancel",
        owned_invariant: "Active-turn input deferral, cancellation after deferral, and no later idle claim.",
        semantic_oracle: "runtime.checkpoint_redrive_cancel",
        required_sim_evidence: RUNTIME_REQUIRED_EVIDENCE,
        oracle_id: "sim.oracle.scenario.runtime-contract.v1",
    },
    ScenarioContractSpec {
        suite: "runtime",
        test_name: "runtime_scenario_commits_after_advisory_session_lease_release",
        owned_invariant: "A released advisory lease permits a current-head commit while the head CAS rejects stale state.",
        semantic_oracle: "runtime.advisory_lease_head_cas",
        required_sim_evidence: RUNTIME_REQUIRED_EVIDENCE,
        oracle_id: "sim.oracle.scenario.runtime-contract.v1",
    },
    ScenarioContractSpec {
        suite: "runtime",
        test_name: "runtime_scenario_waits_for_stale_session_lease_ttl",
        owned_invariant: "An unexpired stale holder stays busy; TTL expiry advances the fence and the successor stays protected.",
        semantic_oracle: "runtime.stale_lease_ttl",
        required_sim_evidence: RUNTIME_REQUIRED_EVIDENCE,
        oracle_id: "sim.oracle.scenario.runtime-contract.v1",
    },
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn runtime_scenario_contract_metadata_is_unique_and_complete() {
        assert_eq!(RUNTIME_SCENARIO_CONTRACTS.len(), 8);
        let mut names = BTreeSet::new();
        for contract in RUNTIME_SCENARIO_CONTRACTS {
            assert_eq!(contract.suite, "runtime");
            assert!(contract.test_name.starts_with("runtime_scenario_"));
            assert!(!contract.owned_invariant.trim().is_empty());
            assert!(contract.semantic_oracle.starts_with("runtime."));
            assert!(!contract.required_sim_evidence.is_empty());
            assert_eq!(
                contract.oracle_id,
                "sim.oracle.scenario.runtime-contract.v1"
            );
            assert!(names.insert(contract.test_name));
        }
    }
}
