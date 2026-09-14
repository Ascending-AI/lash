use super::*;
use crate::ast::TypeExpr;
use crate::testing::ast_builders as b;

fn process_typed_artifact(param_name: &str) -> ModuleArtifact {
    // `process target(<param_name>: str) -> bool { finish true }`
    // `process install(handler: Process<(<param_name>: str), bool>) -> bool { finish true }`
    ModuleArtifact::from_program(b::module(
        vec![
            b::process_returning(
                "target",
                vec![b::param(param_name, TypeExpr::Str)],
                TypeExpr::Bool,
                b::block(vec![b::finish(b::bool_lit(true))]),
            ),
            b::process_returning(
                "install",
                vec![b::param(
                    "handler",
                    b::process_type(vec![b::param(param_name, TypeExpr::Str)], TypeExpr::Bool),
                )],
                TypeExpr::Bool,
                b::block(vec![b::finish(b::bool_lit(true))]),
            ),
        ],
        Vec::new(),
    ))
    .expect("artifact builds")
}

#[test]
fn named_process_signature_round_trips_and_names_change_identity() {
    let event = process_typed_artifact("event");
    let payload = process_typed_artifact("payload");
    let bytes = event.to_store_bytes().expect("artifact encodes");
    let decoded = ModuleArtifact::from_store_bytes(&bytes).expect("artifact decodes");

    assert_eq!(decoded, event);
    assert_ne!(event.module_ref, payload.module_ref);
    assert_ne!(event.process_ref("target"), payload.process_ref("target"));
}

#[test]
fn artifact_explicitly_refuses_obsolete_process_type_shape() {
    let artifact = process_typed_artifact("event");
    let mut raw = serde_json::to_value(&artifact).expect("artifact serializes");
    let declarations = raw["canonical_ir"]["declarations"]
        .as_array_mut()
        .expect("declarations array");
    let install = declarations
        .iter_mut()
        .find(|declaration| declaration["Process"]["name"] == "install")
        .expect("install declaration");
    install["Process"]["params"][0]["ty"] = serde_json::json!({
        "Process": {"input": "Str", "output": "Bool", "input_count": 1}
    });

    let error = ModuleArtifact::from_store_bytes(&serde_json::to_vec(&raw).unwrap())
        .expect_err("old process type must be refused");
    assert!(matches!(
        error,
        ModuleArtifactError::ObsoleteProcessTypeShape
    ));
}

#[test]
fn artifact_decoder_refuses_duplicate_signature_fields_and_parameter_extras() {
    let bytes = process_typed_artifact("event")
        .to_store_bytes()
        .expect("artifact encodes");
    let source = String::from_utf8(bytes).expect("artifact encoding is JSON");
    let canonical =
        r#""Process":{"kind":"known","params":[{"name":"event","ty":"Str"}],"output":"Bool"}"#;
    assert_eq!(source.matches(canonical).count(), 1);
    let cases = [
        (
            "duplicate kind",
            r#""Process":{"kind":"unknown","kind":"known","params":[{"name":"event","ty":"Str"}],"output":"Bool"}"#,
        ),
        (
            "duplicate params",
            r#""Process":{"kind":"known","params":[],"params":[{"name":"event","ty":"Str"}],"output":"Bool"}"#,
        ),
        (
            "duplicate output",
            r#""Process":{"kind":"known","params":[{"name":"event","ty":"Str"}],"output":"Str","output":"Bool"}"#,
        ),
        (
            "unknown parameter field",
            r#""Process":{"kind":"known","params":[{"name":"event","ty":"Str","extra":true}],"output":"Bool"}"#,
        ),
    ];

    for (description, replacement) in cases {
        let malformed = source.replacen(canonical, replacement, 1);
        let error = ModuleArtifact::from_store_bytes(malformed.as_bytes())
            .expect_err("malformed signature bytes must be refused");
        assert!(
            matches!(error, ModuleArtifactError::Codec(_)),
            "{description}: {error}"
        );
    }
}

#[test]
fn raw_artifact_builder_refuses_an_incomplete_process_output() {
    // `process plain(message: str) { finish true }` — no declared output.
    let program = b::module(
        vec![b::process(
            "plain",
            vec![b::param("message", TypeExpr::Str)],
            b::block(vec![b::finish(b::bool_lit(true))]),
        )],
        Vec::new(),
    );
    let error = ModuleArtifact::from_program(program)
        .expect_err("raw artifact IR must carry a complete process output");
    assert!(matches!(
        error,
        ModuleArtifactError::IncompleteProcessSignature { ref process }
            if process == "plain"
    ));
}

