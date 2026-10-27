//! Tier selection: an operation whose operand types are known is a direct
//! kernel call, and any other is the helper that carries JavaScript's
//! meaning.

use std::collections::BTreeMap;

use {lash_kernel_value::EffectName, lash_kernel_value::Signature, lash_kernel_value::Type};

use super::{lower_against, main_text};

/// Unknown operands retain the semantic helper, whether its implementation
/// is a native call or an imported code body.
#[test]
fn unknown_types_keep_the_helper() {
    for (params, expression, helper) in [
        ("a, b", "a + b", "ts.add"),
        ("a: any, b: number", "a + b", "ts.add"),
        ("a: number | string, b: number", "a + b", "ts.add"),
        ("a: number, b: string", "a + b", "ts.add"),
        ("a: number, b: number", "a ** b", "ts.pow"),
        ("a: string, b: string", "a < b", "ts.lt"),
        ("a: string, b: string", "a === b", "ts.strict_equals"),
        ("a: number, b: number", "(a as any) + b", "ts.add"),
        ("a: Missing, b: number", "a + b", "ts.add"),
    ] {
        let document = super::lower(&format!("function f({params}) {{ return {expression}; }}"))
            .unwrap()
            .document;
        assert!(calls_helper(&document, helper), "{params}: {expression}");
    }
    let document = super::lower("type T = number; function f<T>(a: T, b: T) { return a + b; }")
        .unwrap()
        .document;
    assert!(calls_helper(&document, "ts.add"));
}

fn calls_helper(document: &lash_kernel_doc::Document, helper: &str) -> bool {
    let function = lash_kernel_dialect::Library::resolve(super::library(), helper).unwrap();
    let mut nodes = document
        .units()
        .filter(|(unit, _)| match unit {
            lash_kernel_value::sites::Unit::Library(id) => {
                lash_kernel_doc::FunctionCatalog::header(super::library(), id).is_none()
                    && matches!(
                        document.functions[id].implementation,
                        lash_kernel_doc::Implementation::Guest {
                            metering: lash_kernel_doc::Metering::Statements,
                            ..
                        }
                    )
            }
            lash_kernel_value::sites::Unit::Main => true,
            _ => false,
        })
        .map(|(_, block)| lash_kernel_doc::Node::Block(block))
        .collect::<Vec<_>>();
    let mut walked = Vec::new();
    while let Some(node) = nodes.pop() {
        nodes.extend(node.children());
        walked.push(node);
    }
    walked.into_iter().any(|node| match node {
        lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Call {
            function: called, ..
        }) => *called == function,
        lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Act(action)) => match action.kind {
            lash_kernel_doc::ActKind::Call(lash_kernel_doc::Callee::Local(called)) => {
                called == function
            }
            _ => false,
        },
        lash_kernel_doc::Node::Action(lash_kernel_doc::Action::Call { callee, .. }) => match callee
        {
            lash_kernel_doc::Callee::Local(called) => *called == function,
            lash_kernel_doc::Callee::Import(_) => false,
            _ => false,
        },
        _ => false,
    })
}

