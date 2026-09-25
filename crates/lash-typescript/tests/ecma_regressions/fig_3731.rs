//! FIG-3731: decodeURI/encodeURI throw URIError on malformed escapes and
//! ill-formed UTF-8 sequences.

use super::*;

/// Runs `try { <call>; } catch (e) { ... }` the way the failing test262 rows
/// do, answering `urierror` only when the thrown value is a real URIError.
fn caught_name(call: &str) -> Value {
    finished(&format!(
        "try {{ {call}; finish('returned'); }} catch (e) {{ finish((e instanceof URIError) ? 'urierror' : 'other:' + e.name); }}"
    ))
}

/// Every malformed-escape shape the failing decodeURI/decodeURIComponent
/// rows exercise at a *representable* code point already throws a real
/// URIError: a bad hex pair after `%`, a lead byte with a non-hex or
/// non-escape continuation, a truncated escape, a stray continuation byte,
/// an overlong form, a UTF-8-encoded surrogate and a sequence above
/// U+10FFFF.
#[test]
fn uri_decoders_throw_urierror_on_every_malformed_escape_shape() {
    let cases = [
        // A1.2: a non-hex digit either side of the `%` pair.
        "%z1",
        "%1z",
        // A1.10: a `110xxxxx` lead whose continuation is not `%XX`.
        "%C0%zz",
        "%C0x",
        "%C0%",
        // A1.11: a `1110xxxx` lead with the same at either continuation.
        "%E0%zz%A0",
        "%E0%A0%zz",
        // A1.12: a `11110xxx` lead with the same at any continuation.
        "%F0%zz%A0%A0",
        "%F0%A0%zz%A0",
        "%F0%A0%A0%zz",
        // Stray continuation, overlong, encoded surrogate, out of range.
        "%80",
        "%C0%AF",
        "%ED%A0%80",
        "%F4%90%80%80",
        // A bare or truncated `%` at the end.
        "%",
        "%A",
    ];
    for name in ["decodeURI", "decodeURIComponent"] {
        for input in cases {
            let call = format!("{name}('{input}')");
            assert_eq!(
                caught_name(&call),
                Value::String("urierror".into()),
                "{call} must throw URIError"
            );
        }
    }
}

/// Well-formed input still decodes: ASCII escapes decode, reserved escapes
/// stay literal under decodeURI, and a four-byte sequence decodes to its
/// astral character.
#[test]
fn uri_decoders_still_decode_well_formed_input() {
    for (source, expected) in [
        ("finish(decodeURI('a%20b%3Fc%23'));", "a b%3Fc%23"),
        ("finish(decodeURIComponent('a%20b%3Fc%23'));", "a b?c#"),
        ("finish(decodeURI('%F0%9F%98%80'));", "😀"),
        (
            "finish(encodeURIComponent('a b/é😀'));",
            "a%20b%2F%C3%A9%F0%9F%98%80",
        ),
    ] {
        assert_eq!(finished(source), Value::String(expected.into()), "{source}");
    }
}

/// A lone-surrogate literal under an encoder throws Node's real URIError
/// (the register's literal carve-out), in both the low and high halves and
/// inside a longer literal.
#[test]
fn uri_encoders_throw_urierror_on_lone_surrogate_literals() {
    for call in [
        "encodeURI('\\uD800')",
        "encodeURI('\\uDC00')",
        "encodeURIComponent('\\uD800')",
        "encodeURIComponent('\\uDC00')",
        "encodeURI('a\\uD800b')",
    ] {
        assert_eq!(
            caught_name(call),
            Value::String("urierror".into()),
            "{call} must throw URIError"
        );
    }
}

/// The same lone surrogate arriving any other way is the registered
/// `TS_LONE_SURROGATE_UNSUPPORTED` refusal — the value model cannot hold
/// one — which is what the failing test262 rows depend on: they build the
/// input with `String.fromCharCode` inside their own `try`, so the refusal
/// is caught before the URI codec runs. `decodeURI` of a lone-surrogate
/// literal rejects outright since ECMA would return the unrepresentable
/// string unchanged.
#[test]
fn lone_surrogate_construction_is_the_registered_refusal_the_tests_depend_on() {
    for source in [
        "finish(String.fromCharCode(0xD800));",
        "finish(decodeURI(String.fromCharCode(0xD800)));",
        "finish(encodeURI(String.fromCharCode(0xD800)));",
    ] {
        let error = execute(source).expect_err("a lone surrogate is not representable");
        assert!(
            error.to_string().contains("TS_LONE_SURROGATE_UNSUPPORTED"),
            "{source}: {error}"
        );
    }
    let error = lash_typescript::validate("finish(decodeURI('\\uD800'));")
        .expect_err("a lone-surrogate literal refuses");
    assert_eq!(error.code.as_str(), "TS_LONE_SURROGATE_LITERAL_UNSUPPORTED");
}

/// The failing rows' own sweep, minus the surrogate band they cannot
/// construct: every representable non-hex trailing character still throws a
/// catchable URIError. In the vendored tests the band [0xD800, 0xDFFF]
/// diverges inside `String.fromCharCode`, which the record owns as the
/// registered lone-surrogate deviation.
#[test]
fn the_failing_rows_malformed_escape_sweep_throws_off_the_surrogate_band() {
    let source = r#"
        var result = true;
        var interval = [[0x00, 0x2F], [0x3A, 0x40], [0x47, 0x60], [0x67, 0xD7FF], [0xE000, 0xFFFF]];
        for (var indexI = 0; indexI < interval.length; indexI++) {
            for (var indexJ = interval[indexI][0]; indexJ <= interval[indexI][1]; indexJ++) {
                try {
                    decodeURI("%C0%" + String.fromCharCode(indexJ, indexJ));
                    result = false;
                } catch (e) {
                    if ((e instanceof URIError) !== true) { result = false; }
                }
            }
        }
        finish(result);
    "#;
    assert_eq!(finished(source), Value::Bool(true));
}
