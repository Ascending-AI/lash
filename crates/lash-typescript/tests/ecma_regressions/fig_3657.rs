//! FIG-3657: error objects carry message, cause and errors as own properties.

use super::*;

/// `message` is an own data property only when the constructor received a
/// non-`undefined` argument (ECMA-262 20.5.1.1 step 4); otherwise the read
/// answers `Error.prototype.message`, `""`, without the property being own.
/// The heap used to carry `message` as a bare `String`, so `new Error()` and
/// `new Error('x')` were indistinguishable to `Object.hasOwn` — and Test262's
/// `propertyHelper.js` failed on every Error-family constructor test
/// (FIG-3657).
#[test]
fn error_message_is_an_own_property_only_when_supplied() {
    for (source, expected) in [
        ("finish(Object.hasOwn(new Error('x'), 'message'));", true),
        ("finish(Object.hasOwn(new Error(), 'message'));", false),
        (
            "finish(Object.hasOwn(new Error(undefined), 'message'));",
            false,
        ),
        // An explicitly empty message is still a supplied argument.
        ("finish(Object.hasOwn(new Error(''), 'message'));", true),
        (
            "finish(Object.hasOwn(new TypeError('x'), 'message'));",
            true,
        ),
        ("finish(Object.hasOwn(new TypeError(), 'message'));", false),
        ("finish('message' in new Error('x'));", true),
        // `in` walks the prototype chain, and `Error.prototype` owns a
        // `message` — so the answer is true even when nothing was supplied.
        ("finish('message' in new Error());", true),
    ] {
        assert_eq!(finished(source), Value::Bool(expected), "{source}");
    }
    // The read itself answers the prototype's `""` when the property is absent.
    for source in [
        "finish(new Error().message === '');",
        "finish(new Error(undefined).message === '');",
        "finish(new TypeError().toString() === 'TypeError');",
        "finish(new TypeError('x').toString() === 'TypeError: x');",
        "finish(String(new RangeError()) === 'RangeError');",
    ] {
        assert_eq!(finished(source), Value::Bool(true), "{source}");
    }
}

/// The constructor ToString's the message argument before installing it.
#[test]
fn error_message_is_to_string_converted() {
    for (source, expected) in [
        ("finish(new Error(42).message);", "42"),
        ("finish(new Error(true).message);", "true"),
        ("finish(new Error(false).message);", "false"),
        ("finish(new Error(null).message);", "null"),
        ("finish(new Error({}).message);", "[object Object]"),
        ("finish(new AggregateError([], '42').message);", "42"),
    ] {
        assert_eq!(finished(source), Value::String(expected.into()), "{source}");
    }
}

/// `cause` is installed exactly when `options` is an object carrying a
/// `cause` property (InstallErrorCause) — `Some(undefined)` when the property
/// exists with an `undefined` value, absent otherwise.
#[test]
fn error_cause_is_an_own_property_only_when_options_carry_it() {
    for (source, expected) in [
        (
            "finish(Object.hasOwn(new Error('m', { cause: 7 }), 'cause'));",
            true,
        ),
        ("finish(new Error('m', { cause: 7 }).cause === 7);", true),
        (
            "finish(Object.hasOwn(new Error('m', { cause: undefined }), 'cause'));",
            true,
        ),
        (
            "finish(new Error('m', { cause: undefined }).cause === undefined);",
            true,
        ),
        ("finish(Object.hasOwn(new Error('m'), 'cause'));", false),
        ("finish(Object.hasOwn(new Error('m', {}), 'cause'));", false),
        // A non-object options argument installs nothing.
        (
            "finish(Object.hasOwn(new Error('m', 'x'), 'cause'));",
            false,
        ),
        (
            "finish(Object.hasOwn(new Error('m', null), 'cause'));",
            false,
        ),
        ("finish(new Error('m').cause === undefined);", true),
    ] {
        assert_eq!(finished(source), Value::Bool(expected), "{source}");
    }
    // The installed value keeps object identity.
    assert_eq!(
        finished("const cause = { tag: 'c' }; finish(new Error('m', { cause }).cause === cause);"),
        Value::Bool(true)
    );
}

