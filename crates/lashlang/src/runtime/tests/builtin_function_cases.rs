use super::*;
use crate::ast::{JavaScriptBinaryOp, JavaScriptUnaryOp};
use crate::runtime::vm::VmContinuation;
use crate::runtime::{BuiltinFunction, BuiltinPrototype};

// Built-in method values (FIG-3701): `'x'.includes` reads one function object
// per built-in, whichever receiver it was read through, and that identity has
// to hold across a suspension exactly as it does in a resident VM.

fn strict_equal(left: Expr, right: Expr) -> Expr {
    Expr::JavaScriptBinary {
        left: Box::new(left),
        op: JavaScriptBinaryOp::StrictEqual,
        right: Box::new(right),
    }
}

fn type_of(expr: Expr) -> Expr {
    Expr::JavaScriptUnary {
        op: JavaScriptUnaryOp::TypeOf,
        expr: Box::new(expr),
    }
}

/// `f = 'x'.includes` / `t = {}.toString` / `marker = await tools.echo(..)` /
/// `g = 'y'['includes']` / `finish [f === g, f === [].includes, typeof f,
/// f.length, f.name, t === {a: 1}.toString, t()]`
///
/// The awaited echo puts an effect boundary between the two reads, so a
/// restored run has to find the object the first read allocated.
fn method_value_program() -> Program {
    builders::program(vec![
        builders::assign("f", builders::field(builders::string("x"), "includes")),
        builders::assign("t", builders::field(builders::record(vec![]), "toString")),
        builders::assign(
            "marker",
            builders::module_call(
                &["tools"],
                "echo",
                vec![builders::record(vec![("value", builders::num(1.0))])],
            ),
        ),
        builders::assign(
            "g",
            builders::index(builders::string("y"), builders::string("includes")),
        ),
        builders::finish(builders::list(vec![
            strict_equal(builders::var("f"), builders::var("g")),
            strict_equal(
                builders::var("f"),
                builders::field(builders::list(vec![]), "includes"),
            ),
            type_of(builders::var("f")),
            builders::field(builders::var("f"), "length"),
            builders::field(builders::var("f"), "name"),
            strict_equal(
                builders::var("t"),
                builders::field(
                    builders::record(vec![("a", builders::num(1.0))]),
                    "toString",
                ),
            ),
            builders::call(builders::var("t"), vec![]),
        ])),
    ])
}

fn expected_method_value_outcome() -> ExecutionOutcome {
    ExecutionOutcome::Finished(Value::List(
        vec![
            Value::Bool(true),
            Value::Bool(false),
            Value::String("function".into()),
            Value::Number(1.0),
            Value::String("includes".into()),
            Value::Bool(true),
            Value::String("[object Undefined]".into()),
        ]
        .into(),
    ))
}

#[tokio::test(flavor = "current_thread")]
async fn method_values_keep_their_identity_resident() {
    let program = compile_program_for_tests(method_value_program());
    assert_eq!(
        uninterrupted_continuation_result(&program).await,
        expected_method_value_outcome()
    );
}

/// A continuation's JSON less the wall-clock time the run spent, which is the
/// one field two runs of one program never share.
fn without_wall_clock(bytes: &[u8]) -> serde_json::Value {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).expect("continuation JSON");
    value
        .as_object_mut()
        .expect("a continuation is an object")
        .remove("active_execution_elapsed")
        .expect("a continuation records its elapsed time");
    value
}

