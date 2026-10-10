//! Tier selection: an operation whose operand types are known is a direct
//! kernel call, and any other is the helper that carries JavaScript's
//! meaning.

use std::collections::BTreeMap;

use lash_kernel_doc::{EffectName, Signature, Type, print_document};

use super::{lower_against, lower_in_session, main_text};

/// The statements of `source`'s lowered `main` that compute something: a
/// direct kernel call or a helper call, without the binding's name. A
/// function's calling convention (its closure, its token and its positional
/// arguments) is not one.
fn operations(source: &str) -> Vec<String> {
    main_text(source)
        .lines()
        .filter_map(|line| line.trim().split_once(" = "))
        .map(|(_, value)| value.to_string())
        .filter(|value| value.contains('('))
        .filter(|value| {
            !value.starts_with("fn(")
                && !value.starts_with("(\"ts.function\"")
                && !value.starts_with("invoke ts.pad(")
        })
        .collect()
}

/// Declared operand types choose the kernel function of that type. A
/// declared number may be an integer, so it is made a float first, as
/// JavaScript's numbers are; a literal and an operator's result already
/// are.
#[test]
fn declared_types_choose_the_kernel_function() {
    for (body, expected) in [
        ("a + b", "num.add(num.to_float(a), num.to_float(b))"),
        ("a - 1", "num.sub(num.to_float(a), 1.0)"),
        ("a * b / 2", "num.mul(num.to_float(a), num.to_float(b))"),
        ("a % b", "num.rem_trunc(num.to_float(a), num.to_float(b))"),
        ("a < b", "num.lt(num.to_float(a), num.to_float(b))"),
        ("a >= b", "num.le(num.to_float(b), num.to_float(a))"),
        ("a === b", "eq(num.to_float(a), num.to_float(b))"),
        ("a !== b", "bool.not(eq(num.to_float(a), num.to_float(b)))"),
        ("-a", "num.neg(num.to_float(a))"),
        ("s + s", "text.concat(s, s)"),
        ("!ok", "bool.not(ok)"),
    ] {
        let source = format!(
            "function f(a: number, b: number, s: string, ok: boolean, xs: number[], \
             o: {{ n: number }}) {{ return {body}; }}"
        );
        let operations = operations(&source);
        assert_eq!(
            operations.first().map(String::as_str),
            Some(expected),
            "{body}: {operations:?}"
        );
    }
    // The second operation of a chain takes the first one's result as the
    // float it is.
    let chain = operations("function f(a: number, b: number) { return a * b / 2; }");
    assert!(chain[1].starts_with("num.div("), "{chain:?}");
    assert!(!chain[1].contains("num.to_float("), "{chain:?}");
    // A property read keeps JavaScript's meaning, in place where the
    // value is a plain record; what it gives is believed.
    let property = operations("function f(o: { n: number }) { return o.n + 1; }");
    assert_eq!(property[0], "invoke ts.read(o, \"n\")");
    assert!(
        property
            .last()
            .unwrap()
            .starts_with("num.add(num.to_float("),
        "{property:?}"
    );
}

/// Where a type is not known the operation is the helper: an unannotated
/// parameter, `any`, a union that nothing has narrowed, an operator with
/// no kernel function of its own, and a value asserted `as any`.
#[test]
fn unknown_types_keep_the_helper() {
    for (params, body, expected) in [
        ("a, b", "a + b", "invoke ts.add(a, b)"),
        ("a: any, b: number", "a + b", "invoke ts.add(a, b)"),
        (
            "a: number | string, b: number",
            "a + b",
            "invoke ts.add(a, b)",
        ),
        ("a: number, b: string", "a + b", "invoke ts.add(a, b)"),
        ("a: number, b: number", "a ** b", "invoke ts.pow(a, b)"),
        ("a: string, b: string", "a < b", "invoke ts.lt(a, b)"),
        (
            "a: string, b: string",
            "a === b",
            "invoke ts.strict_equals(a, b)",
        ),
        (
            "a: number, b: number",
            "(a as any) + b",
            "invoke ts.add(a, b)",
        ),
        ("a: Missing, b: number", "a + b", "invoke ts.add(a, b)"),
    ] {
        let operations = operations(&format!("function f({params}) {{ return {body}; }}"));
        assert_eq!(operations, [expected], "({params}) => {body}");
    }
    // A type parameter is whatever the caller passes, even when the source
    // declares a type of the same name.
    assert_eq!(
        operations("type T = number; function f<T>(a: T, b: T) { return a + b; }"),
        ["invoke ts.add(a, b)"]
    );
}