/// `AggregateError` shifts the same rules one argument over:
/// `AggregateError(errors, message, options)` installs `errors` always,
/// `message` when supplied, and `cause` when `options` carries it.
#[test]
fn aggregate_error_installs_errors_message_and_cause_as_own_properties() {
    for (source, expected) in [
        (
            "finish(Object.hasOwn(new AggregateError([], 'm'), 'errors'));",
            true,
        ),
        (
            "finish(new AggregateError([1, 2], 'm').errors.length === 2);",
            true,
        ),
        (
            "finish(Object.hasOwn(new AggregateError([], 'm'), 'message'));",
            true,
        ),
        (
            "finish(Object.hasOwn(new AggregateError([]), 'message'));",
            false,
        ),
        (
            "finish(Object.hasOwn(new AggregateError([], 'm', { cause: 9 }), 'cause'));",
            true,
        ),
        (
            "finish(new AggregateError([], 'm', { cause: 9 }).cause === 9);",
            true,
        ),
        (
            "finish(Object.hasOwn(new AggregateError([], 'm'), 'cause'));",
            false,
        ),
        (
            "finish(Object.hasOwn(new AggregateError([], 'm', { cause: undefined }), 'cause'));",
            true,
        ),
    ] {
        assert_eq!(finished(source), Value::Bool(expected), "{source}");
    }
}

/// `message`, `cause`, and `errors` are writable and configurable but not
/// enumerable — the `propertyHelper.js` contract. `Object.keys` and
/// `for...in` see none of them, a write lands in the slot, and `delete`
/// removes it for good.
#[test]
fn error_own_properties_are_writable_configurable_and_non_enumerable() {
    for (source, expected) in [
        (
            "finish(Object.keys(new Error('x', { cause: 1 })).length === 0);",
            true,
        ),
        (
            "finish(Object.keys(new AggregateError([1], 'x', { cause: 1 })).length === 0);",
            true,
        ),
        (
            "const e: any = new Error('x'); e.message = 'y'; finish(e.message === 'y');",
            true,
        ),
        (
            "const e: any = new Error('x', { cause: 1 }); e.cause = 2; finish(e.cause === 2);",
            true,
        ),
        (
            "const e: any = new Error('x'); e.cause = 3; finish(Object.hasOwn(e, 'cause') && e.cause === 3);",
            true,
        ),
        (
            "const e: any = new Error('x'); delete e.message; finish(!Object.hasOwn(e, 'message'));",
            true,
        ),
        (
            "const e: any = new Error('x', { cause: 1 }); delete e.cause; finish(!Object.hasOwn(e, 'cause'));",
            true,
        ),
        (
            "const e: any = new AggregateError([1], 'x'); delete e.errors; finish(!Object.hasOwn(e, 'errors'));",
            true,
        ),
        // After deletion the reads answer the prototype defaults again.
        (
            "const e: any = new Error('x'); delete e.message; finish(e.message === '');",
            true,
        ),
        (
            "const e: any = new Error('x', { cause: 1 }); delete e.cause; finish(e.cause === undefined);",
            true,
        ),
    ] {
        assert_eq!(finished(source), Value::Bool(expected), "{source}");
    }
    // `for...in` visits nothing on an error carrying every own property.
    assert_eq!(
        finished(
            "const e: any = new AggregateError([1], 'x', { cause: 1 }); let n = 0; for (const k in e) { n += 1; } finish(n);"
        ),
        Value::Number(0.0)
    );
    // `name` answers from the brand but is never an own property.
    assert_eq!(
        finished(
            "finish(new TypeError().name === 'TypeError' && !Object.hasOwn(new TypeError(), 'name'));"
        ),
        Value::Bool(true)
    );
}
