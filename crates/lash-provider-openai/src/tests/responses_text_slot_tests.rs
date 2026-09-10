//! Tests for how the Responses stream accumulator resolves a server output
//! slot (`output_index`) to the `Text` part that carries its message.

use super::*;

#[test]
fn responses_final_answer_phase_hides_commentary_from_visible_text() {
    let mut state = ResponsesStreamState::default();
    for event in [
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_commentary","phase":"commentary"}}"#,
        r#"{"type":"response.output_text.delta","output_index":0,"item_id":"msg_commentary","delta":"Working notes."}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_commentary","status":"completed","phase":"commentary","content":[{"type":"output_text","text":"Working notes."}]}}"#,
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_final","phase":"final_answer"}}"#,
        r#"{"type":"response.output_text.delta","output_index":1,"item_id":"msg_final","delta":"Final answer."}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"msg_final","status":"completed","phase":"final_answer","content":[{"type":"output_text","text":"Final answer."}]}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[{"type":"message","id":"msg_commentary","status":"completed","phase":"commentary","content":[{"type":"output_text","text":"Working notes."}]},{"type":"message","id":"msg_final","status":"completed","phase":"final_answer","content":[{"type":"output_text","text":"Final answer."}]}]}}"#,
    ] {
        OpenAiCompatibleProvider::process_sse_event(event, &mut state, None).unwrap();
    }

    let parts = state.response_parts();
    assert_eq!(state.full_text(), "Final answer.");
    assert_eq!(
        parts
            .iter()
            .filter(|part| matches!(part, LlmOutputPart::Text { .. }))
            .count(),
        2
    );
    let response = LlmResponse {
        parts,
        response_metadata: Default::default(),
        ..LlmResponse::default()
    };
    let visible = lash_core::facade_support::normalized_response_parts(&response);
    assert_eq!(
        visible
            .iter()
            .filter_map(|part| match part {
                LlmOutputPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>(),
        "Final answer."
    );
}

/// A `response.output_text.delta` can arrive before the matching
/// `response.output_item.added`. The fallback slot opened for that delta must
/// be registered against its `output_index`, or the later
/// `response.output_item.done` allocates a second slot and the same message is
/// emitted as two `Text` parts.
#[test]
fn responses_text_delta_before_item_added_yields_one_text_part() {
    let mut state = ResponsesStreamState::default();
    for event in [
        r#"{"type":"response.output_text.delta","output_index":0,"item_id":"msg_1","delta":"Hello"}"#,
        r#"{"type":"response.output_text.delta","output_index":0,"item_id":"msg_1","delta":" world"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_1","status":"completed","content":[{"type":"output_text","text":"Hello world"}]}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[{"type":"message","id":"msg_1","status":"completed","content":[{"type":"output_text","text":"Hello world"}]}]}}"#,
    ] {
        OpenAiCompatibleProvider::process_sse_event(event, &mut state, None).unwrap();
    }

    let parts = state.response_parts();
    let texts = parts
        .iter()
        .filter_map(|part| match part {
            LlmOutputPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(texts, vec!["Hello world"], "parts: {parts:?}");
    assert_eq!(state.full_text(), "Hello world");
}

#[test]
fn responses_text_item_id_without_output_index_then_item_done_emits_once() {
    let mut state = ResponsesStreamState::default();
    let mut emitted_parts = Vec::new();
    for event in [
        r#"{"type":"response.output_text.delta","item_id":"msg_1","delta":"Hello world"}"#,
        r#"{"type":"response.output_item.done","item":{"type":"message","id":"msg_1","status":"completed","content":[{"type":"output_text","text":"Hello world"}]}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[{"type":"message","id":"msg_1","status":"completed","content":[{"type":"output_text","text":"Hello world"}]}]}}"#,
    ] {
        OpenAiCompatibleProvider::process_sse_event(event, &mut state, Some(&mut emitted_parts))
            .unwrap();
    }

    let parts = state.response_parts();
    let texts = parts
        .iter()
        .filter_map(|part| match part {
            LlmOutputPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(texts, vec!["Hello world"], "parts: {parts:?}");
    assert_eq!(
        emitted_parts
            .iter()
            .filter(|part| matches!(part, LlmOutputPart::Text { .. }))
            .count(),
        1,
        "emitted parts: {emitted_parts:?}"
    );
}

#[test]
fn responses_text_item_id_then_output_index_aliases_emit_once() {
    let mut state = ResponsesStreamState::default();
    let mut emitted_parts = Vec::new();
    for event in [
        r#"{"type":"response.output_text.delta","item_id":"msg_1","delta":"Hello world"}"#,
        r#"{"type":"response.output_item.done","output_index":7,"item":{"type":"message","id":"msg_1","status":"completed","content":[{"type":"output_text","text":"Hello world"}]}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[{"type":"message","id":"msg_1","status":"completed","content":[{"type":"output_text","text":"Hello world"}]}]}}"#,
    ] {
        OpenAiCompatibleProvider::process_sse_event(event, &mut state, Some(&mut emitted_parts))
            .unwrap();
    }

    let parts = state.response_parts();
    let texts = parts
        .iter()
        .filter_map(|part| match part {
            LlmOutputPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(texts, vec!["Hello world"], "parts: {parts:?}");
    assert_eq!(
        emitted_parts
            .iter()
            .filter(|part| matches!(part, LlmOutputPart::Text { .. }))
            .count(),
        1,
        "emitted parts: {emitted_parts:?}"
    );
    assert_eq!(state.full_text(), "Hello world");
}

#[test]
fn responses_tool_item_id_then_output_index_preserves_arguments() {
    let mut state = ResponsesStreamState::default();
    let mut emitted_parts = Vec::new();
    for event in [
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"city\":\"Berlin\"}"}"#,
        r#"{"type":"response.output_item.done","output_index":7,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"weather","arguments":""}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[]}}"#,
    ] {
        OpenAiCompatibleProvider::process_sse_event(event, &mut state, Some(&mut emitted_parts))
            .unwrap();
    }

    let tools = emitted_parts
        .iter()
        .filter_map(|part| match part {
            LlmOutputPart::ToolCall {
                call_id,
                tool_name,
                input_json,
                replay,
            } => Some((call_id, tool_name, input_json, replay)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tools.len(), 1, "emitted parts: {emitted_parts:?}");
    let (call_id, tool_name, input_json, replay) = tools[0];
    assert_eq!(call_id, "call_1");
    assert_eq!(tool_name, "weather");
    assert_eq!(input_json, r#"{"city":"Berlin"}"#);
    assert_eq!(
        replay.as_ref().and_then(|meta| meta.item_id.as_deref()),
        Some("fc_1")
    );
}

#[test]
fn responses_output_index_keeps_owner_when_item_id_changes() {
    let mut state = ResponsesStreamState::default();
    let mut emitted_parts = Vec::new();
    for event in [
        r#"{"type":"response.output_item.added","output_index":4,"item":{"type":"message","id":"msg_initial"}}"#,
        r#"{"type":"response.output_text.delta","output_index":4,"item_id":"msg_initial","delta":"Hello world"}"#,
        r#"{"type":"response.output_item.done","output_index":4,"item":{"type":"message","id":"msg_final","status":"completed","content":[{"type":"output_text","text":"Hello world"}]}}"#,
    ] {
        OpenAiCompatibleProvider::process_sse_event(event, &mut state, Some(&mut emitted_parts))
            .unwrap();
    }

    let texts = state
        .response_parts()
        .into_iter()
        .filter_map(|part| match part {
            LlmOutputPart::Text { text, .. } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(texts, vec!["Hello world"]);
    assert_eq!(emitted_parts.len(), 1, "emitted parts: {emitted_parts:?}");
}

#[test]
fn responses_same_identity_value_keeps_part_kinds_distinct() {
    let mut state = ResponsesStreamState::default();
    let mut emitted_parts = Vec::new();
    for event in [
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"shared"}}"#,
        r#"{"type":"response.output_text.delta","output_index":0,"item_id":"shared","delta":"Hello"}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"shared"}}"#,
        r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"item_id":"shared","delta":"Think"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"shared","summary":[{"type":"summary_text","text":"Think"}]}}"#,
        r#"{"type":"response.function_call_arguments.delta","output_index":0,"item_id":"shared","delta":"{\"x\":1}"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"shared","call_id":"call_1","name":"tool","arguments":"{\"x\":1}"}}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"shared","status":"completed","content":[{"type":"output_text","text":"Hello"}]}}"#,
    ] {
        OpenAiCompatibleProvider::process_sse_event(event, &mut state, Some(&mut emitted_parts))
            .unwrap();
    }

    let parts = state.response_parts();
    assert_eq!(
        parts
            .iter()
            .filter(|part| matches!(part, LlmOutputPart::Text { .. }))
            .count(),
        1,
        "parts: {parts:?}"
    );
    assert_eq!(
        parts
            .iter()
            .filter(|part| matches!(part, LlmOutputPart::Reasoning { .. }))
            .count(),
        1,
        "parts: {parts:?}"
    );
    assert_eq!(
        parts
            .iter()
            .filter(|part| matches!(part, LlmOutputPart::ToolCall { .. }))
            .count(),
        1,
        "parts: {parts:?}"
    );
}
