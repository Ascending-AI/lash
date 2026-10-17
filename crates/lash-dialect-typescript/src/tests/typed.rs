//! Tier selection: an operation whose operand types are known is a direct
//! kernel call, and any other is the helper that carries JavaScript's
//! meaning.

use std::collections::BTreeMap;

use lash_kernel_doc::{EffectName, Signature, Type, print_document};

use super::{lower_against, main_text};

/// The statements of `source`'s lowered `main` that compute something: a
/// direct kernel call or a helper call, without the binding's name, or the
/// call a `return` gives. A function's calling convention (its closure, its
/// token and its positional arguments) is not one.
fn operations(source: &str) -> Vec<String> {
    main_text(source)
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            line.split_once(" = ")
                .map(|(_, value)| value)
                .or_else(|| line.strip_prefix("return "))
        })
        .map(str::to_string)
        .filter(|value| value.contains('('))
        .filter(|value| {
            !value.starts_with("fn(")
                && !value.starts_with("(\"ts.function\"")
                && !value.contains("invoke ts.pad(")
        })
        .collect()
}

/// Where a type is not known the operation is the helper: an unannotated
/// parameter, `any`, a union that nothing has narrowed, an operator with
/// no kernel function of its own, and a value asserted `as any`. An
/// operation helper, which states a native, is called inside the
/// expression (FIG-6440); any other is invoked.
#[test]
fn unknown_types_keep_the_helper() {
    for (params, body, expected) in [
        ("a, b", "a + b", "ts.add(a, b)"),
        ("a: any, b: number", "a + b", "ts.add(a, b)"),
        ("a: number | string, b: number", "a + b", "ts.add(a, b)"),
        ("a: number, b: string", "a + b", "ts.add(a, b)"),
        ("a: number, b: number", "a ** b", "invoke ts.pow(a, b)"),
        ("a: string, b: string", "a < b", "ts.lt(a, b)"),
        (
            "a: string, b: string",
            "a === b",
            "invoke ts.strict_equals(a, b)",
        ),
        ("a: number, b: number", "(a as any) + b", "ts.add(a, b)"),
        ("a: Missing, b: number", "a + b", "ts.add(a, b)"),
    ] {
        let operations = operations(&format!("function f({params}) {{ return {body}; }}"));
        assert_eq!(operations, [expected], "({params}) => {body}");
    }
    // A type parameter is whatever the caller passes, even when the source
    // declares a type of the same name.
    assert_eq!(
        operations("type T = number; function f<T>(a: T, b: T) { return a + b; }"),
        ["ts.add(a, b)"]
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
        computed("n").starts_with("num.add(num.to_float(perform count() as Int then yield)"),
        "{text}"
    );
    assert!(
        computed("r").starts_with("num.mul(perform ratio() as Float then yield"),
        "{text}"
    );
    assert!(
        computed("a").starts_with("ts.add(perform any() as Any then yield"),
        "{text}"
    );
}

/// What a cell finishes with, which every law below finishes as JSON text.
fn finished(source: &str) -> String {
    match super::machine::end(source) {
        super::machine::Ended::Finished(lash_kernel_doc::Datum::Text(text)) => text,
        other => panic!("{source}: {other:?}"),
    }
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
            r#"
        let current = new Map([[1, 'old']]);
        const alias = current;
        const captured = () => current.get(1);
        const change = () => { current = new Map([[1, 'new']]); return 1; };
        const first = current.get(change());
        const generic = (o, change) => o.get(change());
        const o = { get(x) { return 'own' + x; } };
        const replace = () => { o.get = (x) => 'replacement' + x; return 2; };
        const own = generic(o, replace);
        let order = '';
        const arg = (x) => { order += x; return x; };
        const same = alias.set(arg('k'), arg('v'), arg('ignored')) === alias;
        alias.set(...['spread', 8]);
        const missing = alias.get() === undefined && !alias.has();
        const borrowed = alias.get;
        await finish(JSON.stringify([first, captured(), alias.get(1), own,
            same, order, alias.has('k'), alias.get('spread'), missing,
            borrowed.call(alias, 1)]));
    "#
        ),
        r#"["old","new","old","own2",true,"kvignored",true,8,true,"old"]"#
    );

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
    assert!(main_text("const n = 1 + 1; const s = `${n}`;").contains("ts.number.to_string(n)"));
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

