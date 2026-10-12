use super::*;

const PROBE: ProcessConfigKey<serde_json::Value> =
    ProcessConfigKey::new("probe", FormatVersion::ONE);

fn restricted() -> SessionToolAccess {
    SessionToolAccess::restricted(Vec::new()).expect("no tools")
}

/// ADR 0004: a process environment is a closed, typed shape. A field the
/// type does not declare is refused when the bytes decode, not discovered
/// later during process recovery; so is the retired captured-session shape.
#[test]
fn a_process_env_refuses_unknown_fields_and_the_retired_shape() {
    let env = ProcessEnv::builder(restricted(), MaxToolCalls::new(8))
        .component(PROBE, &serde_json::json!({ "probe": 1 }))
        .build()
        .expect("env");
    let bytes = env.to_store_bytes().expect("encode");
    assert_eq!(ProcessEnv::from_store_bytes(&bytes).expect("decode"), env);

    let mut fields: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&bytes).expect("object");
    fields.insert("plugin_config".into(), serde_json::json!({}));
    let error = ProcessEnv::from_store_bytes(&serde_json::to_vec(&fields).expect("encode"))
        .expect_err("an undeclared field is refused");
    assert!(error.to_string().contains("unknown field"), "{error}");
}

/// D11: component order is composition order and part of the address; an
/// id names one component once.
#[test]
fn component_order_is_part_of_the_address_and_ids_are_unique() {
    const OTHER: ProcessConfigKey<()> = ProcessConfigKey::new("other", FormatVersion::ONE);
    let forward = ProcessEnv::builder(restricted(), MaxToolCalls::new(8))
        .component(PROBE, &serde_json::Value::Null)
        .component(OTHER, &())
        .build()
        .expect("env");
    let reversed = ProcessEnv::builder(restricted(), MaxToolCalls::new(8))
        .component(OTHER, &())
        .component(PROBE, &serde_json::Value::Null)
        .build()
        .expect("env");
    assert_ne!(
        forward.stable_ref().expect("ref"),
        reversed.stable_ref().expect("ref")
    );
    assert_eq!(
        ProcessEnv::builder(restricted(), MaxToolCalls::new(8))
            .component(PROBE, &serde_json::Value::Null)
            .component(PROBE, &serde_json::Value::Null)
            .build(),
        Err(ProcessEnvRefusal::DuplicateComponent { id: "probe".into() })
    );
    assert_eq!(
        ProcessComponentId::new("a:b"),
        Err(ProcessComponentIdError::Colon { id: "a:b".into() })
    );
    assert!(
        serde_json::from_value::<ProcessComponentId>(serde_json::json!("a:b")).is_err(),
        "a stored id with `:` is refused at decode"
    );
}
