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
    let declarations = raw["ir"]["declarations"]
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
fn frozen_predecessor_artifact_is_refused_by_its_shape() {
    // A pre-FIG-3571 artifact carries a renamed `canonical_ir` and no program
    // `language`; the one-carrier shape refuses it before any identity check.
    let mut raw: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/module-artifact-old.json"
    ))
    .expect("frozen fixture should be JSON");
    let object = raw.as_object_mut().expect("artifact is an object");
    object.remove("trigger_key_manifest");
    // The frozen fixture predates ADR 0096 and still records a dialect,
    // which is its own typed refusal (see
    // `a_recorded_compilation_dialect_is_refused_as_a_retired_field`).
    object.remove("compilation_dialect");
    let error = ModuleArtifact::from_store_bytes(
        &serde_json::to_vec(&raw).expect("legacy artifact should encode"),
    )
    .expect_err("a predecessor artifact must be refused");
    assert!(
        matches!(&error, ModuleArtifactError::Codec(message) if message.contains("`ir`")),
        "{error}"
    );
}

#[test]
fn future_shape_refuses_before_serde_reaches_unknown_variants() {
    let mut raw: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/module-artifact-old.json"
    ))
    .expect("frozen fixture should be JSON");
    raw["compilation_dialect"] = serde_json::json!("future_dialect");
    raw["ir"] = serde_json::json!({"language": "typescript", "main": {"FutureExpr": null}});

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
    raw["ir"] = serde_json::json!({"language": "typescript", "main": {"FutureExpr": null}});

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

/// An artifact's refs are private and derived from its content, so a forged
/// ref can only arrive as stored bytes; the store decoder is what refuses it.
/// The refs are derived from borrowed content rather than by rebuilding the
/// artifact (FIG-3088), and each comparison is exercised on its own.
///
/// Red side: dropping the `artifact.verify()?;` line from
/// `ModuleArtifact::from_store_bytes`, or either of the two ref comparisons
/// exercised here, lets the forged bytes decode.
#[test]
fn store_decode_refuses_bytes_whose_refs_do_not_match_their_content() {
    let honest = process_typed_artifact("event");

    // A forged `module_ref`: the content is the "payload" program, the ref is
    // the one the "event" program hashes to.
    let mut forged_module_ref = process_typed_artifact("payload");
    forged_module_ref.module_ref = honest.module_ref.clone();
    let error = ModuleArtifact::from_store_bytes(
        &forged_module_ref
            .to_store_bytes()
            .expect("the forged artifact encodes"),
    )
    .expect_err("a module_ref that does not hash its own content must be refused");
    assert!(
        error.to_string().contains("module_ref"),
        "expected a module_ref mismatch, got {error}"
    );

    // A forged `host_requirements_ref`: the content and the module_ref are the
    // honest ones, only the requirements ref names requirements this artifact
    // does not carry, so the module_ref comparison passes and the second
    // comparison is the one that has to refuse it.
    let mut forged_requirements_ref = process_typed_artifact("event");
    let mut unrequested = forged_requirements_ref.host_requirements.clone();
    unrequested.globals.insert("unrequested_global".to_string());
    forged_requirements_ref.host_requirements_ref = hash_host_requirements(&unrequested);
    assert_ne!(
        forged_requirements_ref.host_requirements_ref,
        honest.host_requirements_ref
    );
    assert_eq!(forged_requirements_ref.module_ref, honest.module_ref);
    let error = ModuleArtifact::from_store_bytes(
        &forged_requirements_ref
            .to_store_bytes()
            .expect("the forged artifact encodes"),
    )
    .expect_err("host requirements that do not hash to their ref must be refused");
    assert!(
        error.to_string().contains("host_requirements_ref"),
        "expected a host_requirements_ref mismatch, got {error}"
    );

    // The honest artifact's bytes decode, so the refusals above are the ref
    // check and not a blanket rejection.
    let decoded = ModuleArtifact::from_store_bytes(
        &honest
            .to_store_bytes()
            .expect("the honest artifact encodes"),
    )
    .expect("an artifact whose refs match its content decodes");
    assert_eq!(decoded, honest);
}

