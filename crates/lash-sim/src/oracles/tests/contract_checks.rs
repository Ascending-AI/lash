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
        "standard.empty_response_finishes",
        "standard.provider_error_without_checkpoint",
        "standard.native_tool_loop_reenters_model",
        "standard.parallel_tool_results_checkpoint_once",
        "standard.tool_failure_feedback_reenters_model",
        "standard.streamed_text_finalizes_once",
    ] {
        if let Err(err) = scenario_contract_generated_facts_for_semantic(contract, &events) {
            panic!("positive fixture should prove Standard replay-backed fact {contract}: {err}");
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
        scenario_contract_generated_facts_for_semantic("agent.parallel_spawn_and_join", &events)
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
    .expect_err("Agent shell projection fact must require later same-actor provider projection");
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
    runtime_turn_id: Option<&TurnId>,
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
        process_wake_turn_event(
            1,
            "wake:first",
            source_key,
            Some(&TurnId::from("turn:first")),
        ),
        process_wake_turn_event(2, "wake:duplicate", source_key, None),
    ];
    assert!(process_wake_at_most_once(&valid).is_passed());

    let duplicate = vec![
        process_wake_turn_event(
            1,
            "wake:first",
            source_key,
            Some(&TurnId::from("turn:first")),
        ),
        process_wake_turn_event(
            2,
            "wake:duplicate",
            source_key,
            Some(&TurnId::from("turn:second")),
        ),
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
