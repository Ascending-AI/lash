use std::sync::Mutex;

use lash_typescript::DiagnosticCode;
use lashlang::{AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, State, Value};
use serde_json::json;

#[derive(Default)]
struct PrintHost(Mutex<Vec<Value>>);

impl ExecutionHost for PrintHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Print(value) => {
                self.0.lock().expect("print journal").push(value);
                Ok(AbilityResult::Value(Value::Null))
            }
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new(
                "unexpected structural-test ability",
            )),
        }
    }
}

#[test]
fn unsupported_parameter_and_declare_shapes_have_precise_diagnostics() {
    let cases = [
        (
            "function f(value = 1) { return value; }",
            DiagnosticCode::ParameterDefaultUnsupported,
        ),
        (
            "function f(...values) { return 1; }",
            DiagnosticCode::ParameterRestUnsupported,
        ),
        (
            "declare const value: number;",
            DiagnosticCode::DeclareUnsupported,
        ),
    ];
    for (source, expected) in cases {
        let error = lash_typescript::compile(source).expect_err("shape must reject");
        assert_eq!(error.code, expected, "source: {source}");
    }
}

#[test]
fn console_log_accepts_zero_and_multiple_arguments_with_to_string_joining() {
    let program =
        lash_typescript::compile("console.log(); console.log(1, null, [2, 3]); finish(0);")
            .expect("console.log arities compile");
    let host = PrintHost::default();
    futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &host))
        .expect("console.log arities execute");
    assert_eq!(
        *host.0.lock().expect("print journal"),
        vec![Value::String("".into()), Value::String("1 null 2,3".into())]
    );
}

#[test]
fn tool_signatures_are_synchronous_reserved_safe_and_collision_proof() {
    let signature = lash_typescript::render_tool_signature(
        "search-docs",
        &json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "delete": { "type": "string" } },
            "required": ["delete"]
        }),
        Some(&json!({ "type": "number" })),
    );
    assert_eq!(
        signature,
        "declare function __lash_tool_7365617263682d646f6373(input: { \"delete\": string }): number;"
    );
    assert!(!signature.contains("Promise"));

    let reserved = lash_typescript::render_tool_signature("delete", &json!({}), None);
    let prefix_collision =
        lash_typescript::render_tool_signature("__lash_tool_64656c657465", &json!({}), None);
    assert!(reserved.starts_with("declare function __lash_tool_64656c657465("));
    assert_ne!(reserved, prefix_collision);
}
