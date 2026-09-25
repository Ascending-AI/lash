//! FIG-3700: method calls and callbacks bind exact receivers — a detached built-in call answers as Node does.

use super::*;

/// A plain call passes `undefined` as the receiver, and the built-in answers
/// as node's does: a TypeError, except `Object.prototype.toString`, which
/// tags `undefined`. Argument steps ECMA orders ahead of the receiver check
/// run first.
#[test]
fn a_detached_builtin_call_answers_as_node_does() {
    let caught = |call: &str| {
        finished(&format!(
            "try {{ {call}; finish('returned'); }} catch (e) {{ finish(e instanceof TypeError ? e.message : 'not a TypeError'); }}"
        ))
    };
    for (call, message) in [
        (
            "const f = 'x'.includes; f('x')",
            "String.prototype.includes called on null or undefined",
        ),
        (
            "const f = 'x'.valueOf; f()",
            "String.prototype.valueOf requires that 'this' be a String",
        ),
        (
            "const f = (1).toFixed; f(2)",
            "Number.prototype.toFixed requires that 'this' be a Number",
        ),
        (
            "const f = [].map; f((x: number) => x)",
            "Array.prototype.map called on null or undefined",
        ),
        (
            "const f = new Map().get; f(1)",
            "Method Map.prototype.get called on incompatible receiver undefined",
        ),
        (
            "const f = ({}).hasOwnProperty; f('a')",
            "Cannot convert undefined or null to object",
        ),
        (
            "const f = [].sort; f(1)",
            "The comparison function must be either a function or undefined: 1",
        ),
        (
            "const f = [].sort; f()",
            "Cannot convert undefined or null to object",
        ),
        (
            "const f = 'x'.trimEnd; f()",
            "String.prototype.trimRight called on null or undefined",
        ),
        (
            "const f = new Date(0).getTime; f()",
            "this is not a Date object.",
        ),
    ] {
        assert_eq!(caught(call), Value::String(message.into()), "{call}");
    }
    assert_eq!(
        finished("const t = ({}).toString; finish(t());"),
        Value::String("[object Undefined]".into())
    );
    // As a callback it is called the same way, once per element.
    assert_eq!(
        finished("finish(['a', 'b', 'c'].map(({}).toString).join());"),
        Value::String("[object Undefined],[object Undefined],[object Undefined]".into())
    );
    assert_eq!(
        caught("['a'].forEach('x'.trim)"),
        Value::String("String.prototype.trim called on null or undefined".into())
    );
}
