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
            "content": "effect wake",
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

#[test]
fn append_request_identity_v4_effect_cause_is_canonical_and_scope_bound() {
    let turn_node = effect_cause_message(crate::ExecutionScope::turn("session", "turn"));
    let process_node = effect_cause_message(crate::ExecutionScope::process("process"));
    assert_eq!(
        append_request_identity_encoding_version(std::slice::from_ref(&turn_node)),
        APPEND_REQUEST_IDENTITY_ENCODING_VERSION,
        "a scoped Effect cause must select the current identity generation"
    );

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
            hex(&append_node_identity_bytes_with_version(
                &turn_node,
                APPEND_REQUEST_IDENTITY_ENCODING_VERSION,
            )
            .expect("encode current turn-scoped node")),
        ),
        ("turn_effect_request", hex(&turn_request)),
        ("process_effect_request", hex(&process_request)),
    ]
    .into_iter()
    .map(|(name, value)| format!("{name}={value}"))
    .collect::<Vec<_>>()
    .join("\n")
        + "\n";
    if std::env::var_os("UPDATE_APPEND_REQUEST_IDENTITY_V4_EFFECT_GOLDEN").is_some() {
        std::fs::write(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src/store/testdata/append_request_identity_v4_effect.hex"),
            &rendered,
        )
        .expect("write v4 Effect-cause golden corpus");
    }
    assert_eq!(
        rendered,
        include_str!("testdata/append_request_identity_v4_effect.hex"),
        "current Effect-cause bytes moved; refresh only for an intentional v4 grammar change"
    );
}
