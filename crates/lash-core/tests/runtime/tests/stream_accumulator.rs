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
            ..Default::default()
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
        "get_time".to_string(),
        "{\"timezone\":\"UTC\"}".to_string(),
        Some(lash_sansio::llm::types::ProviderReplayMeta {
            item_id: Some("item_1".to_string()),
            opaque: Some("sig".to_string()),
            ..Default::default()
        }),
    );

    let mut response = LlmResponse {
        parts: vec![LlmOutputPart::ToolCall {
            call_id: "call_1".to_string(),
            tool_name: "get_time".to_string(),
            input_json: "{\"timezone\":\"UTC\"}".to_string(),
            replay: Some(lash_sansio::llm::types::ProviderReplayMeta {
                item_id: Some("item_1".to_string()),
                opaque: Some("sig".to_string()),
                ..Default::default()
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
        LlmOutputPart::ToolCall { tool_name, .. } if tool_name == "get_time"
    ));
}

#[test]
fn stream_accumulator_preserves_duplicate_provider_calls_until_repair() {
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
        2
    );
    let mut response = LlmResponse::default();
    accumulator.apply_to_response_for_request(&mut response, "request-1");
    let ids: Vec<&str> = response
        .parts
        .iter()
        .filter_map(|part| match part {
            LlmOutputPart::ToolCall { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids[0], "call_1");
    assert_ne!(ids[1], ids[0]);
    assert!(ids[1].starts_with("lashcall_"));
}

#[test]
fn stream_accumulator_repairs_missing_blank_and_duplicate_ids_deterministically() {
    let original = || LlmResponse {
        parts: ["", " \t", "provider", "provider"]
            .into_iter()
            .enumerate()
            .map(|(index, id)| LlmOutputPart::ToolCall {
                call_id: id.to_string(),
                tool_name: format!("tool_{index}"),
                input_json: "{}".to_string(),
                replay: None,
            })
            .collect(),
        ..Default::default()
    };
    let accumulator = LlmStreamAccumulator::default();
    let mut first = original();
    accumulator.apply_to_response_for_request(&mut first, "journaled-request-1");
    let mut replay = original();
    accumulator.apply_to_response_for_request(&mut replay, "journaled-request-1");
    let ids = |response: &LlmResponse| {
        response
            .parts
            .iter()
            .filter_map(|part| match part {
                LlmOutputPart::ToolCall { call_id, .. } => Some(call_id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let first_ids = ids(&first);
    assert_eq!(first_ids, ids(&replay));
    assert_eq!(first_ids[2], "provider");
    assert_eq!(
        first_ids
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        4
    );
    assert!(first_ids[0].starts_with("lashcall_"));
    assert!(first_ids[1].starts_with("lashcall_"));
    assert!(first_ids[3].starts_with("lashcall_"));
    accumulator.apply_to_response_for_request(&mut first, "journaled-request-1");
    assert_eq!(ids(&first), first_ids);
    let mut different_request = original();
    accumulator.apply_to_response_for_request(&mut different_request, "journaled-request-2");
    assert_ne!(ids(&different_request)[0], first_ids[0]);
}

#[test]
fn stream_accumulator_accepts_reused_id_in_later_response() {
    for request_id in ["request-1", "request-2"] {
        let mut response = LlmResponse {
            parts: vec![LlmOutputPart::ToolCall {
                call_id: "reused".to_string(),
                tool_name: "lookup".to_string(),
                input_json: "{}".to_string(),
                replay: None,
            }],
            ..Default::default()
        };
        LlmStreamAccumulator::default().apply_to_response_for_request(&mut response, request_id);
        assert!(
            matches!(&response.parts[0], LlmOutputPart::ToolCall { call_id, .. } if call_id == "reused")
        );
    }
}

#[test]
fn stream_accumulator_keeps_a_later_provider_id_that_matches_a_candidate() {
    let tool = |id: &str| LlmOutputPart::ToolCall {
        call_id: id.to_string(),
        tool_name: "lookup".to_string(),
        input_json: "{}".to_string(),
        replay: None,
    };
    let accumulator = LlmStreamAccumulator::default();
    let mut first = LlmResponse {
        parts: vec![tool("")],
        ..Default::default()
    };
    accumulator.apply_to_response_for_request(&mut first, "request-1");
    let LlmOutputPart::ToolCall {
        call_id: candidate, ..
    } = &first.parts[0]
    else {
        panic!("expected tool call");
    };
    let mut response = LlmResponse {
        parts: vec![tool(""), tool(candidate)],
        ..Default::default()
    };
    accumulator.apply_to_response_for_request(&mut response, "request-1");
    let LlmOutputPart::ToolCall {
        call_id: repaired, ..
    } = &response.parts[0]
    else {
        panic!("expected tool call");
    };
    assert_ne!(repaired, candidate);
    assert!(
        matches!(&response.parts[1], LlmOutputPart::ToolCall { call_id, .. } if call_id == candidate)
    );
}

#[test]
fn stream_accumulator_does_not_duplicate_complete_final_response() {
    let mut accumulator = LlmStreamAccumulator::default();
    accumulator.push_reasoning("I'll answer.".to_string(), None, Vec::new(), None);
    accumulator.push_text("Done.");

    let mut response = LlmResponse {
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
fn stream_accumulator_projected_text_covers_reconciled_parts() {
    let mut accumulator = LlmStreamAccumulator::default();
    accumulator.push_text("Streamed prefix. ");

    let mut response = LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: "Provider suffix.".to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..Default::default()
    };

    accumulator.apply_to_response(&mut response);

    assert_eq!(
        response.full_text(),
        lash_core::facade_support::visible_response_text_from_parts(&response.parts)
    );
}

#[test]
fn stream_accumulator_full_text_prefers_final_answer_over_commentary() {
    let mut accumulator = LlmStreamAccumulator::default();
    accumulator.push_text_part(
        "Working notes.".to_string(),
        Some(lash_sansio::llm::types::ResponseTextMeta {
            id: Some("msg_commentary".to_string()),
            status: Some("completed".to_string()),
            phase: Some(lash_sansio::llm::types::ResponsePhase::Commentary),
            ..Default::default()
        }),
    );
    accumulator.push_text_part(
        "Final answer.".to_string(),
        Some(lash_sansio::llm::types::ResponseTextMeta {
            id: Some("msg_final".to_string()),
            status: Some("completed".to_string()),
            phase: Some(lash_sansio::llm::types::ResponsePhase::FinalAnswer),
            ..Default::default()
        }),
    );

    let mut response = LlmResponse::default();
    accumulator.apply_to_response(&mut response);

    assert_eq!(response.full_text(), "Final answer.");
    assert_eq!(
        response
            .parts
            .iter()
            .filter(|part| matches!(part, LlmOutputPart::Text { .. }))
            .count(),
        2
    );
}
