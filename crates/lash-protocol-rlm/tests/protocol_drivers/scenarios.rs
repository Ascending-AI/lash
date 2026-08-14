use super::support::*;
use std::collections::BTreeSet;

// User-visible RLM protocol scenarios: turn termination, exec loops, tool-control, final values, and schema repair.

#[derive(Clone, Copy, Debug)]
struct RlmProtocolScenarioCoverage {
    test_name: &'static str,
    declared_test: fn(),
    display_name: &'static str,
    owned_invariant: &'static str,
}

macro_rules! rlm_protocol_coverage {
    ($test_fn:ident, $display_name:literal, $owned_invariant:literal) => {
        RlmProtocolScenarioCoverage {
            test_name: stringify!($test_fn),
            declared_test: $test_fn,
            display_name: $display_name,
            owned_invariant: $owned_invariant,
        }
    };
}

const NATURAL_PROSE_CLASSIFICATION: RlmProtocolScenarioCoverage = rlm_protocol_coverage!(
    rlm_protocol_scenario_prose_only_response_finishes_by_default,
    "natural prose classification",
    "Natural RLM prose-only response finalizes by default."
);
const FINISH_REQUIRED_PROSE_REQUESTS_FINISH: RlmProtocolScenarioCoverage = rlm_protocol_coverage!(
    rlm_protocol_scenario_typed_prose_only_response_requests_finish,
    "finish-required prose requests finish",
    "Typed RLM output requires explicit finish rather than prose-only success."
);
const FINISH_REQUIRED_PROSE_MAX_TURN: RlmProtocolScenarioCoverage = rlm_protocol_coverage!(
    rlm_protocol_scenario_finish_required_prose_at_max_turns_stops_without_retry_prompt,
    "finish-required prose max-turn stop",
    "Finish-required prose stops at max turns without an extra retry prompt."
);
const FINISH_REQUIRED_EXEC_ERROR_MAX_TURN: RlmProtocolScenarioCoverage = rlm_protocol_coverage!(
    rlm_protocol_scenario_finish_required_exec_error_at_max_turns_stops_without_retry,
    "finish-required exec error max-turn stop",
    "Exec errors at max turns stop cleanly without another repair turn."
);
const FINISH_REQUIRED_PROSE_DIAGNOSTIC: RlmProtocolScenarioCoverage = rlm_protocol_coverage!(
    rlm_protocol_scenario_finish_required_prose_only_diagnostic_has_clean_counts,
    "finish-required prose diagnostic",
    "Finish-required diagnostics classify prose-only responses with clean counts."
);
const NATURAL_PROSE_DIAGNOSTIC: RlmProtocolScenarioCoverage = rlm_protocol_coverage!(
    rlm_protocol_scenario_natural_prose_only_diagnostic_has_clean_counts,
    "natural prose diagnostic",
    "Natural diagnostics classify prose-only responses with clean counts."
);
const CELL_REASONING_PROSE_CODE_DIAGNOSTIC: RlmProtocolScenarioCoverage = rlm_protocol_coverage!(
    rlm_protocol_scenario_cell_reasoning_prose_code_diagnostic_has_clean_counts,
    "cell reasoning prose code diagnostic",
    "Mixed reasoning/prose/code diagnostics keep separate counts."
);
const RETIRED_PERCENT_MARKER: RlmProtocolScenarioCoverage = rlm_protocol_coverage!(
    rlm_protocol_scenario_retired_percent_marker_inside_source_is_plain_lashlang_text,
    "retired percent marker in lashlang source",
    "Retired percent cell marker remains Lashlang source text, not a control marker."
);
const LASHLANG_CELL_EXECUTION: RlmProtocolScenarioCoverage = rlm_protocol_coverage!(
    rlm_protocol_scenario_lashlang_cell_runs_exec_and_continues,
    "lashlang cell execution",
    "Lashlang cell execution feeds the protocol loop and continues."
);
const STREAMED_LASHLANG_CELL_EXECUTION: RlmProtocolScenarioCoverage = rlm_protocol_coverage!(
    rlm_protocol_scenario_streamed_lashlang_cell_runs_exec_and_persists_trajectory,
    "streamed lashlang cell execution",
    "A complete streamed Lashlang cell executes and persists trajectory events."
);
const PLUGIN_STREAM_MASK_CHUNK_SPANNING_REEXTRACTION: RlmProtocolScenarioCoverage = rlm_protocol_coverage!(
    rlm_protocol_scenario_plugin_stream_mask_splices_chunk_spanning_cell_for_reextraction,
    "plugin stream mask splices a chunk-spanning cell for re-extraction",
    "The registered RLM stream mask hides a chunk-spanning Lashlang cell, splices its exact body back into the final response, and the protocol re-extracts that body for execution."
);
const EMPTY_TURN_OPTIONS_DEFAULT: RlmProtocolScenarioCoverage = rlm_protocol_coverage!(
    rlm_protocol_scenario_empty_turn_options_use_natural_default,
    "empty turn options default to natural",
    "Empty RLM turn options use the natural default."
);
const EXEC_RESULT_TOOL_CALL_IDS_INTERNAL: RlmProtocolScenarioCoverage = rlm_protocol_coverage!(
    rlm_protocol_scenario_exec_result_emits_accounting_without_storing_tool_call_ids,
    "exec result emits tool-call accounting outside model history",
    "Exec result feedback emits accounting events without storing tool-call ids in trajectory."
);
const EXEC_TOOL_CONTROL_FRAME_SWITCH: RlmProtocolScenarioCoverage = rlm_protocol_coverage!(
    rlm_protocol_scenario_exec_any_tool_control_frame_switch_is_terminal,
    "exec tool-control frame switch is terminal",
    "Tool control frame-switch from exec is terminal."
);
const EXEC_TOOL_CONTROL_FAIL: RlmProtocolScenarioCoverage = rlm_protocol_coverage!(
    rlm_protocol_scenario_exec_any_tool_control_fail_is_terminal_error,
    "exec tool-control fail is terminal",
    "Tool control fail from exec is a terminal error."
);
const TYPED_FINAL_VALUE: RlmProtocolScenarioCoverage = rlm_protocol_coverage!(
    rlm_protocol_scenario_typed_finish_emits_turn_outcome_and_done,
    "typed final value",
    "Typed finish emits both turn outcome and done status."
);
const NATURAL_FINAL_VALUE: RlmProtocolScenarioCoverage = rlm_protocol_coverage!(
    rlm_protocol_scenario_natural_allows_finish_value,
    "natural allows final value",
    "Natural RLM mode accepts an explicit finish value."
);
const TYPED_SCHEMA_MISMATCH_REPAIR: RlmProtocolScenarioCoverage = rlm_protocol_coverage!(
    rlm_protocol_scenario_typed_schema_mismatch_loops_with_feedback,
    "typed schema mismatch loops with feedback",
    "Typed schema mismatch loops with repair feedback."
);
const TYPED_SCHEMA_MISMATCH_ANY_OF: RlmProtocolScenarioCoverage = rlm_protocol_coverage!(
    rlm_protocol_scenario_typed_schema_mismatch_checks_any_of,
    "typed schema mismatch checks anyOf",
    "Typed schema validation checks anyOf mismatches."
);

