//! FIG-3653 and FIG-3654: faults in ECMA-specified operations throw their ECMA class — caught, an instance with Node's message; uncaught, the cell's own error.

use super::*;

/// The detached `{ name, message }` of an uncaught thrown error.
fn uncaught(source: &str) -> (String, String) {
    let error = execute(source).expect_err("the program throws");
    let RuntimeError::UncaughtException {
        value: Value::Record(record),
    } = &error
    else {
        panic!("{source}: expected an uncaught error object, got {error:?}");
    };
    let text = |field: &str| match record.get(field) {
        Some(Value::String(text)) => text.to_string(),
        other => panic!("{source}: `{field}` is {other:?}"),
    };
    (text("name"), text("message"))
}

/// A fault in an operation ECMA-262 specifies to throw is that operation's
/// own error (FIG-3653, FIG-3654): caught, it is an instance of its class with
/// Node's message and no `cause`; uncaught, it ends the cell as an uncaught
/// exception of that class. Before, each of these was a `RuntimeError` brand
/// or threw nothing at all.
#[test]
fn ecma_specified_faults_throw_their_ecma_class() {
    let caught = [
        (
            "const n: any = null; n.x;",
            "TypeError",
            "Cannot read properties of null (reading 'x')",
        ),
        (
            "const n: any = undefined; n[0];",
            "TypeError",
            "Cannot read properties of undefined (reading '0')",
        ),
        (
            "const n: any = null; n.x = 1;",
            "TypeError",
            "Cannot set properties of null (setting 'x')",
        ),
        (
            "const {} = null as any;",
            "TypeError",
            "Cannot destructure 'null' as it is null.",
        ),
        (
            "(({}: any) => 1)(undefined);",
            "TypeError",
            "Cannot destructure 'undefined' as it is undefined.",
        ),
        (
            "[].find(null as any);",
            "TypeError",
            "object null is not a function",
        ),
        (
            "Map.groupBy([], null as any);",
            "TypeError",
            "object null is not a function",
        ),
        (
            "JSON.parse('{');",
            "SyntaxError",
            "JSON.parse: EOF while parsing an object at line 1 column 1",
        ),
        (
            "(1).toFixed(101);",
            "RangeError",
            "toFixed() digits argument must be between 0 and 100",
        ),
        ("'a'.repeat(-1);", "RangeError", "Invalid count value: -1"),
        (
            "'a'.startsWith(/a/ as any);",
            "TypeError",
            "First argument to String.prototype.startsWith must not be a regular expression",
        ),
        (
            "Object.keys(undefined as any);",
            "TypeError",
            "Cannot convert undefined or null to object",
        ),
        (
            "Error({ toString: undefined, valueOf: undefined } as any);",
            "TypeError",
            "Cannot convert object to primitive value",
        ),
        (
            "(function () {} as any).caller;",
            "TypeError",
            "'caller', 'callee', and 'arguments' properties may not be accessed on strict mode functions or the arguments objects for calls to them",
        ),
        (
            "const m: any = new Map(); m.size = 1;",
            "TypeError",
            "Cannot set property size of #<Map> which has only a getter",
        ),
        (
            "const n: any = 5; for (const x of n) {}",
            "TypeError",
            "5 is not iterable",
        ),
    ];
    for (statement, name, message) in caught {
        let source = format!(
            "try {{ {statement} finish('no throw'); }} catch (e) {{ finish([e instanceof Error, e.name, e.message, e.cause === undefined]); }}"
        );
        assert_eq!(
            finished(&source),
            Value::List(
                vec![
                    Value::Bool(true),
                    Value::String(name.into()),
                    Value::String(message.into()),
                    Value::Bool(true),
                ]
                .into()
            ),
            "{statement}"
        );
        assert_eq!(
            uncaught(statement),
            (name.to_string(), message.to_string()),
            "uncaught: {statement}"
        );
    }
    // Through a `finally` with nothing to catch it, the error leaves as the
    // thrown object, not as the substrate failure it was raised from.
    assert_eq!(
        uncaught("try { const n: any = null; n.x; } finally { console.log('cleanup'); }"),
        (
            "TypeError".to_string(),
            "Cannot read properties of null (reading 'x')".to_string()
        )
    );
    assert_eq!(
        finished(
            "try { try { const n: any = null; n.x; } finally { } } catch (e) { finish(e instanceof TypeError); }"
        ),
        Value::Bool(true)
    );
}

/// A write that ECMA answers by creating an own property on an object this
/// value model gives no slot refuses by name; a write ECMA makes read-only in
/// strict code throws its TypeError.
#[test]
fn writes_onto_exotic_objects_refuse_or_throw_as_ecma_does() {
    for (source, code) in [
        (
            "const f: any = () => 1; f.cache = 1;",
            "TS_EXOTIC_PROPERTY_UNSUPPORTED",
        ),
        (
            "const e: any = new Error('m'); e.name = 'Custom';",
            "TS_EXOTIC_PROPERTY_UNSUPPORTED",
        ),
        (
            "const a: any = [1]; a.size = 3;",
            "TS_ARRAY_NON_INDEX_PROPERTY_UNSUPPORTED",
        ),
        (
            "const d: any = new Date(0); d.label = 'x';",
            "TS_DATE_IMMUTABLE",
        ),
    ] {
        let error = execute(&format!("{source} finish(1);")).expect_err(source);
        assert!(error.to_string().contains(code), "{source}: {error}");
    }
    for (source, message) in [
        (
            "const f: any = () => 1; f.name = 'x';",
            "Cannot assign to read only property 'name' of function",
        ),
        (
            "const f: any = () => 1; f.caller = 1;",
            "'caller', 'callee', and 'arguments' properties may not be accessed on strict mode functions or the arguments objects for calls to them",
        ),
        (
            "(Number as any).MAX_VALUE = 1;",
            "Cannot assign to read only property 'MAX_VALUE' of function 'Number'",
        ),
    ] {
        assert_eq!(
            uncaught(source),
            ("TypeError".to_string(), message.to_string()),
            "{source}"
        );
    }
}
