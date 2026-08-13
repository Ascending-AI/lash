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
        "declare function search(input: { query: string }): Array<string>;"
    );
}
