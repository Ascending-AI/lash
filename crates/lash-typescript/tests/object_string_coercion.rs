//! String coercion of a value whose only ECMA string is a type tag refuses.
//!
//! `"" + value`, `` `${value}` `` and `String(value)` all lower to `+`. For a
//! plain object, a `Map` or a `Set` ECMA-262 answers `[object Object]`,
//! `[object Map]` or `[object Set]` — text that names the type and discards
//! everything the cell computed. FIG-3166 refuses those three spellings with
//! `TS_OBJECT_STRING_COERCION` rather than guessing a JSON body for them,
//! matching every other gap in this dialect.
//!
//! Everything else keeps its exact ECMA-262 string, and the conversions that
//! ask for the type tag on purpose — property keys, `.toString()`,
//! `console.log`'s fallback — are untouched. This suite pins both halves.

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

/// Runs `source` and returns the finished value.
fn finished(source: &str) -> Value {
    let program = lash_typescript::compile(source).expect("TypeScript should compile");
    let host = Host::default();
    match futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &host))
        .expect("TypeScript should execute")
    {
        lashlang::ExecutionOutcome::Finished(value) => value,
        other => panic!("expected a finished value, got {other:?}"),
    }
}

/// Runs `source` and returns the finished string.
fn finished_string(source: &str) -> String {
    match finished(source) {
        Value::String(text) => text.to_string(),
        other => panic!("expected a string, got {other:?}"),
    }
}

/// Runs `source` and returns the refusal's debug text.
fn refusal(source: &str) -> String {
    let program = lash_typescript::compile(source).expect("TypeScript should compile");
    let host = Host::default();
    let error = futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &host))
        .expect_err("string coercion of a tag-only object refuses");
    format!("{error:?}")
}

/// Every spelling of "put this value into a string", for a value the dialect
/// has no string for.
fn coercion_spellings(expression: &str) -> Vec<String> {
    vec![
        format!(r#"finish("" + ({expression}));"#),
        format!(r#"finish(`${{{expression}}}`);"#),
        format!("finish(String({expression}));"),
        format!(r#"finish(({expression}) + "");"#),
    ]
}

#[test]
fn every_string_coercion_of_a_plain_object_refuses() {
    for source in coercion_spellings("{ a: 1 }") {
        let text = refusal(&source);
        assert!(
            text.contains("TS_OBJECT_STRING_COERCION"),
            "expected the refusal code for {source}, got {text}"
        );
        assert!(
            text.contains("[object Object]"),
            "the refusal names the string it would have produced: {text}"
        );
        assert!(
            text.contains("JSON.stringify"),
            "the refusal points at an explicit serialization: {text}"
        );
        assert!(
            text.contains("console.log"),
            "the refusal points at examining the value: {text}"
        );
    }
}

#[test]
fn every_string_coercion_of_a_map_or_set_refuses() {
    for (expression, tag) in [
        (r#"new Map([["k", 1]])"#, "[object Map]"),
        ("new Set([1])", "[object Set]"),
    ] {
        for source in coercion_spellings(expression) {
            let text = refusal(&source);
            assert!(
                text.contains("TS_OBJECT_STRING_COERCION") && text.contains(tag),
                "expected the {tag} refusal for {source}, got {text}"
            );
        }
    }
}

#[test]
fn an_object_reached_through_a_container_refuses_and_says_so() {
    let text = refusal("finish(`${[{ a: 1 }]}`);");
    assert!(
        text.contains("TS_OBJECT_STRING_COERCION") && text.contains("inside a container"),
        "a nested object names where it was reached: {text}"
    );
}

#[test]
fn a_tool_result_shaped_object_refuses_rather_than_finishing_a_placeholder() {
    // The shape FIG-3166 was filed for: a cell interpolates the whole result of
    // a tool call instead of the field it was asked for.
    let text =
        refusal("const result = { rows: [{ id: 1 }], total: 1 };\nfinish(`found ${result}`);");
    assert!(
        text.contains("TS_OBJECT_STRING_COERCION"),
        "expected the refusal, got {text}"
    );
}

#[test]
fn arrays_keep_their_exact_ecma_string() {
    assert_eq!(finished_string("finish(String([1, 2, 3]));"), "1,2,3");
    assert_eq!(finished_string("finish(`${[1, 2, 3]}`);"), "1,2,3");
    assert_eq!(finished_string(r#"finish("" + [1, 2, 3]);"#), "1,2,3");
    assert_eq!(finished_string("finish(String([]));"), "");
    assert_eq!(
        finished_string(r#"finish(String([1, null, undefined, 2]));"#),
        "1,,,2"
    );
    assert_eq!(finished_string(r#"finish(String(["a", "b"]));"#), "a,b");
}

#[test]
fn errors_keep_their_exact_ecma_string() {
    assert_eq!(
        finished_string(r#"finish(String(new Error("boom")));"#),
        "Error: boom"
    );
    assert_eq!(
        finished_string(r#"finish(`${new Error("boom")}`);"#),
        "Error: boom"
    );
    assert_eq!(
        finished_string(r#"finish(new Error("boom") + "");"#),
        "Error: boom"
    );
}

#[test]
fn regexps_and_urls_keep_their_exact_ecma_string() {
    assert_eq!(finished_string("finish(String(/ab+c/gi));"), "/ab+c/gi");
    assert_eq!(
        finished_string(r#"finish(String(new URL("https://example.com/a?b=1")));"#),
        "https://example.com/a?b=1"
    );
}

#[test]
fn primitives_keep_their_exact_ecma_string() {
    for (expression, expected) in [
        ("1", "1"),
        ("2.5", "2.5"),
        ("-0", "0"),
        ("1e21", "1e+21"),
        ("NaN", "NaN"),
        ("Infinity", "Infinity"),
        ("true", "true"),
        ("false", "false"),
        ("null", "null"),
        ("undefined", "undefined"),
    ] {
        assert_eq!(
            finished_string(&format!("finish(String({expression}));")),
            expected,
            "String({expression})"
        );
        assert_eq!(
            finished_string(&format!("finish(`${{{expression}}}`);")),
            expected,
            "`${{{expression}}}`"
        );
    }
}

#[test]
fn a_date_keeps_its_pre_existing_refusal() {
    // `Date` string coercion was already a refusal with its own code
    // (`TS_DATE_STRING_COERCION_PENDING`); FIG-3166 does not touch it, and does
    // not shadow it with the new one.
    let text = refusal(r#"finish("" + new Date(0));"#);
    assert!(
        text.contains("TS_DATE_STRING_COERCION_PENDING")
            && !text.contains("TS_OBJECT_STRING_COERCION"),
        "the Date refusal is unchanged: {text}"
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
            && text.contains("[object Object]")
            && !text.contains("TS_OBJECT_STRING_COERCION"),
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
