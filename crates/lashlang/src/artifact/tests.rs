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

/// The refs are now derived from borrowed content instead of by rebuilding the
/// artifact, so the store's admission check has to keep refusing an artifact
/// whose refs do not describe the content it carries.
///
/// Red side: dropping the `artifact.verify()?;` line from
/// `InMemoryLashlangArtifactStore::publish_module_artifact`, or either of the
/// two ref comparisons exercised here, lets the forged artifacts publish.
/// Each half gets its own store so the refusal under test is the ref check and
/// never the immutability check on an already-published ref.
#[tokio::test(flavor = "current_thread")]
async fn publish_refuses_an_artifact_whose_refs_do_not_match_its_content() {
    let owner = lash_core::ArtifactOwner::host("fig-3088");
    let honest = process_typed_artifact("event");

    // A forged `module_ref`: the content is the "payload" program, the ref is
    // the one the "event" program hashes to.
    let mut forged_module_ref = process_typed_artifact("payload");
    let payload_ref = forged_module_ref.module_ref.clone();
    forged_module_ref.module_ref = honest.module_ref.clone();
    let store = InMemoryLashlangArtifactStore::new();
    let error = store
        .publish_module_artifact(&owner, &forged_module_ref)
        .await
        .expect_err("a module_ref that does not hash its own content must be refused");
    assert!(
        error.to_string().contains("module_ref"),
        "expected a module_ref mismatch, got {error}"
    );
    for refused in [&honest.module_ref, &payload_ref] {
        assert!(
            store
                .get_module_artifact(refused)
                .await
                .expect("the store reads back")
                .is_none(),
            "a refused publish must retain nothing"
        );
    }

    // A forged `host_requirements_ref`: the content and the module_ref are the
    // honest ones, only the requirements ref names requirements this artifact
    // does not carry, so the module_ref comparison passes and the second
    // comparison is the one that has to refuse it.
    let mut forged_requirements_ref = process_typed_artifact("event");
    let mut unrequested = forged_requirements_ref.host_requirements.clone();
    unrequested.globals.insert("unrequested_global".to_string());
    forged_requirements_ref.host_requirements_ref = host_requirements_ref(&unrequested);
    assert_ne!(
        forged_requirements_ref.host_requirements_ref,
        honest.host_requirements_ref
    );
    assert_eq!(forged_requirements_ref.module_ref, honest.module_ref);
    let store = InMemoryLashlangArtifactStore::new();
    let error = store
        .publish_module_artifact(&owner, &forged_requirements_ref)
        .await
        .expect_err("host requirements that do not hash to their ref must be refused");
    assert!(
        error.to_string().contains("host_requirements_ref"),
        "expected a host_requirements_ref mismatch, got {error}"
    );
    assert!(
        store
            .get_module_artifact(&honest.module_ref)
            .await
            .expect("the store reads back")
            .is_none(),
        "a refused publish must retain nothing"
    );

    // The same store still admits the artifact whose refs do match, so the
    // refusals above are the ref check and not a blanket rejection.
    let store = InMemoryLashlangArtifactStore::new();
    store
        .publish_module_artifact(&owner, &honest)
        .await
        .expect("an artifact whose refs match its content publishes");
    let stored = store
        .get_module_artifact(&honest.module_ref)
        .await
        .expect("the store reads back")
        .expect("the honest artifact is retained");
    assert_eq!(*stored, honest);
}
