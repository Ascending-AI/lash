//! FIG-3732: a rest element holding an object binding pattern collects the
//! remaining elements, then binds the pattern from that array.

use super::*;

/// `for (<kind> [...{ length }] = [1, 2, 3]; ...)` — ECMA-262's
/// IteratorBindingInitialization collects the rest elements into an Array
/// and binds the object pattern from it, so `length` reads 3. The lowering
/// left the inner binding `undefined` instead.
#[test]
fn classic_for_rest_object_pattern_binds_from_the_collected_array() {
    for kind in ["var", "let", "const"] {
        let source = format!(
            "let seen = 0; for ({kind} [...{{ length }}] = [1, 2, 3]; seen < 1; ) {{ seen += length; }} finish(seen);"
        );
        assert_eq!(finished(&source), Value::Number(3.0), "{source}");
    }
    // A property rest pattern binds each named property from the collected
    // array, and a shorthand name the object does not own binds `undefined`.
    for kind in ["var", "let", "const"] {
        let source = format!(
            "let seen = ''; for ({kind} [...{{ 0: v, 1: w, 2: x, 3: y, length: z }}] = [7, 8, 9]; seen.length < 3; ) {{ seen += `${{v}}${{w}}${{x}}${{y}}${{z}}`; }} finish(seen);"
        );
        assert_eq!(
            finished(&source),
            Value::String("789undefined3".into()),
            "{source}"
        );
    }
}

/// The same rest object pattern in an ordinary declaration: the collected
/// rest is an Array, so a nested array pattern binds from it too.
#[test]
fn rest_pattern_binds_nested_patterns_from_the_collected_array() {
    assert_eq!(
        finished("const [a, ...{ length }] = [4, 5, 6]; finish(a * 10 + length);"),
        Value::Number(42.0)
    );
    assert_eq!(
        finished("const [...{ length: n, 0: first }] = [2, 4]; finish(first * 10 + n);"),
        Value::Number(22.0)
    );
}