#[test]
fn artifact_with_obsolete_trigger_manifest_field_is_explicitly_rejected() {
    let error = ModuleArtifact::from_store_bytes(
        include_str!("../../tests/fixtures/module-artifact-old.json").as_bytes(),
    )
    .expect_err("an artifact carrying current-trigger manifest state must be refused");
    assert!(matches!(error, ModuleArtifactError::FutureShape { .. }));
    assert!(error.to_string().contains("trigger_key_manifest"));
}

/// TypeScript is the sole RLM language (ADR 0096), so an artifact that
/// still records a compilation dialect was published by a pre-cutover build
/// and is refused as an incompatible format rather than read with a default.
#[test]
fn a_recorded_compilation_dialect_is_refused_as_a_retired_field() {
    let mut raw: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/module-artifact-old.json"
    ))
    .expect("frozen fixture should be JSON");
    raw.as_object_mut()
        .expect("artifact is an object")
        .remove("trigger_key_manifest");
    assert!(
        raw.get("compilation_dialect").is_some(),
        "the frozen fixture must still carry the retired field"
    );
    let error = ModuleArtifact::from_store_bytes(
        &serde_json::to_vec(&raw).expect("legacy artifact should encode"),
    )
    .expect_err("an artifact recording a dialect must be refused");
    assert!(matches!(
        error,
        ModuleArtifactError::RetiredCompilationDialect
    ));
    assert!(error.to_string().contains("compilation_dialect"));
}

#[test]
fn frozen_sha256_artifact_without_the_obsolete_field_hits_the_identity_fence() {
    let mut raw: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/module-artifact-old.json"
    ))
    .expect("frozen fixture should be JSON");
    let object = raw.as_object_mut().expect("artifact is an object");
    object.remove("trigger_key_manifest");
    // The frozen fixture predates ADR 0096 and still records a dialect,
    // which is its own typed refusal (see
    // `a_recorded_compilation_dialect_is_refused_as_a_retired_field`).
    // Drop it so the subject here stays the identity fence.
    object.remove("compilation_dialect");
    raw["canonical_ir"]["declarations"][0]["Process"]["return_ty"] = serde_json::json!("Str");
    let error = ModuleArtifact::from_store_bytes(
        &serde_json::to_vec(&raw).expect("legacy artifact should encode"),
    )
    .expect_err("a SHA-256 artifact must not verify under the BLAKE3 generation");
    assert!(matches!(error, ModuleArtifactError::HashMismatch { .. }));
    assert!(error.to_string().contains("lashlang:v2:blake3:"));
}

#[test]
fn future_shape_refuses_before_serde_reaches_unknown_variants() {
    let mut raw: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/module-artifact-old.json"
    ))
    .expect("frozen fixture should be JSON");
    raw["compilation_dialect"] = serde_json::json!("future_dialect");
    raw["canonical_ir"]["main"] = serde_json::json!({"FutureExpr": null});

    let error = ModuleArtifact::from_store_bytes(
        &serde_json::to_vec(&raw).expect("future fixture should encode"),
    )
    .expect_err("a future artifact shape must be refused");
    assert!(matches!(error, ModuleArtifactError::FutureShape { .. }));
    let message = error.to_string();
    assert!(message.contains("recompile and republish"), "{message}");
    assert!(!message.contains("unknown variant"), "{message}");
}

#[test]
fn unchanged_dialect_with_unknown_nested_variant_is_a_future_shape_refusal() {
    let mut raw: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/module-artifact-old.json"
    ))
    .expect("frozen fixture should be JSON");
    raw["canonical_ir"]["main"] = serde_json::json!({"FutureExpr": null});

    let error = ModuleArtifact::from_store_bytes(
        &serde_json::to_vec(&raw).expect("future fixture should encode"),
    )
    .expect_err("a known-dialect future variant must be refused legibly");
    assert!(matches!(error, ModuleArtifactError::FutureShape { .. }));
    let message = error.to_string();
    assert!(message.contains("recompile and republish"), "{message}");
    assert!(!message.contains("unknown variant"), "{message}");
}

#[test]
fn malformed_artifact_json_remains_an_undecodable_codec_error() {
    let error = ModuleArtifact::from_store_bytes(br#"{"#)
        .expect_err("malformed JSON must remain undecodable");
    assert!(matches!(error, ModuleArtifactError::Codec(_)));
    assert!(!matches!(error, ModuleArtifactError::FutureShape { .. }));
}
