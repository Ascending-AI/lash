use super::*;

/// ADR 0004: a process execution environment is a closed, typed shape —
/// policy plus plugin-owned options. A field the type does not declare is a
/// missing capability, and it is refused when the environment decodes, not
/// discovered later during process recovery.
#[test]
fn a_process_execution_environment_rejects_unknown_fields() {
    let spec = ProcessExecutionEnvSpec::new(
        crate::AdmittedPluginConfig::default(),
        crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024)),
    );
    let encoded = spec.to_store_bytes().expect("encode the environment");
    assert_eq!(
        ProcessExecutionEnvSpec::from_store_bytes(&encoded).expect("the environment decodes"),
        spec,
        "a conforming environment round-trips"
    );

    let mut fields: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&encoded).expect("the environment encodes an object");
    fields.insert(
        "undeclared_capability".to_string(),
        serde_json::json!({"plugin": "never-installed"}),
    );
    let error = ProcessExecutionEnvSpec::from_store_bytes(
        &serde_json::to_vec(&fields).expect("re-encode the doctored environment"),
    )
    .expect_err("an environment field the type does not declare is refused");
    assert!(
        error.to_string().contains("unknown field"),
        "the refusal names the unknown field: {error}"
    );
}

#[test]
fn a_process_execution_policy_rejects_the_retired_session_id() {
    let policy =
        crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024));
    let encoded = serde_json::to_value(&policy).expect("encode policy");
    assert!(encoded.get("session_id").is_none());
    for retired in [serde_json::Value::Null, serde_json::json!("session")] {
        let mut fields = encoded.clone();
        fields["session_id"] = retired;
        let error = serde_json::from_value::<crate::SessionPolicy>(fields)
            .expect_err("the retired session id is refused");
        assert!(error.to_string().contains("unknown field `session_id`"));
    }
}

#[test]
fn sequential_process_id_mint_preserves_every_ordinal_and_parses() {
    let ordinals = [0, 1, (1 << 62) - 1, 1 << 62, 1 << 63, u64::MAX];
    let ids = ordinals.map(ProcessIdMint::sequential_id_for_testing);
    for (index, id) in ids.iter().enumerate() {
        assert_eq!(ProcessId::parse(id.as_str()).unwrap(), *id);
        assert!(
            !ids[..index].contains(id),
            "different ordinals mint different ids"
        );
    }
}

fn process_wake(
    target_session_id: &str,
    process_id: ProcessId,
    sequence: u64,
) -> ProcessWakeDelivery {
    ProcessWakeDelivery {
        version: PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        target_session_id: SessionId::from(target_session_id),
        process_id,
        sequence,
        event_type: "process.wake".to_string(),
        process_caused_by: None,
        authority: crate::QueuedWorkAuthority::default(),
        input: "wake".to_string(),
        created_at_ms: 10,
    }
}

#[test]
fn process_wake_v1_identity_golden() {
    let hex = |bytes: &[u8]| -> String { bytes.iter().map(|byte| format!("{byte:02x}")).collect() };
    let process_id = crate::process_id_for_test("process:λ");
    let preimage = process_wake_identity_preimage(&SessionId::from("session\0x"), &process_id, 42);
    assert_eq!(
        hex(&preimage),
        "6c6173682d737461626c652d6964656e74697479020100000000000000116c6173682e70726f636573732d77616b65000000000000000973657373696f6e00780000000000000022705f3732383666323863306130393737653138633335666635643130373538363633000000000000002a"
    );
    assert_eq!(
        process_wake("session\0x", process_id, 42).wake_id(),
        *"wake:v1:blake3:7fe5d63df8c2f43b4c31274fc065b7dd306e42e69abfdb821b9bc73915dac87a"
    );
}

/// A wake's identity is a function of its target session, process and event
/// sequence and of nothing else it carries, so two wakes agree on their id
/// exactly when they agree on those three.
#[test]
fn a_process_wake_id_is_computed_from_its_target_process_and_sequence() {
    let process_id = crate::process_id_for_test("process");
    let wake = process_wake("session", process_id.clone(), 4);
    let mut same_wake = wake.clone();
    same_wake.event_type = "process.other".to_string();
    same_wake.input = "other".to_string();
    same_wake.created_at_ms = 99;
    assert_eq!(same_wake.wake_id(), wake.wake_id());
    for other in [
        process_wake("other-session", process_id.clone(), 4),
        process_wake("session", crate::process_id_for_test("other-process"), 4),
        process_wake("session", process_id, 5),
    ] {
        assert_ne!(other.wake_id(), wake.wake_id());
    }
}

/// The encoded wake carries neither a stored id nor a copy of the event's
/// invocation.
#[test]
fn an_encoded_process_wake_states_no_identity_and_no_event_invocation() {
    let encoded = serde_json::to_value(process_wake(
        "session",
        crate::process_id_for_test("process"),
        4,
    ))
    .expect("encode wake delivery");
    let fields = encoded.as_object().expect("wake delivery object");
    assert!(!fields.contains_key("wake_id"));
    assert!(!fields.contains_key("event_invocation"));
}
