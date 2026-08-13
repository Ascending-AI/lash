use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, RuntimeError,
    State, Value,
};

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityResult::Value(Value::Null)),
            _ => Err(ExecutionHostError::new(
                "unsupported ECMA regression ability",
            )),
        }
    }
}

fn execute(source: &str) -> Result<ExecutionOutcome, RuntimeError> {
    let program = lash_typescript::compile(source).expect("TypeScript should compile");
    futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
}

fn finished(source: &str) -> Value {
    match execute(source).expect("TypeScript should execute") {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    }
}

#[test]
fn missing_and_non_index_property_reads_produce_undefined() {
    let cases = [
        "finish(({ a: 1 }).missing);",
        "finish([1, 2][9]);",
        "finish([1, 2][-1]);",
    ];
    for source in cases {
        assert_eq!(finished(source), Value::Undefined, "{source}");
    }
    assert_eq!(
        finished("finish(({ a: 1 }).missing === undefined);"),
        Value::Bool(true)
    );
}

#[test]
fn array_writes_extend_with_undefined_holes_and_never_wrap() {
    assert_eq!(
        finished("const a = [1]; a[3] = 9; finish(a);"),
        Value::List(
            vec![
                Value::Number(1.0),
                Value::Undefined,
                Value::Undefined,
                Value::Number(9.0),
            ]
            .into()
        )
    );

    let error = execute("const a = [1, 2]; a[-1] = 9; finish(a);")
        .expect_err("non-index array properties are not representable in v1");
    assert!(
        error
            .to_string()
            .starts_with("TS_ARRAY_NON_INDEX_PROPERTY_UNSUPPORTED"),
        "{error}"
    );
}

#[test]
fn typeof_uses_ecma_object_kinds_and_allows_unresolvable_references() {
    let cases = [
        ("finish(typeof {});", "object"),
        ("finish(typeof []);", "object"),
        ("finish(typeof (() => 1));", "function"),
        ("finish(typeof someUndeclared);", "undefined"),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), Value::String(expected.into()), "{source}");
    }
}

#[test]
fn loose_equality_recurses_after_boolean_to_number_conversion() {
    let cases = [
        ("finish(null == false);", false),
        ("finish(undefined == false);", false),
        ("finish('0' == false);", true),
        ("finish([] == false);", true),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), Value::Bool(expected), "{source}");
    }
}

#[test]
fn number_to_string_matches_ecma_thresholds_and_shortest_digits() {
    let cases = [
        ("finish(`${1e21}`);", "1e+21"),
        ("finish(`${1e20}`);", "100000000000000000000"),
        ("finish(`${1e-6}`);", "0.000001"),
        ("finish(`${1e-7}`);", "1e-7"),
        ("finish(`${1.5e-10}`);", "1.5e-10"),
        (
            "finish(`${123456789012345678901234}`);",
            "1.2345678901234569e+23",
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), Value::String(expected.into()), "{source}");
    }
}

#[test]
fn string_to_number_accepts_only_the_ecma_string_numeric_grammar() {
    for source in [
        "finish(+'inf');",
        "finish(+'infinity');",
        "finish(+'INF');",
        "finish(+'+0x10');",
    ] {
        let Value::Number(value) = finished(source) else {
            panic!("expected number for {source}");
        };
        assert!(value.is_nan(), "{source}: {value}");
    }
    assert_eq!(
        finished("finish(+'0xFFFFFFFFFFFFFFFFF');"),
        Value::Number(295_147_905_179_352_830_000.0)
    );
}

#[test]
fn split_and_join_use_javascript_string_conversion() {
    assert_eq!(
        finished("finish('abc'.split(''));"),
        Value::List(
            ["a", "b", "c"]
                .into_iter()
                .map(|value| Value::String(value.into()))
                .collect::<Vec<_>>()
                .into()
        )
    );
    assert_eq!(
        finished("finish([1, null, undefined, 2].join(','));"),
        Value::String("1,,,2".into())
    );
    assert_eq!(
        finished("finish([[1, 2], 3].join(','));"),
        Value::String("1,2,3".into())
    );
}

#[test]
fn length_and_standard_number_globals_are_available() {
    let cases = [
        ("finish('abc'.length);", 3.0),
        ("finish('😀'.length);", 2.0),
        ("finish([1, 2, 3].length);", 3.0),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), Value::Number(expected), "{source}");
    }
    let Value::Number(nan) = finished("finish(NaN);") else {
        panic!("NaN global should be numeric");
    };
    assert!(nan.is_nan());
    assert_eq!(finished("finish(Infinity);"), Value::Number(f64::INFINITY));
}

#[test]
fn string_relational_comparison_uses_utf16_code_units() {
    assert_eq!(finished("finish('\\u{10000}' < '｡');"), Value::Bool(true));
}

#[test]
fn lone_surrogate_literals_reject_without_lossy_transcoding() {
    let error = lash_typescript::validate(r#"finish('\uD800');"#)
        .expect_err("lone surrogates are not representable in v1");
    assert_eq!(error.code.as_str(), "TS_LONE_SURROGATE_LITERAL_UNSUPPORTED");
}

#[test]
fn typescript_this_parameter_is_erased_before_runtime_arity() {
    assert_eq!(
        finished("function f(this: number, a: number): number { return a; } finish(f(1));"),
        Value::Number(1.0)
    );
}