/// Inference without an annotation is proof. A `const`, and a `let` whose
/// every assignment gives one type, are that type. A `let` assigned two
/// types, a `var` (which is `undefined` before its declaration runs), two
/// bindings of one name that disagree and a name an earlier cell left are
/// not known.
#[test]
fn an_unannotated_binding_is_typed_only_by_proof() {
    assert_eq!(
        operations("const n = 2; let i = 0; i = i + n; i++;"),
        ["num.add(i, n)", "num.add(t2, 1.0)"]
    );
    assert_eq!(
        operations("let i = 0; i = 'a'; const r = i + 1;"),
        ["invoke ts.add(i, 1.0)"]
    );
    assert_eq!(
        operations("var i = 0; const r = i + 1;"),
        ["invoke ts.add(i, 1.0)"]
    );
    assert_eq!(
        operations(
            "{ const value = 1; const r = value + 1; } { const value = 'a'; const s = value + 1; }"
        ),
        ["invoke ts.add(value_1, 1.0)", "invoke ts.add(value_2, 1.0)"]
    );
    let lowered =
        lower_in_session("{ const total = 1; } const r = total + 1;", &["total"]).unwrap();
    let text = print_document(&lowered.document);
    assert!(text.contains("invoke ts.add(total, 1.0)"), "{text}");
}

/// A test narrows a binding that is declared once and never assigned, for
/// the code that runs only on that side of the test. An assigned binding
/// is not narrowed: the test saw another value.
#[test]
fn a_test_narrows_a_binding_nothing_assigns() {
    // `typeof` proves a number on one side; the other side knows nothing.
    assert_eq!(
        operations("function f(x) { if (typeof x === 'number') { return x + 1; } return x + 1; }")
            [2..],
        ["num.add(num.to_float(x), 1.0)", "invoke ts.add(x, 1.0)"]
    );
    // A union loses the member a test excludes, and an `if` that always
    // leaves narrows what follows it.
    let union = operations(
        "function f(x: number | undefined, y?: number, z: number | null = null) { \
             if (x === undefined) { return 0; } \
             if (y == null || z === null) { return 1; } \
             return x + y + z; }",
    );
    assert_eq!(
        union[union.len() - 2],
        "num.add(num.to_float(x), num.to_float(y))"
    );
    assert!(union.last().unwrap().starts_with("num.add("), "{union:?}");
    assert!(
        union.last().unwrap().ends_with(", num.to_float(z))"),
        "{union:?}"
    );
    assert_eq!(
        operations(
            "function f(x: number | string) { return typeof x === 'string' ? x + x : x + 1; }"
        )[2..],
        ["text.concat(x, x)", "num.add(num.to_float(x), 1.0)"]
    );
    assert_eq!(
        operations(
            "function f(x) { if (typeof x === 'number') { x = 'a'; return x + 1; } return 0; }"
        )
        .last()
        .map(String::as_str),
        Some("invoke ts.add(x, 1.0)")
    );
}

/// A tool's declared result type is stated at its `perform`, so an `Int`
/// or a `Float` is decoded as declared (`K-EFF-005`), and what the call
/// gives is known to be that type: an `Int` is a number that may be an
/// integer, a `Float` is a float, and `Any` is unknown.
#[test]
fn a_tools_declared_result_type_is_stated_and_known() {
    let effects: BTreeMap<EffectName, Signature> = [
        ("count", Type::Int),
        ("ratio", Type::Float),
        ("any", Type::Any),
    ]
    .into_iter()
    .map(|(name, result)| {
        let signature = Signature {
            params: Vec::new(),
            result,
        };
        (EffectName::new(name).unwrap(), signature)
    })
    .collect();
    let source = "const n = (await count()) + 1; const r = (await ratio()) * 2; \
                  const a = (await any()) + 1;";
    let lowered = lower_against(source, &[], &effects).unwrap();
    let text = print_document(&lowered.document);
    for expected in [
        "perform count() as Int",
        "perform ratio() as Float",
        "perform any() as Any",
    ] {
        assert!(text.contains(expected), "missing `{expected}` in\n{text}");
    }
    let computed = |name: &str| {
        text.lines()
            .find_map(|line| line.trim().strip_prefix(&format!("let {name} = ")))
            .unwrap_or_else(|| panic!("no `{name}` in\n{text}"))
            .to_string()
    };
    assert!(
        computed("n").starts_with("num.add(num.to_float(t"),
        "{text}"
    );
    assert!(computed("r").starts_with("num.mul(t"), "{text}");
    assert!(computed("a").starts_with("invoke ts.add(t"), "{text}");
}

/// What a cell finishes with, which every law below finishes as JSON text.
fn finished(source: &str) -> String {
    match super::machine::end(source) {
        super::machine::Ended::Finished(lash_kernel_doc::Datum::Text(text)) => text,
        other => panic!("{source}: {other:?}"),
    }
}

