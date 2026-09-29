use super::*;

/// ADR 0004: a process execution environment is a closed, typed shape —
/// policy plus plugin-owned options. A field the type does not declare is a
/// missing capability, and it is refused when the environment decodes, not
/// discovered later during process recovery.
#[test]
fn a_process_execution_environment_rejects_unknown_fields() {
    let spec = ProcessExecutionEnvSpec::new(
        crate::PluginOptions::empty(),
        crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
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
