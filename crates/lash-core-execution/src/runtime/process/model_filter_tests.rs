use serde_json::json;

use super::model::{
    ProcessExecutionEnvRef, ProcessIdentity, ProcessInput, ProcessProvenance, ProcessRecord,
    ProcessRegistration,
};

#[test]
fn process_identity_keeps_typed_engine_definitions_in_durable_encodings() {
    for signature in [
        crate::ProcessSignature::Unknown,
        crate::ProcessSignature::known(json!({"result": "string"})),
    ] {
        let reference = crate::ProcessDefinitionRef::new(
            "test-engine",
            json!({"program": "retained"}),
            signature.clone(),
        );
        let identity = ProcessIdentity::for_definition(reference, Some("retained"));
        let stored = serde_json::to_value(&identity).expect("encode engine identity");
        assert_eq!(
            stored,
            json!({
                "kind": "test-engine",
                "label": "retained",
                "definition": {
                    "engine_kind": "test-engine",
                    "definition": {"program": "retained"},
                    "signature": signature,
                },
            }),
            "durable identity must keep the admitted engine value and signature"
        );
        let decoded: ProcessIdentity =
            serde_json::from_value(stored).expect("decode typed engine identity");
        assert_eq!(decoded, identity);
        let packed = rmp_serde::to_vec_named(&identity).expect("pack engine identity");
        let unpacked: ProcessIdentity =
            rmp_serde::from_slice(&packed).expect("unpack engine identity");
        assert_eq!(unpacked, identity);
    }

    for malformed in [
        json!({"kind": "test-engine", "definition": {"program": "retained"}}),
        json!({"kind": "test-engine", "unexpected": true}),
    ] {
        assert!(serde_json::from_value::<ProcessIdentity>(malformed).is_err());
    }
}

#[test]
fn retained_host_start_requires_the_same_persisted_engine_definition() {
    let reference = crate::ProcessDefinitionRef::new(
        "test-engine",
        json!({"program": "retained"}),
        crate::ProcessSignature::known(json!({"result": "string"})),
    );
    let registration = ProcessRegistration::new(
        ProcessInput::Engine {
            kind: "test-engine".to_string(),
            payload: json!({"program": "retained"}),
        },
        ProcessProvenance::host(),
        crate::Lifetime::Detached,
    )
    .with_start_key(Some(crate::StartKey::for_host("retained-engine-identity")))
    .with_execution_env_ref(Some(ProcessExecutionEnvRef::new("retained-engine-env")))
    .with_admitted_identity(crate::AdmittedProcessIdentity::for_testing(
        ProcessIdentity::for_definition(reference, Some("retained")),
    ));
    let retained = ProcessRecord::from_registration(
        registration.clone(),
        crate::process_id_for_test("retained-engine-identity"),
    );
    let retained: ProcessRecord =
        serde_json::from_value(serde_json::to_value(retained).expect("persist retained process"))
            .expect("reopen retained process");
    super::validation::check_retained_start(&registration, &retained)
        .expect("the same host start returns the persisted process");

    for changed in [
        crate::ProcessDefinitionRef::new(
            "test-engine",
            json!({"program": "different"}),
            crate::ProcessSignature::known(json!({"result": "string"})),
        ),
        crate::ProcessDefinitionRef::new(
            "test-engine",
            json!({"program": "retained"}),
            crate::ProcessSignature::known(json!({"result": "number"})),
        ),
    ] {
        let conflicting = registration.clone().with_admitted_identity(
            crate::AdmittedProcessIdentity::for_testing(ProcessIdentity::for_definition(
                changed,
                Some("retained"),
            )),
        );
        assert!(matches!(
            super::validation::check_retained_start(&conflicting, &retained),
            Err(crate::PluginError::StartKeyConflict { start_key })
                if registration.start_key.as_ref() == Some(&start_key)
        ));
    }
}
