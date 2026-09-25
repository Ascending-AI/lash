//! FIG-3703: var, parameter and catch bindings accept reassignment.

use super::*;

/// FIG-3703: a `var`, a parameter, and a `catch` binding are ordinary mutable
/// slots in ECMA-262 — reassigning them compiles and writes through, where the
/// lowerer used to admit `let` alone.
#[test]
fn var_parameter_and_catch_bindings_reassign() {
    for (source, expected) in [
        ("var v = 1; v = 2; finish(v);", 2.0),
        ("var v = 1; v += 2; v++; finish(v);", 4.0),
        ("var v = 10; ++v; v -= 3; finish(v);", 8.0),
        ("finish((function (p) { p = p + 1; return p; })(1));", 2.0),
        ("finish(((p) => { p += 10; return p; })(1));", 11.0),
        ("try { null.x; } catch (e) { e = 4; finish(e); }", 4.0),
    ] {
        assert_eq!(finished(source), Value::Number(expected), "{source}");
    }
}

/// FIG-3703: `var` reassignment keeps `var` semantics — the slot is the
/// function's (or the cell's), hoisted to `undefined`, not the block's.
#[test]
fn var_reassignment_keeps_function_scope_and_hoisting() {
    // A block does not confine the `var`.
    assert_eq!(
        finished("finish((() => { if (true) { var x = 7; } x = 8; return x; })());"),
        Value::Number(8.0)
    );
    // The binding reads `undefined` before its declaration runs.
    assert_eq!(
        finished(
            "finish((() => { const seen = x; var x = 4; return seen === undefined && x === 4; })());"
        ),
        Value::Bool(true)
    );
}

/// FIG-3703: a classic `for` head's `var` is the enclosing function's (or the
/// cell's) one binding — hoisted ahead of the loop and still bound after it.
#[test]
fn classic_for_var_head_is_function_scoped_and_hoisted() {
    assert_eq!(
        finished(
            "finish((() => { const before = i; for (var i = 0; i < 3; i++) {} return before === undefined && i === 3; })());"
        ),
        Value::Bool(true)
    );
    // The loop's `var` is the cell's session slot, so a later statement — and
    // a later read — reach it.
    assert_eq!(
        finished("for (var i = 0; i < 2; i++) {} i = 9; finish(i);"),
        Value::Number(9.0)
    );
}

/// FIG-3703: a parenthesized identifier is still an assignment target — the
/// parentheses cover the reference without changing what it assigns — but a
/// covered name is no IdentifierReference, so an anonymous function assigned
/// to it takes no name.
#[test]
fn parenthesized_assignment_targets() {
    for (source, expected) in [
        ("var x = 0; (x) = 5; finish(x);", 5.0),
        ("var x = 1; (x) += 2; finish(x);", 3.0),
        ("var x = 1; (x)++; finish(x);", 2.0),
        ("let y = 0; (y) = 6; finish(y);", 6.0),
    ] {
        assert_eq!(finished(source), Value::Number(expected), "{source}");
    }
    assert_eq!(
        finished(
            "var f; f = function () {}; var g; (g) = function () {}; finish(f.name + '|' + g.name);"
        ),
        Value::String("f|".into())
    );
}

/// FIG-3703: inside a function `undefined`, `NaN` and `Infinity` are ordinary
/// names — tsc refuses them only as *global* redeclares (TS2397, TS2403,
/// TS2451), so a function-local `var` may bind each.
#[test]
fn function_local_vars_may_name_the_reserved_value_identifiers() {
    assert_eq!(
        finished("finish((() => { var NaN = 3; return NaN; })());"),
        Value::Number(3.0)
    );
    assert_eq!(
        finished("finish((() => { var undefined = 9; return undefined; })());"),
        Value::Number(9.0)
    );
    assert_eq!(
        finished("finish((() => { var Infinity; return Infinity === undefined; })());"),
        Value::Bool(true)
    );
}
