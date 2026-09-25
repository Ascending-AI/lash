//! FIG-3707: callback-mutation, live-element and parameter-environment regressions the re-bless surfaced.

use super::*;

/// Callbacks that shrink the array they walk (FIG-3707's Test262 re-bless:
/// these ran once closures could assign what they capture). ECMA-262 checks
/// HasProperty before each visit, so the indices past the new length are
/// skipped by the methods that skip holes; each answer is Node's. Before, the
/// driver visited the original length: `forEach` counted 5, `reduce`
/// answered NaN.
#[test]
fn array_callbacks_skip_the_elements_a_callback_removes() {
    let cases = [
        (
            "const arr = [1, 2, 3, 4, 5]; let n = 0; arr.forEach(() => { arr.length = 3; n++; }); finish(n);",
            Value::Number(3.0),
        ),
        (
            "const arr = [1, 2, 3, 4, 6]; finish(arr.every((v) => { arr.length = 3; return v < 4; }));",
            Value::Bool(true),
        ),
        (
            "const arr = [1, 2, 3, 4, 6]; finish(arr.some((v) => { arr.length = 3; return v > 3; }));",
            Value::Bool(false),
        ),
        (
            "const arr = [1, 2, 3, 4, 6]; finish(arr.filter(() => { arr.length = 2; return true; }).length);",
            Value::Number(2.0),
        ),
        (
            "const arr = [1, 2, 3, 4, 5]; finish(arr.reduce((a, b) => { arr.length = 2; return a + b; }));",
            Value::Number(3.0),
        ),
        (
            "const arr = [1, 2, 3, 4, 5]; finish(arr.reduceRight((a, b) => { arr.length = 2; return a + b; }));",
            Value::Number(12.0),
        ),
        (
            "const arr = [1, 2, 3]; finish(JSON.stringify(arr.flatMap((v) => { arr.length = 2; return [v, v]; })));",
            Value::String("[1,1,2,2]".into()),
        ),
        // `find` reads every index, present or not.
        (
            "const arr = [1, 2, 3]; let n = 0; arr.find(() => { arr.length = 1; n++; return false; }); finish(n);",
            Value::Number(3.0),
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), expected, "{source}");
    }
    // `map`'s result keeps the original length, so a skipped element is a
    // hole, which the dense array model refuses by name.
    let error = execute(
        "const arr = [1, 2, 3, 4, 5]; finish(arr.map(() => { arr.length = 2; return 1; }).length);",
    )
    .expect_err("a map result with holes is not representable");
    assert!(
        error.to_string().contains("TS_SPARSE_ARRAY_UNSUPPORTED"),
        "{error}"
    );
}

/// `Array.from` with a mapper walks an array source through its iterator,
/// which reads each index and the length live; an array copy keeps function
/// elements as the values they are (both surfaced by FIG-3707's re-bless).
#[test]
fn array_from_walks_an_array_live_and_copies_function_elements() {
    let cases = [
        (
            "const src = [1, 2, 3]; finish(JSON.stringify(Array.from(src, (v, i) => { if (i + 1 < src.length) { src[i + 1] = 9; } return v; })));",
            Value::String("[1,9,9]".into()),
        ),
        (
            "const src = [1, 2]; finish(JSON.stringify(Array.from(src, (v) => { if (src.length < 4) { src.push(v * 10); } return v; })));",
            Value::String("[1,2,10,20]".into()),
        ),
        (
            "const [f, g] = [() => 1, () => 2]; finish(f() + g());",
            Value::Number(3.0),
        ),
        (
            "let total = 0; for (const [f] of [[() => 4]]) { total += f(); } finish(total);",
            Value::Number(4.0),
        ),
        (
            "const inner = { n: 1 }; const [copy] = [inner]; copy.n = 5; finish(inner.n);",
            Value::Number(5.0),
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), expected, "{source}");
    }
}

/// A parameter default that closes over an outer binding the body redeclares
/// sees the outer one: ECMA-262 gives such a body its own variable
/// environment (FIG-3707's re-bless; the body's binding used to share the
/// captured slot).
#[test]
fn a_parameter_default_closes_over_the_outer_binding_the_body_redeclares() {
    let cases = [
        (
            "var x = 'outside'; let probe: any; function f(_ = probe = () => x) { var x = 'inside'; return x; } const inner = f(); finish(inner + ',' + probe());",
            Value::String("inside,outside".into()),
        ),
        (
            "function run() { let x = 'outer'; const f = (a = () => x) => { let x = 'body'; return a() + ',' + x; }; return f(); } finish(run());",
            Value::String("outer,body".into()),
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), expected, "{source}");
    }
}