/// TS-PLAIN-PATH: a closed, source-built record spine needs no repeated
/// kind/brand admission, and leaf writes preserve its heap graph.
#[test]
fn plain_record_spines_skip_admission_and_preserve_heap_state() {
    let mut source = "const t24 = { leaf: [0] };\n".to_string();
    for level in (0..24).rev() {
        source.push_str(&format!("const t{level} = {{ next: t{} }};\n", level + 1));
    }
    let half = ".next".repeat(12);
    source.push_str(&format!(
        "const tree = t0;\nfor (let n = 0; n < 3; n++) {{ \
         const middle = tree{half}; middle{half}.leaf = [n]; }}\n\
         await finish(tree);"
    ));
    let text = super::main_text_with_effects(&source);
    assert!(!text.contains("__ts_brand"), "{text}");
    assert!(!text.contains("kind("), "{text}");
    let guarded = source.replace(
        "const tree = t0;",
        "function opaque(x) { return x; } const tree = opaque(t0);",
    );
    assert!(super::main_text_with_effects(&guarded).contains("__ts_brand"));
    assert_eq!(super::machine::end(&source), super::machine::end(&guarded));
}

fn plain_fallback(source: &str) {
    let text = super::main_text_with_effects(source);
    assert!(text.contains(".__ts_brand, ())"), "{text}");
    assert_eq!(
        super::machine::end(source),
        super::machine::Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
}

/// TS-PLAIN-ALIAS-WRITE: writes through another name invalidate the field
/// everywhere; internal brands remain invisible as guest properties.
#[test]
fn plain_record_alias_writes_keep_nested_admission() {
    plain_fallback(
        "const r = { next: {} }; const alias = r; alias.next = new Date(7); await finish(r.next.__ts_brand === undefined);",
    );
    // A computed alias write may replace any structural link.
    plain_fallback(
        "const r = { next: {} }; const alias = r; let key = 'next'; alias[key] = new Date(7); await finish(r.next.__ts_brand === undefined);",
    );
    // Deletion removes presence, even when the old field was a record.
    assert_eq!(
        super::machine::end(
            "const r = { next: {} }; delete r.next; await finish(r.next === undefined);"
        ),
        super::machine::Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
}

/// TS-PLAIN-CALL-ESCAPE: an unknown callee can mutate a passed record or
/// a field, including through an alias kept in another fresh record.
#[test]
fn plain_record_mutating_calls_keep_admission() {
    plain_fallback(
        "const r = { next: {} }; function change(x) { x.next = new Date(7); } change(r); await finish(r.next.__ts_brand === undefined);",
    );
    plain_fallback(
        "const inner = { next: {} }; const r = { inner }; function change(x) { x.next = new Date(7); } change(r.inner); await finish(inner.next.__ts_brand === undefined);",
    );
}

/// TS-PLAIN-CAPTURE: a closure may change any captured binding or field.
#[test]
fn plain_record_captures_keep_admission() {
    plain_fallback(
        "const r = { next: {} }; const change = () => { r.next = new Date(7); }; change(); await finish(r.next.__ts_brand === undefined);",
    );
}

/// TS-PLAIN-REASSIGN: neither an annotation nor a former literal proves a
/// reassigned internal receiver plain. User classes keep their refusal.
#[test]
fn plain_record_reassignment_keeps_brand_admission() {
    plain_fallback("let r = {}; r = new Date(7); await finish(r.__ts_brand === undefined);");
    assert_eq!(
        super::lower("let r = {}; class C {} r = new C(); r.x;")
            .unwrap_err()
            .code,
        crate::DiagnosticCode::ClassUnsupported
    );
}

/// TS-PLAIN-ACCESSOR: the existing source refusal cannot become a literal
/// proof; no getter can enter the guard-free record path.
#[test]
fn plain_record_getters_keep_the_accessor_refusal() {
    assert_eq!(
        super::lower("const r = { next: { get value() { return 7; } } }; r.next.value;")
            .unwrap_err()
            .code,
        crate::DiagnosticCode::AccessorUnsupported
    );
}

/// TS-PLAIN-DEFINITION: every definition and field must agree. Spread,
/// absent/duplicate fields, circular aliases and restored names prove nothing.
#[test]
fn plain_record_unknown_definitions_keep_admission() {
    for source in [
        "const r = { next: {}, next: new Date(7) }; await finish(r.next.__ts_brand === undefined);",
        "const r = { next: {} }; const copy = { ...r }; copy.next = new Date(7); await finish(copy.next.__ts_brand === undefined);",
        "let r = { next: {} }; r = { next: new Date(7) }; await finish(r.next.__ts_brand === undefined);",
    ] {
        plain_fallback(source);
    }
    assert_eq!(
        super::machine::end("let r = { next: {} }; r = {}; await finish(r.next === undefined);"),
        super::machine::Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
    let text = lash_kernel_doc::print_document(
        &super::lower_in_session("const r = { next: {} }; r.next.x;", &["r"])
            .unwrap()
            .document,
    );
    assert!(text.contains("__ts_brand"), "{text}");
    // Cyclic definitions cannot manufacture a proof or expand it without
    // bound before the ordinary boundness refusal runs.
    assert_eq!(
        super::lower("const a = { left: b, right: b }; const b = { left: a, right: a }; a.left.x;")
            .unwrap_err()
            .code,
        crate::DiagnosticCode::TemporalDeadZone
    );
    // The root exists before a var initializer executes.
    assert_eq!(
        super::machine::end("await finish(r === undefined); var r = { next: {} };"),
        super::machine::Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
}

/// TS-PLAIN-COERCION: coercion calls user code with the record as receiver;
/// destructuring and assertions cannot hide aliases from escape exclusions.
#[test]
fn plain_record_implicit_calls_and_hidden_aliases_keep_admission() {
    plain_fallback(
        "const r = { next: {}, toString() { Object.assign(this, { next: new Date(7) }); return 'x'; } }; const text = '' + r; await finish(r.next.__ts_brand === undefined);",
    );
    plain_fallback(
        "const inner = { next: {} }; const r = ({ inner } as any); Object.assign(r.inner, { next: new Date(7) }); await finish(inner.next.__ts_brand === undefined);",
    );
    plain_fallback(
        "const r = { next: { child: {} } }; let alias; ({ next: alias } = r); Object.assign(alias, { child: new Date(7) }); await finish(r.next.child.__ts_brand === undefined);",
    );
}

/// TS-PLAIN-OBJECT-KEY: A computed literal key may coerce and mutate its receiver before a later member path reads it.
#[test]
fn plain_record_computed_object_keys_keep_admission() {
    let source = "const r = { next: {}, toString() { Object.assign(this, { next: new Date(7) }); return 'key'; } }; const keyed = { [r]: 1 }; await finish(r.next.__ts_brand === undefined);";
    let guarded = super::machine::end(&source.replace("const r =", "var r ="));
    assert_eq!(
        guarded,
        super::machine::Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
    assert_eq!(super::machine::end(source), guarded);
}

/// TS-PLAIN-PATTERN-KEY: Destructuring key coercion has the same escape semantics as computed member access.
#[test]
fn plain_record_computed_pattern_keys_keep_admission() {
    let source = "const r = { next: {}, toString() { Object.assign(this, { next: new Date(7) }); return 'key'; } }; const { [r]: ignored } = {}; await finish(r.next.__ts_brand === undefined);";
    let guarded = super::machine::end(&source.replace("const r =", "var r ="));
    assert_eq!(
        guarded,
        super::machine::Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
    assert_eq!(super::machine::end(source), guarded);
}

/// TS-PLAIN-THROW: A catch binding aliases the thrown value, so a call on that binding can invalidate the original nested facts.
#[test]
fn plain_record_thrown_aliases_keep_admission() {
    let source = "const r = { next: {} }; try { throw r; } catch (caught) { Object.assign(caught, { next: new Date(7) }); } await finish(r.next.__ts_brand === undefined);";
    let guarded = super::machine::end(&source.replace("const r =", "var r ="));
    assert_eq!(
        guarded,
        super::machine::Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
    assert_eq!(super::machine::end(source), guarded);
}

/// TS-PLAIN-FRESH-FIELD-ESCAPE: A field of an unnamed literal can expose a local alias to a mutating call.
#[test]
fn plain_record_fresh_literal_field_aliases_keep_admission() {
    let source = "const r = { next: {} }; Object.assign(({ inner: r }).inner, { next: new Date(7) }); await finish(r.next.__ts_brand === undefined);";
    let guarded = super::machine::end(&source.replace("const r =", "var r ="));
    assert_eq!(
        guarded,
        super::machine::Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
    assert_eq!(super::machine::end(source), guarded);
}

/// TS-PLAIN-BRAND: restored kernel data can carry the internal marker.
/// Neither a literal nor a static/computed field write may certify it plain.
#[test]
fn plain_record_restored_brand_markers_keep_admission() {
    let bindings = super::pages::Session::seeded([(
        lash_kernel_doc::Name::new("marker"),
        lash_kernel_doc::Datum::Tuple(Vec::new()),
    )]);
    let sources = [
        "const r = {}; r.__ts_type = 'brand:date'; r.__ts_brand = marker; await finish(r.__ts_brand === undefined);",
        "const r = {}; r.__ts_type = 'brand:date'; const key = '__ts_brand'; r[key] = marker; await finish(r.__ts_brand === undefined);",
        "const r = { __ts_brand: marker, __ts_type: 'brand:date' }; await finish(({ __ts_brand: marker, __ts_type: 'brand:date' }).__ts_brand === undefined);",
    ];
    let observed: Vec<_> = sources
        .iter()
        .map(|source| {
            let guarded = source
                .replace("const r =", "var r =")
                .replace("finish(({", "finish(opaque({");
            let guarded = format!("function opaque(x) {{ return x; }} {guarded}");
            assert_eq!(
                super::machine::end_with_bindings(&guarded, bindings.clone()),
                super::machine::Ended::Finished(lash_kernel_doc::Datum::Bool(true))
            );
            super::machine::end_with_bindings(source, bindings.clone())
        })
        .collect();
    assert_eq!(
        observed,
        (0..sources.len())
            .map(|_| super::machine::Ended::Finished(lash_kernel_doc::Datum::Bool(true)))
            .collect::<Vec<_>>()
    );
}

fn stable_path_matches_guarded(source: &str) {
    let guarded = source
        .replace("const r =", "var r =")
        .replace("let r =", "var r =");
    assert_eq!(
        super::machine::end(source),
        super::machine::end(&guarded),
        "{source}"
    );
}

/// TS-STABLE-PATH-ROOT: roots replaced by other plain records and locals
/// freshly constructed on each iteration must never retain an old pointer.
#[test]
fn stable_paths_reject_rebound_and_iteration_local_roots() {
    stable_path_matches_guarded(
        "let r = { a: { b: { leaf: 0 } } }; let sum = 0; for (let i = 0; i < 3; i++) { sum += r.a.b.leaf; r = { a: { b: { leaf: i + 10 } } }; } await finish(sum);",
    );
    stable_path_matches_guarded(
        "let sum = 0; for (let i = 0; i < 3; i++) { const r = { a: { b: { leaf: i } } }; sum += r.a.b.leaf; } await finish(sum);",
    );
    stable_path_matches_guarded(
        "const r = { a: { b: { leaf: 9 } } }; let sum = 0; for (let i = 0; i < 2; i++) { const r = { a: { b: { leaf: i } } }; sum += r.a.b.leaf; } await finish(sum);",
    );
}

/// TS-STABLE-PATH-ALIAS: replacement, deletion and computed writes through
/// aliases invalidate every path containing that link, at every hop.
#[test]
fn stable_paths_reject_alias_replacement_deletion_and_computed_writes() {
    for mutation in [
        "alias.b = { leaf: i + 10 };",
        "delete alias.b;",
        "let key = 'b'; alias[key] = { leaf: i + 10 };",
        "alias.b = null;",
    ] {
        stable_path_matches_guarded(&format!(
            "const r = {{ a: {{ b: {{ leaf: 0 }} }} }}; const alias = r.a; let result = ''; for (let i = 0; i < 2; i++) {{ try {{ result += String(r.a.b.leaf); {mutation} }} catch (e) {{ result += 'caught'; }} }} await finish(result);"
        ));
    }
}

/// TS-STABLE-PATH-ESCAPE: callbacks, opaque mutators, thrown aliases and
/// implicit coercion can all replace an ancestor without a direct write.
#[test]
fn stable_paths_reject_captures_calls_throws_and_coercions() {
    for setup in [
        "const change = () => { r.a.b = { leaf: 7 }; };",
        "function change() { Object.assign(r.a, { b: { leaf: 7 } }); }",
        "function change() { try { throw r.a; } catch (x) { Object.assign(x, { b: { leaf: 7 } }); } }",
        "const key = { toString() { Object.assign(r.a, { b: { leaf: 7 } }); return 'key'; } }; function change() { const value = { [key]: 0 }; }",
    ] {
        stable_path_matches_guarded(&format!(
            "const r = {{ a: {{ b: {{ leaf: 0 }} }} }}; {setup} let sum = 0; for (let i = 0; i < 2; i++) {{ sum += r.a.b.leaf; change(); }} await finish(sum);"
        ));
    }
    stable_path_matches_guarded(
        "const r = { a: { b: { leaf: 0 } } }; function change(x) { Object.assign(x, { b: { leaf: 7 } }); } let sum = 0; for (let i = 0; i < 2; i++) { sum += r.a.b.leaf; change(r.a); } await finish(sum);",
    );
}

/// TS-STABLE-PATH-ORDER: absent/null/nonrecord hops keep their failure
/// order, and paths in zero-trip loops or untaken branches are never read.
#[test]
fn stable_paths_keep_hop_failures_and_lazy_evaluation() {
    for initial in [
        "{ a: null }",
        "{}",
        "{ a: { b: null } }",
        "{ a: { b: new Date(7) } }",
    ] {
        stable_path_matches_guarded(&format!(
            "const r = {initial}; let result = ''; for (let i = 0; i < 2; i++) {{ try {{ result += String(r.a.b.leaf); }} catch (e) {{ result += 'caught'; }} }} await finish(result);"
        ));
    }
    stable_path_matches_guarded(
        "const r = { a: { b: { leaf: 7 } } }; let sum = 0; for (let i = 0; i < 0; i++) { sum += r.a.b.leaf; } for (let i = 0; i < 3; i++) { if (i === 2) { sum += r.a.b.leaf; } } await finish(sum);",
    );
    stable_path_matches_guarded(
        "const r = { a: { b: { leaf: 7 } } }; let sum = 0; for (let i = 0; i < 3; i++) { for (let j = 0; j < 2; j++) { sum += r.a.b.leaf; } } await finish(sum);",
    );
}

/// TS-STABLE-PATH-BOUNDARY: restored roots and effect results are unknown;
/// a mid-loop park cannot share source facts with a later guest mutation.
#[test]
fn stable_paths_keep_restored_roots_and_nonterminal_effects_guarded() {
    use lash_kernel_doc::{Datum, Name};
    let source = "let sum = 0; for (let i = 0; i < 2; i++) { sum += r.a.b.leaf; r.a.b = { leaf: 7 }; } await finish(sum);";
    let bindings = super::pages::Session::seeded([(
        Name::new("r"),
        Datum::Record(vec![(
            "a".into(),
            Datum::Record(vec![(
                "b".into(),
                Datum::Record(vec![(
                    "leaf".into(),
                    Datum::Float(lash_kernel_doc::Float::new(1.0)),
                )]),
            )]),
        )]),
    )]);
    assert_eq!(
        super::machine::end_with_bindings(source, bindings),
        super::machine::Ended::Finished(Datum::Float(lash_kernel_doc::Float::new(8.0)))
    );
    let effect_source = "const r = { a: { b: { leaf: 0 } } }; let sum = 0; for (let i = 0; i < 2; i++) { sum += r.a.b.leaf; r.a.b = await echo({ leaf: 7 }); } await finish(sum);";
    assert_eq!(
        super::machine::run(effect_source, &[]),
        super::machine::run(&effect_source.replace("const r =", "var r ="), &[])
    );
    assert_eq!(super::lower("const r = { a: { b: {} } }; for (let i = 0; i < 2; i++) { middle.b; const middle = r.a; }").unwrap_err().code, crate::DiagnosticCode::TemporalDeadZone);
}
