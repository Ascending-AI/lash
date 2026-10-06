use super::*;

#[test]
fn standard_execution_section_uses_only_surviving_tool_examples() {
    let section = standard_execution_section(BatchSugar::default());
    for removed_tool in [
        "read_file",
        "\"edit\"",
        "\"write\"",
        "\"glob\"",
        "fetch_url",
        "search_web",
    ] {
        assert!(
            !section.contains(removed_tool),
            "standard prompt should not mention removed tool `{removed_tool}`"
        );
    }
    let enabled = standard_execution_section(BatchSugar::default());
    assert!(enabled.contains("declared JSON arguments"));
    assert!(enabled.contains("Check each batch result’s success flag"));
    assert!(enabled.contains("at most 64 per batch"));
    let disabled = standard_execution_section(BatchSugar::Disabled);
    assert!(
        !disabled.contains("batch"),
        "a disabled batch is not offered in the prompt: {disabled}"
    );
}

#[test]
fn protocol_message_ids_include_turn_identity() {
    let first = standard_message_id(&TurnId::from("turn-1"), 0, "assistant");
    let replay = standard_message_id(&TurnId::from("turn-1"), 0, "assistant");
    let next_turn = standard_message_id(&TurnId::from("turn-2"), 0, "assistant");

    assert_eq!(first, replay);
    assert_ne!(first, next_turn);
}

fn sequence_part(kind: usize, position: usize) -> (LlmOutputPart, PartKind, String) {
    match kind {
        0 => {
            let marker = format!("text-{position}");
            (
                LlmOutputPart::Text {
                    text: marker.clone(),
                    response_meta: None,
                },
                PartKind::Prose,
                marker,
            )
        }
        1 => {
            let marker = format!("reasoning-{position}");
            (
                LlmOutputPart::Reasoning {
                    text: marker.clone(),
                    replay: None,
                },
                PartKind::Reasoning,
                marker,
            )
        }
        2 => {
            let marker = format!("tool-{position}");
            (
                LlmOutputPart::ToolCall {
                    call_id: format!("call-{position}"),
                    tool_name: marker.clone(),
                    input_json: format!(r#"{{"position":{position}}}"#),
                    replay: None,
                },
                PartKind::ToolCall,
                marker,
            )
        }
        _ => unreachable!("base-three sequence kind"),
    }
}

#[test]
fn mixed_response_sequences_reassemble_in_arrival_order() {
    for len in 1_u32..=5 {
        for encoded in 0..3_usize.pow(len) {
            let mut cursor = encoded;
            let mut input = Vec::with_capacity(len as usize);
            let mut expected = Vec::with_capacity(len as usize);
            for position in 0..len as usize {
                let (part, kind, marker) = sequence_part(cursor % 3, position);
                cursor /= 3;
                input.push(part);
                expected.push((kind, marker));
            }

            let response = collect_standard_response(
                &LlmResponse {
                    parts: input,
                    ..LlmResponse::default()
                },
                &lash_core::sansio::ModelToolCalls::fixture()
                    .response(0, lash_core::sansio::EffectId(0)),
            );
            let (actual, calls) = reassemble_standard_response("assistant", response.parts);

            assert_eq!(actual.len(), expected.len(), "sequence {encoded} len {len}");
            for (position, (actual, (expected_kind, marker))) in
                actual.iter().zip(expected.iter()).enumerate()
            {
                assert_eq!(
                    actual.kind(),
                    *expected_kind,
                    "kind at {position} in sequence {encoded} len {len}"
                );
                match actual.kind() {
                    PartKind::ToolCall => assert_eq!(
                        actual.tool_name(),
                        Some(marker.as_str()),
                        "tool marker at {position} in sequence {encoded} len {len}"
                    ),
                    _ => assert!(
                        actual.content().contains(marker),
                        "content marker at {position} in sequence {encoded} len {len}: {actual:?}"
                    ),
                }
            }
            assert_eq!(
                calls.len(),
                expected
                    .iter()
                    .filter(|(kind, _)| *kind == PartKind::ToolCall)
                    .count(),
                "tool dispatch count in sequence {encoded} len {len}"
            );
        }
    }
}
