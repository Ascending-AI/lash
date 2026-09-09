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
fn parameter_defaults_and_rest_are_accepted_while_declare_stays_rejected() {
    lash_typescript::compile(
        "function f(value = 1, ...values) { return value + values.length; } finish(f());",
    )
    .expect("parameter defaults and rest compile");
    let error = lash_typescript::compile("declare const value: number;")
        .expect_err("ambient declarations remain outside executable cells");
    assert_eq!(error.code, DiagnosticCode::DeclareUnsupported);
}

/// Console arities join with a single space, and each argument is rendered for
/// the observation rather than string-coerced.
///
/// The joining rule is unchanged; the per-argument rendering is not. This test
/// asserted `[2, 3]` arriving as `"2,3"` — ECMAScript's array coercion, which is
/// what turned every logged object into `"[object Object]"` (FIG-2767). Arrays
/// and plain objects now render as JSON; every other value keeps its JavaScript
/// string, which is why `1` and `null` are untouched here.
#[test]
fn console_methods_accept_zero_and_multiple_arguments_with_observation_rendering() {
    let program = lash_typescript::compile(
        "console.log(); console.warn(1, null, [2, 3]); console.error('e'); console.info('i'); console.debug('d'); finish(0);",
    )
    .expect("console method arities compile");
    let host = PrintHost::default();
    futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &host))
        .expect("console.log arities execute");
    assert_eq!(
        *host.0.lock().expect("print journal"),
        vec![
            Value::String("".into()),
            Value::String("1 null [2,3]".into()),
            Value::String("e".into()),
            Value::String("i".into()),
            Value::String("d".into()),
        ]
    );
}

#[test]
fn tool_signatures_are_async_reserved_safe_and_collision_proof() {
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
        "declare function __lash_tool_7365617263682d646f6373(input: { \"delete\": string }): Promise<number>;"
    );
    assert!(signature.contains("Promise"));

    let reserved = lash_typescript::render_tool_signature("delete", &json!({}), None);
    let prefix_collision =
        lash_typescript::render_tool_signature("__lash_tool_64656c657465", &json!({}), None);
    assert!(reserved.starts_with("declare function __lash_tool_64656c657465("));
    assert_ne!(reserved, prefix_collision);
}