const TYPED_SCHEMA_REPAIR_ACROSS_CELL_BOUNDARY: RlmProtocolScenarioCoverage = rlm_protocol_coverage!(
    rlm_protocol_scenario_typed_schema_repair_survives_a_cell_checkpoint_boundary,
    "typed schema repair survives a cell checkpoint boundary",
    "Schema-mismatch repair feedback survives a real TurnCheckpoint round trip taken while the finish cell is still pending."
);

const RLM_PROTOCOL_SCENARIO_COVERAGE: &[RlmProtocolScenarioCoverage] = &[
    NATURAL_PROSE_CLASSIFICATION,
    FINISH_REQUIRED_PROSE_REQUESTS_FINISH,
    FINISH_REQUIRED_PROSE_MAX_TURN,
    FINISH_REQUIRED_EXEC_ERROR_MAX_TURN,
    FINISH_REQUIRED_PROSE_DIAGNOSTIC,
    NATURAL_PROSE_DIAGNOSTIC,
    CELL_REASONING_PROSE_CODE_DIAGNOSTIC,
    RETIRED_PERCENT_MARKER,
    LASHLANG_CELL_EXECUTION,
    STREAMED_LASHLANG_CELL_EXECUTION,
    PLUGIN_STREAM_MASK_CHUNK_SPANNING_REEXTRACTION,
    EMPTY_TURN_OPTIONS_DEFAULT,
    EXEC_RESULT_TOOL_CALL_IDS_INTERNAL,
    EXEC_TOOL_CONTROL_FRAME_SWITCH,
    EXEC_TOOL_CONTROL_FAIL,
    TYPED_FINAL_VALUE,
    NATURAL_FINAL_VALUE,
    TYPED_SCHEMA_MISMATCH_REPAIR,
    TYPED_SCHEMA_MISMATCH_ANY_OF,
    TYPED_SCHEMA_REPAIR_ACROSS_CELL_BOUNDARY,
];