/// A tool's declared result type is stated at its `perform`, so an `Int`
/// or a `Float` is decoded as declared (`K-EFF-005`), and what the call
/// gives is known to be that type: an `Int` is a
/// BigInt, a `Float` is a Number, and `Any` is unknown.
#[test]
fn a_tools_declared_result_type_is_stated_and_known() {
    let effects: BTreeMap<EffectName, Signature> = [
        ("stats.count", Type::Int),
        ("stats.ratio", Type::Float),
        ("stats.any", Type::Any),
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
    let source = "const n = (await stats.count()) + 1n; const r = (await stats.ratio()) * 2; \
                  const a = (await stats.any()) + 1;";
    let lowered = lower_against(source, &[], &effects).unwrap();
    // The Import owns its public signature; the action selects that slot.
    let mut pending = lowered
        .document
        .units()
        .map(|(_, body)| lash_kernel_doc::Node::Block(body))
        .collect::<Vec<_>>();
    let mut performed = BTreeMap::new();
    while let Some(node) = pending.pop() {
        pending.extend(node.children());
        let operation = match node {
            lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Act(action)) => match &action.kind {
                lash_kernel_doc::ActKind::Perform { slot, result, .. } => Some((*slot, result)),
                _ => None,
            },
            lash_kernel_doc::Node::Action(lash_kernel_doc::Action::Perform {
                slot,
                result,
                ..
            }) => Some((*slot, result)),
            _ => None,
        };
        if let Some((slot, result)) = operation {
            performed.insert(
                lowered.document.imports[&slot].name.to_string(),
                result.clone(),
            );
        }
    }
    for (name, signature) in effects {
        assert_eq!(performed[&name.to_string()], signature.result);
    }
    for helper in ["num.mul", "ts.add"] {
        assert!(calls_helper(&lowered.document, helper), "{helper}");
    }
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
             await control.finish(JSON.stringify([r1, r2, r3, r4, xs.length, xs[0] === undefined]));"
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
        await control.finish(JSON.stringify([first, captured(), alias.get(1), own,
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
             await control.finish(first + ',' + second);"
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
             await control.finish(String(add(1, 2)) + before + pick());"
        ),
        "3ab"
    );
}

/// `concat`, `filter`, `map` and `slice` of an array make a new list, so
/// what they give is a list the source can use in place.
#[test]
fn a_built_in_array_methods_new_array_is_a_list() {
    let source = "const xs = [1, 2]; const ys = xs.map((x) => x); ys.push(3);";
    let text = main_text(source);
    let lowered = super::lower(source).unwrap();
    for helper in ["ts.member.push", "ts.array.push"] {
        assert!(!calls_helper(&lowered.document, helper), "{helper}\n{text}");
    }
    assert_eq!(
        finished(
            "const xs = [1, , 3]; const ys = xs.map((x) => x * 2); ys.push(8); \
             const zs = xs.slice(1).concat([4]).filter((x) => x !== undefined); \
             await control.finish(JSON.stringify([ys.length, 1 in ys, ys[3], zs]));"
        ),
        "[4,false,8,[3,4]]"
    );
}

/// Named evaluation gives an anonymous function its binding's name and
/// nothing else: its parameters and the outer bindings it reads keep their
/// types, so its body lowers the typed operation a declaration's does.
#[test]
fn a_named_anonymous_function_keeps_its_bindings_types() {
    let typed = "num.add(ts.number.checked(a), limit)";
    for source in [
        "const limit = 3; function f(a: number) { return a + limit; }",
        "const limit = 3; const f = (a: number) => a + limit;",
        "const limit = 3; const f = (a: number) => { return a + limit; };",
        "const limit = 3; const f = function (a: number) { return a + limit; };",
    ] {
        let text = main_text(source);
        assert!(
            text.contains(typed) && !text.contains("ts.add("),
            "{source}:\n{text}"
        );
    }
}

/// A type belongs to a binding, not to its name. A parameter and an outer
/// `const` of one name, an outer `let` and a function's own `const`, and a
/// block's `const` under an outer one are each typed by what that binding
/// alone holds: the list keeps the list's own `push` whatever the other
/// binding holds, and the binding that may hold anything keeps the helper.
/// So is whether something assigns it: a receiver is held aside from the
/// call's arguments, and a declared function is called through what its
/// binding holds now, only when an assignment may write that binding.
#[test]
fn a_shadowed_name_types_each_binding_by_what_it_holds() {
    // How many pushes take the list's own path, how many of those read the
    // receiver from a temporary it was held in, and whether any push takes
    // the helper.
    let pushes = |source: &str| {
        let text = main_text(source);
        let receivers: Vec<String> = text
            .lines()
            .filter_map(|line| {
                let (receiver, _) = line.trim().strip_prefix("set ")?.split_once("[list.len(")?;
                Some(receiver.to_owned())
            })
            .collect();
        let held = receivers
            .iter()
            .filter(|receiver| {
                receiver
                    .strip_prefix('t')
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            })
            .count();
        (
            receivers.len(),
            held,
            calls_helper(&super::lower(source).unwrap().document, "ts.member.push"),
        )
    };
    for (source, expected) in [
        (
            "function demo(items) { items.push(1); return items; } \
             const items = []; items.push(2); demo(items);",
            (1, 0, true),
        ),
        (
            "let result = null; function one() { return 1; } \
             function body() { const result = []; result.push(one()); return result; } \
             result = body(); result.push(2);",
            (1, 0, true),
        ),
        (
            "const xs = []; function f(o) { { const xs = o; xs.push(1); } } xs.push(2); f(xs);",
            (1, 0, true),
        ),
        (
            "const xs = []; { let xs = 'a'; xs = xs + 'b'; } xs.push(1);",
            (1, 0, false),
        ),
        (
            "let ys = []; function g() { ys = []; return 1; } \
             function h() { const ys = []; ys.push(g()); return ys; } ys.push(g()); h();",
            (2, 1, false),
        ),
    ] {
        assert_eq!(pushes(source), expected, "{source}");
    }
    assert_eq!(
        finished(
            "function demo(items) { return items.push('own'); } \
             const items = []; items.push(1); \
             const custom = demo({ push(x) { return 'custom ' + x; } }); \
             let result = null; \
             function body() { const result = []; result.push(2); return result; } \
             result = body(); \
             let ys = [0]; function g() { ys = ['new']; return 1; } \
             function h() { const ys = []; ys.push(g()); return ys; } \
             const before = ys; ys.push(g()); const inner = h(); \
             function pick() { return 'declared'; } \
             function other() { function pick() { return 'inner'; } \
               pick = () => 'replaced'; return pick(); } \
             await control.finish(JSON.stringify([items, custom, result, before, ys, inner, \
               pick(), other()]));"
        ),
        r#"[[1],"custom own",[2],[0,1],["new"],[1],"declared","replaced"]"#
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
         await control.finish(tree);"
    ));
    let text = super::main_text_with_effects(&source);
    assert!(!text.contains("__lash_brand"), "{text}");
    assert!(!text.contains("kind("), "{text}");
    let guarded = source.replace(
        "const tree = t0;",
        "function opaque(x) { return x; } const tree = opaque(t0);",
    );
    assert!(super::main_text_with_effects(&guarded).contains("__lash_brand"));
    assert_eq!(super::machine::end(&source), super::machine::end(&guarded));
}

