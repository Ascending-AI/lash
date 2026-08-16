use lash_typescript::{DiagnosticCode, SourceSpan};

#[test]
fn typescript_frontend_compiles_valid_source_and_names_rejections() {
    let program = lash_typescript::parse("const answer: number = 40 + 2; finish(answer);")
        .expect("parse TypeScript");
    lash_typescript::validate("const answer: number = 40 + 2; finish(answer);")
        .expect("validate TypeScript");
    let compiled =
        lash_typescript::compile("const answer = 42; finish(answer);").expect("compile TypeScript");
    assert!(matches!(program.main, lashlang::Expr::Block(_)));
    assert_eq!(compiled.compile_stats().type_literals_dynamic, 0);

    let diagnostic = lash_typescript::parse("class Unsupported {}").expect_err("reject class");
    assert_eq!(diagnostic.code, DiagnosticCode::ClassUnsupported);
    assert_eq!(diagnostic.code.as_str(), "TS_CLASS_UNSUPPORTED");
    assert!(diagnostic.message.contains("classes"));
    let span: SourceSpan = diagnostic.span.expect("class span");
    assert!(span.end > span.start);
}

#[test]
fn typescript_linking_and_schema_signatures_use_shared_lash_types() {
    let host = lashlang::LashlangHostEnvironment::default();
    let linked = lash_typescript::link("const answer = 42; finish(answer);", &host)
        .expect("link TypeScript");
    let compiled = lash_typescript::compile_linked(&linked);
    assert_eq!(compiled.compile_stats().type_literals_dynamic, 0);

    let output_schema = serde_json::json!({"type": "array", "items": {"type": "string"}});
    let signature = lash_typescript::render_tool_signature(
        "search",
        &serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {"query": {"type": "string"}},
            "required": ["query"]
        }),
        Some(&output_schema),
    );
    assert_eq!(
        signature,
        "declare function search(input: { query: string }): Promise<Array<string>>;"
    );
}

#[test]
fn flagship_codemode_examples_stay_valid_in_both_dialects() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../codemode-parity");
    let host = {
        let mut catalog = lashlang::LashlangHostCatalog::new();
        catalog
            .add_module_operation_binding(
                ["web"],
                "Web",
                "fetch",
                "tool:web/fetch",
                lashlang::ResourceOperationBinding {
                    input_ty: lashlang::TypeExpr::Any,
                    output_ty: lashlang::TypeExpr::Any,
                    output_from_input: None,
                },
            )
            .expect("example web binding");
        lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::all())
    };

    let lashlang_turn = std::fs::read_to_string(root.join("turn.lash")).expect("Lashlang turn");
    lashlang::LinkedModule::link(
        lashlang::parse(&lashlang_turn).expect("parse Lashlang turn"),
        &host,
    )
    .expect("link Lashlang turn");
    let typescript_turn = std::fs::read_to_string(root.join("turn.ts")).expect("TypeScript turn");
    lash_typescript::link(&typescript_turn, &host).expect("link TypeScript turn");

    let lashlang_process =
        std::fs::read_to_string(root.join("durable-process.lash")).expect("Lashlang process");
    let lashlang_program = lashlang::parse(&lashlang_process).expect("parse Lashlang process");
    lashlang::LinkedModule::link(lashlang_program, &host).expect("link Lashlang process");
    let typescript_process =
        std::fs::read_to_string(root.join("durable-process.ts")).expect("TypeScript process");
    let linked =
        lash_typescript::link(&typescript_process, &host).expect("link TypeScript process");
    assert_eq!(
        linked.artifact.compilation_dialect,
        lashlang::CompilationDialect::Typescript
    );
}
