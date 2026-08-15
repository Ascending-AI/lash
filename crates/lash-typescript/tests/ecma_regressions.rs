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
fn sparse_and_non_index_array_writes_reject_by_name() {
    let error = execute("const a = [1]; a[3] = 9; finish(a);")
        .expect_err("sparse arrays are not representable in v1");
    assert!(
        error.to_string().contains("TS_SPARSE_ARRAY_UNSUPPORTED"),
        "{error}"
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
fn join_uses_javascript_string_conversion() {
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
fn agent_stdlib_regressions_match_ecmascript() {
    assert_eq!(
        finished("finish('abc'.charCodeAt(0, 99));"),
        Value::Number(97.0)
    );
    assert_eq!(
        finished("finish('abc'.replace('b', '[$&]'));"),
        Value::String("a[b]c".into())
    );
    assert_eq!(
        finished("finish(Object.keys({b:2,2:'two',a:1,1:'one'}).join(','));"),
        Value::String("1,2,b,a".into())
    );
    assert_eq!(
        finished("finish(JSON.stringify({b:2,2:'two',a:1,1:'one'}));"),
        Value::String(r#"{"1":"one","2":"two","b":2,"a":1}"#.into())
    );
    assert_eq!(
        finished("finish(Number.parseInt('1000000000000000000000000000000000000000'));"),
        Value::Number(1e39)
    );
    assert_eq!(
        finished("finish(Number.parseInt('10', 4294967298));"),
        Value::Number(2.0)
    );
    assert_eq!(
        finished("finish(Number.parseInt('10', 4294967296));"),
        Value::Number(10.0)
    );
    assert_eq!(finished("finish(Number.isNaN());"), Value::Bool(false));
    assert_eq!(finished("finish(Object.is());"), Value::Bool(true));
    let Value::Number(value) = finished("finish(Math.abs());") else {
        panic!("Math.abs() should return NaN");
    };
    assert!(value.is_nan());
    assert_eq!(
        finished("finish('x'.repeat());"),
        Value::String(String::new().into())
    );
    assert_eq!(
        finished("finish(Object.keys([1]).join(','));"),
        Value::String("0".into())
    );
    assert_eq!(
        finished(r#"finish(JSON.stringify(JSON.parse('{"b":1,"a":2}')));"#),
        Value::String(r#"{"b":1,"a":2}"#.into())
    );
    assert_eq!(
        finished("try { null.toString(); } catch (error) { finish(error.code); }"),
        Value::String("ValidationFailed".into())
    );
}

#[test]
fn every_string_growth_path_is_bounded_before_allocation() {
    for source in [
        "try { 'x'.repeat(1e100); } catch (error) { finish(error.code); }",
        "try { let s='x'.repeat(8388608); s=s.concat(s); } catch (error) { finish(error.code); }",
        "try { let s='x'.repeat(8388608); s=s+s; } catch (error) { finish(error.code); }",
        r#"let s="a".repeat(8388607)+"z"; let r="$`".repeat(30000); finish(s.replace("z",r));"#,
    ] {
        assert!(
            matches!(
                execute(source),
                Err(RuntimeError::MemoryLimitExceeded { .. })
            ),
            "{source}"
        );
    }
}

#[test]
fn json_math_and_last_index_edges_match_ecmascript() {
    assert_eq!(
        finished("finish(JSON.stringify(1e20));"),
        Value::String("100000000000000000000".into())
    );
    assert_eq!(
        finished("finish(JSON.stringify(1e-6));"),
        Value::String("0.000001".into())
    );
    let Value::List(values) = finished("finish([Math.pow(1, Infinity), Math.pow(-1, Infinity)]);")
    else {
        panic!("expected Math.pow result list");
    };
    assert!(
        values
            .iter()
            .all(|value| matches!(value, Value::Number(number) if number.is_nan()))
    );
    assert_eq!(
        finished("finish([1,2,1].lastIndexOf(1,undefined));"),
        Value::Number(0.0)
    );
    assert_eq!(
        finished("finish('abcabc'.lastIndexOf('a',NaN));"),
        Value::Number(3.0)
    );
}

#[test]
fn for_of_strings_iterates_unicode_code_points() {
    assert_eq!(
        finished(
            "let result=''; for (const value of 'a😀b') { result=result+'['+value+']'; } finish(result);"
        ),
        Value::String("[a][😀][b]".into())
    );
}

#[test]
fn classic_for_continue_crossing_finally_is_named_rejection() {
    let error = lash_typescript::compile(
        "let x=-1; for(let i=0;i<1;i++){try{continue;}finally{x=i;}} finish(x);",
    )
    .expect_err("unsupported continue/finally shape must reject");
    assert_eq!(error.code, lash_typescript::DiagnosticCode::ForUnsupported);
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

/// An empty search expands per UTF-16 code unit in ECMA, so on an astral
/// receiver node produces lone surrogates between the halves of a surrogate
/// pair. Those are not representable here, and quietly expanding per Unicode
/// scalar instead would be a silent divergence — the dialect's whole claim is
/// that a difference is either absent or named. `split('')` already refuses the
/// same shape for the same reason, so this refuses alongside it.
#[test]
fn empty_search_replace_all_refuses_to_split_a_surrogate_pair() {
    let error = execute("finish('\u{1F600}'.replaceAll('', '-'));")
        .expect_err("an astral receiver cannot expand per code unit");
    assert!(
        error
            .to_string()
            .contains("TS_LONE_SURROGATE_UNSUPPORTED"),
        "the refusal is the named one: {error}"
    );

    // BMP receivers are unaffected, including multi-byte ones.
    assert_eq!(
        finished("finish('café'.replaceAll('', '.'));"),
        Value::String(".c.a.f.é.".into())
    );
    assert_eq!(
        finished("finish('ab'.replaceAll('', '-'));"),
        Value::String("-a-b-".into())
    );
}