fn plain_fallback(source: &str) {
    let text = super::main_text_with_effects(source);
    assert!(text.contains("\"__lash_brand\""), "{text}");
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
        "const r = { next: {} }; const alias = r; alias.next = new Date(7); await control.finish(r.next.__lash_brand === undefined);",
    );
    // A computed alias write may replace any structural link.
    plain_fallback(
        "const r = { next: {} }; const alias = r; let key = 'next'; alias[key] = new Date(7); await control.finish(r.next.__lash_brand === undefined);",
    );
    // Deletion removes presence, even when the old field was a record.
    assert_eq!(
        super::machine::end(
            "const r = { next: {} }; delete r.next; await control.finish(r.next === undefined);"
        ),
        super::machine::Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
}

/// TS-PLAIN-CAPTURE: a closure may change any captured binding or field.
#[test]
fn plain_record_captures_keep_admission() {
    plain_fallback(
        "const r = { next: {} }; const change = () => { r.next = new Date(7); }; change(); await control.finish(r.next.__lash_brand === undefined);",
    );
}

/// TS-PLAIN-REASSIGN: neither an annotation nor a former literal proves a
/// reassigned internal receiver plain. User classes keep their refusal.
#[test]
fn plain_record_reassignment_keeps_brand_admission() {
    plain_fallback(
        "let r = {}; r = new Date(7); await control.finish(r.__lash_brand === undefined);",
    );
    assert_eq!(
        super::lower("let r = {}; class C {} r = new C(); r.x;")
            .unwrap_err()
            .code(),
        crate::DiagnosticCode::ClassUnsupported
    );
}

