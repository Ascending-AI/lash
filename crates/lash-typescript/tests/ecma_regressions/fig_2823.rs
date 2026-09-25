//! FIG-2823: ECMA stdlib arity stated once — padded argument shapes, lastIndexOf's explicit-undefined case and flat's default depth.

use super::*;

/// Fixed-length normalization pads omitted arguments with `undefined` before
/// the dispatcher matches, so an arm shorter than the declared arity can
/// never be selected — the deleted one-argument patterns these calls
/// exercise now arrive through the padded shape. If a signature's declared
/// arity ever moves away from the arm serving it, the call falls to the
/// `TS_METHOD_UNSUPPORTED` catchall and fails here rather than silently.
#[test]
fn fixed_length_methods_serve_their_padded_argument_shapes() {
    let number = |value: f64| Value::Number(value);
    let string = |value: &str| Value::String(value.into());
    let cases: &[(&str, Value)] = &[
        // Zero-argument forms pad to the declared index/count parameter.
        ("finish('abc'.charAt());", string("a")),
        ("finish('abc'.charCodeAt());", number(97.0)),
        ("finish('abc'.codePointAt());", number(97.0)),
        ("finish('ab'.repeat());", string("")),
        ("finish('abc'.at());", string("a")),
        ("finish([1, 2].at());", number(1.0)),
        // The omitted optional position pads to `undefined`, which every
        // fromIndex-style parameter reads as its start or end default.
        ("finish('abc'.startsWith('ab'));", Value::Bool(true)),
        ("finish('abc'.startsWith('bc', 1));", Value::Bool(true)),
        ("finish('abc'.startsWith('bc'));", Value::Bool(false)),
        ("finish('abc'.includes('b'));", Value::Bool(true)),
        ("finish('abc'.indexOf('b'));", number(1.0)),
        ("finish('abc'.lastIndexOf('b'));", number(1.0)),
        ("finish('abc'.endsWith('c'));", Value::Bool(true)),
        ("finish('abc'.endsWith('c', undefined));", Value::Bool(true)),
        ("finish('abc'.endsWith('c', 2));", Value::Bool(false)),
        ("finish('x'.padStart(3));", string("  x")),
        ("finish('x'.padStart(3, undefined));", string("  x")),
        ("finish('x'.padStart(3, '-'));", string("--x")),
        ("finish('x'.padEnd(3));", string("x  ")),
        ("finish('x'.padEnd(3, undefined));", string("x  ")),
        ("finish('abc'.substring(1));", string("bc")),
        ("finish('abc'.slice());", string("abc")),
        ("finish([1, 2, 3].includes(2));", Value::Bool(true)),
        ("finish([1, 2, 3].indexOf(3));", number(2.0)),
        ("finish([1, 2].join());", string("1,2")),
        ("finish([1, 2].join('-'));", string("1-2")),
        ("finish([1, 2].join(undefined));", string("1,2")),
        (
            "finish([1, 2, 3].slice(1));",
            Value::List(vec![number(2.0), number(3.0)].into()),
        ),
        ("finish([1, 2].toString());", string("1,2")),
        // `split` stays unnormalized — its `limit` distinguishes an omitted
        // argument from an explicit `undefined` — so each shape reaches its
        // own arm. (The zero-argument form is a lowerer refusal, not a
        // dispatch shape.)
        (
            "finish('a,b'.split(','));",
            Value::List(vec![string("a"), string("b")].into()),
        ),
        (
            "finish('a,b,c'.split(',', 1));",
            Value::List(vec![string("a")].into()),
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), expected.clone(), "{source}");
    }
}

/// `Array.prototype.lastIndexOf` is the one normalized call whose ECMA
/// contract checks argument *presence*: `fromIndex` defaults to the last
/// index when omitted but coerces to `+0` when written as `undefined`. The
/// dispatcher saves `argument_count` before padding so the distinction
/// survives normalization.
#[test]
fn array_last_index_of_distinguishes_omitted_from_explicit_undefined() {
    assert_eq!(
        finished("finish([1, 2, 1].lastIndexOf(1));"),
        Value::Number(2.0)
    );
    assert_eq!(
        finished("finish([1, 2, 1].lastIndexOf(1, undefined));"),
        Value::Number(0.0)
    );
    // String.prototype.lastIndexOf takes the other spec branch: `undefined`
    // means the end default, exactly like an omitted argument.
    assert_eq!(
        finished("finish('aba'.lastIndexOf('a'));"),
        Value::Number(2.0)
    );
    assert_eq!(
        finished("finish('aba'.lastIndexOf('a', undefined));"),
        Value::Number(2.0)
    );
}

/// `flat`'s default depth is 1 whether the argument is omitted or written as
/// `undefined`: normalization pads the call to `[undefined]`, and reading the
/// padding through `ToNumber` used to produce NaN — a depth of zero that
/// flattened nothing.
#[test]
fn array_flat_defaults_to_depth_one() {
    let flattened = Value::List(vec![Value::Number(1.0), Value::Number(2.0)].into());
    assert_eq!(finished("finish([1, [2]].flat());"), flattened);
    assert_eq!(finished("finish([1, [2]].flat(undefined));"), flattened);
    assert_eq!(finished("finish([[1, [2]]].flat(2));"), flattened);
    // Depth zero still returns the elements untouched.
    assert_eq!(
        finished("finish([1, [2]].flat(0));"),
        Value::List(
            vec![
                Value::Number(1.0),
                Value::List(vec![Value::Number(2.0)].into())
            ]
            .into()
        )
    );
}
