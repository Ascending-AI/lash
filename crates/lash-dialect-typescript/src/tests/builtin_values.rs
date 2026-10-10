//! Built-ins as values (FIG-5789): a built-in function, constructor,
//! namespace or prototype read as a value, and the typed refusal of
//! reflection on one (Sam 2026-10-10, option B).

use super::machine::{self, Ended};
use super::remaining_builtins::agrees;
use crate::DiagnosticCode;

/// A built-in passed, held or borrowed as a value is called with the
/// receiver and arguments the program gives it.
#[test]
fn built_in_functions_are_callable_values() {
    agrees(
        "const numbers = ['1', '2', 'x'].map(Number); const kept = [0, 1, '', 'a'].filter(Boolean); const slice = Array.prototype.slice; function rest() { return Array.prototype.slice.call(arguments, 1); } const largest = Math.max; const atLeastTen = Math.max.bind(null, 10); await finish(numbers[1] === 2 && Number.isNaN(numbers[2]) && kept.length === 2 && slice.call([1, 2, 3], 1)[0] === 2 && rest(1, 2, 3).length === 2 && largest.call(null, 4, 7) === 7 && Math.max.apply(null, [1, 5, 2]) === 5 && atLeastTen(3) === 10 && Number.isNaN([3, 9].reduce(Math.max)) && [-1, 2].map(Math.abs)[0] === 1);",
    );
}

/// Every read of one built-in is the same value, which has the `name` and
/// `length` ECMA-262 gives it; namespaces and prototypes are objects.
#[test]
fn built_in_values_have_identity_name_and_length() {
    agrees(
        "const max = Math.max; const keyed = new Map([[max, 'max']]); const f = Array.prototype.slice; await finish([].map === Array.prototype.map && f === [1].slice && keyed.get(max) === 'max' && [...keyed.keys()][0] === Math.max && [...keyed.keys()][0](1, 2) === 2 && Math.max.name === 'max' && Math.max.length === 2 && f.name === 'slice' && f.length === 2 && Map.length === 0 && typeof Math === 'object' && typeof Array.prototype === 'object' && typeof f === 'function' && typeof Map === 'function' && Array.prototype.constructor === Array && [].constructor === Array && Object.prototype.toString.call(Math) === '[object Math]' && String(Math.abs) === 'function abs() { [native code] }');",
    );
}

/// A method read from one receiver keeps its own helper: called on a value
/// of another kind it rejects that receiver, as ECMA-262's brand checks do,
/// rather than answering with the other kind's method.
#[test]
fn a_borrowed_method_checks_the_receiver_it_is_given() {
    agrees(
        "const has = new Set([1]).has; let rejected = false; try { has.call(new Map([[1, 2]]), 1); } catch (e) { rejected = e instanceof TypeError; } await finish(rejected && has.call(new Set([1]), 1));",
    );
}

/// An own-property test, a write or a descriptor of a built-in is refused
/// with the typed reflection error: at lowering where the source names the
/// built-in, at run time where it reaches one through a value.
#[test]
fn reflection_on_a_built_in_is_a_typed_refusal() {
    for source in [
        "'max' in Math;",
        "Math.max.length = 3;",
        "Object.getOwnPropertyDescriptor(Array.prototype, 'map');",
        "Math.hasOwnProperty('max');",
    ] {
        let refused = super::lower(source).expect_err(source);
        assert_eq!(
            refused.code,
            DiagnosticCode::ReflectionUnsupported,
            "{source}"
        );
    }
    for source in [
        "const f = Math.max; Object.hasOwn(f, 'length');",
        "const m = Math; await finish('max' in m);",
        "const p = Array.prototype; p.extra = 1;",
        "const f = Math.max; Object.defineProperty(f, 'x', {value: 1});",
    ] {
        assert_eq!(
            machine::end(source),
            Ended::Raised("TS_REFLECTION_UNSUPPORTED".to_string()),
            "{source}"
        );
    }
}

/// A program reaches the helpers of the built-ins and member names it
/// names, however it calls them: calling a function value, reading or
/// calling a member no built-in has, and passing a built-in as a value do
/// not bring in every built-in's helper.
#[test]
fn a_program_reaches_only_the_built_ins_it_names() {
    let lowered = super::lower(
        "function twice(f, x) { return f(f(x)); } const o = { scale: 2 }; o.scale; o.missing; twice(Math.abs, -3);",
    )
    .unwrap();
    let listed: Vec<&str> = lowered
        .document
        .manifest
        .functions
        .values()
        .map(|name| name.as_str())
        .collect();
    assert!(listed.contains(&"ts.math.abs"), "{listed:?}");
    for unnamed in [
        "ts.math.max",
        "ts.array.map",
        "ts.string.slice",
        "ts.map.construct",
    ] {
        assert!(
            !listed.contains(&unnamed),
            "{unnamed} is reached: {listed:?}"
        );
    }
}

/// The global eval is a function value even though dynamic evaluation is refused.
#[test]
fn eval_is_a_function_value_without_dynamic_evaluation() {
    assert_eq!(
        machine::end(
            "function sameReceiver() { return this === eval; } await finish([1].every(sameReceiver, eval) && eval.length === 1 && eval.prototype === undefined);"
        ),
        Ended::Finished(lash_kernel_doc::Datum::Bool(true)),
    );
    assert_eq!(
        machine::end("const evaluate = eval; evaluate('1 + 2');"),
        Ended::Raised("TS_EVAL_UNSUPPORTED".to_string()),
    );
}

/// Object instances include containers and functions, but exclude primitives.
#[test]
fn object_instanceof_distinguishes_objects_from_primitives() {
    agrees(
        "let valid = true; for (const value of [{}, [], new Map(), new Set(), new Date(0), /x/, function() {}, Math, new Error()]) { valid = valid && value instanceof Object; } for (const value of [null, undefined, false, 0, 'x']) { valid = valid && !(value instanceof Object); } await finish(valid && !(Object.prototype instanceof Object));",
    );
}

/// Option B refuses reflective access to symbols and replacement of globals.
#[test]
fn reflective_protocols_and_builtin_global_writes_are_typed_refusals() {
    for source in [
        "Symbol.iterator;",
        "NaN = 12;",
        "undefined = 12;",
        "Object = 12;",
        "Number = 12;",
    ] {
        assert_eq!(
            super::lower(source).expect_err(source).code,
            DiagnosticCode::ReflectionUnsupported
        );
    }
}
