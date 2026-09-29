use super::*;

fn invocation(kind: RuntimeEffectKind) -> RuntimeEffectInvocation {
    let _ = kind;
    RuntimeEffectInvocation::new(
        EffectAddress::new(ExecutionScope::runtime_operation("session"), "replay")
            .expect("valid rejection-test address"),
        RuntimeAttribution::for_session("session"),
        "effect",
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn process_transfer_v1_identity_golden() {
    let process_ids = vec![
        crate::process_id_for_test("process:a:b"),
        crate::process_id_for_test("process\0b"),
        crate::process_id_for_test("λ"),
    ];
    assert_eq!(
        hex(&process_transfer_set_preimage(&process_ids)),
        "6c6173682d737461626c652d6964656e74697479020100000000000000196c6173682e70726f636573732d7472616e736665722d73657400000000000000030000000000000022705f34393161316432626135353737323338383637313337383231623261396465620000000000000022705f63353534366261373037303637376535613536633432613361353835333531630000000000000022705f3038383039336135386361623762653939616130373333303534623739623865"
    );
    assert_eq!(
        process_transfer_set_identity(&process_ids),
        "process-transfer-set:v1:blake3:2ab65b6834652333f4817015bfe92bdcc40e9ac97b3cf03a40d76578917a999e"
    );
}

fn prepared_call(call_id: &str) -> crate::PreparedToolCall {
    crate::PreparedToolCall::from_parts(
        call_id,
        "tool:test",
        "test",
        serde_json::json!({}),
        None,
        serde_json::Value::Null,
    )
}

fn attempt(call_id: &str, attempt: u32, max_attempts: u32) -> RuntimeEffectCommand {
    RuntimeEffectCommand::ToolAttempt {
        call: prepared_call(call_id),
        execution_grant: None,
        attempt,
        max_attempts,
    }
}

fn assert_rejected(
    invocation: RuntimeEffectInvocation,
    command: RuntimeEffectCommand,
    expected_code: &str,
) {
    let error = RuntimeEffectEnvelope::try_new(invocation, command)
        .expect_err("invalid envelope must be rejected");
    assert_eq!(error.code.as_str(), expected_code);
}

#[test]
fn rejects_empty_effect_id() {
    let error = RuntimeEffectInvocation::try_new(
        EffectAddress::new(ExecutionScope::runtime_operation("session"), "replay")
            .expect("valid rejection-test address"),
        RuntimeAttribution::for_session("session"),
        "  ",
    )
    .expect_err("empty descriptive labels are refused");
    assert_eq!(error.code.as_str(), "runtime_effect_invocation_subject");
}

#[test]
fn rejects_empty_address_replay_key() {
    let mut empty_address = invocation(RuntimeEffectKind::Sleep);
    empty_address.address.replay_key.clear();
    assert_rejected(
        empty_address,
        RuntimeEffectCommand::Sleep {
            spec: crate::SleepSpec::For { duration_ms: 1 },
        },
        "runtime_effect_replay_required",
    );
}

#[test]
fn effect_header_round_trips_without_universal_subject_or_replay_slots() {
    let invocation =
        invocation(RuntimeEffectKind::Sleep).with_caused_by(Some(CausalRef::Process {
            process_id: crate::process_id_for_test("process"),
        }));
    let encoded = serde_json::to_value(&invocation).expect("effect header encodes");
    assert!(encoded.get("address").is_some());
    assert!(encoded.get("subject").is_none());
    assert!(encoded.get("replay").is_none());
    assert_eq!(
        serde_json::from_value::<RuntimeEffectInvocation>(encoded).expect("effect header decodes"),
        invocation
    );
}

#[test]
fn legacy_universal_effect_header_is_refused() {
    let legacy = serde_json::to_value(RuntimeInvocation::effect(
        EffectAddress::new(ExecutionScope::runtime_operation("session"), "replay")
            .expect("valid legacy address"),
        RuntimeAttribution::for_session("session"),
        "effect",
    ))
    .expect("legacy header encodes");
    assert!(serde_json::from_value::<RuntimeEffectInvocation>(legacy).is_err());
}

#[test]
fn session_node_identity_is_structural_and_missing_identity_is_refused() {
    let invocation = RuntimeInvocation {
        attribution: RuntimeAttribution::none(),
        subject: RuntimeSubject::SessionNode {
            session_id: SessionId::from("session"),
            node_id: "node".to_string(),
        },
        caused_by: None,
        replay: None,
    };
    assert_eq!(
        serde_json::from_value::<RuntimeInvocation>(
            serde_json::to_value(&invocation).expect("session-node invocation encodes")
        )
        .expect("session-node invocation decodes")
        .causal_ref(),
        invocation.causal_ref()
    );
    let missing = serde_json::json!({
        "attribution": {},
        "subject": {"type": "session_node", "node_id": "node"}
    });
    assert!(serde_json::from_value::<RuntimeInvocation>(missing).is_err());
}

#[test]
fn rejects_empty_tool_attempt_call_id() {
    assert_rejected(
        invocation(RuntimeEffectKind::ToolAttempt),
        attempt(" ", 1, 1),
        "runtime_effect_tool_attempt_call_id",
    );
}

#[test]
fn rejects_tool_attempt_indices_outside_one_through_max() {
    for (attempt_index, max_attempts) in [(0, 1), (1, 0), (2, 1)] {
        assert_rejected(
            invocation(RuntimeEffectKind::ToolAttempt),
            attempt("call", attempt_index, max_attempts),
            "runtime_effect_tool_attempt_index",
        );
    }
}
