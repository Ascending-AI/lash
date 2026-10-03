//! FIG-1304: the dialect contract's first regressions — member reads, typeof, loose equality, number and string conversion, globals, lone surrogates and this-parameter erasure.

use super::*;

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
