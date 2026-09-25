//! FIG-1304: the dialect contract's first regressions — member reads, typeof, loose equality, number and string conversion, globals, lone surrogates and this-parameter erasure.

use super::*;

#[test]
fn missing_and_non_index_property_reads_produce_undefined() {
    let cases = [
        "finish(({ a: 1 }).missing);",
        "finish([1, 2][9]);",
        "finish([1, 2][-1]);",
    ];
    for source in cases {
        assert_eq!(finished(source), Value::Undefined, "{source}");
    }
    assert_eq!(
        finished("finish(({ a: 1 }).missing === undefined);"),
        Value::Bool(true)
    );
}

#[test]
fn typeof_uses_ecma_object_kinds_and_allows_unresolvable_references() {
    let cases = [
        ("finish(typeof {});", "object"),
        ("finish(typeof []);", "object"),
        ("finish(typeof (() => 1));", "function"),
        ("finish(typeof someUndeclared);", "undefined"),
        // The reserved value idents lower to literals, so `typeof` classifies
        // the literal — `NaN` is a number, not an unbound name (FIG-3649).
        ("finish(typeof NaN);", "number"),
        ("finish(typeof (0 / 0));", "number"),
        ("finish(typeof Number.NaN);", "number"),
        ("finish(typeof -NaN);", "number"),
        ("finish(typeof Infinity);", "number"),
        ("finish(typeof undefined);", "undefined"),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), Value::String(expected.into()), "{source}");
    }
}

#[test]
fn loose_equality_recurses_after_boolean_to_number_conversion() {
    let cases = [
        ("finish(null == false);", false),
        ("finish(undefined == false);", false),
        ("finish('0' == false);", true),
        ("finish([] == false);", true),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), Value::Bool(expected), "{source}");
    }
}

#[test]
fn number_to_string_matches_ecma_thresholds_and_shortest_digits() {
    let cases = [
        ("finish(`${1e21}`);", "1e+21"),
        ("finish(`${1e20}`);", "100000000000000000000"),
        ("finish(`${1e-6}`);", "0.000001"),
        ("finish(`${1e-7}`);", "1e-7"),
        ("finish(`${1.5e-10}`);", "1.5e-10"),
        (
            "finish(`${123456789012345678901234}`);",
            "1.2345678901234569e+23",
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), Value::String(expected.into()), "{source}");
    }
}

#[test]
fn string_to_number_accepts_only_the_ecma_string_numeric_grammar() {
    for source in [
        "finish(+'inf');",
        "finish(+'infinity');",
        "finish(+'INF');",
        "finish(+'+0x10');",
    ] {
        let Value::Number(value) = finished(source) else {
            panic!("expected number for {source}");
        };
        assert!(value.is_nan(), "{source}: {value}");
    }
    assert_eq!(
        finished("finish(+'0xFFFFFFFFFFFFFFFFF');"),
        Value::Number(295_147_905_179_352_830_000.0)
    );
}

#[test]
fn length_and_standard_number_globals_are_available() {
    let cases = [
        ("finish('abc'.length);", 3.0),
        ("finish('😀'.length);", 2.0),
        ("finish([1, 2, 3].length);", 3.0),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), Value::Number(expected), "{source}");
    }
    let Value::Number(nan) = finished("finish(NaN);") else {
        panic!("NaN global should be numeric");
    };
    assert!(nan.is_nan());
    assert_eq!(finished("finish(Infinity);"), Value::Number(f64::INFINITY));
}

#[test]
fn string_relational_comparison_uses_utf16_code_units() {
    assert_eq!(finished("finish('\\u{10000}' < '｡');"), Value::Bool(true));
}

#[test]
fn lone_surrogate_literals_reject_without_lossy_transcoding() {
    for source in [
        r#"finish('\uD800');"#,
        r#"function encodeURI(value) { return 'shadowed'; } finish(encodeURI('\uD800'));"#,
    ] {
        let error = lash_typescript::validate(source)
            .expect_err("lone surrogates are not representable in ordinary guest values");
        assert_eq!(error.code.as_str(), "TS_LONE_SURROGATE_LITERAL_UNSUPPORTED");
    }
}

#[test]
fn typescript_this_parameter_is_erased_before_runtime_arity() {
    assert_eq!(
        finished("function f(this: number, a: number): number { return a; } finish(f(1));"),
        Value::Number(1.0)
    );
}
