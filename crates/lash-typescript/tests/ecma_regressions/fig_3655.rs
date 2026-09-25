//! FIG-3655: functions carry own name and length, and names are inferred in the named-evaluation positions.

use super::*;

/// FIG-3655: function values expose ECMA's `name` and `length` own
/// properties, including the names NamedEvaluation/SetFunctionName infer.
#[test]
fn function_name_is_inferred_in_the_named_evaluation_positions() {
    let cases = [
        ("const f = () => {}; finish(f.name);", "f"),
        ("let f = function () {}; finish(f.name);", "f"),
        ("var f = function () {}; finish(f.name);", "f"),
        ("function g() {} finish(g.name);", "g"),
        ("finish((function named() {}).name);", "named"),
        ("finish((() => {}).name);", ""),
        ("const f = function g() {}; finish(f.name);", "g"),
        ("let f; f = () => {}; finish(f.name);", "f"),
        ("let f; f = function () {}; finish(f.name);", "f"),
        ("const [f = () => {}] = []; finish(f.name);", "f"),
        ("const { x = () => {} } = {}; finish(x.name);", "x"),
        ("const { a: f = () => {} } = {}; finish(f.name);", "f"),
        (
            "const o = { m() {}, p: () => {}, q: function () {} }; finish(o.m.name + '|' + o.p.name + '|' + o.q.name);",
            "m|p|q",
        ),
        (
            "const o = { 'quoted-key': () => {} }; finish(o['quoted-key'].name);",
            "quoted-key",
        ),
        // A nested anonymous function sees no enclosing pending name.
        ("const outer = () => () => 1; finish(outer().name);", ""),
        // `f ??= () => {}` is a NamedEvaluation position too.
        ("let f; f ??= () => {}; finish(f.name);", "f"),
        // A member assignment is not a naming position.
        ("const o: any = {}; o.f = () => {}; finish(o.f.name);", ""),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), Value::String(expected.into()), "{source}");
    }
}

#[test]
fn function_length_counts_the_parameters_before_the_first_default_or_rest() {
    let cases = [
        ("const f = () => {}; finish(f.length);", 0.0),
        ("const f = (a, b) => {}; finish(f.length);", 2.0),
        ("const f = (a, b = 1, c) => {}; finish(f.length);", 1.0),
        ("const f = (a, ...rest) => {}; finish(f.length);", 1.0),
        ("const f = (...rest) => {}; finish(f.length);", 0.0),
        ("const f = (a = 1, b = 2) => {}; finish(f.length);", 0.0),
        ("function g(a, b = 0) {} finish(g.length);", 1.0),
        ("function g(a, b, c) {} finish(g.length);", 3.0),
        ("finish(((a, { b }) => {}).length);", 2.0),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), Value::Number(expected), "{source}");
    }
}

#[test]
fn function_name_and_length_are_own_non_enumerable_properties() {
    let cases = [
        "const f = () => {}; finish(Object.hasOwn(f, 'name'));",
        "const f = () => {}; finish(Object.hasOwn(f, 'length'));",
        "const f = () => {}; finish('name' in f && 'length' in f);",
        "const f = () => {}; finish(Object.keys(f).length === 0);",
        "const f = () => {}; let keys = ''; for (const k in f) { keys += k; } finish(keys === '');",
        "const f = () => {}; finish(f['name'] === 'f' && f['length'] === 0);",
    ];
    for source in cases {
        assert_eq!(finished(source), Value::Bool(true), "{source}");
    }
}

#[test]
fn function_name_and_length_are_non_writable_and_configurable() {
    // Writes to a present `name`/`length` throw a catchable TypeError.
    for source in [
        "const f = () => {}; try { f.name = 'x'; } catch (e) { finish(e instanceof TypeError); }",
        "const f = () => {}; try { f.length = 9; } catch (e) { finish(e instanceof TypeError); }",
        "const f = () => {}; try { f['name'] = 'x'; } catch (e) { finish(e instanceof TypeError); }",
    ] {
        assert_eq!(finished(source), Value::Bool(true), "{source}");
    }
    // An uncaught write fails the run and leaves the property untouched.
    assert!(
        execute("const f = () => {}; f.name = 'x'; finish(f.name);").is_err(),
        "a non-writable property write must not succeed silently"
    );
    // `name`/`length` are configurable: delete removes the own property, and
    // the read then answers `Function.prototype`'s non-writable `""`/`0`.
    let cases = [
        (
            "const f = () => {}; finish(delete f.name && !Object.hasOwn(f, 'name') && f.name === '');",
            true,
        ),
        (
            "const f = () => {}; finish(delete f['length'] && f.length === 0);",
            true,
        ),
        (
            // The prototype's non-writable data property blocks the write even
            // after the own one is gone: still a TypeError, never a shadowing
            // own property.
            "const f = () => {}; delete f.name; try { f.name = 're'; } catch (e) { finish(e instanceof TypeError && f.name === ''); }",
            true,
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), Value::Bool(expected), "{source}");
    }
}
