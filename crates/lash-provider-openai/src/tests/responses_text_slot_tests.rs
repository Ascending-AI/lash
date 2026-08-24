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
