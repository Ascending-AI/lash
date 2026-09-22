// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;

fn operation(id: &str) -> OperationId {
    OperationId::new(
        crate::ExecutionScope::runtime_operation(format!("session:root:boundary:{id}")),
        "append-session-nodes",
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn effect_cause_message(scope: crate::ExecutionScope) -> crate::SessionAppendNode {
    serde_json::from_value(serde_json::json!({
        "kind": "message",
        "message": {
            "role": "Event",
            "parts": [{"id":"", "kind":"Text", "content":"effect wake"}],
            "origin": {
                "kind": "process",
                "process_id": "target-process",
                "event_type": "signal.resume",
                "sequence": 7,
                "caused_by": {
                    "type": "effect",
                    "address": {
                        "execution_scope": scope,
                        "replay_key": "effect-replay"
                    }
                }
            }
        }
    }))
    .expect("valid append-node fixture")
}

pub(super) fn effect_identity_rows() -> String {
    let turn_node = effect_cause_message(crate::ExecutionScope::turn("session", "turn"));
    let process_node = effect_cause_message(crate::ExecutionScope::process("process"));
    let turn_request = append_request_identity_bytes(
        &operation("effect-cause"),
        Some("ancestor"),
        std::slice::from_ref(&turn_node),
    )
    .expect("encode turn-scoped effect cause");
    let retry = append_request_identity_bytes(
        &operation("effect-cause"),
        Some("ancestor"),
        std::slice::from_ref(&turn_node),
    )
    .expect("encode identical retry");
    let process_request = append_request_identity_bytes(
        &operation("effect-cause"),
        Some("ancestor"),
        std::slice::from_ref(&process_node),
    )
    .expect("encode process-scoped effect cause");
    assert_eq!(turn_request, retry, "same-scope retry bytes must be stable");
    assert_ne!(
        turn_request, process_request,
        "different admitted scopes must not share append identity"
    );

    let rendered = [
        (
            "turn_effect_node",
            hex(&append_node_identity_bytes(&turn_node).expect("encode current turn-scoped node")),
        ),
        ("turn_effect_request", hex(&turn_request)),
        ("process_effect_request", hex(&process_request)),
    ]
    .into_iter()
    .map(|(name, value)| format!("{name}={value}"))
    .collect::<Vec<_>>()
    .join("\n")
        + "\n";
    rendered
}
