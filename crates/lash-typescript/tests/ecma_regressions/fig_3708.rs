//! FIG-3708: the `arguments` object outside a function refuses with its own
//! code, `TS_ARGUMENTS_UNSUPPORTED`, instead of borrowing `TS_THIS_UNSUPPORTED`.

use super::*;

/// Top-level `arguments` refuses by name and points at rest parameters.
/// Inside a non-arrow function the arguments object is supported, so the
/// refusal cannot have moved.
#[test]
fn arguments_outside_a_function_refuses_with_its_own_code() {
    for source in [
        "const x = arguments;",
        "finish(arguments.length);",
        "finish(typeof arguments);",
    ] {
        let error = lash_typescript::validate(source).expect_err(source);
        assert_eq!(
            error.code,
            lash_typescript::DiagnosticCode::ArgumentsUnsupported,
            "{source}: {error}"
        );
        assert_eq!(
            error.code.as_str(),
            "TS_ARGUMENTS_UNSUPPORTED",
            "{source}: {error}"
        );
        assert!(error.to_string().contains("...rest"), "{source}: {error}");
    }
}

/// A non-arrow function's `arguments` is the materialized arguments object:
/// it indexes and reports `length` exactly as ECMA-262 does.
#[test]
fn arguments_inside_a_function_still_materializes() {
    assert_eq!(
        finished("function f(): number { return arguments.length; } finish(f(1, 2, 3));"),
        Value::Number(3.0)
    );
    assert_eq!(
        finished("function f(): number { return arguments[0] + arguments[1]; } finish(f(2, 3));"),
        Value::Number(5.0)
    );
}
