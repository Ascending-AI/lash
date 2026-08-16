use lashlang::{
    AbilityOp, AbilityResult, ExecutionBound, ExecutionBounds, ExecutionEnvironment, ExecutionHost,
    ExecutionHostError, ExecutionOutcome, RuntimeError, State, Value,
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
    match execute(source)
        .unwrap_or_else(|error| panic!("TypeScript should execute: {source}: {error}"))
    {
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
fn widened_array_callback_surface_is_sequential_and_ecma_shaped() {
    let cases = [
        (
            "finish([1,2,3].map((v,i,a)=>v+i+a.length).join(','));",
            "4,6,8",
        ),
        (
            "finish([1,2,3,4].filter((v,i,a)=>v%2===0&&a.length===4).join(','));",
            "2,4",
        ),
        (
            "finish(String([1,2,3].reduce((a,v,i,x)=>a+v+i+x.length,0)));",
            "18",
        ),
        ("finish(String([1,2,3].reduceRight((a,v)=>a-v)));", "0"),
        ("finish(String([1,3,4].find((v,i)=>v+i>4)));", "4"),
        ("finish(String([1,3,4].findIndex((v,i)=>v+i>4)));", "2"),
        ("finish(String([1,3,4].findLast(v=>v<4)));", "3"),
        ("finish(String([1,3,4].findLastIndex(v=>v<4)));", "1"),
        ("finish(String([1,2,3].some(v=>v===2)));", "true"),
        ("finish(String([1,2,3].every(v=>v>0)));", "true"),
        (
            "const s={n:0}; [1,2,3].forEach(()=>s.n++); finish(String(s.n));",
            "3",
        ),
        ("finish([1,2].flatMap(v=>[v,v+10]).join(','));", "1,11,2,12"),
        (
            "const a=[3,1,2]; const b=a.sort((x,y)=>x-y); finish(a.join(',')+'|'+(a===b));",
            "1,2,3|true",
        ),
        (
            "const a=[3,1,2]; const b=a.toSorted((x,y)=>y-x); finish(a.join(',')+'|'+b.join(','));",
            "3,1,2|3,2,1",
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), Value::String(expected.into()), "{source}");
    }

    assert_eq!(
        finished(
            "const s={order:''}; function mark(x){s.order+=x;return x;} [1].reduce((a,v)=>a+v,mark('i'),mark('e')); finish(s.order);"
        ),
        Value::String("ie".into()),
        "reduce evaluates initialValue before ignored excess arguments"
    );
    assert_eq!(
        finished(
            "const s={seen:false}; function mark(){s.seen=true;return {};}; const a=Array.from([1,2],v=>v+1,mark()); finish(a.join(',')+'|'+s.seen);"
        ),
        Value::String("2,3|true".into())
    );
}

#[test]
fn widened_non_callback_stdlib_matches_dense_ecma_surface() {
    let cases = [
        (
            "const a=[1,2,3]; const b=a.reverse(); finish(a.join(',')+'|'+(a===b));",
            "3,2,1|true",
        ),
        (
            "const a=[1,2,3,4]; const r=a.splice(-3,2,'x','y'); finish(a.join(',')+'|'+r.join(','));",
            "1,x,y,4|2,3",
        ),
        (
            "const a=[1,2,3]; a.fill('x',-2); finish(a.join(','));",
            "1,x,x",
        ),
        ("finish([1,[2,[3]]].flat(Infinity).join(','));", "1,2,3"),
        (
            "const a=[3,1,2]; const b=a.toReversed(); const c=a.toSpliced(1,1,9); const d=a.with(-1,8); finish(a.join(',')+'|'+b.join(',')+'|'+c.join(',')+'|'+d.join(','));",
            "3,1,2|2,1,3|3,9,2|3,1,8",
        ),
        (
            "const a=[10,2,1]; const b=a.sort(); finish(a.join(',')+'|'+(a===b));",
            "1,10,2|true",
        ),
        (
            "finish(Array.from({0:'a',2:'c',length:3},(v,i)=>String(v)+i).join(','));",
            "a0,undefined1,c2",
        ),
        (
            "finish(String.fromCharCode(65,66)+String.fromCodePoint(0x1f600));",
            "AB😀",
        ),
        (
            "finish('abc'.replace('b',(match,index,input)=>match.toUpperCase()+index+input.length));",
            "aB13c",
        ),
        (
            "finish(String(Number.EPSILON)+'|'+String(Number.MIN_SAFE_INTEGER)+'|'+String(Math.PI));",
            "2.220446049250313e-16|-9007199254740991|3.141592653589793",
        ),
        (
            "finish([Math.atan2(1,1),Math.clz32(1),Math.imul(0xffffffff,5),Math.hypot(3,4)].join(','));",
            "0.7853981633974483,31,-5,5",
        ),
        (
            "finish((1.25).toFixed(1)+'|'+(123).toExponential(1)+'|'+(123).toPrecision(2));",
            "1.3|1.2e+2|1.2e+2",
        ),
        (
            "const a={x:1}; const b=Object.assign(a,{y:2}); finish(JSON.stringify(a)+'|'+(a===b)+'|'+a.hasOwnProperty('y'));",
            "{\"x\":1,\"y\":2}|true|true",
        ),
        (
            "const a=new Set([1,2]); const b=new Set([2,3]); const u=a.union(b); const i=a.intersection(b); finish([...u].join(',')+'|'+[...i].join(',')+'|'+a.isDisjointFrom(new Set([9])));",
            "1,2,3|2|true",
        ),
        (
            "finish(JSON.stringify(Object.groupBy([1,2,3,4],v=>v%2)));",
            "{\"0\":[2,4],\"1\":[1,3]}",
        ),
        (
            "const g=Map.groupBy([1,2,3],v=>v%2); finish(Array.from(g).map(([k,v])=>k+':'+v.join(',')).join('|'));",
            "1:1,3|0:2",
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), Value::String(expected.into()), "{source}");
    }
    assert_eq!(
        finished(
            "const a=[1,2,3]; const b=a; a.length=0; finish(a.length+'|'+b.length+'|'+a.join(','));"
        ),
        Value::String("0|0|".into())
    );
}