/// One module ref addresses one byte string, and a name is part of it.
///
/// The FIG-3120 pair: the perf guard's `durable_agent_child_turn_*` cell
/// (`const spawnChild = ...`) and its high-traffic twin (`const loadChild =
/// ...`) differ only in one main-level binder. Before FIG-3571 the identity
/// alpha-normalized that binder, so the pair shared a ref and the stored IR had
/// to be renamed to match. The artifact now stores the linked program verbatim
/// and the ref hashes it names included, so the pair names two modules, each
/// ref addresses exactly the bytes it hashes, and both round-trip.
#[test]
fn alpha_variant_cells_name_distinct_modules() {
    fn artifact(binding: &str) -> ModuleArtifact {
        ModuleArtifact::from_program(b::module(
            vec![b::process_returning(
                "worker",
                vec![b::param("tick", TypeExpr::Str)],
                TypeExpr::Bool,
                b::block(vec![b::finish(b::bool_lit(true))]),
            )],
            vec![
                b::assign(binding, b::string("seed")),
                b::finish(b::var(binding)),
            ],
        ))
        .expect("artifact builds")
    }

    let spawn_child = artifact("spawnChild");
    let load_child = artifact("loadChild");
    assert_ne!(
        spawn_child.module_ref, load_child.module_ref,
        "alpha variants name distinct modules"
    );
    for (artifact, name) in [(&spawn_child, "spawnChild"), (&load_child, "loadChild")] {
        let bytes = artifact.to_store_bytes().expect("artifact encodes");
        let encoded = String::from_utf8(bytes.clone()).expect("artifact bytes are UTF-8");
        assert!(
            encoded.contains(name),
            "the stored artifact keeps the binder name: {encoded}"
        );
        assert_eq!(
            &ModuleArtifact::from_store_bytes(&bytes).expect("artifact decodes"),
            artifact,
            "the artifact round-trips"
        );
    }
}

/// An ABI name is not a local: a process parameter still names itself in the
/// stored artifact, and renaming one still moves the module ref.
#[test]
fn process_parameter_names_stay_in_the_ir() {
    let event = process_typed_artifact("event");
    let encoded =
        String::from_utf8(event.to_store_bytes().expect("encodes")).expect("bytes are UTF-8");
    assert!(encoded.contains("\"event\""), "{encoded}");
    assert_ne!(
        event.module_ref,
        process_typed_artifact("payload").module_ref
    );
}

/// A process's origin is derived by the linker (FIG-3571): a program handed
/// to it cannot claim a lifted process, and no program an artifact carries can
/// hold an origin its declaration contradicts.
#[test]
fn process_origins_are_derived_never_authored() {
    let lifted_body =
        || crate::testing::ast_builders::finish(crate::testing::ast_builders::bool_lit(true));
    let with_process = |name: &str, origin: crate::ProcessOrigin, params: usize| {
        let mut declaration = crate::testing::ast_builders::process_returning(
            name,
            (0..params)
                .map(|index| {
                    crate::testing::ast_builders::param(&format!("p{index}"), TypeExpr::Any)
                })
                .collect(),
            TypeExpr::Bool,
            lifted_body(),
        );
        if let Declaration::Process(process) = &mut declaration {
            process.origin = origin;
        }
        crate::testing::ast_builders::module(vec![declaration], Vec::new())
    };
    let lifted_name = format!("{}{}", crate::LIFTED_PROCESS_NAME_PREFIX, "0".repeat(64));
    let lifted = |hidden_params| crate::ProcessOrigin::Lifted {
        site: crate::AstPath::main(vec![0, 0]),
        hidden_params,
    };
    for (program, reason) in [
        (
            with_process(&lifted_name, crate::ProcessOrigin::Declared, 0),
            "a declared process cannot take a lifted process's name",
        ),
        (
            with_process("authored", lifted(0), 0),
            "a lifted process is named by its literal's digest",
        ),
        (
            with_process(&lifted_name, lifted(2), 1),
            "a lifted process has more hidden parameters than parameters",
        ),
    ] {
        assert!(matches!(
            crate::validate_ast(&program),
            Err(crate::InvalidAst::InvalidProcessOrigin { reason: refused, .. }) if refused == reason
        ));
        assert!(ModuleArtifact::from_program(program).is_err(), "{reason}");
    }
    let claimed = with_process(&lifted_name, lifted(0), 0);
    crate::validate_ast(&claimed).expect("a well-formed lifted declaration validates");
    assert!(matches!(
        crate::LinkedModule::link(claimed, crate::testing::harness::test_environment()),
        Err(crate::LinkError::InvalidAst {
            source: crate::InvalidAst::InvalidProcessOrigin { .. }
        })
    ));
}
