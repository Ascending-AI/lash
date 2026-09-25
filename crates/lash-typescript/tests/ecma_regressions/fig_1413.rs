//! FIG-1413: the broad dialect on the heap substrate — callback and non-callback stdlib, JSON.stringify, memory ceilings, member calls, array mutators, matchAll, prototype-chain keys and iterable sinks.

use super::*;

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
        (
            "const a=[0,0]; const b=[0,0]; finish(a.fill(1,0,undefined).join(',')+'|'+b.fill(1,0,null).join(','));",
            "1,1|0,0",
        ),
        (
            "const a=[1,2,3,4,5]; const r=a.copyWithin(-2); finish(a.join(',')+'|'+(a===r));",
            "1,2,3,1,2|true",
        ),
        (
            "const b=[1,2,3,4,5].copyWithin(0,3); const c=[1,2,3,4,5].copyWithin(0,3,4); const d=[1,2,3,4,5].copyWithin(-2,-3,-1); finish(b.join(',')+'|'+c.join(',')+'|'+d.join(','));",
            "4,5,3,4,5|4,2,3,4,5|1,2,3,3,4",
        ),
        (
            "const e=[1,2,3,4,5].copyWithin(1,0,3); const f=[1,2,3].copyWithin(0,1); finish(e.join(',')+'|'+f.join(','));",
            "1,1,2,3,5|2,3,3",
        ),
        (
            "const t={raw:['a','b','c']}; finish(String.raw(t,1,2)+'|'+String.raw(t)+'|'+String.raw({raw:{length:0}})+'|'+String.raw({raw:{length:undefined}}));",
            "a1b2c|abc||",
        ),
        (
            "const r={length:5,0:'e',1:'',2:null,3:undefined,4:123,5:'past'}; finish(String.raw({raw:r}));",
            "enullundefined123",
        ),
        (
            "try { String.raw(null); finish('no'); } catch(e) { finish(e.name); }",
            "TypeError",
        ),
        (
            "try { String.raw({raw:undefined}); finish('no'); } catch(e) { finish(e.name); }",
            "TypeError",
        ),
        (
            "const calls=[]; JSON.parse('{\"p1\":0,\"p2\":0,\"p1\":0,\"2\":0,\"1\":0}',(k,v)=>{calls.push(k);return v;}); finish(calls.join(','));",
            "1,2,p1,p2,",
        ),
        (
            "const o=JSON.parse('{\"a\":1,\"b\":2}',(k,v)=>k==='b'?undefined:v); const l=JSON.parse('[1,2,3]',(k,v)=>k==='1'?undefined:v); finish(Object.keys(o).join(',')+'|'+l.length+'|'+l[1]+'|'+JSON.parse('{\"n\":5}',(k,v)=>typeof v==='number'?v*2:v).n+'|'+JSON.parse('4',7));",
            "a|3|undefined|10|4",
        ),
        (
            "const d=new Date('2014-03-27T00:00:00Z'); const e=new Date('0020-01-01T00:00:00Z'); const n=new Date(NaN); finish(d.toUTCString()+'|'+d.toString()+'|'+e.toUTCString()+'|'+n.toUTCString()+'|'+n.toString()+'|'+String(d));",
            "Thu, 27 Mar 2014 00:00:00 GMT|Thu Mar 27 2014 00:00:00 GMT+0000 (Coordinated Universal Time)|Wed, 01 Jan 0020 00:00:00 GMT|Invalid Date|Invalid Date|Thu Mar 27 2014 00:00:00 GMT+0000 (Coordinated Universal Time)",
        ),
        (
            "const g=new Date('-000123-07-01T00:00Z'); finish(g.toUTCString()+'|'+g.toString());",
            "Sun, 01 Jul -0123 00:00:00 GMT|Sun Jul 01 -0123 00:00:00 GMT+0000 (Coordinated Universal Time)",
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
            "finish(String(Number.MIN_VALUE)+'|'+String(Number.POSITIVE_INFINITY)+'|'+String(Number.NEGATIVE_INFINITY));",
            "5e-324|Infinity|-Infinity",
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
            "const s=new Set([1,2]); const m=new Map([[2,'x'],[3,'y']]); finish([...s.union(m)].join(',')+'|'+[...s.intersection(m)].join(',')+'|'+[...s.difference(m)].join(',')+'|'+[...s.symmetricDifference(m)].join(','));",
            "1,2,3|2|1|1,3",
        ),
        (
            "const m=new Map([[2,'a']]); finish(new Set([2]).isSubsetOf(m)+'|'+new Set([2,5]).isSupersetOf(m)+'|'+new Set([1]).isDisjointFrom(m));",
            "true|true|true",
        ),
        (
            "finish([...new Set([3,2,1,0]).intersection(new Set([1,3,5]))].join(',')+'|'+[...new Set([1,2,3]).difference(new Set([7,6,3,2]))].join(','));",
            "1,3|1",
        ),
        (
            "try { new Set([1]).union([3]); finish('no'); } catch(e) { finish(e.name); }",
            "TypeError",
        ),
        (
            "const o={size:2,has:()=>true,keys:undefined}; try { new Set([1]).union(o); finish('no'); } catch(e) { finish(e.name); }",
            "TypeError",
        ),
        (
            "const o={size:2,has:undefined,keys:()=>[]}; try { new Set([1]).difference(o); finish('no'); } catch(e) { finish(e.name); }",
            "TypeError",
        ),
        (
            "const o={size:'x',has:()=>true,keys:()=>[]}; try { new Set([1]).isSubsetOf(o); finish('no'); } catch(e) { finish(e.name); }",
            "TypeError",
        ),
        (
            "try { new Set([1]).isDisjointFrom(null); finish('no'); } catch(e) { finish(e.name); }",
            "TypeError",
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
            rendered.contains("Cannot read properties of undefined (reading 'get')"),
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
            .contains("Cannot read properties of null (reading 'get')"),
        "{error}"
    );

    // A plain object has no `get`: calling a member it lacks is ECMA-262's
    // TypeError, naming the method (FIG-3700).
    assert_eq!(
        finished(
            "const holder: any = { a: 1 }; let r: any = 'no'; try { holder.get('k'); } catch (e) { r = [e instanceof TypeError, e.message]; } finish(r);"
        ),
        Value::List(
            vec![
                Value::Bool(true),
                Value::String("get is not a function".into())
            ]
            .into()
        )
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
/// Property order is observable in ECMA — `Object.keys`, `JSON.stringify`, spread — so that
/// rotated the last key to the front.
/// `{ a, ...rest }` lowers to copy-then-delete, which made every object rest over three or
/// more surviving keys come out scrambled.
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
    lash_typescript::testing::compile("finish('ab'.push('c'));")
        .expect_err("a string receiver has no `push`");
    execute("const m = new Map(); finish(m.push(1));").expect_err("a Map receiver has no `push`");
}

