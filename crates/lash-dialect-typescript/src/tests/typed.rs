//! Tier selection: an operation whose operand types are known is a direct
//! kernel call, and any other is the helper that carries JavaScript's
//! meaning.

use std::collections::BTreeMap;

use lash_kernel_doc::{EffectName, Signature, Type, print_document};

use super::{lower_against, lower_in_session, main_text};

/// The statements of `source`'s lowered `main` that compute something: a
/// direct kernel call or a helper call, without the binding's name.
fn operations(source: &str) -> Vec<String> {
    main_text(source)
        .lines()
        .filter_map(|line| line.trim().split_once(" = "))
        .map(|(_, value)| value.to_string())
        .filter(|value| value.contains('('))
        .filter(|value| !value.starts_with("fn(") && !value.starts_with("invoke ts.pad("))
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
        ("xs[a]", "list.get(xs, t8)"),
        ("xs.length", "list.len(xs)"),
        ("xs[0] + o.n", "list.get(xs, 0.0)"),
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
    assert_eq!(
        operations("function f(a: number, b: number) { return a * b / 2; }")[1],
        "num.div(t4, 2.0)"
    );
    // A property read keeps JavaScript's meaning; what it gives is believed.
    assert_eq!(
        operations("function f(o: { n: number }) { return o.n + 1; }"),
        ["invoke ts.get(o, \"n\")", "num.add(num.to_float(t3), 1.0)"]
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
        ("a: number[], b: any", "a[b]", "invoke ts.get(a, t4)"),
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
        operations("{ const k = 1; const r = k + 1; } { const k = 'a'; const s = k + 1; }"),
        ["invoke ts.add(k_1, 1.0)", "invoke ts.add(k_2, 1.0)"]
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
    assert_eq!(
        operations(
            "function f(x: number | undefined, y?: number, z: number | null = null) { \
             if (x === undefined) { return 0; } \
             if (y == null || z === null) { return 1; } \
             return x + y + z; }"
        )
        .iter()
        .rev()
        .take(2)
        .collect::<Vec<_>>(),
        [
            "num.add(t10, num.to_float(z))",
            "num.add(num.to_float(x), num.to_float(y))"
        ]
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

/// A declared return type, a type alias, an interface and a loop over a
/// declared array each carry a type to where it is used.
#[test]
fn declared_types_reach_their_uses() {
    assert_eq!(
        operations(
            "type Price = number; interface Item { price: Price } \
             function total(items: Item[]): number { let sum = 0; \
             for (const item of items) { sum += item.price; } return sum; } \
             const twice = total([]) * 2;"
        ),
        [
            "invoke ts.iterate(items)",
            "invoke ts.get(item, \"price\")",
            "num.add(t5, num.to_float(t6))",
            "apply t9(absent, t11)",
            "num.mul(num.to_float(t12), 2.0)"
        ]
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
