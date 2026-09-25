//! FIG-3706: a classic for runs every head, condition and update form.

use super::*;

/// ECMA-262's ForStatement in every head, condition and update form
/// (FIG-3706). Each of these refused as `TS_FOR_UNSUPPORTED` while only
/// `for (let i = start; i < end; i++)` was accepted; the answers are Node's.
#[test]
fn classic_for_runs_every_head_condition_and_update_form() {
    let cases = [
        // Other updates and conditions.
        (
            "let s = 0; for (let i = 10; i > 0; i -= 3) { s = s + i; } finish(s);",
            Value::Number(22.0),
        ),
        (
            "let s = 0; for (let i = 1; i <= 16; i = i * 2) { s = s + i; } finish(s);",
            Value::Number(31.0),
        ),
        (
            "let s = ''; for (let i = 3; i--; ) { s = s + i; } finish(s);",
            Value::String("210".into()),
        ),
        (
            "let s = 0; for (let i = 0, j = 10; i < j; i++) { j = j - 1; s = s + 1; } finish(s);",
            Value::Number(5.0),
        ),
        (
            "let s = 0; for (const k = 3; s < k; ) { s++; } finish(s);",
            Value::Number(3.0),
        ),
        // An expression head, an empty head, and no condition at all.
        (
            "let i = 0; let s = 0; for (i = 5; i < 8; i++) { s = s + i; } finish(s * 10 + i);",
            Value::Number(188.0),
        ),
        (
            "let s = 0; let i = 0; for (; i < 4; i++) { s = s + i; } finish(s);",
            Value::Number(6.0),
        ),
        (
            "let n = 0; for (;;) { n++; if (n === 3) break; } finish(n);",
            Value::Number(3.0),
        ),
        // A `var` head declares the enclosing function's binding, visible
        // after the loop.
        (
            "let r = 0; for (var i = 7; ; ) { r = i; break; } finish(r * 10 + i);",
            Value::Number(77.0),
        ),
        // `continue` runs the update first.
        (
            "let s = ''; for (let i = 0; i < 5; i++) { if (i % 2) continue; s = s + i; } finish(s);",
            Value::String("024".into()),
        ),
        // With no update there is nothing to run before a `finally`.
        (
            "let s = ''; let i = 0; for (; i < 3; ) { i++; try { if (i === 2) continue; s = s + i; } finally { s = s + '.'; } } finish(s);",
            Value::String("1..3.".into()),
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), expected, "{source}");
    }
}

/// `x++` in a classic `for` update converts its operand with ToNumeric. The
/// canonical loop used to lower it to `x = x + 1`, which concatenates a
/// string: this loop ran twice (`"0"`, `"01"`) where Node runs it three times.
#[test]
fn classic_for_update_converts_its_operand_to_a_number() {
    assert_eq!(
        finished(
            "const start: any = '0'; let n = 0; for (let i: any = start; i < 3; i++) { n++; } finish(n);"
        ),
        Value::Number(3.0)
    );
    assert_eq!(
        finished(
            "const start: any = '5'; let s = ''; for (let i: any = start; i > 2; i--) { s = s + typeof i; } finish(s);"
        ),
        Value::String("stringnumbernumber".into())
    );
}
