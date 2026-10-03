//! FIG-3737: each built-in `Ctor.prototype` object carries its ECMA-specified
//! own-property surface — the names a `hasOwnProperty` sees, the values a read
//! answers, and the method identity and metadata a `verifyProperty` probes.

use super::*;

/// Every `Date.prototype` member is an own property reading as a function
/// whose `name` and `length` match ECMA's rows — including the members a call
/// still refuses, which carry their specified surface regardless.
#[test]
fn date_prototype_carries_its_specified_methods() {
    for (member, length) in [
        ("getDate", 0.0),
        ("getDay", 0.0),
        ("getFullYear", 0.0),
        ("getHours", 0.0),
        ("getMilliseconds", 0.0),
        ("getMinutes", 0.0),
        ("getMonth", 0.0),
        ("getSeconds", 0.0),
        ("getTime", 0.0),
        ("getTimezoneOffset", 0.0),
        ("getUTCDate", 0.0),
        ("getUTCDay", 0.0),
        ("getUTCFullYear", 0.0),
        ("getUTCHours", 0.0),
        ("getUTCMilliseconds", 0.0),
        ("getUTCMinutes", 0.0),
        ("getUTCMonth", 0.0),
        ("getUTCSeconds", 0.0),
        ("getYear", 0.0),
        ("setDate", 1.0),
        ("setFullYear", 3.0),
        ("setHours", 4.0),
        ("setMilliseconds", 1.0),
        ("setMinutes", 3.0),
        ("setMonth", 2.0),
        ("setSeconds", 2.0),
        ("setTime", 1.0),
        ("setUTCDate", 1.0),
        ("setUTCFullYear", 3.0),
        ("setUTCHours", 4.0),
        ("setUTCMilliseconds", 1.0),
        ("setUTCMinutes", 3.0),
        ("setUTCMonth", 2.0),
        ("setUTCSeconds", 2.0),
        ("setYear", 1.0),
        ("toDateString", 0.0),
        ("toISOString", 0.0),
        ("toJSON", 1.0),
        ("toLocaleDateString", 0.0),
        ("toLocaleString", 0.0),
        ("toLocaleTimeString", 0.0),
        ("toString", 0.0),
        ("toTimeString", 0.0),
        ("toUTCString", 0.0),
        ("valueOf", 0.0),
    ] {
        let source = format!(
            "const f: any = Date.prototype.{member}; \
             finish([Date.prototype.hasOwnProperty('{member}'), typeof f, f.name, f.length]);"
        );
        assert_eq!(
            finished(&source),
            Value::List(
                vec![
                    Value::Bool(true),
                    Value::String("function".into()),
                    Value::String(member.into()),
                    Value::Number(length),
                ]
                .into()
            ),
            "{member}"
        );
    }
}

/// `Error.prototype` and each native-error prototype own `name` and `message`
/// with ECMA's initial values; `toString` is `Error.prototype`'s, inherited —
/// not an own property of the derived prototypes.
#[test]
fn error_prototypes_carry_name_and_message() {
    for (ctor, name) in [
        ("Error", "Error"),
        ("AggregateError", "AggregateError"),
        ("EvalError", "EvalError"),
        ("RangeError", "RangeError"),
        ("ReferenceError", "ReferenceError"),
        ("SyntaxError", "SyntaxError"),
        ("TypeError", "TypeError"),
        ("URIError", "URIError"),
    ] {
        let source = format!(
            "const p: any = {ctor}.prototype; \
             finish([p.name, p.message, p.hasOwnProperty('name'), p.hasOwnProperty('message')]);"
        );
        assert_eq!(
            finished(&source),
            Value::List(
                vec![
                    Value::String(name.into()),
                    Value::String("".into()),
                    Value::Bool(true),
                    Value::Bool(true),
                ]
                .into()
            ),
            "{ctor}"
        );
    }
    assert_eq!(
        finished("finish(TypeError.prototype.hasOwnProperty('toString'));"),
        Value::Bool(false)
    );
    assert_eq!(
        finished("finish(TypeError.prototype.toString === Error.prototype.toString);"),
        Value::Bool(true)
    );
    // An instance without an own message reads the prototype's empty string.
    assert_eq!(
        finished("finish(new Error().message === Error.prototype.message);"),
        Value::Bool(true)
    );
}

/// `Array.prototype` is itself an array: `isArray` says so and its exotic
/// `length` reads 0, stays non-configurable across a write, and `toLocaleString`
/// is a specified own method.
#[test]
fn array_prototype_is_an_exotic_array() {
    assert_eq!(
        finished("finish(Array.isArray(Array.prototype));"),
        Value::Bool(true)
    );
    assert_eq!(
        finished("finish(Array.prototype.length);"),
        Value::Number(0.0)
    );
    assert_eq!(
        finished(
            "const p: any = Array.prototype; p.length = 42; \
             try { delete p.length; finish('deleted'); } \
             catch (e) { finish([e instanceof TypeError, p.length]); }"
        ),
        Value::List(vec![Value::Bool(true), Value::Number(42.0)].into())
    );
    assert_eq!(
        finished("finish(typeof Array.prototype.toLocaleString);"),
        Value::String("function".into())
    );
}
