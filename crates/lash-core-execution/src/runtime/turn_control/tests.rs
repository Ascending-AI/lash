use super::*;
use crate::TurnFinish;
use std::collections::BTreeSet;

fn address(label: &str) -> TurnAddress {
    TurnAddress::new(
        lash_sansio::SessionId::fixture(format!("turn-control-{label}-{}", uuid::Uuid::new_v4())),
        "turn-a",
    )
}

fn request(address: TurnAddress, request_id: &str) -> TurnCancelRequest {
    TurnCancelRequest::new(address, request_id, Some("user".to_string())).with_reason("stop button")
}

#[test]
fn foreground_turn_cancel_peek_replay_keys_remain_unchanged() {
    let address = TurnAddress::new("foreground-session", "foreground-turn");
    let scope = address.execution_scope();
    let identities = [
        (TurnCancelPeekIdentity::StartGate, "turn_cancel.start_gate"),
        (
            TurnCancelPeekIdentity::PostAbortGate,
            "turn_cancel.post_abort_gate",
        ),
        (
            TurnCancelPeekIdentity::AfterLlm {
                protocol_iteration: 7,
            },
            "turn_cancel.after_llm.7",
        ),
        (
            TurnCancelPeekIdentity::AfterStep {
                protocol_iteration: 7,
            },
            "turn_cancel.after_step.7",
        ),
    ];

    for (identity, expected) in identities {
        let causal_identity = identity.causal_identity();
        assert_eq!(causal_identity, expected);
        assert_eq!(
            turn_cancel_peek_replay_key(&scope, &address, &causal_identity),
            expected
        );
        let escalation = identity.escalation_causal_identity();
        assert_eq!(
            turn_cancel_peek_replay_key(&scope, &address, &escalation),
            escalation
        );
    }
}

#[test]
fn process_turn_cancel_peek_replay_keys_cover_physical_turn_and_gate() {
    let scope = ExecutionScope::process(crate::process_id_for_test("process:subagent:peek-key"));
    let run = TurnAddress::new("session:subagent:peek-key", "process:subagent:peek-key");
    let follow_on = TurnAddress::new(&run.session_id, "process:subagent:peek-key:agent-frame:1");
    assert_eq!(
        turn_cancel_peek_replay_key(
            &scope,
            &run,
            &TurnCancelPeekIdentity::StartGate.causal_identity(),
        ),
        "turn-cancel-peek:v1:blake3:da49b5246b68613ad7de048d74eca8f0655c4fc2b62db4872af62491b5f7656e",
    );
    let mut keys = BTreeSet::new();

    for address in [&run, &follow_on] {
        for identity in [
            TurnCancelPeekIdentity::StartGate,
            TurnCancelPeekIdentity::PostAbortGate,
            TurnCancelPeekIdentity::AfterLlm {
                protocol_iteration: 7,
            },
            TurnCancelPeekIdentity::AfterStep {
                protocol_iteration: 7,
            },
        ] {
            for causal_identity in [
                identity.causal_identity(),
                identity.escalation_causal_identity(),
            ] {
                let key = turn_cancel_peek_replay_key(&scope, address, &causal_identity);
                assert!(
                    key.starts_with("turn-cancel-peek:v1:blake3:"),
                    "unexpected Process peek key: {key}"
                );
                assert!(keys.insert(key), "Process peek identity collided");
            }
        }
    }

    assert_eq!(keys.len(), 16);
}

#[test]
fn every_shared_scope_cancel_peek_key_covers_physical_turn_and_gate() {
    let cases = [
        (
            ExecutionScope::turn("turn-session", "turn-run"),
            TurnAddress::new("turn-session", "turn-run"),
            TurnAddress::new("turn-session", "turn-run:agent-frame:1"),
        ),
        (
            ExecutionScope::session_operation("queue-session", "queue-operation"),
            TurnAddress::new("queue-session", "queue-run"),
            TurnAddress::new("queue-session", "queue-follow-on"),
        ),
        (
            ExecutionScope::runtime_operation("runtime-operation"),
            TurnAddress::new("runtime-session", "runtime-run"),
            TurnAddress::new("runtime-session", "runtime-follow-on"),
        ),
    ];

    for (scope, run, follow_on) in cases {
        let mut keys = BTreeSet::new();
        for address in [&run, &follow_on] {
            for identity in [
                TurnCancelPeekIdentity::StartGate,
                TurnCancelPeekIdentity::PostAbortGate,
                TurnCancelPeekIdentity::AfterLlm {
                    protocol_iteration: 7,
                },
                TurnCancelPeekIdentity::AfterStep {
                    protocol_iteration: 7,
                },
            ] {
                for causal_identity in [
                    identity.causal_identity(),
                    identity.escalation_causal_identity(),
                ] {
                    let key = turn_cancel_peek_replay_key(&scope, address, &causal_identity);
                    assert!(keys.insert(key), "shared-scope peek identity collided");
                }
            }
        }
        assert_eq!(keys.len(), 16);
        assert_eq!(
            turn_cancel_peek_replay_key(
                &scope,
                &run,
                &TurnCancelPeekIdentity::StartGate.causal_identity(),
            ),
            turn_cancel_peek_replay_key(
                &scope,
                &run,
                &TurnCancelPeekIdentity::StartGate.causal_identity(),
            ),
            "same-frame replay must reconstruct the same key"
        );
    }
}

