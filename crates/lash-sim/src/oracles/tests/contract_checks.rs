use super::*;

#[test]
fn provider_counter_gap_round_trips_and_stays_on_original_turn() {
    let mut store = ModelStore::default();
    let mut events = Vec::new();
    for (turn, graph_node_count) in [(1, Some(3)), (2, None), (3, Some(7))] {
        let mut observed = json!({
            "provider_output": format!("answer for session-001 turn {turn}"),
            "provider_exchange_count": turn,
            "graph_node_count": graph_node_count,
            "transcript_message_count": turn * 2,
        });
        if graph_node_count.is_none() {
            observed
                .as_object_mut()
                .expect("object observation")
                .remove("graph_node_count");
        }
        let event = delivered_with_payload(
            turn as usize,
            &format!("provider-{turn}"),
            "session-001",
            BoundaryKind::Provider,
            json!({"text": format!("answer for session-001 turn {turn}")}),
            observed,
        );
        store.apply_observed_boundary(&event.as_event(), &event.observed);
        events.push(event);
    }
    let summary = store.summary();

    let verdict = runtime_session_graph_law(&summary, None);
    assert!(
        verdict.message.contains("turn 2 graph"),
        "the missing turn-2 graph count must fail turn 2, got: {}",
        verdict.message
    );

    let trace = SimulationTrace::new(
        1,
        "test-generator",
        "test",
        "1/1",
        "provider-counter-gap",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "test-script-bundle",
        WorkloadExpectations::default(),
        BTreeMap::new(),
        events,
        Vec::new(),
        verdict.clone(),
        vec![verdict],
        summary,
    );
    let directory = tempfile::tempdir().expect("temporary trace directory");
    let path = directory.path().join("trace.json");
    write_trace(&path, &trace).expect("write trace");
    let serialized: Value = serde_json::from_slice(
        &std::fs::read(&path).expect("read serialized trace for shape assertions"),
    )
    .expect("parse serialized trace");
    assert!(serialized.get("replay_command").is_none());
    let serialized_session = &serialized["final_summary"]["sessions"][0];
    assert!(serialized_session.get("provider_turns").is_some());
    assert!(serialized_session.get("provider_outputs").is_none());
    assert!(serialized_session.get("graph_node_counts").is_none());

    let mut legacy_shape = serialized;
    legacy_shape["final_summary"]["sessions"][0]
        .as_object_mut()
        .expect("serialized session object")
        .remove("provider_turns");
    assert!(serde_json::from_value::<SimulationTrace>(legacy_shape).is_err());

    let round_tripped = read_trace(&path).expect("read trace");
    let turns = &round_tripped.final_summary.sessions[0].provider_turns;
    assert_eq!(turns[0].graph_node_count, Some(3));
    assert_eq!(turns[1].graph_node_count, None);
    assert_eq!(turns[2].graph_node_count, Some(7));
    assert!(
        round_tripped.events[1]
            .observed
            .get("graph_node_count")
            .is_none()
    );

    let mut repaired = round_tripped.final_summary;
    repaired.sessions[0].provider_turns[1].graph_node_count = Some(5);
    assert!(runtime_session_graph_law(&repaired, None).is_passed());
}

#[test]
fn unmapped_scenario_semantics_fail_loudly_for_every_suite() {
    let summary = AbstractWorldView::with_digest(0, 0, vec![], vec![]);

    for (suite, verdict) in [
        (
            "runtime",
            runtime_contract_semantics("runtime.brand_new_contract", &[], &summary),
        ),
        (
            "standard",
            standard_contract_semantics(
                "standard.brand_new_contract",
                &[],
                &summary,
                &ScenarioFactMemo::default(),
            ),
        ),
        (
            "rlm",
            rlm_contract_semantics(
                "rlm.brand_new_contract",
                &[],
                &summary,
                &ScenarioFactMemo::default(),
            ),
        ),
        (
            "agent",
            agent_contract_semantics(
                "agent.brand_new_contract",
                &[],
                &summary,
                &ScenarioFactMemo::default(),
            ),
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
fn a_slot_claims_its_own_declared_oracle_id_and_no_other() {
    for contract in all_scenario_contracts() {
        let slot = OracleSlot::ScenarioContract(contract);
        let declared = slot.declared_oracle_id();
        assert!(slot.declares_oracle_id(&declared));
        assert!(!slot.declares_oracle_id(contract.oracle_id));
        assert!(!slot.declares_oracle_id(&format!("{declared}-suffix")));
    }

    let slot = OracleSlot::Battery(CANCELLATION_ORACLE);
    assert!(slot.declares_oracle_id(CANCELLATION_ORACLE.id));
    assert!(!slot.declares_oracle_id(EXEC_CODE_ORACLE.id));
}

#[test]
fn every_execution_fact_contract_resolves_to_its_registry_fact_spec_row() {
    // Exhaustiveness is proven against the imported registries, not a hand-
    // maintained count: adding a contract to a spec table without a fact-spec
    // row fails here, and a row naming a contract outside its family's table
    // fails to compile at `contract_spec` resolution.
    for contracts in [
        STANDARD_PROTOCOL_SCENARIO_CONTRACTS,
        RLM_PROTOCOL_SCENARIO_CONTRACTS,
        AGENT_SCENARIO_CONTRACTS,
    ] {
        for contract in contracts {
            if contract.semantic_oracle == NO_EXECUTION_FACT_CONTRACT.semantic_oracle {
                assert!(
                    contract_fact_row(contract.semantic_oracle).is_none(),
                    "{} is the named generated-boundary bypass and must not carry a fact-spec row",
                    contract.semantic_oracle
                );
                continue;
            }
            let (row, _) = contract_fact_row(contract.semantic_oracle).unwrap_or_else(|| {
                panic!(
                    "{} registry contract `{}` has no fact-spec row",
                    contract.suite, contract.semantic_oracle
                )
            });
            // The registries are `pub const` slices, so identity is proven by
            // the spec's fields, not by pointer equality.
            assert_eq!(
                row.spec.suite, contract.suite,
                "{} resolved to a fact-spec row bound to another suite's spec",
                contract.semantic_oracle
            );
            assert_eq!(
                row.spec.test_name, contract.test_name,
                "{} resolved to a fact-spec row bound to another registry spec",
                contract.semantic_oracle
            );
        }
    }
}

#[test]
fn fact_spec_rows_publish_each_contract_under_its_registry_oracle_name() {
    // A row can never attribute its facts to a contract name other than the
    // one its bound spec carries: the dispatcher key, the execution-payload
    // contract field, and the memo key are all `row.spec.semantic_oracle`.
    let mut seen = BTreeSet::new();
    for row in all_contract_fact_specs() {
        assert!(
            seen.insert(row.spec.semantic_oracle),
            "duplicate fact-spec row for {}",
            row.spec.semantic_oracle
        );
        assert!(
            !row.fact.is_empty() && !row.assertion.is_empty(),
            "{} must publish a named fact and assertion",
            row.spec.semantic_oracle
        );
    }
}