/// At every instruction boundary the run can park at, a continuation restored
/// from its bytes finishes exactly as the resident VM does, and a second cold
/// run parked at the same boundary captures the same state: a replay rebuilds
/// the very heap, built-in function objects included.
#[tokio::test(flavor = "current_thread")]
async fn method_values_survive_every_suspension_point_and_replay_to_the_same_bytes() {
    let program = compile_program_for_tests(method_value_program());
    let host = Host;
    let mut parked_with_a_builtin = 0usize;
    for budget in 1..=program.chunk.code.len() * 4 {
        let mut vm = continuation_test_vm(&program, &host);
        vm.suspend_after_instructions(budget);
        if !matches!(vm.run_for_mode().await, Ok(ExecutionOutcome::Continued)) {
            continue;
        }
        let continuation = vm.suspend().expect("every boundary is capturable");
        let bytes = serde_json::to_vec(&continuation).expect("continuation serializes");
        if String::from_utf8_lossy(&bytes).contains("\"builtin_function\"") {
            parked_with_a_builtin += 1;
        }

        let mut cold = continuation_test_vm(&program, &host);
        cold.suspend_after_instructions(budget);
        assert_eq!(
            cold.run_for_mode().await.expect("the cold run parks too"),
            ExecutionOutcome::Continued
        );
        let cold_bytes =
            serde_json::to_vec(&cold.suspend().expect("the cold run captures")).expect("encode");
        assert_eq!(
            without_wall_clock(&cold_bytes),
            without_wall_clock(&bytes),
            "boundary {budget} replays to another state"
        );

        // The exhausted budget parks the resident VM after every instruction
        // from here on; driving it on is the run that never left memory.
        let resident = loop {
            let outcome = vm.run_for_mode().await.expect("the resident VM runs");
            if outcome != ExecutionOutcome::Continued {
                break outcome;
            }
        };
        let restored = round_trip_and_resume(&program, continuation).await;
        assert_eq!(
            resident,
            expected_method_value_outcome(),
            "boundary {budget}"
        );
        assert_eq!(restored, resident, "boundary {budget}");
    }
    assert!(
        parked_with_a_builtin > 3,
        "only {parked_with_a_builtin} boundaries held a built-in function"
    );
}

async fn continuation_holding_a_builtin() -> (CompiledProgram, VmContinuation) {
    let program = compile_program_for_tests(method_value_program());
    let continuation = find_instruction_continuation(&program, |continuation| {
        serde_json::to_string(continuation)
            .is_ok_and(|text| text.contains(r#""kind":"builtin_function""#))
    })
    .await;
    (program, continuation)
}

/// The wire names a built-in by owner scope and name, never by table position.
#[tokio::test(flavor = "current_thread")]
async fn a_continuation_names_a_builtin_by_prototype_and_name() {
    let (_, continuation) = continuation_holding_a_builtin().await;
    let text = serde_json::to_string(&continuation).expect("encode");
    assert!(
        text.contains(r#"{"kind":"builtin_function","owner":"String","name":"includes"}"#),
        "{text}"
    );
}

/// A wire naming a function the table does not carry is refused, not
/// restored as some other function.
#[tokio::test(flavor = "current_thread")]
async fn a_continuation_naming_an_unknown_builtin_is_refused() {
    let (_, continuation) = continuation_holding_a_builtin().await;
    let text = serde_json::to_string(&continuation).expect("encode");
    for forged in [
        text.replace(r#""name":"includes""#, r#""name":"bogus""#),
        text.replace(r#""owner":"String""#, r#""owner":"Symbol""#),
    ] {
        assert_ne!(forged, text);
        let error = serde_json::from_str::<VmContinuation>(&forged)
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default();
        assert!(error.contains("unknown built-in function"), "{error}");
    }
}

/// `Set.prototype.keys` is the function object `Set.prototype.values`, and
/// every prototype's chain ends at `Object.prototype`.
#[test]
fn inherited_reads_follow_ecma_identity() {
    let inherited = |prototype, key| {
        BuiltinFunction::inherited(prototype, key).map(|function| {
            (
                function.prototype().expect("a method's owner"),
                function.name(),
            )
        })
    };
    assert_eq!(
        inherited(BuiltinPrototype::Set, "keys"),
        Some((BuiltinPrototype::Set, "values"))
    );
    assert_eq!(
        inherited(BuiltinPrototype::Map, "keys"),
        Some((BuiltinPrototype::Map, "keys"))
    );
    assert_eq!(
        inherited(BuiltinPrototype::Array, "hasOwnProperty"),
        Some((BuiltinPrototype::Object, "hasOwnProperty"))
    );
    assert_eq!(
        inherited(BuiltinPrototype::Array, "toString"),
        Some((BuiltinPrototype::Array, "toString"))
    );
    assert_eq!(
        inherited(BuiltinPrototype::Array, "valueOf"),
        Some((BuiltinPrototype::Object, "valueOf"))
    );
    assert_eq!(inherited(BuiltinPrototype::String, "map"), None);
    assert_eq!(
        inherited(BuiltinPrototype::String, "localeCompare"),
        Some((BuiltinPrototype::String, "localeCompare"))
    );
}
