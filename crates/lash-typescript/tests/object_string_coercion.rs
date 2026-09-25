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

use lashlang::{AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, State, Value};

#[derive(Default)]
struct Host(std::sync::Mutex<Vec<String>>);

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Print(value) => {
                self.0.lock().expect("print lock").push(match value {
                    Value::String(text) => text.to_string(),
                    other => format!("{other:?}"),
                });
                Ok(AbilityResult::Unit)
            }
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unsupported test ability")),
        }
    }
}

fn finished(source: &str) -> Value {
    let program = lash_typescript::testing::compile(source).expect("TypeScript should compile");
    let host = Host::default();
    match futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &host))
        .expect("TypeScript should execute")
    {
        lashlang::ExecutionOutcome::Finished(value) => value,
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
    let error = futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &host))
        .expect_err("the coercion refuses");
    format!("{error:?}")
}

/// Every spelling of "put this value into a string".
fn coercion_spellings(expression: &str) -> Vec<String> {
    vec![
        format!(r#"finish("" + ({expression}));"#),
        format!(r#"finish(`${{{expression}}}`);"#),
        format!("finish(String({expression}));"),
        format!(r#"finish(({expression}) + "");"#),
    ]
}

#[test]
fn every_string_coercion_of_a_tag_only_object_answers_its_type_tag() {
    for (expression, tag) in [
        ("{ a: 1 }", "[object Object]"),
        (r#"new Map([["k", 1]])"#, "[object Map]"),
        ("new Set([1])", "[object Set]"),
    ] {
        for source in coercion_spellings(expression) {
            assert_eq!(finished_string(&source), tag, "{source}");
        }
    }
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
fn converting_a_function_to_a_string_refuses_by_name() {
    for source in coercion_spellings("() => 1") {
        let text = refusal(&source);
        assert!(
            text.contains("TS_FUNCTION_STRING_COERCION"),
            "expected the function refusal for {source}, got {text}"
        );
    }
}

#[test]
fn a_date_coerces_to_its_ecma_date_string() {
    // FIG-3704 gave `Date` a real ECMA string, so the FIG-3166 refusal no
    // longer applies to it — `+` answers the deterministic UTC DateString.
    assert_eq!(
        finished_string(r#"finish("" + new Date(0));"#),
        "Thu Jan 01 1970 00:00:00 GMT+0000 (Coordinated Universal Time)"
    );
}

#[test]
fn property_key_coercion_is_untouched() {
    // An object used as a property key still coerces to `"[object Object]"` —
    // the key is the one conversion that asked for the type tag on purpose, so
    // the array refusal that reports it keeps reporting it, with its own code.
    let text = refusal("const a: any = [1]; a[{ b: 2 }] = 3;");
    assert!(
        text.contains("TypeScriptArrayNonIndexPropertyUnsupported")
            && text.contains("[object Object]"),
        "key coercion still produces the type tag: {text}"
    );
}

#[test]
fn explicit_to_string_is_untouched() {
    assert_eq!(
        finished_string("finish(new Map().toString());"),
        "[object Map]"
    );
    assert_eq!(
        finished_string("finish(new Set().toString());"),
        "[object Set]"
    );
}

#[test]
fn number_coercion_is_untouched() {
    assert!(
        matches!(finished("finish(Number({ a: 1 }));"), Value::Number(n) if n.is_nan()),
        "a number-hinted conversion of a plain object is still NaN"
    );
    assert!(
        matches!(finished("finish(+{ a: 1 });"), Value::Number(n) if n.is_nan()),
        "unary plus is still NaN"
    );
}

#[test]
fn loose_equality_is_untouched() {
    // `==` reads the primitive to compare it, never to show it.
    assert_eq!(
        finished(r#"finish(({}) == "[object Object]");"#),
        Value::Bool(true)
    );
}

#[test]
fn json_stringify_is_the_documented_way_out() {
    assert_eq!(
        finished_string("finish(JSON.stringify({ a: 1, b: [2, 3] }));"),
        r#"{"a":1,"b":[2,3]}"#
    );
    assert_eq!(
        finished_string("finish(`rows=${JSON.stringify({ a: 1 })}`);"),
        r#"rows={"a":1}"#
    );
}

#[test]
fn an_object_with_its_own_hooks_converts_through_them() {
    // `+` asks valueOf first; ToString (templates and String()) asks toString
    // first; with neither answering, the built-in toString's type tag does.
    let cases = [
        (
            "const o = { toString() { return 'T'; }, valueOf() { return 5; } }; finish([o + '', `${o}`, String(o)].join('|'));",
            "5|T|T",
        ),
        (
            "const o = { toString() { return 'only'; } }; finish('<' + o + '>');",
            "<only>",
        ),
        (
            "const o = { n: 3, valueOf() { return this.n; } }; finish(`${o}` + (o + 1));",
            "[object Object]4",
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished_string(source), expected, "{source}");
    }
}
