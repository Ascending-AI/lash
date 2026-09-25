//! FIG-3722: an advertised built-in global reads and writes as the global
//! object property it spells. Reads are the FIG-3656/FIG-3701 surface —
//! identity-stable objects, one per built-in; a bare write (`Object = x`)
//! used to reject `TS_UNKNOWN_BINDING` while `globalThis.Object = x` worked.
//! The global object is the model for both spellings now: the cell binds the
//! written name as a session slot seeded with the built-in, so a read beside
//! the write sees the live property.

use super::*;

/// Reads of advertised built-in globals as values: identity-stable objects,
/// one per built-in (FIG-3656/FIG-3701), usable anywhere a value is.
#[test]
fn builtin_globals_read_as_identity_stable_values() {
    for source in [
        "finish(JSON === JSON);",
        "const o = JSON; finish(o === JSON && Object.is(o, JSON));",
        "finish(Array.isArray(Math) === false && Array.isArray([]));",
        "finish([].constructor === Array);",
        "finish(typeof Object === 'function' && typeof Array === 'function');",
        "finish(typeof Math === 'object' && typeof JSON === 'object');",
        "const n = Number; finish(n === Number && n.MAX_VALUE > 0);",
        "finish(Math.max === Math.max);",
        // A name passed to a call is the same object the member read sees.
        "finish(Object.is(Math, Math));",
        "finish(typeof 'x'.includes === 'function' && [].map.length === 1);",
    ] {
        assert_eq!(finished(source), Value::Bool(true), "{source}");
    }
}

/// A bare write to an advertised built-in global lands on the global
/// property: the name then reads the new value, including through
/// `globalThis`, until it is written back.
#[test]
fn builtin_global_writes_update_the_global_property() {
    for source in [
        // The Test262 shape: the constructor reads, writes, restores.
        "const keep = Object; Object = 12; finish(Object === 12 && typeof Object === 'number');",
        "const keep = Object; try { Object = 12; } finally { Object = keep; } finish(Object === keep);",
        "const keep = Number; try { Number = 12; } finally { Number = keep; } finish(Number === keep && typeof Number === 'function');",
        // Bare and `globalThis` spellings share the one property.
        "Object = 8; finish(globalThis.Object === 8);",
        "globalThis.Object = 9; finish(Object === 9);",
        "Object = 8; finish('Object' in globalThis);",
        // A compound target reads the seeded built-in, so `??=` keeps it.
        "Object ??= 9; finish(Object === ({}).constructor);",
        // Destructuring and loop targets land on the same property.
        "({ n: Number } = { n: 9 }); finish(Number === 9);",
        "for (Map of [4]) {} finish(Map === 4);",
        "[Set] = [6]; finish(Set === 6);",
        // A name an authored `let`/`function`/parameter binds is not the
        // global property: the write reaches the authored binding.
        "let Object = 1; Object = 2; finish(Object === 2);",
        "function f(Object) { Object = 3; return Object; } finish(f(1) === 3 && typeof Object === 'function');",
    ] {
        assert_eq!(finished(source), Value::Bool(true), "{source}");
    }
}

/// Inside a function a bare read or write of a session global reaches the
/// root slot live, the same answer `globalThis.name` gives — never the copy
/// a capture would freeze at the closure's creation.
#[test]
fn functions_reach_the_live_global_property() {
    for source in [
        "function write(v) { Object = v; } const keep = Object; write(7); finish(Object === 7 && globalThis.Object === 7);",
        "const read = function() { return Object; }; const keep = Object; Object = 5; finish(read() === 5);",
        "function kind() { return typeof Object; } const keep = Object; Object = 5; finish(kind() === 'number');",
        // A function created before any write answers the seeded built-in.
        "const read = function() { return Object; }; Object = 5; const other = read; finish(other() === 5);",
        "const read = function() { return Object; }; Object = 5; Object = read(); finish(Object === 5);",
    ] {
        assert_eq!(finished(source), Value::Bool(true), "{source}");
    }
    // A read inside a function before any write sees the built-in itself.
    assert_eq!(
        finished("const read = function() { return typeof Math; }; finish(read());"),
        Value::String("object".into())
    );
}

/// `NaN`, `undefined` and `Infinity` are the global object's non-writable
/// value properties: a bare write in strict code evaluates the right-hand
/// side and then throws the `TypeError` Node reports — it is not an unknown
/// binding.
#[test]
fn writes_to_the_reserved_value_globals_throw_type_error() {
    for name in ["NaN", "undefined", "Infinity"] {
        // The Test262 shape is the write inside a function, caught by name.
        let caught = format!(
            "var threw = 'no'; try {{ (function() {{ {name} = 12; }})(); }} catch (e) {{ threw = e.name; }} finish(threw);"
        );
        assert_eq!(
            finished(&caught),
            Value::String("TypeError".into()),
            "{caught}"
        );
        for source in [
            format!("{name} = 12;"),
            format!("(function() {{ {name} = 12; }})();"),
        ] {
            let error = execute(&source).expect_err("a non-writable global property throws");
            assert!(
                error
                    .to_string()
                    .contains(&format!("Cannot assign to read only property '{name}'")),
                "{source}: {error}"
            );
        }
    }
}

/// `eval` and `arguments` are never simple assignment targets in strict
/// code: the early error is a `SyntaxError` (ECMA-262 §13.15.2), which the
/// parser misses only inside destructuring patterns.
#[test]
fn eval_and_arguments_targets_are_early_syntax_errors() {
    for source in [
        "({ eval } = {});",
        "({ eval = 0 } = {});",
        "for ({ eval } of [{}]) {}",
        "for ({ eval = 0 } of [{}]) {}",
        "({ x: arguments } = { x: 1 });",
    ] {
        let error = lash_typescript::testing::compile(source)
            .expect_err("an eval or arguments target is an early SyntaxError");
        assert_eq!(
            error.code,
            lash_typescript::DiagnosticCode::SyntaxError,
            "{source}: {error}"
        );
    }
}

/// `new` stays refused exactly where it was: making the constructor
/// readable as a value does not make `new Object()` constructible.
#[test]
fn unsupported_new_stays_refused() {
    for source in ["finish(new Object());", "finish(new Promise());"] {
        let error = lash_typescript::testing::compile(source)
            .expect_err("an unsupported `new` stays refused");
        assert_eq!(
            error.code,
            lash_typescript::DiagnosticCode::NewUnsupported,
            "{source}: {error}"
        );
    }
}
