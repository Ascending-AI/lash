use super::*;
use crate::TurnFinish;
use std::collections::BTreeSet;

fn address(label: &str) -> TurnAddress {
    TurnAddress::new(
        format!("turn-control-{label}-{}", uuid::Uuid::new_v4()),
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
    let scope = ExecutionScope::process("process:subagent:peek-key");
    let root = TurnAddress::new("session:subagent:peek-key", "process:subagent:peek-key");
    let follow_on = TurnAddress::new(&root.session_id, "process:subagent:peek-key:agent-frame:1");
    assert_eq!(
        turn_cancel_peek_replay_key(
            &scope,
            &root,
            &TurnCancelPeekIdentity::StartGate.causal_identity(),
        ),
        "turn-cancel-peek:v1:blake3:da49b5246b68613ad7de048d74eca8f0655c4fc2b62db4872af62491b5f7656e",
    );
    let mut keys = BTreeSet::new();

    for address in [&root, &follow_on] {
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
            ExecutionScope::turn("turn-session", "turn-root"),
            TurnAddress::new("turn-session", "turn-root"),
            TurnAddress::new("turn-session", "turn-root:agent-frame:1"),
        ),
        (
            ExecutionScope::queue_drain("queue-session", "queue-drain"),
            TurnAddress::new("queue-session", "queue-root"),
            TurnAddress::new("queue-session", "queue-follow-on"),
        ),
        (
            ExecutionScope::runtime_operation("runtime-operation"),
            TurnAddress::new("runtime-session", "runtime-root"),
            TurnAddress::new("runtime-session", "runtime-follow-on"),
        ),
    ];

    for (scope, root, follow_on) in cases {
        let mut keys = BTreeSet::new();
        for address in [&root, &follow_on] {
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
                &root,
                &TurnCancelPeekIdentity::StartGate.causal_identity(),
            ),
            turn_cancel_peek_replay_key(
                &scope,
                &root,
                &TurnCancelPeekIdentity::StartGate.causal_identity(),
            ),
            "same-frame replay must reconstruct the same key"
        );
    }
}

#[test]
fn cancel_request_without_disposition_fails_decode() {
    let missing: Result<TurnCancelRequest, _> = serde_json::from_value(serde_json::json!({
        "address": { "session_id": "legacy-session", "turn_id": "legacy-turn" },
        "request_id": "legacy-request"
    }));
    assert!(
        missing.is_err(),
        "a pre-disposition cancel request is refused, not defaulted"
    );
    let encoded =
        serde_json::to_value(request(address("encoded"), "request")).expect("encode request");
    assert_eq!(
        encoded.get("undelivered"),
        Some(&serde_json::json!("defer")),
        "the disposition is always encoded on the durable row"
    );
}

#[test]
fn local_cancel_origin_hint_preserves_first_origin() {
    let hint = TurnCancelOriginHint::default();
    assert!(!hint.was_set());
    hint.set(Some("shutdown".to_string()));
    hint.set(Some("user".to_string()));

    assert!(hint.was_set());
    assert_eq!(hint.get().as_deref(), Some("shutdown"));
}

#[test]
fn local_cancel_origin_hint_preserves_explicit_absence() {
    let hint = TurnCancelOriginHint::default();
    assert!(!hint.was_set());
    hint.set(None);
    hint.set(Some("user".to_string()));

    assert!(hint.was_set());
    assert_eq!(hint.get(), None);
}

#[test]
fn installed_originless_token_does_not_block_a_later_registry_origin() {
    let hint = TurnCancelOriginHint::default();
    hint.configure_local_token(None);

    assert!(!hint.was_set());

    hint.set(Some("user".to_string()));
    assert_eq!(hint.get().as_deref(), Some("user"));
}

#[test]
fn observed_registry_origin_wins_over_configured_token_origin() {
    let hint = TurnCancelOriginHint::default();
    hint.configure_local_token(Some("shutdown".to_string()));
    assert_eq!(hint.get().as_deref(), Some("shutdown"));

    hint.set(Some("user".to_string()));
    assert_eq!(hint.get().as_deref(), Some("user"));
}

#[test]
fn terminal_success_has_no_cancellation_evidence() {
    let terminal = TurnTerminal::Committed {
        outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage {
            text: "done".to_string(),
        }),
        session_revision: None,
    };
    let encoded = terminal_resolution(&terminal).expect("encode terminal");
    assert!(matches!(encoded, Resolution::Ok(_)));
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
    assert_eq!(decoded.undelivered, TurnCancelDisposition::Drop);
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

#[test]
fn after_step_request_and_evidence_round_trip_the_mode() {
    let request = request(address("mode"), "request-1").mode(TurnCancelMode::AfterStep);
    let encoded = serde_json::to_value(&request).expect("encode request");
    assert_eq!(encoded["mode"], serde_json::json!("after_step"));
    let decoded: TurnCancelRequest =
        serde_json::from_value(encoded).expect("decode after-step request");
    assert_eq!(decoded, request);
    let evidence = TurnCancellationEvidence {
        honoured_after_step: Some(3),
        ..decoded.evidence()
    };
    assert_eq!(evidence.mode, TurnCancelMode::AfterStep);
    let encoded = serde_json::to_value(&evidence).expect("encode evidence");
    assert_eq!(encoded["mode"], serde_json::json!("after_step"));
    assert_eq!(encoded["honoured_after_step"], serde_json::json!(3));
    let decoded: TurnCancellationEvidence =
        serde_json::from_value(encoded).expect("decode evidence");
    assert_eq!(decoded, evidence);
}

#[test]
fn cancel_mode_ordering_only_lets_immediate_escalate_after_step() {
    assert!(TurnCancelMode::Immediate.is_stronger_than(TurnCancelMode::AfterStep));
    assert!(!TurnCancelMode::AfterStep.is_stronger_than(TurnCancelMode::Immediate));
    assert!(!TurnCancelMode::Immediate.is_stronger_than(TurnCancelMode::Immediate));
    assert!(!TurnCancelMode::AfterStep.is_stronger_than(TurnCancelMode::AfterStep));
    assert!(TurnCancelMode::default().is_immediate());
}

#[test]
fn peek_identities_are_replay_deterministic_and_name_their_escalation() {
    let after_step = TurnCancelPeekIdentity::AfterStep {
        protocol_iteration: 4,
    };
    assert_eq!(after_step.causal_identity(), "turn_cancel.after_step.4");
    assert_eq!(
        after_step.escalation_causal_identity(),
        "turn_cancel.escalation.after_step.4"
    );
    assert_eq!(after_step.honours_after_step(), Some(Some(4)));
    assert_eq!(
        TurnCancelPeekIdentity::StartGate.honours_after_step(),
        Some(None)
    );
    assert_eq!(
        TurnCancelPeekIdentity::PostAbortGate.honours_after_step(),
        Some(None)
    );
    assert_eq!(
        TurnCancelPeekIdentity::AfterLlm {
            protocol_iteration: 0
        }
        .honours_after_step(),
        None
    );
    assert_eq!(
        TurnCancelPeekIdentity::AfterLlm {
            protocol_iteration: 0
        }
        .escalation_causal_identity(),
        "turn_cancel.escalation.after_llm.0"
    );
}
