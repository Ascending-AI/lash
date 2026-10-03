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
