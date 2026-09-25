//! FIG-1305: the agent surface and standard library — sparse and non-index writes, join, stdlib regressions, string growth bounds, lastIndex, for-of strings, finally crossings and surrogate splits.

use super::*;

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
    // Reading a member of `null` is ECMA's TypeError, delivered exactly as
    // Node delivers it: the native class, Node's message, and no `cause`.
    assert_eq!(
        finished(
            "try { null.toString(); } catch (error) { finish([error instanceof TypeError, error.name, error.message, error.cause === undefined]); }"
        ),
        Value::List(
            vec![
                Value::Bool(true),
                Value::String("TypeError".into()),
                Value::String("Cannot read properties of null (reading 'toString')".into()),
                Value::Bool(true),
            ]
            .into()
        )
    );
    // A catchable runtime fault with no ECMA brand of its own is delivered
    // as an ordinary JavaScript error branded `RuntimeError`, with its typed
    // payload on `cause`. `AggregateError` requires an `errors` list: the
    // fault has no ECMA class, so it keeps the substrate's.
    assert_eq!(
        finished(
            "try { new AggregateError(5, 'x'); } catch (error) { finish(error instanceof Error ? error.name : String(error)); }"
        ),
        Value::String("RuntimeError".into())
    );
    assert_eq!(
        finished("try { new AggregateError(5, 'x'); } catch (error) { finish(error.cause.code); }"),
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

    assert_eq!(
        finished("try { Array.from({length: 4294967296}); } catch (error) { finish(error.name); }"),
        Value::String("RangeError".into())
    );
    let program = lash_typescript::testing::compile("finish(Array.from({length: 2000000}));")
        .expect("large array-like source compiles without allocating");
    let environment = ExecutionEnvironment::new(&Host).with_execution_bounds(ExecutionBounds::new(
        ExecutionBound::Unbounded,
        ExecutionBound::Unbounded,
        ExecutionBound::logical_bytes(16 * 1024 * 1024),
    ));
    assert!(matches!(
        futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &environment)),
        Err(RuntimeError::MemoryLimitExceeded { .. })
    ));
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
    let error = lash_typescript::testing::compile(
        "let x=-1; for(let i=0;i<1;i++){try{continue;}finally{x=i;}} finish(x);",
    )
    .expect_err("unsupported continue/finally shape must reject");
    assert_eq!(error.code, lash_typescript::DiagnosticCode::ForUnsupported);
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
        error.to_string().contains("TS_LONE_SURROGATE_UNSUPPORTED"),
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