#[test]
fn rlm_protocol_scenario_coverage_metadata_is_unique_and_complete() {
    assert_eq!(RLM_PROTOCOL_SCENARIO_COVERAGE.len(), 20);
    let mut names = BTreeSet::new();
    for coverage in RLM_PROTOCOL_SCENARIO_COVERAGE {
        let _declared_test = coverage.declared_test;
        assert!(
            coverage.test_name.starts_with("rlm_protocol_scenario_"),
            "RLM Protocol Scenario tests must use the scenario prefix, got {}",
            coverage.test_name
        );
        assert!(!coverage.display_name.trim().is_empty());
        assert!(!coverage.owned_invariant.trim().is_empty());
        assert!(
            names.insert(coverage.test_name),
            "duplicate RLM Protocol Scenario coverage metadata for {}",
            coverage.test_name
        );
    }
}

#[test]
fn rlm_protocol_property_response_cell_classification_is_part_order_invariant() {
    fn assert_case(
        name: &'static str,
        parts: Vec<LlmOutputPart>,
        expected_payload: serde_json::Value,
        exec_codes: Vec<&'static str>,
    ) {
        RlmProtocolScenario::new(name)
            .user_message("classify response cells")
            .llm_response(parts)
            .expect(RlmProtocolExpectations {
                exec_codes,
                llm_extraction_payload: Some(expected_payload),
                ..RlmProtocolExpectations::default()
            })
            .run();
    }

    let reasoning_text = "Plan first.";
    let prose = "Ready.";
    let code = "print \"hi\"";
    let cell_text = lashlang_block_with_prose(prose, code);
    let cell_payload = serde_json::json!({
        "decision": "execute_lashlang",
        "termination": "natural",
        "counts": {
            "full_text_chars": cell_text.chars().count(),
            "prose_chars": prose.chars().count(),
            "code_chars": code.chars().count(),
            "reasoning_chars": reasoning_text.chars().count(),
            "lashlang_cell_count": 1,
        },
    });

    assert_case(
        "response cell classification: reasoning before text",
        vec![reasoning_part(reasoning_text), text_part(&cell_text)],
        cell_payload.clone(),
        vec![code],
    );
    assert_case(
        "response cell classification: reasoning after text",
        vec![text_part(&cell_text), reasoning_part(reasoning_text)],
        cell_payload,
        vec![code],
    );

    let split_open = "<lashlang>\nprint \"split\"";
    let split_close = "</lashlang>";
    let split_code = "print \"split\"\n";
    let split_full_text_chars = split_open.chars().count() + split_close.chars().count() + 2;
    assert_case(
        "response cell classification: split text parts",
        vec![text_part(split_open), text_part(split_close)],
        serde_json::json!({
            "decision": "execute_lashlang",
            "termination": "natural",
            "counts": {
                "full_text_chars": split_full_text_chars,
                "prose_chars": 0,
                "code_chars": split_code.chars().count(),
                "reasoning_chars": 0,
                "lashlang_cell_count": 1,
            },
        }),
        vec![split_code],
    );
}

