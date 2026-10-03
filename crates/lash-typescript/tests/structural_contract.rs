use lash_typescript::DiagnosticCode;
use serde_json::json;

#[test]
fn parameter_defaults_and_rest_are_accepted_while_declare_stays_rejected() {
    lash_typescript::testing::compile(
        "function f(value = 1, ...values) { return value + values.length; } finish(f());",
    )
    .expect("parameter defaults and rest compile");
    let error = lash_typescript::testing::compile("declare const value: number;")
        .expect_err("ambient declarations remain outside executable cells");
    assert_eq!(error.code, DiagnosticCode::DeclareUnsupported);
}

#[test]
fn tool_schema_uses_typescript_field_spelling() {
    let ty =
        lash_typescript::render_schema_shape(&lash_sansio::SchemaShape::from_json_schema(&json!({
            "type": "object", "additionalProperties": false,
            "properties": { "delete": { "type": "string" } }, "required": ["delete"]
        })));
    assert_eq!(ty, r#"{ "delete": string }"#);
}
