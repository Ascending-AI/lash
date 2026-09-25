//! FIG-3656: built-in objects are first-class values — member semantics, prototypes, expandos and lastIndex reads.

use super::*;

/// A built-in object is a first-class value: a missing member reads
/// `undefined`, a write lands an expando, a read-only constant write throws
/// Node's TypeError, and `Ctor.prototype` reads the prototype object
/// (FIG-3656).
#[test]
fn builtin_member_semantics_match_node() {
    assert_eq!(
        finished("finish(Math.extra);"),
        Value::Undefined,
        "Math.extra reads undefined, as Node answers"
    );
    assert_eq!(
        finished("Math.extra = 1; finish(Math.extra);"),
        Value::Number(1.0),
        "an expando on a built-in lands and reads back"
    );
    assert_eq!(
        finished(
            "try { Math.PI = 2; finish('no throw'); } catch (e) { finish([e instanceof TypeError, e.message]); }"
        ),
        Value::List(
            vec![
                Value::Bool(true),
                Value::String("Cannot assign to read only property 'PI' of object 'Math'".into()),
            ]
            .into()
        )
    );
    for (source, expected) in [
        ("finish(typeof String.prototype);", "object"),
        ("finish(typeof String.prototype.trim);", "function"),
        ("finish(typeof Number.prototype);", "object"),
    ] {
        assert_eq!(finished(source), Value::String(expected.into()));
    }
    assert_eq!(
        finished("finish(String.prototype === String.prototype);"),
        Value::Bool(true),
        "a built-in's prototype has one canonical object"
    );
    assert_eq!(
        finished("finish(Math.prototype);"),
        Value::Undefined,
        "Math has no `prototype` own property, as Node answers"
    );
}

/// ECMA makes `lastIndex` an ordinary writable data property and coerces on
/// use; Node therefore reads back `-1`, `2.7`, `NaN` and `Infinity` verbatim,
/// and so does this store. The raw value is not always a durable integer,
/// though: what the `u64` slot cannot hold rides an in-memory override while
/// the slot keeps the value's ToLength floor, so a resumed process reads the
/// coercion where the live one read the raw write.
#[test]
fn last_index_reads_back_the_written_value() {
    for (source, expected) in [
        (
            "const r = /a/g; r.lastIndex = -1; finish(r.lastIndex);",
            -1.0,
        ),
        (
            "const r = /a/g; r.lastIndex = 2.7; finish(r.lastIndex);",
            2.7,
        ),
        (
            "const r = /a/g; r.lastIndex = Infinity; finish(r.lastIndex);",
            f64::INFINITY,
        ),
    ] {
        assert_eq!(finished(source), Value::Number(expected), "{source}");
    }
    let Value::Number(value) = finished("const r = /a/g; r.lastIndex = NaN; finish(r.lastIndex);")
    else {
        panic!("lastIndex reads back the written NaN");
    };
    assert!(value.is_nan());
}