/// Growth through `push` is charged against the heap budget like any other
/// allocation, so a loop that pushes without end refuses instead of consuming
/// the process. The bound here is small so the refusal arrives early; on a
/// default host the same loop meets `DEFAULT_HOST_MEMORY_LIMIT_BYTES`.
#[test]
fn pushing_past_the_memory_budget_is_a_clean_refusal() {
    let program = lash_typescript::testing::compile(
        "const xs: string[] = []; const chunk = 'x'.repeat(65536); for (let i = 0; i < 10000; i++) { xs.push(chunk); } finish(xs.length);",
    )
    .expect("an unbounded push loop compiles");
    let environment = ExecutionEnvironment::new(&Host).with_execution_bounds(ExecutionBounds::new(
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

/// A cycle is ECMA-shaped where ECMA has an opinion, and named where the
/// runtime does.
///
/// `JSON.stringify` on a circular structure throws Node's catchable
/// `TypeError`, in the guest, with Node's message — this row is oracle-exempt
/// because the message is pinned here rather than regenerated: Node's text
/// names the constructor and the closing property, which is host detail no
/// oracle row should carry.
///
/// Where the runtime does have an opinion is at the cell boundary. Durable
/// state is a value *tree*, so a durable binding still holding a cycle when the
/// cell ends cannot be written down. That refusal used to be a bare internal
/// error naming an object id; it now says what the constraint is and what to do
/// about it.
#[test]
fn a_cycle_is_a_guest_type_error_in_json_and_a_named_refusal_at_the_boundary() {
    let stringified = finished(
        "function build(): string { const node: any = {}; node.self = node; try { return JSON.stringify(node); } catch (error: any) { return error.name + '|' + error.message; } } finish(build());",
    );
    let Value::String(rendered) = &stringified else {
        panic!("expected a string, got {stringified:?}");
    };
    assert!(
        rendered.starts_with("TypeError|Converting circular structure to JSON"),
        "the guest must catch Node's circular-structure TypeError: {rendered}"
    );

    let error = execute("const node: any = {}; node.self = node; finish(1);")
        .expect_err("a durable cycle cannot be persisted");
    let rendered = error.to_string();
    assert!(
        rendered.contains("contains a cycle") && rendered.contains("value trees"),
        "the boundary refusal must state the constraint: {rendered}"
    );
}

/// `matchAll` is accepted in all five iterable sinks, not three.
///
/// The collection iterators took `for...of`, spread, `Array.from`,
/// `new Map`/`Set`, and `Object.fromEntries`; `matchAll` took the first three.
/// Every position on that list is a bounded materialization — the whole
/// property the restriction exists to guarantee — so the asymmetry had nothing
/// behind it, and the two extra sinks are exactly where a match-pair iterator
/// is most useful.
#[test]
fn match_all_is_accepted_in_every_iterable_sink() {
    assert_eq!(
        finished("finish(new Map('a1b2'.matchAll(/([a-z])(\\d)/g)).size);"),
        Value::Number(2.0)
    );
    assert_eq!(
        finished("finish(new Set('a1b2'.matchAll(/[a-z]/g)).size);"),
        Value::Number(2.0)
    );
    assert_eq!(
        finished("finish(JSON.stringify(Object.fromEntries('a1b2'.matchAll(/([a-z])(\\d)/g))));"),
        Value::String(r#"{"a1":"a","b2":"b"}"#.into())
    );

    // A retained iterator is still refused, and the repair now names all five.
    let error = lash_typescript::validate("const it = 'a'.matchAll(/a/g);")
        .expect_err("a retained matchAll iterator must refuse");
    let rendered = error.to_string();
    assert!(rendered.contains("new Map|Set"), "{rendered}");
    assert!(rendered.contains("Object.fromEntries"), "{rendered}");
}

/// A prototype-chain name arriving as a data key refuses where the value
/// enters, not later.
///
/// The value model has no prototype chain, so every read of such a key already
/// refused. A record could still be *built* with one from outside the guest,
/// though: `JSON.parse('{"__proto__":1}')` succeeded, `Object.keys` listed the
/// key, and `JSON.stringify` then failed — an enumerable key nothing could
/// read and nothing could serialize, reachable from ordinary untrusted-JSON
/// round-tripping. The over-rejection is uniform now: the key is refused at
/// entry, so no value ever carries one.
#[test]
fn a_prototype_chain_key_refuses_at_json_parse() {
    for source in [
        r#"finish(JSON.parse('{"__proto__":1}'));"#,
        r#"finish(Object.keys(JSON.parse('{"a":1,"__proto__":{"x":1}}')).length);"#,
        r#"finish(JSON.parse('[{"__proto__":null}]'));"#,
        r#"finish(JSON.parse('{"a":{"b":{"__defineGetter__":1}}}'));"#,
    ] {
        let error = execute(source).expect_err("a parsed prototype-chain key must refuse");
        assert!(
            error
                .to_string()
                .contains("TS_PROTOTYPE_MUTATION_UNSUPPORTED"),
            "{source}: {error}"
        );
    }

    // Everything else parses, including a name that merely looks similar.
    assert_eq!(
        finished(r#"finish(JSON.stringify(JSON.parse('{"__proto":1,"a":[1,2]}')));"#),
        Value::String(r#"{"__proto":1,"a":[1,2]}"#.into())
    );
}

struct ProtoToolHost;

impl ExecutionHost for ProtoToolHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(_) => {
                let mut record = lashlang::Record::new();
                record.insert("__proto__".to_string(), Value::Number(1.0));
                Ok(AbilityResult::Value(Value::Record(std::sync::Arc::new(
                    record,
                ))))
            }
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unsupported proto-tool ability")),
        }
    }
}

/// The same refusal on the other value-entry path: a value handed back by the
/// host — a tool result, an awaited process result — is external data by
/// another route, and a record built from it is the same unreadable key.
#[test]
fn a_prototype_chain_key_refuses_when_a_tool_result_carries_it() {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    catalog
        .add_module_operation_contract(
            ["web"],
            "Web",
            "fetch",
            "tool:web/fetch",
            &lashlang::OperationContract::new(serde_json::json!({}), serde_json::json!({})),
        )
        .expect("web binding");
    let environment =
        lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::default());
    let linked = lash_typescript::link(r#"finish(await web.fetch({ url: "u" }));"#, &environment)
        .expect("TypeScript should link");
    let error = futures::executor::block_on(lashlang::execute(
        &lashlang::testing::harness::compile_linked_main(&linked),
        &mut State::new(),
        &ProtoToolHost,
    ))
    .expect_err("a tool result carrying a prototype-chain key must refuse");
    assert!(
        error
            .to_string()
            .contains("TS_PROTOTYPE_MUTATION_UNSUPPORTED"),
        "{error}"
    );
}
