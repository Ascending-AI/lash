use super::SessionStreamEvent;
use serde_json::json;

#[test]
fn an_unknown_message_kind_is_refused() {
    let err = serde_json::from_value::<SessionStreamEvent>(json!({
        "type": "message",
        "text": "x",
        "kind": "final",
    }))
    .expect_err("the message kind set is closed");
    assert!(err.to_string().contains("final"), "{err}");
}
