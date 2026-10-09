use super::*;
use crate::ast::{CoercingBinaryOp, CoercingUnaryOp};
use crate::runtime::vm::VmContinuation;
use crate::runtime::{BuiltinFunction, BuiltinPrototype};

// Built-in method values (FIG-3701): `'x'.includes` reads one function object
// per built-in, whichever receiver it was read through, and that identity has
// to hold across a suspension exactly as it does in a resident VM.

fn strict_equal(left: Expr, right: Expr) -> Expr {
    Expr::CoercingBinary {
        left: Box::new(left),
        op: CoercingBinaryOp::StrictEqual,
        right: Box::new(right),
    }
}

fn type_of(expr: Expr) -> Expr {
    Expr::CoercingUnary {
        op: CoercingUnaryOp::TypeOf,
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
            cold_bytes, bytes,
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

// ECMA detached calls must use the same receiver, iterable and error contracts
// as member calls. These laws pin the gaps found in Confidence 37336144906.
fn global_builtin(name: &str) -> Expr {
    builders::builtin(
        "__lash_vm_stdlib",
        vec![builders::string("Lash.Builtin"), builders::string(name)],
    )
}

fn prototype_method(owner: &str, name: &str) -> Expr {
    builders::field(builders::field(global_builtin(owner), "prototype"), name)
}

fn call_with_receiver(method: Expr, receiver: Expr, args: Vec<Expr>) -> Expr {
    Expr::MethodCall {
        receiver: Box::new(method),
        method: crate::MethodKey::Field("call".into()),
        args: std::iter::once(receiver).chain(args).collect(),
    }
}

fn exotic(kind: &str, args: Vec<Expr>) -> Expr {
    builders::builtin(
        "__lash_vm_heap_new",
        std::iter::once(builders::string(kind))
            .chain(args)
            .collect(),
    )
}

async fn detached_result(expr: Expr) -> Result<ExecutionOutcome, RuntimeError> {
    let program = compile_program(&builders::program(vec![builders::finish(expr)]));
    execute_compiled(&program, &mut State::new(), &Host).await
}

async fn detached_error_message(expr: Expr) -> String {
    let caught = super::exception_cases::exception_try(
        expr,
        Some((
            "error",
            builders::list(vec![
                builders::field(builders::var("error"), "name"),
                builders::field(builders::var("error"), "message"),
            ]),
        )),
        None,
    );
    let ExecutionOutcome::Finished(Value::List(parts)) = detached_result(caught)
        .await
        .expect("catch returns the error")
    else {
        panic!("the detached call must throw");
    };
    assert_eq!(parts[0], Value::String("TypeError".into()));
    let Value::String(message) = &parts[1] else {
        panic!("message must be text")
    };
    message.to_string()
}

#[tokio::test(flavor = "current_thread")]
async fn detached_regexp_and_error_methods_preserve_results_and_identity() {
    let program = compile_program(&builders::program(vec![
        builders::assign(
            "regexp",
            exotic("RegExp", vec![builders::string("a"), builders::string("g")]),
        ),
        builders::assign("error", exotic("Error", vec![builders::string("broken")])),
        builders::finish(builders::list(vec![
            call_with_receiver(
                prototype_method("RegExp", "test"),
                builders::var("regexp"),
                vec![builders::string("cat")],
            ),
            call_with_receiver(
                prototype_method("RegExp", "test"),
                builders::var("regexp"),
                vec![builders::string("zzz")],
            ),
            call_with_receiver(
                prototype_method("RegExp", "exec"),
                builders::var("regexp"),
                vec![builders::string("cat")],
            ),
            call_with_receiver(
                prototype_method("RegExp", "toString"),
                builders::var("regexp"),
                vec![],
            ),
            call_with_receiver(
                prototype_method("Error", "toString"),
                builders::record(vec![
                    ("name", builders::string("Oops")),
                    ("message", builders::string("broken")),
                ]),
                vec![],
            ),
            strict_equal(
                call_with_receiver(
                    prototype_method("Object", "valueOf"),
                    builders::var("error"),
                    vec![],
                ),
                builders::var("error"),
            ),
            strict_equal(
                call_with_receiver(
                    prototype_method("Object", "valueOf"),
                    builders::var("regexp"),
                    vec![],
                ),
                builders::var("regexp"),
            ),
        ])),
    ]));
    let ExecutionOutcome::Finished(Value::List(values)) =
        execute_compiled(&program, &mut State::new(), &Host)
            .await
            .expect("detached exotic methods run")
    else {
        panic!("expected results")
    };
    assert_eq!(values[0], Value::Bool(true));
    assert_eq!(values[1], Value::Bool(false));
    let Value::List(found) = &values[2] else {
        panic!("exec returns a match")
    };
    assert_eq!(found[0], Value::String("a".into()));
    assert_eq!(
        &values[3..],
        &[
            Value::String("/a/g".into()),
            Value::String("Oops: broken".into()),
            Value::Bool(true),
            Value::Bool(true)
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn detached_collection_methods_validate_kind_and_return_values() {
    for (owner, other, method, source, expected) in [
        (
            "Map",
            "Set",
            "get",
            builders::list(vec![builders::list(vec![
                builders::string("key"),
                builders::num(7.0),
            ])]),
            Value::Number(7.0),
        ),
        (
            "Set",
            "Map",
            "has",
            builders::list(vec![builders::string("key")]),
            Value::Bool(true),
        ),
    ] {
        assert_eq!(
            detached_result(call_with_receiver(
                prototype_method(owner, method),
                exotic(owner, vec![source]),
                vec![builders::string("key")]
            ))
            .await
            .expect("matching collection"),
            ExecutionOutcome::Finished(expected)
        );
        let message = detached_error_message(call_with_receiver(
            prototype_method(owner, method),
            exotic(other, vec![]),
            vec![builders::string("key")],
        ))
        .await;
        assert_eq!(
            message,
            format!("Method {owner}.prototype.{method} called on incompatible receiver #<{other}>")
        );
    }
    // A value-only callback cannot start a nested collection driver.
    let expr = call_with_receiver(
        prototype_method("Array", "map"),
        builders::list(vec![exotic("Map", vec![])]),
        vec![prototype_method("Map", "forEach"), exotic("Map", vec![])],
    );
    assert!(
        matches!(detached_result(expr).await, Err(RuntimeError::ValidationFailed { reason }) if reason.starts_with("TS_NESTED_DRIVER_UNSUPPORTED"))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn detached_receiver_errors_keep_family_specific_messages() {
    for (owner, method, expected) in [
        (
            "String",
            "toString",
            "String.prototype.toString requires that 'this' be a String",
        ),
        (
            "String",
            "trimEnd",
            "String.prototype.trimRight called on null or undefined",
        ),
        (
            "String",
            "trimStart",
            "String.prototype.trimLeft called on null or undefined",
        ),
        (
            "Array",
            "concat",
            "Array.prototype.concat called on null or undefined",
        ),
        (
            "Date",
            "toString",
            "Method Date.prototype.toString called on incompatible receiver undefined",
        ),
        (
            "Date",
            "toISOString",
            "Method Date.prototype.toISOString called on incompatible receiver undefined",
        ),
        (
            "Date",
            "toJSON",
            "Cannot convert undefined or null to object",
        ),
    ] {
        assert_eq!(
            detached_error_message(builders::call(prototype_method(owner, method), vec![])).await,
            expected
        );
    }
    for (owner, method, receiver, expected) in [
        (
            "String",
            "toString",
            builders::record(vec![]),
            "String.prototype.toString requires that 'this' be a String",
        ),
        (
            "Number",
            "valueOf",
            builders::record(vec![]),
            "Number.prototype.valueOf requires that 'this' be a Number",
        ),
        (
            "Boolean",
            "valueOf",
            builders::record(vec![]),
            "Boolean.prototype.valueOf requires that 'this' be a Boolean",
        ),
        (
            "Date",
            "getTime",
            builders::list(vec![]),
            "this is not a Date object.",
        ),
        (
            "RegExp",
            "exec",
            builders::list(vec![]),
            "Method RegExp.prototype.exec called on incompatible receiver [object Array]",
        ),
        (
            "URL",
            "toString",
            builders::record(vec![]),
            "Method 'URL.prototype.toString' called on incompatible receiver #<Object>",
        ),
        (
            "URLSearchParams",
            "toString",
            builders::bool_lit(false),
            "Value of \"this\" must be of type URLSearchParams",
        ),
    ] {
        assert_eq!(
            detached_error_message(call_with_receiver(
                prototype_method(owner, method),
                receiver,
                vec![]
            ))
            .await,
            expected
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn builtin_objects_keep_ecma_to_string_tags() {
    for (owner, expected) in [
        ("Array", "Array"),
        ("String", "String"),
        ("Number", "Number"),
        ("Boolean", "Boolean"),
        ("Map", "Map"),
        ("Set", "Set"),
        ("URL", "URL"),
        ("URLSearchParams", "URLSearchParams"),
        ("Date", "Object"),
    ] {
        assert_eq!(
            detached_result(call_with_receiver(
                prototype_method("Object", "toString"),
                builders::field(global_builtin(owner), "prototype"),
                vec![]
            ))
            .await
            .expect("prototype tag"),
            ExecutionOutcome::Finished(Value::String(format!("[object {expected}]").into()))
        );
    }
    for owner in ["Math", "JSON"] {
        let tag = if owner == "JSON" { "JSON" } else { "Math" };
        assert_eq!(
            detached_result(call_with_receiver(
                prototype_method("Object", "toString"),
                global_builtin(owner),
                vec![]
            ))
            .await
            .expect("namespace tag"),
            ExecutionOutcome::Finished(Value::String(format!("[object {tag}]").into()))
        );
    }
}

fn array_from(items: Expr, mapfn: Option<Expr>) -> Expr {
    call_with_receiver(
        builders::field(global_builtin("Array"), "from"),
        Expr::Null,
        std::iter::once(items).chain(mapfn).collect(),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn detached_array_from_copies_iterables_and_drives_callable_mappers() {
    for (source, expected) in [
        (
            builders::list(vec![builders::num(2.0), builders::num(3.0)]),
            vec![Value::Number(2.0), Value::Number(3.0)],
        ),
        (
            builders::string("a😀"),
            vec![Value::String("a".into()), Value::String("😀".into())],
        ),
        (
            exotic(
                "Set",
                vec![builders::list(vec![
                    builders::num(2.0),
                    builders::num(2.0),
                    builders::num(3.0),
                ])],
            ),
            vec![Value::Number(2.0), Value::Number(3.0)],
        ),
        (
            exotic(
                "Map",
                vec![builders::list(vec![builders::list(vec![
                    builders::string("x"),
                    builders::num(7.0),
                ])])],
            ),
            vec![Value::List(
                vec![Value::String("x".into()), Value::Number(7.0)].into(),
            )],
        ),
        (
            exotic("URLSearchParams", vec![builders::string("x=1&x=2")]),
            vec![
                Value::List(vec![Value::String("x".into()), Value::String("1".into())].into()),
                Value::List(vec![Value::String("x".into()), Value::String("2".into())].into()),
            ],
        ),
        (
            call_with_receiver(
                prototype_method("RegExp", "exec"),
                exotic("RegExp", vec![builders::string("(a)")]),
                vec![builders::string("cat")],
            ),
            vec![Value::String("a".into()), Value::String("a".into())],
        ),
        (
            builders::record(vec![
                ("length", builders::num(2.0)),
                ("0", builders::string("x")),
            ]),
            vec![Value::String("x".into()), Value::Undefined],
        ),
    ] {
        assert_eq!(
            detached_result(array_from(source, None))
                .await
                .expect("copy source"),
            ExecutionOutcome::Finished(Value::List(expected.into()))
        );
    }
    let program = compile_program(&builders::program(vec![builders::finish(array_from(
        builders::var("tuple"),
        None,
    ))]));
    let mut state = State::new();
    state
        .insert_global(
            "tuple",
            Value::Tuple(vec![Value::Number(5.0), Value::String("x".into())].into()),
        )
        .expect("seed an IR tuple");
    assert_eq!(
        execute_compiled(&program, &mut state, &Host)
            .await
            .expect("copy an IR tuple"),
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Number(5.0), Value::String("x".into())].into()
        ))
    );
    let mapper = builders::closure(
        None,
        &["value", "index"],
        &[],
        builders::binary(
            builders::var("value"),
            CoercingBinaryOp::Add,
            builders::var("index"),
        ),
    );
    assert_eq!(
        detached_result(array_from(
            builders::list(vec![builders::num(2.0), builders::num(3.0)]),
            Some(mapper)
        ))
        .await
        .expect("mapper drives"),
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Number(2.0), Value::Number(4.0)].into()
        ))
    );
    assert_eq!(
        detached_error_message(array_from(
            builders::list(vec![]),
            Some(builders::record(vec![]))
        ))
        .await,
        "#<Object> is not a function"
    );
    assert_eq!(
        detached_result(array_from(
            builders::list(vec![builders::num(2.0)]),
            Some(Expr::Absent)
        ))
        .await
        .expect("undefined mapper is omitted"),
        ExecutionOutcome::Finished(Value::List(vec![Value::Number(2.0)].into()))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn detached_array_from_distinguishes_array_length_from_memory_limits() {
    let host = super::continuation_cases::HeapConformanceHost {
        stress_gc: false,
        memory_limit: ExecutionBound::logical_bytes(4096),
    };
    for (length, range_error) in [(4_294_967_295.0, false), (4_294_967_296.0, true)] {
        let program = compile_program(&builders::program(vec![builders::finish(array_from(
            builders::record(vec![("length", builders::num(length))]),
            None,
        ))]));
        let error = execute_compiled(&program, &mut State::new(), &host)
            .await
            .expect_err("allocation is refused before walking");
        if range_error {
            assert!(
                matches!(error, RuntimeError::UncaughtException { value } if value.as_record().is_some_and(|record| record.get("name") == Some(&Value::String("RangeError".into()))))
            );
        } else {
            assert!(
                matches!(error, RuntimeError::MemoryLimitExceeded { .. }),
                "{error:?}"
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn detached_array_from_maps_iterables_before_charging_a_dense_result() {
    // Array.from's iterable path starts with an empty output, rather than
    // allocating the source's length before calling the first mapper. A mapper
    // that throws must keep that throw even when a full copy would exceed the
    // memory bound (ECMA-262 Array.from, iterable branch).
    let host = super::continuation_cases::HeapConformanceHost {
        stress_gc: false,
        memory_limit: ExecutionBound::logical_bytes(120_000),
    };
    let numbers = || builders::list((0..1_000).map(|_| builders::num(1.0)).collect());
    let regexp_match = call_with_receiver(
        prototype_method("RegExp", "exec"),
        exotic("RegExp", vec![builders::string(&"(a)".repeat(1_000))]),
        vec![builders::string(&"a".repeat(1_000))],
    );
    for (source_kind, source) in [
        ("array", numbers()),
        ("tuple", builders::var("tuple")),
        ("match", regexp_match),
    ] {
        let mapper = builders::closure(
            None,
            &["value"],
            &[],
            Expr::Throw(Box::new(builders::num(7.0))),
        );
        let program = compile_program(&builders::program(vec![builders::finish(array_from(
            source,
            Some(mapper),
        ))]));
        let mut state = State::new();
        if matches!(source_kind, "tuple") {
            state
                .insert_global(
                    "tuple",
                    Value::Tuple(vec![Value::Number(1.0); 1_000].into()),
                )
                .expect("seed a tuple source");
        }
        assert!(
            matches!(
                execute_compiled(&program, &mut state, &host).await,
                Err(RuntimeError::UncaughtException {
                    value: Value::Number(7.0)
                })
            ),
            "{source_kind}: the mapper's throw precedes dense-result allocation"
        );
    }
}