#[test]
fn rlm_protocol_scenario_prose_only_response_finishes_by_default() {
    RlmProtocolScenario::new(NATURAL_PROSE_CLASSIFICATION.display_name)
        .user_message("hello")
        .llm_response(vec![text_part("Hello there!")])
        .checkpoint()
        .expect(RlmProtocolExpectations {
            initial_request_tools_empty: true,
            checkpoints: vec![CheckpointKind::BeforeCompletion],
            llm_call_count: Some(1),
            done: Some(true),
            no_final_message_event: true,
            no_assistant_conversation_progress: true,
            turn_outcome: Some(lash_sansio::TurnOutcome::Finished(
                lash_sansio::TurnFinish::AssistantMessage {
                    text: "Hello there!".to_string(),
                },
            )),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_protocol_unclosed_cell_retries_in_natural_mode_without_journaling_markup() {
    RlmProtocolScenario::new("natural unclosed cell retry")
        .termination(RlmTermination::Natural)
        .llm_response(vec![text_part(
            "Visible plan.\n<lashlang>\nprint \"unfinished\"",
        )])
        .checkpoint()
        .expect(RlmProtocolExpectations {
            checkpoints: vec![CheckpointKind::AfterWork],
            llm_call_count: Some(2),
            no_exec_code: true,
            system_message_contains: vec!["did not close", "complete paired block"],
            assistant_visible_texts: Some(vec!["Visible plan."]),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_protocol_unclosed_cell_retries_in_finish_required_mode() {
    RlmProtocolScenario::new("finish-required unclosed cell retry")
        .termination(RlmTermination::FinishRequired { schema: None })
        .llm_response(vec![text_part("<lashlang>\nfinish { ok: true }")])
        .checkpoint()
        .expect(RlmProtocolExpectations {
            checkpoints: vec![CheckpointKind::AfterWork],
            llm_call_count: Some(2),
            no_exec_code: true,
            system_message_contains: vec!["did not close", "complete paired block"],
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_protocol_scenario_typed_prose_only_response_requests_finish() {
    RlmProtocolScenario::new(FINISH_REQUIRED_PROSE_REQUESTS_FINISH.display_name)
        .user_message("hello")
        .termination(RlmTermination::FinishRequired { schema: None })
        .llm_response(vec![text_part("Hello there!")])
        .checkpoint()
        .expect(RlmProtocolExpectations {
            checkpoints: vec![CheckpointKind::AfterWork],
            llm_call_count: Some(2),
            done: Some(false),
            system_message_contains: vec!["explicit final value", "finish <value>"],
            system_message_omits: vec!["required output schema"],
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_protocol_scenario_finish_required_prose_at_max_turns_stops_without_retry_prompt() {
    RlmProtocolScenario::new(FINISH_REQUIRED_PROSE_MAX_TURN.display_name)
        .user_message("hello")
        .termination(RlmTermination::FinishRequired { schema: None })
        .max_turns(1)
        .llm_response(vec![text_part(
            "plain prose cannot finish finish-required RLM",
        )])
        .expect(RlmProtocolExpectations {
            llm_call_count: Some(1),
            done: Some(true),
            system_message_omits: vec!["explicit final value", "finish <value>"],
            turn_outcome: Some(lash_core::facade_support::TurnOutcome::Stopped(
                lash_core::facade_support::TurnStop::MaxTurns,
            )),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_protocol_scenario_finish_required_exec_error_at_max_turns_stops_without_retry() {
    RlmProtocolScenario::new(FINISH_REQUIRED_EXEC_ERROR_MAX_TURN.display_name)
        .user_message("run bad code")
        .termination(RlmTermination::FinishRequired { schema: None })
        .max_turns(1)
        .llm_response(vec![text_part(&lashlang_block("missing_name"))])
        .exec_result(exec_response(
            &[],
            Some("unknown variable `missing_name`"),
            None,
        ))
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["missing_name"],
            llm_call_count: Some(1),
            done: Some(true),
            turn_outcome: Some(lash_core::facade_support::TurnOutcome::Stopped(
                lash_core::facade_support::TurnStop::MaxTurns,
            )),
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "missing_name",
                output: Vec::new(),
                error: Some("unknown variable `missing_name`".to_string()),
                final_output: None,
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_protocol_scenario_finish_required_prose_only_diagnostic_has_clean_counts() {
    let assistant_text = "Hello there!";

    RlmProtocolScenario::new(FINISH_REQUIRED_PROSE_DIAGNOSTIC.display_name)
        .user_message("hello")
        .termination(RlmTermination::FinishRequired { schema: None })
        .llm_response(vec![text_part(assistant_text)])
        .expect(RlmProtocolExpectations {
            checkpoints: vec![CheckpointKind::AfterWork],
            llm_extraction_payload: Some(serde_json::json!({
                "decision": "request_finish",
                "termination": "finish_required",
                "counts": {
                    "full_text_chars": assistant_text.chars().count(),
                    "prose_chars": assistant_text.chars().count(),
                    "code_chars": 0,
                    "reasoning_chars": 0,
                    "lashlang_cell_count": 0,
                },
            })),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_protocol_scenario_natural_prose_only_diagnostic_has_clean_counts() {
    let assistant_text = "Hello there!";

    RlmProtocolScenario::new(NATURAL_PROSE_DIAGNOSTIC.display_name)
        .user_message("hello")
        .termination(RlmTermination::Natural)
        .llm_response(vec![text_part(assistant_text)])
        .expect(RlmProtocolExpectations {
            checkpoints: vec![CheckpointKind::BeforeCompletion],
            llm_extraction_payload: Some(serde_json::json!({
                "decision": "finish_prose",
                "termination": "natural",
                "counts": {
                    "full_text_chars": assistant_text.chars().count(),
                    "prose_chars": assistant_text.chars().count(),
                    "code_chars": 0,
                    "reasoning_chars": 0,
                    "lashlang_cell_count": 0,
                },
            })),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_protocol_scenario_cell_reasoning_prose_code_diagnostic_has_clean_counts() {
    let reasoning_text = "Checking state.";
    let assistant_prose = "Ready.";
    let code = "print \"hi\"";
    let assistant_text = lashlang_block_with_prose(assistant_prose, code);

    RlmProtocolScenario::new(CELL_REASONING_PROSE_CODE_DIAGNOSTIC.display_name)
        .user_message("run some code")
        .llm_response(vec![
            reasoning_part(reasoning_text),
            text_part(&assistant_text),
        ])
        .expect(RlmProtocolExpectations {
            exec_codes: vec![code],
            llm_extraction_payload: Some(serde_json::json!({
                "decision": "execute_lashlang",
                "termination": "natural",
                "counts": {
                    "full_text_chars": assistant_text.chars().count(),
                    "prose_chars": assistant_prose.chars().count(),
                    "code_chars": code.chars().count(),
                    "reasoning_chars": reasoning_text.chars().count(),
                    "lashlang_cell_count": 1,
                },
            })),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_protocol_scenario_retired_percent_marker_inside_source_is_plain_lashlang_text() {
    let assistant_prose = "First.";
    let code = "text = \"%%lashlang is just source here\"\nprint text";
    let assistant_text = lashlang_block_with_prose(assistant_prose, code);

    RlmProtocolScenario::new(RETIRED_PERCENT_MARKER.display_name)
        .user_message("run some code")
        .llm_response(vec![text_part(&assistant_text)])
        .expect(RlmProtocolExpectations {
            exec_codes: vec![code],
            llm_extraction_payload: Some(serde_json::json!({
                "decision": "execute_lashlang",
                "termination": "natural",
                "counts": {
                    "full_text_chars": assistant_text.chars().count(),
                    "prose_chars": assistant_prose.chars().count(),
                    "code_chars": code.chars().count(),
                    "reasoning_chars": 0,
                    "lashlang_cell_count": 1,
                },
            })),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_protocol_scenario_lashlang_cell_runs_exec_and_continues() {
    RlmProtocolScenario::new(LASHLANG_CELL_EXECUTION.display_name)
        .user_message("run some code")
        .llm_response(vec![text_part(&lashlang_block_with_prose(
            "Quick check.\n",
            "print \"hi\"",
        ))])
        .exec_result(exec_response(&["hi\n"], None, None))
        .checkpoint()
        .expect(RlmProtocolExpectations {
            initial_request_tools_empty: true,
            exec_codes: vec!["print \"hi\""],
            checkpoints: vec![CheckpointKind::AfterWork],
            llm_call_count: Some(2),
            done: Some(false),
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "print \"hi\"",
                output: vec!["hi\n".to_string()],
                error: None,
                final_output: None,
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_protocol_scenario_streamed_lashlang_cell_runs_exec_and_persists_trajectory() {
    RlmProtocolScenario::new(STREAMED_LASHLANG_CELL_EXECUTION.display_name)
        .user_message("run streamed code")
        .streamed_llm_response(vec![text_part(&lashlang_block_with_prose(
            "Streaming visible prose.\n",
            "print \"streamed hi\"",
        ))])
        .exec_result(exec_response(&["streamed hi\n"], None, None))
        .checkpoint()
        .expect(RlmProtocolExpectations {
            initial_request_tools_empty: true,
            exec_codes: vec!["print \"streamed hi\""],
            checkpoints: vec![CheckpointKind::AfterWork],
            llm_call_count: Some(2),
            done: Some(false),
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "print \"streamed hi\"",
                output: vec!["streamed hi\n".to_string()],
                error: None,
                final_output: None,
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_protocol_scenario_plugin_stream_mask_splices_chunk_spanning_cell_for_reextraction() {
    const CODE: &str = "alpha = \"first\"\nbeta = alpha + \" second\"\nfinish beta";
    const RESPONSE: &str = "Visible prefix.\n<lashlang>\nalpha = \"first\"\nbeta = alpha + \" second\"\nfinish beta\n</lashlang>";

    RlmProtocolScenario::new(PLUGIN_STREAM_MASK_CHUNK_SPANNING_REEXTRACTION.display_name)
        .user_message("run chunk-spanning streamed code")
        .plugin_factory(rlm_protocol_plugin_factory())
        .plugin_streamed_llm_response(
            vec![
                "Visible prefix.\n<las",
                "hlang>\nalpha = \"fir",
                "st\"\nbeta = alpha + ",
                "\" second\"\nfinish be",
                "ta\n</lash",
                "lang>",
                "\n<lashlang>\nfinish \"must not be consumed\"\n</lashlang>",
            ],
            vec![text_part(&format!(
                "{RESPONSE}\n<lashlang>\nfinish \"must not be consumed\"\n</lashlang>"
            ))],
        )
        .expect(RlmProtocolExpectations {
            initial_request_tools_empty: true,
            exec_codes: vec![CODE],
            llm_call_count: Some(1),
            done: Some(false),
            plugin_stream_visible_texts: Some(vec!["Visible prefix.\n"]),
            plugin_spliced_response_texts: Some(vec![RESPONSE]),
            plugin_stream_abort_requests: Some(vec![true]),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_protocol_scenario_empty_turn_options_use_natural_default() {
    RlmProtocolScenario::new(EMPTY_TURN_OPTIONS_DEFAULT.display_name)
        .user_message("finish")
        .protocol_turn_options(lash_core::ProtocolTurnOptions::empty())
        .llm_response(vec![text_part(&lashlang_block("finish \"done\""))])
        .exec_result(exec_response(&[], None, Some(serde_json::json!("done"))))
        .checkpoint()
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["finish \"done\""],
            checkpoints: vec![CheckpointKind::BeforeCompletion],
            done: Some(true),
            turn_outcome: Some(lash_sansio::TurnOutcome::Finished(
                lash_sansio::TurnFinish::FinalValue {
                    value: serde_json::json!("done"),
                },
            )),
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "finish \"done\"",
                output: Vec::new(),
                error: None,
                final_output: Some(serde_json::json!("done")),
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_protocol_scenario_exec_result_emits_accounting_without_storing_tool_call_ids() {
    RlmProtocolScenario::new(EXEC_RESULT_TOOL_CALL_IDS_INTERNAL.display_name)
        .user_message("run a tool")
        .llm_response(vec![text_part(&lashlang_block(
            "x = await tools.read_file({ path: \"foo\" })?",
        ))])
        .exec_result(lash_sansio::ExecResponse {
            observations: Vec::new(),
            observation_truncation: Vec::new(),
            tool_calls: vec![lash_core::ToolCallRecord {
                call_id: Some("rlm-call-1".to_string()),
                tool: "read_file".to_string(),
                args: serde_json::json!({"path": "foo"}),
                output: lash_core::ToolCallOutput::success(serde_json::json!("contents")),
                duration_ms: 7,
            }],
            executed_calls: vec![lash_core::ExecutedCallRecord {
                operation: "tools.read_file".to_string(),
                outcome: lash_core::ExecutedCallOutcome::Ok,
            }],
            images: Vec::new(),
            printed_images: Vec::new(),
            error: None,
            duration_ms: 7,
            terminal_finish: None,
        })
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["x = await tools.read_file({ path: \"foo\" })?"],
            checkpoints: vec![CheckpointKind::AfterWork],
            tool_call_events: true,
            trajectory_omits_tool_call_ids: true,
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "x = await tools.read_file({ path: \"foo\" })?",
                output: Vec::new(),
                error: None,
                final_output: None,
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_protocol_scenario_exec_any_tool_control_frame_switch_is_terminal() {
    let initial_nodes = vec![lash_core::SessionAppendNode::message(
        lash_core::PluginMessage::text(lash_core::MessageRole::User, "seed"),
    )];
    RlmProtocolScenario::new(EXEC_TOOL_CONTROL_FRAME_SWITCH.display_name)
        .user_message("run a custom frame-switch tool")
        .llm_response(vec![text_part(&lashlang_block(
            "x = await tools.custom_frame_switch({})?",
        ))])
        .exec_result(lash_sansio::ExecResponse {
            observations: Vec::new(),
            observation_truncation: Vec::new(),
            tool_calls: vec![lash_core::ToolCallRecord {
                call_id: Some("custom-call-1".to_string()),
                tool: "custom_frame_switch".to_string(),
                args: serde_json::json!({}),
                output: lash_core::ToolCallOutput::success(serde_json::json!({"ok": true}))
                    .with_control(lash_core::ToolControl::SwitchAgentFrame {
                        frame_key: lash_core::FrameKey::from_caller_material("next-frame")
                            .expect("non-empty caller material"),
                        initial_nodes: initial_nodes.clone(),
                        task: Some("continue".to_string()),
                    }),
                duration_ms: 3,
            }],
            executed_calls: vec![lash_core::ExecutedCallRecord {
                operation: "tools.custom_frame_switch".to_string(),
                outcome: lash_core::ExecutedCallOutcome::Ok,
            }],
            images: Vec::new(),
            printed_images: Vec::new(),
            error: None,
            duration_ms: 3,
            terminal_finish: None,
        })
        .checkpoint()
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["x = await tools.custom_frame_switch({})?"],
            checkpoints: vec![CheckpointKind::BeforeCompletion],
            done: Some(true),
            tool_call_events: true,
            agent_frame_switch: Some(("next-frame", "continue")),
            turn_outcome: Some(lash_sansio::TurnOutcome::AgentFrameSwitch {
                frame_key: lash_core::FrameKey::from_caller_material("next-frame")
                    .expect("non-empty caller material"),
                task: "continue".to_string(),
                initial_nodes,
            }),
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "x = await tools.custom_frame_switch({})?",
                output: Vec::new(),
                error: None,
                final_output: None,
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_protocol_scenario_exec_any_tool_control_fail_is_terminal_error() {
    RlmProtocolScenario::new(EXEC_TOOL_CONTROL_FAIL.display_name)
        .user_message("run a custom failure tool")
        .llm_response(vec![text_part(&lashlang_block(
            "x = await tools.custom_fail({})?",
        ))])
        .exec_result(lash_sansio::ExecResponse {
            observations: Vec::new(),
            observation_truncation: Vec::new(),
            tool_calls: vec![lash_core::ToolCallRecord {
                call_id: Some("custom-call-1".to_string()),
                tool: "custom_fail".to_string(),
                args: serde_json::json!({}),
                output: lash_core::ToolCallOutput::success(serde_json::json!({"ok": true}))
                    .with_control(lash_core::ToolControl::Fail {
                        failure: lash_core::ToolFailure::tool(
                            lash_core::ToolFailureClass::Execution,
                            "custom_fail",
                            "no valid result",
                        ),
                    }),
                duration_ms: 3,
            }],
            executed_calls: vec![lash_core::ExecutedCallRecord {
                operation: "tools.custom_fail".to_string(),
                outcome: lash_core::ExecutedCallOutcome::Ok,
            }],
            images: Vec::new(),
            printed_images: Vec::new(),
            error: None,
            duration_ms: 3,
            terminal_finish: None,
        })
        .checkpoint()
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["x = await tools.custom_fail({})?"],
            checkpoints: vec![CheckpointKind::BeforeCompletion],
            done: Some(true),
            tool_call_events: true,
            tool_error_message: Some(("custom_fail", "no valid result")),
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "x = await tools.custom_fail({})?",
                output: Vec::new(),
                error: None,
                final_output: None,
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_protocol_scenario_typed_finish_emits_turn_outcome_and_done() {
    RlmProtocolScenario::new(TYPED_FINAL_VALUE.display_name)
        .user_message("return typed data")
        .termination(RlmTermination::FinishRequired { schema: None })
        .llm_response(vec![text_part(&lashlang_block("finish { ok: true }"))])
        .exec_result(exec_response(
            &[],
            None,
            Some(serde_json::json!({ "ok": true })),
        ))
        .checkpoint()
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["finish { ok: true }"],
            checkpoints: vec![CheckpointKind::BeforeCompletion],
            done: Some(true),
            no_final_message_event: true,
            turn_outcome: Some(lash_sansio::TurnOutcome::Finished(
                lash_sansio::TurnFinish::FinalValue {
                    value: serde_json::json!({ "ok": true }),
                },
            )),
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "finish { ok: true }",
                output: Vec::new(),
                error: None,
                final_output: Some(serde_json::json!({ "ok": true })),
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_protocol_scenario_natural_allows_finish_value() {
    RlmProtocolScenario::new(NATURAL_FINAL_VALUE.display_name)
        .user_message("return typed data")
        .termination(RlmTermination::Natural)
        .llm_response(vec![text_part(&lashlang_block("finish { ok: true }"))])
        .exec_result(exec_response(
            &[],
            None,
            Some(serde_json::json!({ "ok": true })),
        ))
        .checkpoint()
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["finish { ok: true }"],
            checkpoints: vec![CheckpointKind::BeforeCompletion],
            done: Some(true),
            turn_outcome: Some(lash_sansio::TurnOutcome::Finished(
                lash_sansio::TurnFinish::FinalValue {
                    value: serde_json::json!({ "ok": true }),
                },
            )),
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "finish { ok: true }",
                output: Vec::new(),
                error: None,
                final_output: Some(serde_json::json!({ "ok": true })),
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_protocol_scenario_typed_schema_mismatch_loops_with_feedback() {
    let run = RlmProtocolScenario::new(TYPED_SCHEMA_MISMATCH_REPAIR.display_name)
        .user_message("return typed data")
        .termination(RlmTermination::FinishRequired {
            schema: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "ok": { "type": "boolean" }
                },
                "required": ["ok"]
            })),
        })
        .llm_response(vec![text_part(&lashlang_block("finish { missing: true }"))])
        .exec_result(exec_response(
            &[],
            None,
            Some(serde_json::json!({ "missing": true })),
        ))
        .checkpoint()
        .llm_response(vec![text_part(&lashlang_block("finish { ok: true }"))])
        .exec_result(exec_response(
            &[],
            None,
            Some(serde_json::json!({ "ok": true })),
        ))
        .checkpoint()
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["finish { missing: true }", "finish { ok: true }"],
            checkpoints: vec![CheckpointKind::AfterWork, CheckpointKind::BeforeCompletion],
            llm_call_count: Some(2),
            done: Some(true),
            system_message_contains: vec!["didn't match the required output schema"],
            turn_outcome: Some(lash_sansio::TurnOutcome::Finished(
                lash_sansio::TurnFinish::FinalValue {
                    value: serde_json::json!({ "ok": true }),
                },
            )),
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "finish { ok: true }",
                output: Vec::new(),
                error: None,
                final_output: Some(serde_json::json!({ "ok": true })),
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
    insta::assert_snapshot!(run.transcript.render(), @r#"
    rlm          provider  model.request           messages=1 tools=0
    rlm          observe   message.lashlang_code   text="finish { missing: true }"
    rlm          exec      cell.start              lang="lashlang"
    rlm          commit    checkpoint.request      checkpoint=after_work
    rlm                      usage                 entries=0 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
    rlm          provider  model.request           messages=2 tools=0
    rlm          observe   message.lashlang_code   text="finish { ok: true }"
    rlm          exec      cell.start              lang="lashlang"
    rlm          commit    checkpoint.request      checkpoint=before_completion
    rlm                      usage                 entries=0 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
    rlm          outcome   turn.final_value        value={"ok":true}
    "#);
}

#[test]
fn rlm_protocol_scenario_typed_schema_mismatch_checks_any_of() {
    RlmProtocolScenario::new(TYPED_SCHEMA_MISMATCH_ANY_OF.display_name)
        .user_message("return typed data")
        .termination(RlmTermination::FinishRequired {
            schema: Some(serde_json::json!({
                "anyOf": [
                    { "type": "string" },
                    { "type": "integer" }
                ]
            })),
        })
        .llm_response(vec![text_part(&lashlang_block("finish true"))])
        .exec_result(exec_response(&[], None, Some(serde_json::json!(true))))
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["finish true"],
            checkpoints: vec![CheckpointKind::AfterWork],
            system_message_contains: vec!["didn't match the required output schema"],
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "finish true",
                output: Vec::new(),
                error: Some(
                    "true is not valid under any of the schemas listed in the 'anyOf' keyword"
                        .to_string(),
                ),
                final_output: None,
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_protocol_scenario_typed_schema_repair_survives_a_cell_checkpoint_boundary() {
    // The boundary is real: `checkpoint_round_trip` serializes the machine's
    // `TurnCheckpoint` while the `finish` cell is still pending, deserializes it,
    // and continues on the restored machine. Anything the RLM driver failed to
    // carry in its checkpointed cell state is genuinely lost here.
    let run = RlmProtocolScenario::new(TYPED_SCHEMA_REPAIR_ACROSS_CELL_BOUNDARY.display_name)
        .user_message("return typed data")
        .termination(RlmTermination::FinishRequired {
            schema: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "ok": { "type": "boolean" }
                },
                "required": ["ok"]
            })),
        })
        .llm_response(vec![text_part(&lashlang_block("finish { missing: true }"))])
        .checkpoint_round_trip()
        .exec_result(exec_response(
            &[],
            None,
            Some(serde_json::json!({ "missing": true })),
        ))
        .checkpoint()
        .expect(RlmProtocolExpectations {
            // The restored machine redrives the same pending cell, so the code is
            // observed twice — once before the boundary and once after it.
            exec_codes: vec!["finish { missing: true }", "finish { missing: true }"],
            checkpoints: vec![CheckpointKind::AfterWork],
            llm_call_count: Some(2),
            system_message_contains: vec!["didn't match the required output schema"],
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "finish { missing: true }",
                output: Vec::new(),
                error: Some("\"ok\" is a required property".to_string()),
                final_output: None,
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
    assert_eq!(
        run.round_trips, 1,
        "the scenario must have crossed exactly one real checkpoint boundary"
    );

    // Expect test: the reviewable artifact is the ordering across the durable
    // boundary — the pending cell redriven after restore, the repair message
    // reaching the model, and exactly one checkpoint before re-entry.
    insta::assert_snapshot!(run.transcript.render(), @r#"
    rlm          provider  model.request           messages=1 tools=0
    rlm          observe   message.lashlang_code   text="finish { missing: true }"
    rlm          exec      cell.start              lang="lashlang"
    rlm          park      cell.checkpoint
    rlm          resume    cell.restore
    rlm          exec      cell.start              lang="lashlang"
    rlm          commit    checkpoint.request      checkpoint=after_work
    rlm                      usage                 entries=0 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
    rlm          provider  model.request           messages=2 tools=0
    "#);
}