#[test]
fn json_stringify_options_callbacks_cycles_and_to_json_are_exact() {
    assert_eq!(
        finished("finish(JSON.stringify({a:1,b:2,c:3},['c','a'],2));"),
        Value::String("{\n  \"c\": 3,\n  \"a\": 1\n}".into())
    );
    assert_eq!(
        finished(
            "finish(JSON.stringify({a:1,b:2},(k,v)=>k==='b'?undefined:typeof v==='number'?v+10:v));"
        ),
        Value::String("{\"a\":11}".into())
    );
    assert_eq!(
        finished(
            "const thisValue=7; const x={a:1,toJSON(k){return {key:k,value:thisValue};}}; finish(JSON.stringify(x,(k,v)=>v));"
        ),
        Value::String("{\"key\":\"\",\"value\":7}".into())
    );
    assert_eq!(
        finished(
            "finish((()=>{try{const a={};a.self=a;JSON.stringify(a);}catch(error){return error.name+': '+error.message;}})());"
        ),
        Value::String(
            "TypeError: Converting circular structure to JSON\n    --> starting at object with constructor 'Object'\n    --- property 'self' closes the circle".into()
        )
    );
    assert_eq!(
        finished("const x={toJSON(k){return {key:k,ok:true};}}; finish(JSON.stringify(x));"),
        Value::String("{\"key\":\"\",\"ok\":true}".into())
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
    let program = lash_typescript::compile("finish(Array.from({length: 2000000}));")
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
    for source in [
        r#"finish('\uD800');"#,
        r#"function encodeURI(value) { return 'shadowed'; } finish(encodeURI('\uD800'));"#,
    ] {
        let error = lash_typescript::validate(source)
            .expect_err("lone surrogates are not representable in ordinary guest values");
        assert_eq!(error.code.as_str(), "TS_LONE_SURROGATE_LITERAL_UNSUPPORTED");
    }
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

/// A billion-element array is refused by the budget, not by the OOM killer.
///
/// `Host` above is a plain `ExecutionHost` that never mentions bounds — which
/// is the shape of every host that has not thought about memory, and the shape
/// this test exists to protect. The default `execution_bounds()` used to report
/// memory as `Unbounded`, which set the heap's limit to `u64::MAX` and made the
/// array pre-charge arithmetically unable to trip: `Array.from({ length: 1e9 })`
/// then walked the process into tens of gigabytes of resident memory. The
/// default now carries `DEFAULT_HOST_MEMORY_LIMIT_BYTES`, so the pre-charge
/// answers before a single element is built.
#[test]
fn a_billion_element_array_on_a_default_host_is_a_clean_memory_refusal() {
    for source in [
        "finish(Array.from({ length: 1e9 }));",
        "finish(Array.from({ length: 1e9 }, (_, index: number) => index));",
    ] {
        let error = execute(source).expect_err("an over-budget array must refuse");
        assert!(
            matches!(error, RuntimeError::MemoryLimitExceeded { .. }),
            "{source}: {error}"
        );
    }
}

/// The bound is a ceiling, not a ban: ordinary array construction is untouched.
#[test]
fn ordinary_array_construction_is_unaffected_by_the_default_memory_ceiling() {
    assert_eq!(
        finished("finish(Array.from({ length: 1000 }).length);"),
        Value::Number(1000.0)
    );
}

/// A member read on `undefined` names the undefined value, not the method.
///
/// `globalThis.missing` is `undefined`, so `globalThis.missing.get(k)` is an
/// ECMA `TypeError` about the receiver. Reporting it as
/// `TS_METHOD_UNSUPPORTED: method \`get\` is unavailable on this value` pointed
/// the reader at a missing builtin — the one thing that is not wrong here —
/// and said nothing about which value was undefined.
#[test]
fn a_member_call_on_undefined_names_the_undefined_receiver() {
    for source in [
        "finish(globalThis.missing.get('k'));",
        "const holder: any = undefined; finish(holder.get('k'));",
        "finish((({ a: 1 }) as any).missing.get('k'));",
    ] {
        let error = execute(source).expect_err("a member read on undefined must refuse");
        let rendered = error.to_string();
        assert!(
            rendered.contains("Cannot read properties of undefined (reading `get`)"),
            "{source}: {rendered}"
        );
        assert!(
            !rendered.contains("TS_METHOD_UNSUPPORTED"),
            "the diagnostic must not blame the method: {source}: {rendered}"
        );
    }

    let error = execute("const holder: any = null; finish(holder.get('k'));")
        .expect_err("a member read on null must refuse");
    assert!(
        error
            .to_string()
            .contains("Cannot read properties of null (reading `get`)"),
        "{error}"
    );

    // A method that really is unsupported, on a receiver that really exists,
    // still says so.
    let error = execute("const holder: any = { a: 1 }; finish(holder.get('k'));")
        .expect_err("an unsupported method on a record must refuse");
    assert!(
        error.to_string().contains("TS_METHOD_UNSUPPORTED"),
        "{error}"
    );
}

/// Past the ECMA array limit the answer is node's `RangeError`, not a clamp.
///
/// `Array.from` used to build its array in the pure stdlib function, which has
/// no heap to charge and so did the only thing it could: clamp `length` to
/// `u32::MAX` and `collect()`. That is two failures in one — the guest gets an
/// array of a length it never asked for, and the pre-charge in the VM was the
/// only thing standing between a guest constant and a raw four-billion-element
/// allocation. Both array-like branches now build through the charged path, so
/// the limit is reported rather than silently applied.
#[test]
fn an_array_like_length_past_the_ecma_limit_is_a_catchable_range_error() {
    for source in [
        "try { Array.from({ length: 2 ** 32 }); } catch (error: any) { finish(error.name + ': ' + error.message); } finish('not thrown');",
        "try { Array.from({ length: 1e12 }, (_, index: number) => index); } catch (error: any) { finish(error.name + ': ' + error.message); } finish('not thrown');",
        "const source: any = { length: 2 ** 40 }; try { Array.from(source); } catch (error: any) { finish(error.name + ': ' + error.message); } finish('not thrown');",
    ] {
        assert_eq!(
            finished(source),
            Value::String("RangeError: Invalid array length".into()),
            "{source}"
        );
    }
}

/// Lengths under the array limit keep their node-exact answers.
#[test]
fn array_like_lengths_under_the_limit_are_node_exact() {
    for (source, expected) in [
        ("finish(Array.from({ length: -1 }).length);", 0.0),
        ("finish(Array.from({ length: 2.7 }).length);", 2.0),
        ("finish(Array.from({ length: NaN }).length);", 0.0),
        ("finish(Array.from({}).length);", 0.0),
        ("finish(Array.from({ length: 2, 0: 'a' }).length);", 2.0),
    ] {
        assert_eq!(finished(source), Value::Number(expected), "{source}");
    }
    assert_eq!(
        finished("finish(Array.from({ length: 2, 0: 'a' })[1] === undefined);"),
        Value::Bool(true)
    );
}

/// Deleting a property keeps the survivors in their order.
///
/// Records stored their properties in a vector and removed with `swap_remove`,
/// which backfills the vacated slot from the end. Property order is observable
/// in ECMA — `Object.keys`, `JSON.stringify`, spread — so that rotated the last
/// key to the front. `{ a, ...rest }` lowers to copy-then-delete, which made
/// every object rest over three or more surviving keys come out scrambled.
#[test]
fn property_removal_preserves_the_surviving_order() {
    for (source, expected) in [
        (
            "const o = { a: 1, b: 2, c: 3, d: 4 }; const { a, ...rest } = o; finish(JSON.stringify(rest));",
            r#"{"b":2,"c":3,"d":4}"#,
        ),
        (
            "const o = { a: 1, b: 2, c: 3, d: 4, e: 5 }; const { a, ...rest } = o; finish(JSON.stringify(rest));",
            r#"{"b":2,"c":3,"d":4,"e":5}"#,
        ),
        (
            "const o = { '2': 1, a: 2, '1': 3, b: 4 }; const { a, ...rest } = o; finish(JSON.stringify(rest));",
            r#"{"1":3,"2":1,"b":4}"#,
        ),
        (
            "const o = { a: 1, b: 2, c: 3 }; const { b, ...rest } = o; finish(JSON.stringify(rest));",
            r#"{"a":1,"c":3}"#,
        ),
        // Past the record's index threshold, where removal must also keep the
        // symbol index in step with the shifted slots.
        (
            "const o = { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6, g: 7, h: 8, i: 9, j: 10 }; const { a, c, ...rest } = o; finish(JSON.stringify(rest));",
            r#"{"b":2,"d":4,"e":5,"f":6,"g":7,"h":8,"i":9,"j":10}"#,
        ),
    ] {
        assert_eq!(finished(source), Value::String(expected.into()), "{source}");
    }

    // `delete` reaches the same removal directly.
    assert_eq!(
        finished(
            "const o: any = { a: 1, b: 2, c: 3 }; delete o.a; finish(Object.keys(o).join(','));"
        ),
        Value::String("b,c".into())
    );
    assert_eq!(
        finished(
            "const o: any = { a: 1, b: 2, c: 3, d: 4 }; delete o.b; finish(JSON.stringify(o));"
        ),
        Value::String(r#"{"a":1,"c":3,"d":4}"#.into())
    );
}

/// A computed key that only turns out to be `__proto__` at the access refuses
/// by name rather than diverging silently.
///
/// The static forms are rejected by the adapter (see `rejections.rs`). Here the
/// name is not knowable until the access, and the value model has no prototype
/// chain: node would answer the read with `Object.prototype` and let the write
/// change what the object inherits, while a dense record can only answer
/// `undefined` and store a data key nothing reads through. Both are silent
/// divergences, so both refuse.
#[test]
fn a_computed_prototype_chain_key_refuses_by_name() {
    for source in [
        "const o: any = {}; const key = '__pro' + 'to__'; o[key] = { x: 1 }; finish(1);",
        "const o: any = { a: 1 }; const key = '__pro' + 'to__'; finish(o[key]);",
        "const key = '__pro' + 'to__'; const o: any = { [key]: 1 }; finish(1);",
        "const o: any = {}; const key = '__define' + 'Getter__'; finish(o[key]);",
    ] {
        let error = execute(source).expect_err("a computed prototype-chain key must refuse");
        assert!(
            error
                .to_string()
                .contains("TS_PROTOTYPE_MUTATION_UNSUPPORTED"),
            "{source}: {error}"
        );
    }

    // A name that merely looks similar is an ordinary data key.
    assert_eq!(
        finished("const o: any = { prototypeish: 1 }; finish(o.prototypeish);"),
        Value::Number(1.0)
    );
}

/// The four end-of-array mutators, with their ECMA return values and the
/// composition that made their absence a wall.
///
/// Without `push`, the ordinary accumulate-in-a-callback shape —
/// `const out = []; xs.forEach(v => { out.push(v); })` — was a compile-time
/// rejection, which is the single most common thing a model writes. They mutate
/// the live receiver through the same path `splice` uses, so aliases see the
/// change and the heap budget is charged for the growth.
#[test]
fn array_end_mutators_are_node_exact_and_mutate_the_live_receiver() {
    for (source, expected) in [
        ("const xs = [1, 2]; finish(xs.push(3));", 3.0),
        ("const xs: number[] = []; finish(xs.push());", 0.0),
        ("const xs = [1, 2]; finish(xs.pop());", 2.0),
        ("const xs = [1, 2]; finish(xs.shift());", 1.0),
        ("const xs = [1, 2]; finish(xs.unshift(0));", 3.0),
        ("const xs = [1, 2]; xs.pop(); finish(xs.length);", 1.0),
        // An alias sees the mutation: the receiver is the live heap array.
        (
            "const xs = [1, 2]; const ys = xs; ys.push(3); finish(xs.length);",
            3.0,
        ),
    ] {
        assert_eq!(finished(source), Value::Number(expected), "{source}");
    }

    for source in [
        "const xs: number[] = []; finish(xs.pop() === undefined);",
        "const xs: number[] = []; finish(xs.shift() === undefined);",
        "const xs: number[] = []; xs.pop(); finish(xs.length === 0);",
    ] {
        assert_eq!(finished(source), Value::Bool(true), "{source}");
    }

    for (source, expected) in [
        (
            "const xs = [1, 2]; xs.push(3, 4); finish(xs.join(','));",
            "1,2,3,4",
        ),
        (
            "const xs = [1, 2]; xs.unshift(-1, 0); finish(xs.join(','));",
            "-1,0,1,2",
        ),
        // The rejection wall this closes.
        (
            "const out: number[] = []; [1, 2, 3].forEach((v: number) => { out.push(v * 2); }); finish(out.join(','));",
            "2,4,6",
        ),
        (
            "const xs = [1, 2, 3]; const out: number[] = []; while (xs.length > 0) { out.push(xs.shift()); } finish(out.join(','));",
            "1,2,3",
        ),
    ] {
        assert_eq!(finished(source), Value::String(expected.into()), "{source}");
    }

    // The receiver must be an array; the methods are not a general surface.
    lash_typescript::compile("finish('ab'.push('c'));")
        .expect_err("a string receiver has no `push`");
    execute("const m = new Map(); finish(m.push(1));").expect_err("a Map receiver has no `push`");
}

/// Growth through `push` is charged against the heap budget like any other
/// allocation, so a loop that pushes without end refuses instead of consuming
/// the process. The bound here is small so the refusal arrives early; on a
/// default host the same loop meets `DEFAULT_HOST_MEMORY_LIMIT_BYTES`.
#[test]
fn pushing_past_the_memory_budget_is_a_clean_refusal() {
    let program = lash_typescript::compile(
        "const xs: string[] = []; const chunk = 'x'.repeat(65536); for (let i = 0; i < 10000; i++) { xs.push(chunk); } finish(xs.length);",
    )
    .expect("an unbounded push loop compiles");
    let environment = ExecutionEnvironment::new(&Host).with_execution_bounds(ExecutionBounds::new(
        ExecutionBound::Unbounded,
        ExecutionBound::Unbounded,
        ExecutionBound::logical_bytes(4 * 1024 * 1024),
    ));
    assert!(
        matches!(
            futures::executor::block_on(lashlang::execute(
                &program,
                &mut State::new(),
                &environment
            )),
            Err(RuntimeError::MemoryLimitExceeded { .. })
        ),
        "an unbounded push loop must refuse against the budget"
    );
}
