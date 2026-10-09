//! String coercion of an object is ECMA-262's answer (FIG-3652).
//!
//! `"" + value`, `` `${value}` `` and `String(value)` run ToPrimitive: an
//! object's own `valueOf`/`toString` answer in hint order, and an object with
//! no string of its own answers its type tag — `[object Object]`,
//! `[object Map]`, `[object Set]` — exactly as Node does. Register entry 13's
//! refusal of those three spellings (FIG-3166) is retired: they are supported
//! constructs, so the dialect answers them rather than refusing them. A
//! function has no string the dialect keeps (its source text), so converting
//! one refuses as `TS_FUNCTION_STRING_COERCION`.

use lash_vm::{AbilityOp, AbilityOutcome, ExecutionHost, ExecutionHostError, State, Value};

#[derive(Default)]
struct Host(std::sync::Mutex<Vec<String>>);

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::Print(value) => {
                self.0.lock().expect("print lock").push(match value {
                    Value::String(text) => text.to_string(),
                    other => format!("{other:?}"),
                });
                Ok(AbilityOutcome::Unit)
            }
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            _ => Err(ExecutionHostError::new("unsupported test ability")),
        }
    }
}

fn finished(source: &str) -> Value {
    let program = lash_typescript::testing::compile(source).expect("TypeScript should compile");
    let host = Host::default();
    match futures::executor::block_on(lash_vm::execute(&program, &mut State::new(), &host))
        .expect("TypeScript should execute")
    {
        lash_vm::ExecutionOutcome::Finished(value) => value,
        other => panic!("expected a finished value, got {other:?}"),
    }
}

fn finished_string(source: &str) -> String {
    match finished(source) {
        Value::String(text) => text.to_string(),
        other => panic!("expected a string, got {other:?}"),
    }
}

/// Runs `source` and returns the refusal's debug text.
fn refusal(source: &str) -> String {
    let program = lash_typescript::testing::compile(source).expect("TypeScript should compile");
    let host = Host::default();
    let error = futures::executor::block_on(lash_vm::execute(&program, &mut State::new(), &host))
        .expect_err("the coercion refuses");
    format!("{error:?}")
}

#[test]
fn an_object_reached_through_a_container_answers_its_type_tag() {
    assert_eq!(
        finished_string("finish(`${[{ a: 1 }, 2]}`);"),
        "[object Object],2"
    );
    assert_eq!(
        finished_string(
            "const result = { rows: [{ id: 1 }], total: 1 };\nfinish(`found ${result}`);"
        ),
        "found [object Object]"
    );
}

#[test]
fn property_key_coercion_is_untouched() {
    // An object used as a property key still coerces to `"[object Object]"` —
    // the key is the one conversion that asked for the type tag on purpose, so
    // the array refusal that reports it keeps reporting it, with its own code.
    let text = refusal("const a: any = [1]; a[{ b: 2 }] = 3;");
    assert!(
        text.contains("ArrayNonIndexPropertyUnsupported") && text.contains("[object Object]"),
        "key coercion still produces the type tag: {text}"
    );
}
