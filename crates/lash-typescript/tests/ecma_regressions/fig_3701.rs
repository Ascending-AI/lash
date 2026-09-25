//! FIG-3701: built-in method reads answer ECMA's identity-stable function objects.

use super::*;

/// An advertised method read without a call is its ECMA function object
/// (FIG-3701). It used to read `undefined`, so `typeof 'x'.includes` was
/// `"undefined"` and `'word'.includes.length` threw.
#[test]
fn builtin_method_reads_are_identity_stable_functions() {
    for source in [
        // One function per built-in, whichever receiver it was read from.
        "finish('a'.includes === 'b'.includes);",
        "const f = 'x'.includes; const g = 'x'.includes; finish(f === g && Object.is(f, g));",
        "finish(new Set([1]).keys === new Set().values);",
        "finish([].hasOwnProperty === ({ a: 1 }).hasOwnProperty);",
        "finish(new Date(0).getTime === new Date(5).getTime);",
        "finish(/a/.test === /b/.test);",
        "finish(new Error('x').toString === new TypeError('y').toString);",
        // Distinct prototypes carry distinct functions.
        "finish('a'.includes !== [].includes);",
        "finish([].toString !== ({}).toString);",
        "finish(new Map().keys !== new Map().values);",
        // A variable, a computed and an optional read all find the same one.
        "const s = 'word'; const k = 'includes'; finish(s[k] === s.includes && s?.includes === s.includes);",
        "const { includes } = 'abc'; finish(includes === 'x'.includes);",
        // An own property wins, even one holding `undefined`.
        "finish(({ includes: 7 }).includes === 7);",
        "finish(({ toString: undefined }).toString === undefined);",
        // Identity holds wherever SameValue or SameValueZero asks.
        "finish(new Set(['x'.includes]).has('y'.includes));",
        "finish(['x'.includes].indexOf('z'.includes) === 0);",
        "finish(new Map([['x'.includes, 1]]).get('y'.includes) === 1);",
        // ECMA's function shape: typeof, own name and length, no enumerable keys.
        "finish(typeof 'word'.includes === 'function' && typeof [].map === 'function');",
        "finish('word'.includes.length === 1 && 'word'.includes.name === 'includes');",
        "finish('x'.slice.length === 2 && 'x'.replace.length === 2 && [].flat.length === 0);",
        "finish(new Set().keys.name === 'values' && (1).toFixed.length === 1);",
        "finish(Object.hasOwn('x'.includes, 'length') && Object.hasOwn('x'.includes, 'name'));",
        "finish(Object.keys('x'.includes).length === 0);",
        "finish('x'.includes.toString === (() => 1).toString);",
        // An unadvertised name still reads nothing.
        "finish('x'.map === undefined);",
    ] {
        assert_eq!(finished(source), Value::Bool(true), "{source}");
    }
    // A built-in method value in a coercion is a function: its primitive
    // would be Function.prototype.toString's source text, which the runtime
    // does not keep, so it refuses as `TS_FUNCTION_STRING_COERCION` (the
    // FIG-3652 ruling).
    for source in [
        "finish(String([].map));",
        "finish('x'.includes + '');",
        "finish(`${'x'.includes}`);",
    ] {
        let error = execute(source).expect_err("a built-in function has no primitive");
        assert!(
            error.to_string().contains("TS_FUNCTION_STRING_COERCION"),
            "the refusal is the named one for {source}: {error}"
        );
    }
    assert_eq!(
        finished("finish(JSON.stringify({ f: 'x'.includes, a: [[].map], n: 1 }));"),
        Value::String("{\"a\":[null],\"n\":1}".into())
    );
}