/// An object or an array the source built is a kernel record or list, so
/// a member the value plainly answers for is read and written in place:
/// a plain record's own field by a spelled name, and a list's length and
/// its element at one of its positions. Everything else keeps the helper:
/// a missing field is `undefined`, a field a record lacks reads what a
/// built-in gives (an own field of that name comes first), a hole and an
/// index the list lacks read `undefined`, and a write past the end leaves
/// holes.
#[test]
fn built_values_are_read_and_written_in_place_with_javascripts_answers() {
    let text =
        main_text("const o = { a: 1 }; const xs = [1, 2]; o.b = o.a; xs[2] = xs[0] + xs.length;");
    for direct in [
        "= o.a",
        ".b = t",
        "num.lt(0.0, list.len(xs))",
        "[2.0] = t",
        "num.to_float(list.len(xs))",
    ] {
        assert!(text.contains(direct), "missing `{direct}` in\n{text}");
    }
    assert_eq!(
        finished(
            "const o = { a: 1, toString: 5 }; const p = {}; const xs = [1, , 3]; \
             const holes = []; for (let i = -1; i < 5; i = i + 0.5) { holes.push(xs[i] === undefined); } \
             const ys = []; ys[0] = 'a'; ys[2] = 'c'; p.a = o.missing; \
             const named = ['x', 'toString', 'y']; const c = {}; c[named[0]] = 1; \
             await finish(JSON.stringify([o.a, o.b === undefined, o.toString, typeof p.toString, \
             p.valueOf === Object.prototype.valueOf, xs[1] === undefined, xs.length, 1 in xs, \
             holes.join(','), ys.length, 1 in ys, 'a' in p, p.a === undefined, \
             c[named[0]], typeof c[named[1]], o[named[1]], c[named[2]] === undefined]));"
        ),
        "[1,true,5,\"function\",true,true,3,false,\
         \"true,true,false,true,true,true,false,true,true,true,true,true\",3,false,true,true,\
         1,\"function\",5,true]"
    );
}

/// A representation is proof of the kind of value, never of what it holds:
/// a field or an element written through an alias reads what it now holds,
/// whatever kind that is.
#[test]
fn a_value_changed_through_an_alias_reads_what_it_now_holds() {
    assert_eq!(
        finished(
            "const a = { inner: { v: 1 } }; const b = a; const read = () => a.inner; \
             b.inner = 5; const r1 = read(); b.inner = [10, 20]; const r2 = a.inner[1]; \
             b.inner = 'str'; const r3 = a.inner.length; b.inner = null; let r4 = 'none'; \
             try { a.inner.v; } catch (e) { r4 = e.name; } \
             const xs = [1, 2]; const ys = xs; ys.length = 0; \
             await finish(JSON.stringify([r1, r2, r3, r4, xs.length, xs[0] === undefined]));"
        ),
        "[5,20,3,\"TypeError\",0,true]"
    );
}

/// A method is read from its object before the call's arguments run, so
/// an argument that replaces the method does not change the call.
#[test]
fn a_method_is_read_before_its_arguments_run() {
    assert_eq!(
        finished(
            "const o = { f: (x) => 'old' + x }; \
             const replace = () => { o.f = (x) => 'new' + x; return 1; }; \
             const first = o.f(replace()); const second = o.f(2); \
             await finish(first + ',' + second);"
        ),
        "old1,new2"
    );
}

/// A binding that only ever holds functions the source made holds their
/// tokens, so a call of it calls the token's closure without
/// `ts.callable`; a binding that may hold anything else keeps it.
#[test]
fn a_function_binding_nothing_else_is_assigned_calls_its_closure() {
    let text =
        main_text("const f = (x) => x; let g = function () { return 1; }; g = () => 2; f(g());");
    assert!(!text.contains("ts.callable"), "{text}");
    let text = main_text("let h = (x) => x; h = 3; h(1);");
    assert!(text.contains("ts.callable"), "{text}");
    assert_eq!(
        finished(
            "const add = (a, b) => a + b; let pick = function () { return 'a'; }; \
             const before = pick(); pick = () => 'b'; \
             await finish(String(add(1, 2)) + before + pick());"
        ),
        "3ab"
    );
}

/// A float in a template is spelled as `String` spells it: it has no
/// `toString` of its own to run.
#[test]
fn a_float_in_a_template_is_spelled_as_javascript_spells_it() {
    assert!(
        main_text("const n = 1 + 1; const s = `${n}`;").contains("invoke ts.number.to_string(n)")
    );
    assert_eq!(
        finished(
            "const n = 0.1 + 0.2; \
             await finish(`${n}|${-0}|${1 / 0}|${-1 / 0}|${0 / 0}|${1e21}|${2 ** 53}|${1e-7}|${-5}`);"
        ),
        "0.30000000000000004|0|Infinity|-Infinity|NaN|1e+21|9007199254740992|1e-7|-5"
    );
}

/// `concat`, `filter`, `map` and `slice` of an array make a new list, so
/// what they give is a list the source can use in place.
#[test]
fn a_built_in_array_methods_new_array_is_a_list() {
    let text = main_text("const xs = [1, 2]; const ys = xs.map((x) => x); ys.push(3);");
    assert!(
        !text.contains("ts.method.push") && !text.contains("ts.array.push"),
        "{text}"
    );
    assert_eq!(
        finished(
            "const xs = [1, , 3]; const ys = xs.map((x) => x * 2); ys.push(8); \
             const zs = xs.slice(1).concat([4]).filter((x) => x !== undefined); \
             await finish(JSON.stringify([ys.length, 1 in ys, ys[3], zs]));"
        ),
        "[4,false,8,[3,4]]"
    );
}
