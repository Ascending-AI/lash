use super::support::*;

#[derive(Clone, Copy, Debug)]
struct RlmProtocolScenarioCoverage {
    display_name: &'static str,
}

const NATURAL_CELL_MAX_TURN: RlmProtocolScenarioCoverage = RlmProtocolScenarioCoverage {
    display_name: "natural cell max-turn stop",
};

const PLUGIN_STREAM_MASK_CHUNK_SPANNING_REEXTRACTION: RlmProtocolScenarioCoverage =
    RlmProtocolScenarioCoverage {
        display_name: "plugin stream mask splices a chunk-spanning cell for re-extraction",
    };

const TYPED_SCHEMA_REPAIR_ACROSS_CELL_BOUNDARY: RlmProtocolScenarioCoverage =
    RlmProtocolScenarioCoverage {
        display_name: "typed schema repair survives a cell checkpoint boundary",
    };

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
    let code = "print(\"hi\");";
    let cell_text = typescript_block_with_prose(prose, code);
    let cell_payload = serde_json::json!({
        "turn_id": "test-turn",
        "decision": "execute_typescript",
        "termination": "natural",
        "counts": {
            "full_text_chars": cell_text.chars().count(),
            "prose_chars": prose.chars().count(),
            "code_chars": code.chars().count(),
            "reasoning_chars": reasoning_text.chars().count(),
            "typescript_cell_count": 1,
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

    let split_open = "<typescript>\nprint(\"split\");";
    let split_close = "</typescript>";
    let split_code = "print(\"split\");\n";
    let split_full_text_chars = split_open.chars().count() + split_close.chars().count() + 2;
    assert_case(
        "response cell classification: split text parts",
        vec![text_part(split_open), text_part(split_close)],
        serde_json::json!({
            "turn_id": "test-turn",
            "decision": "execute_typescript",
            "termination": "natural",
            "counts": {
                "full_text_chars": split_full_text_chars,
                "prose_chars": 0,
                "code_chars": split_code.chars().count(),
                "reasoning_chars": 0,
                "typescript_cell_count": 1,
            },
        }),
        vec![split_code],
    );
}

#[test]
fn rlm_protocol_unclosed_cell_retries_in_natural_mode_without_journaling_markup() {
    RlmProtocolScenario::new("natural unclosed cell retry")
        .termination(RlmTermination::Natural { schema: None })
        .llm_response(vec![text_part(
            "Visible plan.\n<typescript>\nprint(\"unfinished\");",
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
        .llm_response(vec![text_part("<typescript>\nfinish({ ok: true });")])
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
fn rlm_protocol_scenario_natural_cell_at_budget_stops_without_another_provider_call() {
    RlmProtocolScenario::new(NATURAL_CELL_MAX_TURN.display_name)
        .user_message("run exactly one cell")
        .max_turns(1)
        .llm_response(vec![text_part(&typescript_block("print \"allowed\""))])
        .exec_result(exec_response(&["allowed"], None, None))
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["print \"allowed\""],
            llm_call_count: Some(1),
            done: Some(true),
            transcript_system_message_count: Some(0),
            turn_outcome: Some(lash_core::facade_support::TurnOutcome::Stopped(
                lash_core::facade_support::TurnStop::MaxTurns,
            )),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_protocol_scenario_plugin_stream_mask_splices_chunk_spanning_cell_for_reextraction() {
    const CODE: &str = "const alpha = \"first\";\nconst beta = alpha + \" second\";\nfinish(beta);";
    const RESPONSE: &str = "Visible prefix.\n<typescript>\nconst alpha = \"first\";\nconst beta = alpha + \" second\";\nfinish(beta);\n</typescript>";

    RlmProtocolScenario::new(PLUGIN_STREAM_MASK_CHUNK_SPANNING_REEXTRACTION.display_name)
        .user_message("run chunk-spanning streamed code")
        .plugin_factory(rlm_protocol_plugin_factory())
        .plugin_streamed_llm_response(
            // The chunk boundaries still fall inside the open and close tags,
            // which is what this scenario exists to exercise.
            vec![
                "Visible prefix.\n<type",
                "script>\nconst alpha = \"fir",
                "st\";\nconst beta = alpha + ",
                "\" second\";\nfinish(be",
                "ta);\n</type",
                "script>",
                "\n<typescript>\nfinish(\"must not be consumed\");\n</typescript>",
            ],
            vec![text_part(&format!(
                "{RESPONSE}\n<typescript>\nfinish(\"must not be consumed\");\n</typescript>"
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
fn rlm_protocol_scenario_typed_schema_repair_survives_a_cell_checkpoint_boundary() {
    // The boundary is real: `checkpoint_round_trip` serializes the machine's
    // `TurnCheckpoint` while the `finish` cell is still pending, deserializes it,
    // and continues on the restored machine. Anything the RLM driver failed to
    // carry in its checkpointed cell state is genuinely lost here.
    let run = RlmProtocolScenario::new(TYPED_SCHEMA_REPAIR_ACROSS_CELL_BOUNDARY.display_name)
        .user_message("return typed data")
        .termination(RlmTermination::FinishRequired {
            schema: Some(
                lash_sansio::JsonSchema::admit(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" }
                    },
                    "required": ["ok"]
                }))
                .expect("declared result schema"),
            ),
        })
        .llm_response(vec![text_part(&typescript_block(
            "finish({ missing: true });",
        ))])
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
            exec_codes: vec!["finish({ missing: true });", "finish({ missing: true });"],
            checkpoints: vec![CheckpointKind::AfterWork],
            llm_call_count: Some(2),
            system_message_contains: vec!["did not match the required output schema"],
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "finish({ missing: true });",
                output: Vec::new(),
                outcome: lash_core::CellOutcome::Failed(
                    program_failure("\"ok\" is a required property").with_value_mismatch(
                        lash_sansio::ValueMismatch {
                            instance_path: String::new(),
                            message: "\"ok\" is a required property".into(),
                        },
                    ),
                ),
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
    rlm          observe   message.code            text="finish({ missing: true });"
    rlm          exec      cell.start              lang="typescript"
    rlm          park      cell.checkpoint
    rlm          resume    cell.restore
    rlm          exec      cell.start              lang="typescript"
    rlm          commit    checkpoint.request      checkpoint=after_work
    rlm          provider  model.request           messages=2 tools=0
    "#);
}
