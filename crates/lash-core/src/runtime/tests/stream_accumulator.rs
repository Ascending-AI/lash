use super::*;

#[test]
fn stream_accumulator_merges_adjacent_display_reasoning_chunks() {
    let mut accumulator = LlmStreamAccumulator::default();
    accumulator.push_reasoning("I'll".to_string(), None, Vec::new(), None);
    accumulator.push_reasoning(" check".to_string(), None, Vec::new(), None);
    accumulator.push_reasoning(" the time.".to_string(), None, Vec::new(), None);

    assert_eq!(accumulator.parts.len(), 1);
    assert!(matches!(
        &accumulator.parts[0],
        LlmOutputPart::Reasoning { text, .. } if text == "I'll check the time."
    ));
}

#[test]
fn stream_accumulator_enriches_reasoning_delta_with_later_roundtrip_payload() {
    let mut accumulator = LlmStreamAccumulator::default();
    accumulator.push_reasoning("I'll check the time.".to_string(), None, Vec::new(), None);
    accumulator.push_reasoning(
        "I'll check the time.".to_string(),
        Some("rs_1".to_string()),
        vec!["I'll check the time.".to_string()],
        Some("encrypted".to_string()),
    );

    assert_eq!(accumulator.parts.len(), 1);
    assert!(matches!(
        &accumulator.parts[0],
        LlmOutputPart::Reasoning {
            text,
            replay: Some(replay),
            ..
        } if text == "I'll check the time."
            && replay.item_id.as_deref() == Some("rs_1")
            && replay.encrypted_content.as_deref() == Some("encrypted")
    ));
}

#[test]
fn stream_accumulator_preserves_full_reasoning_replay_metadata() {
    let mut accumulator = LlmStreamAccumulator::default();
    accumulator.push_reasoning_with_replay(
        "[Reasoning redacted]".to_string(),
        Some(lash_sansio::llm::types::ProviderReasoningReplay {
            item_id: Some("rs_1".to_string()),
            encrypted_content: None,
            signature: Some("signature".to_string()),
            redacted: true,
            summary: vec!["hidden".to_string()],
        }),
    );

    assert!(matches!(
        &accumulator.parts[0],
        LlmOutputPart::Reasoning {
            replay: Some(replay),
            ..
        } if replay.item_id.as_deref() == Some("rs_1")
            && replay.signature.as_deref() == Some("signature")
            && replay.redacted
            && replay.summary == vec!["hidden".to_string()]
    ));
}

#[test]
fn stream_accumulator_preserves_reasoning_when_final_response_has_tool_call() {
    let mut accumulator = LlmStreamAccumulator::default();
    accumulator.push_reasoning("I'll check the time.".to_string(), None, Vec::new(), None);
    accumulator.push_tool_call(
        "call_1".to_string(),
        "exec_command".to_string(),
        "{\"cmd\":\"date\"}".to_string(),
        Some(lash_sansio::llm::types::ProviderReplayMeta {
            item_id: Some("item_1".to_string()),
            opaque: Some("sig".to_string()),
        }),
    );

    let mut response = LlmResponse {
        full_text: String::new(),
        parts: vec![LlmOutputPart::ToolCall {
            call_id: "call_1".to_string(),
            tool_name: "exec_command".to_string(),
            input_json: "{\"cmd\":\"date\"}".to_string(),
            replay: Some(lash_sansio::llm::types::ProviderReplayMeta {
                item_id: Some("item_1".to_string()),
                opaque: Some("sig".to_string()),
            }),
        }],
        response_metadata: Default::default(),
        ..Default::default()
    };

    accumulator.apply_to_response(&mut response);

    assert_eq!(response.parts.len(), 2);
    assert!(matches!(
        &response.parts[0],
        LlmOutputPart::Reasoning { text, .. } if text == "I'll check the time."
    ));
    assert!(matches!(
        &response.parts[1],
        LlmOutputPart::ToolCall { tool_name, .. } if tool_name == "exec_command"
    ));
}

#[test]
fn stream_accumulator_dedupes_tool_calls_by_call_id() {
    let mut accumulator = LlmStreamAccumulator::default();
    for _ in 0..2 {
        accumulator.push_tool_call(
            "call_1".to_string(),
            "lookup".to_string(),
            "{\"q\":\"x\"}".to_string(),
            None,
        );
    }

    assert_eq!(
        accumulator
            .parts
            .iter()
            .filter(|part| matches!(part, LlmOutputPart::ToolCall { .. }))
            .count(),
        1
    );
}

#[test]
fn stream_accumulator_does_not_duplicate_complete_final_response() {
    let mut accumulator = LlmStreamAccumulator::default();
    accumulator.push_reasoning("I'll answer.".to_string(), None, Vec::new(), None);
    accumulator.push_text("Done.");

    let mut response = LlmResponse {
        full_text: "Done.".to_string(),
        parts: vec![
            LlmOutputPart::Reasoning {
                text: "I'll answer.".to_string(),
                replay: None,
            },
            LlmOutputPart::Text {
                text: "Done.".to_string(),
                response_meta: None,
            },
        ],
        response_metadata: Default::default(),
        ..Default::default()
    };

    accumulator.apply_to_response(&mut response);

    assert_eq!(response.parts.len(), 2);
    assert!(matches!(
        &response.parts[0],
        LlmOutputPart::Reasoning { text, .. } if text == "I'll answer."
    ));
    assert!(matches!(
        &response.parts[1],
        LlmOutputPart::Text { text, .. } if text == "Done."
    ));
}

#[test]
fn stream_accumulator_full_text_prefers_final_answer_over_commentary() {
    let mut accumulator = LlmStreamAccumulator::default();
    accumulator.push_text_part(
        "Working notes.".to_string(),
        Some(lash_sansio::llm::types::ResponseTextMeta {
            id: Some("msg_commentary".to_string()),
            status: Some("completed".to_string()),
            phase: Some("commentary".to_string()),
            ..Default::default()
        }),
    );
    accumulator.push_text_part(
        "Final answer.".to_string(),
        Some(lash_sansio::llm::types::ResponseTextMeta {
            id: Some("msg_final".to_string()),
            status: Some("completed".to_string()),
            phase: Some("final_answer".to_string()),
            ..Default::default()
        }),
    );

    let mut response = LlmResponse::default();
    accumulator.apply_to_response(&mut response);

    assert_eq!(response.full_text, "Final answer.");
    assert_eq!(
        response
            .parts
            .iter()
            .filter(|part| matches!(part, LlmOutputPart::Text { .. }))
            .count(),
        2
    );
}