/// TS-PLAIN-DEFINITION: every definition and field must agree. Spread,
/// absent/duplicate fields, circular aliases and restored names prove nothing.
#[test]
fn plain_record_unknown_definitions_keep_admission() {
    for source in [
        "const r = { next: {}, next: new Date(7) }; await control.finish(r.next.__lash_brand === undefined);",
        "const r = { next: {} }; const copy = { ...r }; copy.next = new Date(7); await control.finish(copy.next.__lash_brand === undefined);",
        "let r = { next: {} }; r = { next: new Date(7) }; await control.finish(r.next.__lash_brand === undefined);",
    ] {
        plain_fallback(source);
    }
    assert_eq!(
        super::machine::end(
            "let r = { next: {} }; r = {}; await control.finish(r.next === undefined);"
        ),
        super::machine::Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
    let text = lash_kernel_doc::print_document(
        &super::lower_in_session("const r = { next: {} }; r.next.x;", &["r"])
            .unwrap()
            .document,
    );
    assert!(text.contains("__lash_brand"), "{text}");
    // Cyclic definitions cannot manufacture a proof or expand it without
    // bound before the ordinary boundness refusal runs.
    assert_eq!(
        super::lower("const a = { left: b, right: b }; const b = { left: a, right: a }; a.left.x;")
            .unwrap_err()
            .code(),
        crate::DiagnosticCode::TemporalDeadZone
    );
    // The root exists before a var initializer executes.
    assert_eq!(
        super::machine::end("await control.finish(r === undefined); var r = { next: {} };"),
        super::machine::Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
}

/// TS-PLAIN-COERCION: coercion calls user code with the record as receiver;
/// destructuring and assertions cannot hide aliases from escape exclusions.
#[test]
fn plain_record_implicit_calls_and_hidden_aliases_keep_admission() {
    plain_fallback(
        "const r = { next: {}, toString() { Object.assign(this, { next: new Date(7) }); return 'x'; } }; const text = '' + r; await control.finish(r.next.__lash_brand === undefined);",
    );
    plain_fallback(
        "const inner = { next: {} }; const r = ({ inner } as any); Object.assign(r.inner, { next: new Date(7) }); await control.finish(inner.next.__lash_brand === undefined);",
    );
    plain_fallback(
        "const r = { next: { child: {} } }; let alias; ({ next: alias } = r); Object.assign(alias, { child: new Date(7) }); await control.finish(r.next.child.__lash_brand === undefined);",
    );
}

/// TS-PLAIN-OBJECT-KEY: A computed literal key may coerce and mutate its receiver before a later member path reads it.
#[test]
fn plain_record_computed_object_keys_keep_admission() {
    let source = "const r = { next: {}, toString() { Object.assign(this, { next: new Date(7) }); return 'key'; } }; const keyed = { [r]: 1 }; await control.finish(r.next.__lash_brand === undefined);";
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
    let source = "const r = { next: {}, toString() { Object.assign(this, { next: new Date(7) }); return 'key'; } }; const { [r]: ignored } = {}; await control.finish(r.next.__lash_brand === undefined);";
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
    let source = "const r = { next: {} }; try { throw r; } catch (caught) { Object.assign(caught, { next: new Date(7) }); } await control.finish(r.next.__lash_brand === undefined);";
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
    let source = "const r = { next: {} }; Object.assign(({ inner: r }).inner, { next: new Date(7) }); await control.finish(r.next.__lash_brand === undefined);";
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
        lash_kernel_value::Name::new("marker"),
        lash_kernel_doc::Datum::Tuple(vec![
            lash_kernel_doc::Datum::Text("date.ecma".into()),
            lash_kernel_doc::Datum::Text("invalid_date".into()),
        ]),
    )]);
    let sources = [
        "const r = {}; r.__lash_brand = marker; await control.finish(r.__lash_brand === undefined);",
        "const r = {}; const key = '__lash_brand'; r[key] = marker; await control.finish(r.__lash_brand === undefined);",
        "const r = { __lash_brand: marker }; await control.finish(({ __lash_brand: marker }).__lash_brand === undefined);",
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

/// TS-PLAIN-FUNCTION-LOCAL, TS-PLAIN-NUMERIC-KEY: a function's own
/// record spine is proven like the program's, and a write keyed by a
/// number keeps every proof but those of fields a number's text can name.
#[test]
fn plain_record_proofs_hold_for_function_locals_and_numeric_writes() {
    let source = "function body() {\n\
         const tree = { level: { rows: [[0], [1], [2]], counters: { c: 0 } } };\n\
         for (let n = 0; n < 5; n++) {\n\
           tree.level.rows[n % 3] = [n];\n\
           tree.level.counters['c'] = tree.level.counters['c'] + 1;\n\
         }\n\
         return tree;\n\
       }\n\
       let result = null;\n\
       for (let repeat = 0; repeat < 2; repeat++) { result = body(); }\n\
       await control.finish(result);";
    let text = super::main_text_with_effects(source);
    assert!(!text.contains("__lash_brand"), "{text}");
    assert!(!text.contains("kind("), "{text}");
    let guarded = source.replace(
        "const tree = {",
        "function opaque(x) { return x; } const tree = opaque({",
    );
    let guarded = guarded.replace("} } };", "} } });");
    assert!(super::main_text_with_effects(&guarded).contains("__lash_brand"));
    assert_eq!(super::machine::end(source), super::machine::end(&guarded));
}

/// TS-PLAIN-NUMERIC-KEY: a key is a number only when every value it can
/// take is one, never because an annotation says so. Anything else may
/// write the internal brand; a number may still replace a numeric field.
#[test]
fn plain_record_keys_that_may_be_text_keep_brand_admission() {
    let bindings = super::pages::Session::seeded([(
        lash_kernel_value::Name::new("marker"),
        lash_kernel_doc::Datum::Tuple(vec![
            lash_kernel_doc::Datum::Text("date.ecma".into()),
            lash_kernel_doc::Datum::Text("invalid_date".into()),
        ]),
    )]);
    for key in [
        "const key: number = ('__lash_brand' as any);",
        "let key = 0; key = '__lash_brand';",
        "let key = 0; const change = () => { key = '__lash_brand'; }; change();",
        "const prefix = '__lash_'; const key = prefix + 'brand';",
        "const flag = false; const key = flag ? 0 : '__lash_brand';",
        "let key = 0; key += '__lash_brand';",
    ] {
        let source = format!(
            "function body() {{ const r = {{}}; {key} r[key] = marker; \
             return r.__lash_brand === undefined; }} await control.finish(body());"
        );
        let text = super::main_of(lower_against(
            &source,
            &["marker"],
            &super::machine::effects(),
        ));
        assert!(text.contains("\"__lash_brand\""), "{key}\n{text}");
        assert_eq!(
            super::machine::end_with_bindings(&source, bindings.clone()),
            super::machine::Ended::Finished(lash_kernel_doc::Datum::Bool(true)),
            "{key}"
        );
    }
    plain_fallback(
        "function body() { const r = { 0: {}, next: {} }; const n = 3; r[n % 3] = new Date(7); \
         return r['0'].__lash_brand === undefined; } await control.finish(body());",
    );
}

/// TS-PLAIN-CAPTURE: a closure can outlive its function and read a binding
/// whose record left through the function's result, where code the source
/// cannot see changes it.
#[test]
fn plain_record_captured_function_locals_keep_admission() {
    for change in [
        "Object.assign(made.r, { next: new Date(7) });",
        "const change = () => { Object.assign(made.r, { next: new Date(7) }); }; change();",
    ] {
        plain_fallback(&format!(
            "function make() {{ const r = {{ next: {{}} }}; \
             return {{ r: r, check: () => r.next.__lash_brand === undefined }}; }} \
             const made = make(); {change} await control.finish(made.check());"
        ));
    }
}

/// TS-PLAIN-ALIAS-WRITE: a text-keyed write through a function's alias
/// may replace any field of that function's records.
#[test]
fn plain_record_text_keyed_function_local_writes_keep_admission() {
    plain_fallback(
        "function body() { const r = { next: {} }; const alias = r; let key = 'next'; \
         alias[key] = new Date(7); return r.next.__lash_brand === undefined; } await control.finish(body());",
    );
}

/// TS-PLAIN-TEXT-PREFIX: a write keyed by a text proven to begin with a
/// prefix that neither the internal brand nor a built-in name has keeps
/// the record proven plain, and writes in place; a read by such a key is
/// the field, or `undefined`, with no test and no helper.
#[test]
fn plain_record_proofs_survive_text_keys_with_an_ordinary_prefix() {
    let source = "function body() {\n\
         const counts = {};\n\
         for (let n = 0; n < 70; n++) {\n\
           const key = 'key_' + String(n % 64);\n\
           counts[key] = (counts[key] || 0) + n;\n\
         }\n\
         const later = `key_${1}`;\n\
         const missing = 'key_' + 'none';\n\
         return [counts.key_1, counts[later], counts[missing] === undefined];\n\
       }\n\
       await control.finish(body());";
    let text = super::main_text_with_effects(source);
    for refused in ["__lash_brand", "kind("] {
        assert!(!text.contains(refused), "{refused}\n{text}");
    }
    let lowered = super::lower_with_effects(source).unwrap();
    for helper in ["ts.set", "ts.get_computed"] {
        assert!(!calls_helper(&lowered.document, helper), "{helper}\n{text}");
    }
    let guarded = source.replace("const counts = {};", "const counts = opaque({});");
    let guarded = format!("function opaque(x) {{ return x; }} {guarded}");
    assert!(calls_helper(
        &super::lower_with_effects(&guarded).unwrap().document,
        "ts.get_computed"
    ));
    assert_eq!(super::machine::end(source), super::machine::end(&guarded));
}

/// TS-PLAIN-TEXT-PREFIX: only a prefix proven by construction keeps the
/// proofs, and only one the internal brand does not begin with: a prefix
/// of the brand, a text joined after an unproven value, two values whose
/// common prefix is empty, a parameter, and a key copied from one each
/// may write the brand.
#[test]
fn plain_record_text_keys_that_may_be_the_brand_keep_admission() {
    let bindings = super::pages::Session::seeded([(
        lash_kernel_value::Name::new("marker"),
        lash_kernel_doc::Datum::Tuple(vec![
            lash_kernel_doc::Datum::Text("date.ecma".into()),
            lash_kernel_doc::Datum::Text("invalid_date".into()),
        ]),
    )]);
    for (params, key, argument) in [
        (
            "",
            "const part = '_brand'; const key = '__lash' + part;",
            "",
        ),
        (
            "",
            "const part = 'brand'; const key = `__lash_${part}`;",
            "",
        ),
        (
            "",
            "const part = '__lash_brand'; const key = part + '';",
            "",
        ),
        ("", "let key = 'key_0'; key = '__lash_brand';", ""),
        (
            "",
            "let key = 'key_0'; const change = () => { key = '__lash_brand'; }; change();",
            "",
        ),
        (
            "",
            "const flag = false; const key = flag ? 'key_0' : '__lash' + '_brand';",
            "",
        ),
        ("key", "", "'__lash_brand'"),
        ("part", "const key = part as string;", "'__lash_brand'"),
        ("part", "const key = String(part);", "'__lash_brand'"),
    ] {
        let source = format!(
            "function body({params}) {{ const r = {{}}; {key} r[key] = marker; \
             return r.__lash_brand === undefined; }} await control.finish(body({argument}));"
        );
        let text = super::main_of(lower_against(
            &source,
            &["marker"],
            &super::machine::effects(),
        ));
        assert!(text.contains("\"__lash_brand\""), "{key}\n{text}");
        assert_eq!(
            super::machine::end_with_bindings(&source, bindings.clone()),
            super::machine::Ended::Finished(lash_kernel_doc::Datum::Bool(true)),
            "{key}"
        );
    }
}

/// TS-PLAIN-TEXT-PREFIX: a prefix that some built-in name begins with is
/// no ordinary one: a read by such a key answers the member a plain record
/// inherits, and a write by one leaves the record unproven.
#[test]
fn plain_record_text_keys_that_may_name_a_member_keep_the_helper() {
    let source = "function body() { const r = { a: 1 }; const tail = 'String'; \
         const key = 'to' + tail; const other = 'to' + 'X'; r[other] = 1; \
         return [typeof r[key], typeof r.toString, r[other]]; } await control.finish(body());";
    let text = super::main_text_with_effects(source);
    assert!(
        calls_helper(
            &super::lower_with_effects(source).unwrap().document,
            "ts.get_computed"
        ),
        "{text}"
    );
    assert!(text.contains("\"__lash_brand\""), "{text}");
    let guarded = source.replace("const r = {", "const r = opaque({");
    let guarded = guarded.replace("a: 1 };", "a: 1 });");
    let guarded = format!("function opaque(x) {{ return x; }} {guarded}");
    assert_eq!(super::machine::end(source), super::machine::end(&guarded));
    let super::machine::Ended::Finished(lash_kernel_doc::Datum::List(items)) =
        super::machine::end(source)
    else {
        panic!("a list")
    };
    assert_eq!(items[0], lash_kernel_doc::Datum::Text("function".into()));
}

/// TS-COMPUTED-MISS: `ts.builtin_name` agrees with the rows a computed
/// read searches, so a plain record's computed read by a text with no
/// proven prefix answers each built-in name as the static read of that
/// name does, and `undefined` for any other text, in a loop region and
/// in the helper's plan, long texts included.
#[test]
fn computed_reads_of_plain_records_answer_misses_as_the_rows_do() {
    // A static read of a name the dialect refuses to reach, as
    // `r.prototype`, has no answer to compare with.
    let names: Vec<&str> = lash_ext_ts_helpers::package::builtin_names()
        .into_iter()
        .filter(|name| {
            super::lower_with_effects(&format!(
                "const r = {{}}; await control.finish(typeof r.{name});"
            ))
            .is_ok()
        })
        .collect();
    assert!(names.len() > 200, "{names:?}");
    let others = [
        "",
        "key_1",
        "toStrin",
        "toStringX",
        "__lash_brand",
        "zzzz",
        "a-long-row-label-past-the-inline-text-limit",
    ];
    let keys: Vec<String> = names
        .iter()
        .copied()
        .chain(others)
        .map(|name| format!("'{name}'"))
        .collect();
    let expected: Vec<String> = names
        .iter()
        .map(|name| format!("typeof r.{name}"))
        .chain(others.iter().map(|_| "'undefined'".to_string()))
        .collect();
    let source = format!(
        "function probe(r, keys) {{ const out = []; for (let i = 0; i < keys.length; i++) {{ out.push(typeof r[keys[i]]); }} return out; }}\n\
         const r = {{ a: 1 }};\n\
         const keys = [{}];\n\
         const expected = [{}];\n\
         const out = probe(r, keys);\n\
         const bad = [];\n\
         for (let i = 0; i < keys.length; i++) {{ if (out[i] !== expected[i]) {{ bad.push(keys[i]); }} }}\n\
         await control.finish(bad.join(','));",
        keys.join(", "),
        expected.join(", ")
    );
    assert_eq!(
        super::machine::end(&source),
        super::machine::Ended::Finished(lash_kernel_doc::Datum::Text(String::new()))
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
        "let r = { a: { b: { leaf: 0 } } }; let sum = 0; for (let i = 0; i < 3; i++) { sum += r.a.b.leaf; r = { a: { b: { leaf: i + 10 } } }; } await control.finish(sum);",
    );
    stable_path_matches_guarded(
        "let sum = 0; for (let i = 0; i < 3; i++) { const r = { a: { b: { leaf: i } } }; sum += r.a.b.leaf; } await control.finish(sum);",
    );
    stable_path_matches_guarded(
        "const r = { a: { b: { leaf: 9 } } }; let sum = 0; for (let i = 0; i < 2; i++) { const r = { a: { b: { leaf: i } } }; sum += r.a.b.leaf; } await control.finish(sum);",
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
            "const r = {{ a: {{ b: {{ leaf: 0 }} }} }}; const alias = r.a; let result = ''; for (let i = 0; i < 2; i++) {{ try {{ result += String(r.a.b.leaf); {mutation} }} catch (e) {{ result += 'caught'; }} }} await control.finish(result);"
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
            "const r = {{ a: {{ b: {{ leaf: 0 }} }} }}; {setup} let sum = 0; for (let i = 0; i < 2; i++) {{ sum += r.a.b.leaf; change(); }} await control.finish(sum);"
        ));
    }
    stable_path_matches_guarded(
        "const r = { a: { b: { leaf: 0 } } }; function change(x) { Object.assign(x, { b: { leaf: 7 } }); } let sum = 0; for (let i = 0; i < 2; i++) { sum += r.a.b.leaf; change(r.a); } await control.finish(sum);",
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
            "const r = {initial}; let result = ''; for (let i = 0; i < 2; i++) {{ try {{ result += String(r.a.b.leaf); }} catch (e) {{ result += 'caught'; }} }} await control.finish(result);"
        ));
    }
    stable_path_matches_guarded(
        "const r = { a: { b: { leaf: 7 } } }; let sum = 0; for (let i = 0; i < 0; i++) { sum += r.a.b.leaf; } for (let i = 0; i < 3; i++) { if (i === 2) { sum += r.a.b.leaf; } } await control.finish(sum);",
    );
    stable_path_matches_guarded(
        "const r = { a: { b: { leaf: 7 } } }; let sum = 0; for (let i = 0; i < 3; i++) { for (let j = 0; j < 2; j++) { sum += r.a.b.leaf; } } await control.finish(sum);",
    );
}

/// TS-STABLE-PATH-BOUNDARY: restored roots and effect results are unknown;
/// a mid-loop park cannot share source facts with a later guest mutation.
#[test]
fn stable_paths_keep_restored_roots_and_nonterminal_effects_guarded() {
    use lash_kernel_doc::Datum;
    use lash_kernel_value::Name;
    let source = "let sum = 0; for (let i = 0; i < 2; i++) { sum += r.a.b.leaf; r.a.b = { leaf: 7 }; } await control.finish(sum);";
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
    let effect_source = "const r = { a: { b: { leaf: 0 } } }; let sum = 0; for (let i = 0; i < 2; i++) { sum += r.a.b.leaf; r.a.b = await host.echo({ leaf: 7 }); } await control.finish(sum);";
    assert_eq!(
        super::machine::run(effect_source, &[]),
        super::machine::run(&effect_source.replace("const r =", "var r ="), &[])
    );
    assert_eq!(super::lower("const r = { a: { b: {} } }; for (let i = 0; i < 2; i++) { middle.b; const middle = r.a; }").unwrap_err().code(), crate::DiagnosticCode::TemporalDeadZone);
}