#[test]
fn cancel_request_without_undelivered_fails_decode() {
    let missing: Result<TurnCancelRequest, _> = serde_json::from_value(serde_json::json!({
        "address": { "session_id": "legacy-session", "turn_id": "legacy-turn" },
        "request_id": "legacy-request"
    }));
    assert!(
        missing.is_err(),
        "a cancel request without an undelivered policy is refused, not defaulted"
    );
    let encoded =
        serde_json::to_value(request(address("encoded"), "request")).expect("encode request");
    assert_eq!(
        encoded.get("undelivered"),
        Some(&serde_json::json!("defer")),
        "the undelivered policy is always encoded on the durable row"
    );
}

#[test]
fn terminal_success_has_no_cancellation_evidence() {
    let terminal = TurnTerminal::committed(&TurnOutcome::Finished(TurnFinish::AssistantMessage {
        text: "the answer lives in the terminal record".repeat(4096),
    }));
    let encoded = terminal_resolution(&terminal).expect("encode terminal");
    assert_eq!(
        encoded,
        Resolution::Ok(serde_json::json!({"status": "committed", "stop": null}))
    );
}

#[test]
fn legacy_cancel_request_without_mode_decodes_as_immediate() {
    let decoded: TurnCancelRequest = serde_json::from_value(serde_json::json!({
        "address": { "session_id": "legacy-session", "turn_id": "legacy-turn" },
        "request_id": "legacy-request",
        "origin": "user",
        "reason": "stop button",
        "undelivered": "drop"
    }))
    .expect("decode a pre-mode cancel request");
    assert_eq!(decoded.mode, TurnCancelMode::Immediate);
    assert_eq!(decoded.undelivered, TurnCancelUndeliveredInputPolicy::Drop);
    let encoded = serde_json::to_value(&decoded).expect("encode defaulted request");
    assert!(
        encoded.get("mode").is_none(),
        "the Immediate default stays sparse on the durable row: {encoded}"
    );
}

#[test]
fn legacy_cancellation_evidence_without_mode_decodes_as_immediate() {
    let decoded: TurnCancellationEvidence = serde_json::from_value(serde_json::json!({
        "request_id": "legacy-request",
        "origin": "user"
    }))
    .expect("decode pre-mode evidence");
    assert_eq!(decoded.mode, TurnCancelMode::Immediate);
    assert_eq!(decoded.honoured_after_step, None);
    let encoded = serde_json::to_value(&decoded).expect("encode evidence");
    assert!(encoded.get("mode").is_none());
    assert!(encoded.get("honoured_after_step").is_none());
}

/// FIG-3672 P9: a code cell's checkpoints and its after-cell peek are their
/// own journaled identities, unique per cell and checkpoint, and none of them
/// honours an `AfterStep` request: a cell stops mid-run only on an immediate
/// request or an escalation.
#[test]
fn code_cell_peeks_have_their_own_identities_and_defer_after_step() {
    let checkpoint = TurnCancelPeekIdentity::CellCheckpoint {
        cell: "turn:1:0:exec_code:3".to_string(),
        checkpoint: 2,
    };
    assert_eq!(
        checkpoint.causal_identity(),
        "turn_cancel.cell_checkpoint.2.turn:1:0:exec_code:3"
    );
    assert_eq!(
        checkpoint.escalation_causal_identity(),
        "turn_cancel.escalation.cell_checkpoint.2.turn:1:0:exec_code:3"
    );
    assert_eq!(checkpoint.honours_after_step(), None);
    let after_cell = TurnCancelPeekIdentity::AfterCell {
        cell: "turn:1:0:exec_code:3".to_string(),
    };
    assert_eq!(
        after_cell.causal_identity(),
        "turn_cancel.after_cell.turn:1:0:exec_code:3"
    );
    assert_eq!(after_cell.honours_after_step(), None);
    assert_ne!(
        TurnCancelPeekIdentity::CellCheckpoint {
            cell: "turn:1:0:exec_code:3".to_string(),
            checkpoint: 3,
        }
        .causal_identity(),
        checkpoint.causal_identity()
    );
}

#[test]
fn terminal_refuses_unwritten_failure_and_revision_states() {
    for value in [
        serde_json::json!({"status": "committed", "stop": null, "session_revision": 7}),
        serde_json::json!({"status": "failed", "error": {}}),
    ] {
        assert!(serde_json::from_value::<TurnTerminal>(value).is_err());
    }
}
