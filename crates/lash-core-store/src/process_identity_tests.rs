use super::*;

/// ADR 0004: a process execution environment is a closed, typed shape —
/// policy, tool authority and plugin-owned options. A field the type does not declare is a
/// missing capability, and it is refused when the environment decodes, not
/// discovered later during process recovery.
#[test]
fn a_process_execution_environment_rejects_unknown_fields() {
    let spec = ProcessExecutionEnvSpec::new(
        crate::AdmittedPluginConfig::default(),
        crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        ),
        crate::SessionToolAccess::restricted(Vec::new()).expect("no resident tools"),
    );
    let encoded = spec.to_store_bytes().expect("encode the environment");
    assert_eq!(
        ProcessExecutionEnvSpec::from_store_bytes(&encoded).expect("the environment decodes"),
        spec,
        "a conforming environment round-trips"
    );

    let mut missing: serde_json::Value = serde_json::from_slice(&encoded).expect("object");
    missing
        .as_object_mut()
        .expect("object")
        .remove("tool_access");
    assert!(
        ProcessExecutionEnvSpec::from_store_bytes(
            &serde_json::to_vec(&missing).expect("encode missing authority")
        )
        .expect_err("authority is required")
        .to_string()
        .contains("tool_access")
    );
    assert_ne!(
        spec.stable_ref().expect("restricted reference"),
        ProcessExecutionEnvSpec::new(
            spec.plugin_config.clone(),
            spec.policy.clone(),
            crate::SessionToolAccess::ambient()
        )
        .stable_ref()
        .expect("ambient reference")
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
    let policy = crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
        crate::NoProgressBudget::bounded(12),
    );
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
