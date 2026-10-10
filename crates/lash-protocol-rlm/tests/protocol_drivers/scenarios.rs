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
    let code = "console.log(\"hi\");";
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

    let split_open = "<typescript>\nconsole.log(\"split\");";
    let split_close = "</typescript>";
    let split_code = "console.log(\"split\");\n";
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
        .termination(lash_core::TerminationMode::Natural)
        .llm_response(vec![text_part(
            "Visible plan.\n<typescript>\nconsole.log(\"unfinished\");",
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
        .termination(lash_core::TerminationMode::TerminalRequired)
        .llm_response(vec![text_part(
            "<typescript>\nawait control.finish({ ok: true });",
        )])
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
        .llm_response(vec![text_part(&typescript_block(
            "console.log(\"allowed\")",
        ))])
        .exec_result(exec_response(&["allowed"], None, None))
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["console.log(\"allowed\")"],
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
    const CODE: &str =
        "const alpha = \"first\";\nconst beta = alpha + \" second\";\nawait control.finish(beta);";
    const RESPONSE: &str = "Visible prefix.\n<typescript>\nconst alpha = \"first\";\nconst beta = alpha + \" second\";\nawait control.finish(beta);\n</typescript>";

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
                "\" second\";\nawait control.finish(be",
                "ta);\n</type",
                "script>",
                "\n<typescript>\nawait control.finish(\"must not be consumed\");\n</typescript>",
            ],
            vec![text_part(&format!(
                "{RESPONSE}\n<typescript>\nawait control.finish(\"must not be consumed\");\n</typescript>"
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
