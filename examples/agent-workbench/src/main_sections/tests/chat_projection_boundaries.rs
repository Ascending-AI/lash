use super::*;

#[test]
fn ui_input_replacement_uses_turn_provenance_with_unrelated_ids() {
    let row: lash::transcript::TranscriptRowRecord = serde_json::from_value(json!({
        "row_id": "opaque-committed-row", "kind": "user", "timestamp": "2026-10-03T00:00:00Z", "suppressed": null,
        "provenance": { "turn_id": "input-turn", "input_id": "input", "plugin_id": null, "is_turn_reply": false },
        "content": { "text": "committed input", "reasoning": [], "attachments": [], "language": null, "code": null, "output": null, "success": null, "error": null, "tools": [], "tools_omitted": 0 }
    })).unwrap();
    let owned = ChatMessage {
        id: "unrelated-ui-token".into(),
        role: "user".into(),
        text: "UI input".into(),
        at: "UI timestamp".into(),
        attachments: Vec::new(),
        provenance: Some(ChatMessageProvenance::TurnInput {
            turn_id: lash::TurnId::fixture("input-turn"),
        }),
    };
    let messages = displayed_messages(&[row], &[owned]);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].id, "unrelated-ui-token");
    assert_eq!(messages[0].text, "UI input");
}
