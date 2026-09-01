use lash_typescript::DiagnosticCode as Code;

macro_rules! rejection_test {
    ($name:ident, $source:expr, $code:expr) => {
        #[test]
        fn $name() {
            let error = lash_typescript::validate($source).expect_err($source);
            assert_eq!(error.code, $code, "{error}");
            assert!(error.to_string().starts_with($code.as_str()));
        }
    };
}

rejection_test!(rejects_classes, "class A {}", Code::ClassUnsupported);
rejection_test!(
    rejects_generators,
    "function* f() {}",
    Code::GeneratorUnsupported
);
rejection_test!(rejects_with, "with ({}) {}", Code::WithUnsupported);
rejection_test!(rejects_eval, "eval('1');", Code::EvalUnsupported);
rejection_test!(
    rejects_function_constructor,
    "Function('return 1');",
    Code::FunctionConstructorUnsupported
);
rejection_test!(
    rejects_labels,
    "label: while (true) { break; }",
    Code::LabelUnsupported
);
rejection_test!(
    rejects_regexp_indices_flag,
    "const r = /x/d;",
    Code::RegexIndicesFlagUnsupported
);
rejection_test!(
    rejects_regexp_unicode_sets_flag,
    "const r = /x/v;",
    Code::RegexUnicodeSetsFlagUnsupported
);

#[test]
fn retained_match_all_iterator_has_a_sink_repair() {
    let error = lash_typescript::validate("const matches = 'a'.matchAll(/a/g);")
        .expect_err("matchAll must be consumed directly");
    assert_eq!(error.code, Code::RegexIteratorPosition);
    assert_eq!(error.suggestions, ["wrap: [...text.matchAll(regexp)]"]);
    assert!(
        error.to_string().contains("[...text.matchAll(regexp)]"),
        "{error}"
    );
}

#[test]
fn match_all_rejects_non_contract_iterable_sinks() {
    // `new Map`/`Set` and `Object.fromEntries` are contract sinks — the same
    // five the collection iterators take. What is still refused is a position
    // that lets the iterator outlive its materialization.
    for source in [
        "const it = 'a'.matchAll(/a/g);",
        "const wrap = { it: 'a'.matchAll(/a/g) };",
        "function take(x: unknown) { } take('a'.matchAll(/a/g));",
        "finish('a'.matchAll(/a/g));",
    ] {
        let error = lash_typescript::validate(source).expect_err(source);
        assert_eq!(error.code, Code::RegexIteratorPosition, "{error}");
    }
    for source in [
        "new Set('a'.matchAll(/a/g));",
        "new Map('a1'.matchAll(/([a-z])(\\d)/g));",
        "Object.fromEntries('a1'.matchAll(/([a-z])(\\d)/g));",
    ] {
        lash_typescript::validate(source).unwrap_or_else(|error| panic!("{source}: {error}"));
    }
}

rejection_test!(
    rejects_non_string_regexp_constructor_literal,
    "new RegExp(/a/g);",
    Code::NewUnsupported
);
rejection_test!(
    rejects_getters,
    "const x = { get value() { return 1; } };",
    Code::AccessorUnsupported
);
rejection_test!(
    rejects_setters,
    "const x = { set value(v) {} };",
    Code::AccessorUnsupported
);
rejection_test!(
    rejects_prototype_access,
    "const x = Object.prototype;",
    Code::PrototypeMutationUnsupported
);
rejection_test!(rejects_this, "const x = this;", Code::ThisUnsupported);
rejection_test!(
    rejects_namespaces,
    "namespace N {}",
    Code::NamespaceUnsupported
);
rejection_test!(
    rejects_decorators,
    "@sealed class A {}",
    Code::DecoratorUnsupported
);
rejection_test!(
    rejects_dynamic_import,
    "import('x');",
    Code::DynamicImportUnsupported
);
rejection_test!(
    rejects_static_import,
    "import x from 'x';",
    Code::ImportExportUnsupported
);
rejection_test!(
    rejects_static_export,
    "export const x = 1;",
    Code::ImportExportUnsupported
);
rejection_test!(rejects_jsx, "const x = <div />;", Code::JsxUnsupported);
rejection_test!(rejects_using, "using x = resource;", Code::UsingUnsupported);
rejection_test!(
    rejects_arbitrary_new,
    "new WeakMap();",
    Code::NewUnsupported
);
rejection_test!(
    rejects_unsupported_await,
    "await 1;",
    Code::AwaitUnsupported
);
rejection_test!(
    rejects_unawaited_tool_call,
    "web.fetch({});",
    Code::AwaitRequired
);
rejection_test!(rejects_unawaited_sleep, "sleep(1);", Code::AwaitRequired);

#[test]
fn await_permission_stops_at_nested_function_boundaries() {
    let operations = [
        "sleep(1)",
        "waitSignal('ready')",
        "registerTrigger({})",
        "web.fetch({ url: 'https://example.test' })",
    ];
    for operation in operations {
        for source in [
            format!("await (async () => {{ {operation}; }})();"),
            format!("await Promise.all([1].map(async (item) => {{ {operation}; return item; }}));"),
        ] {
            let error = lash_typescript::validate(&source).expect_err(&source);
            assert_eq!(error.code, Code::AwaitRequired, "{source}: {error}");
        }
    }
}

#[test]
fn nested_async_shapes_accept_locally_awaited_effects() {
    for source in [
        "await (async () => { await sleep(1); })();",
        "await Promise.all([1].map(async (item) => { await sleep(1); return item; }));",
    ] {
        lash_typescript::validate(source).unwrap_or_else(|error| panic!("{source}: {error}"));
    }
}

#[test]
fn iterable_sink_permission_stops_at_nested_function_boundaries() {
    let source = "Array.from(function nested() { return 'a'.matchAll(/a/g); });";
    let error = lash_typescript::validate(source).expect_err(source);
    assert_eq!(error.code, Code::RegexIteratorPosition, "{error}");
}

rejection_test!(
    rejects_missing_literal_method,
    "'x'.missing();",
    Code::MethodUnsupported
);
rejection_test!(
    rejects_unknown_method_on_bound_receiver,
    "const s = 'a,b'; s.notAMethod(',');",
    Code::MethodUnsupported
);

#[test]
fn shadowed_module_root_names_the_shadowing_binding() {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    catalog
        .add_module_operation(
            ["text"],
            "TextModule",
            "sha256",
            "tool:text/sha256",
            lashlang::TypeExpr::Any,
            lashlang::TypeExpr::Any,
        )
        .expect("text module operation");
    let environment =
        lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::default())
            .with_globals(["text"]);
    let shadowed = lash_typescript::link("text.sha256({});", &environment)
        .expect_err("a session binding shadowing a real module root should explain the shadowing");
    assert_eq!(shadowed.code, Code::MethodUnsupported);
    assert_eq!(
        shadowed.message,
        "local binding `text` shadows module `text`; rename the binding or call the module before binding"
    );

    for ordinary_source in [
        "const s = 'a,b'; s.notAMethod(',');",
        "function f(items) { return items.notAMethod(); }",
    ] {
        let ordinary = lash_typescript::link(ordinary_source, &environment)
            .expect_err("an ordinary local should keep the method diagnostic");
        assert_eq!(ordinary.code, Code::MethodUnsupported);
        assert_eq!(
            ordinary.message,
            "method `notAMethod` is not in the TypeScript runtime surface"
        );
    }
}

rejection_test!(
    rejects_unknown_method_on_chained_receiver,
    "'abc'.repeat(2).notAMethod(10, 'x');",
    Code::MethodUnsupported
);
rejection_test!(
    rejects_unknown_method_on_computed_receiver,
    "const xs = [['a']]; xs[0].notAMethod();",
    Code::MethodUnsupported
);
rejection_test!(
    rejects_unknown_method_under_await,
    "const s = 'a'; await s.notAMethod();",
    Code::MethodUnsupported
);
rejection_test!(
    rejects_dynamic_process_config,
    "const config = {}; const worker = defineProcess(config);",
    Code::ProcessConfigLiteralRequired
);
rejection_test!(
    rejects_yield,
    "function* f() { yield 1; }",
    Code::GeneratorUnsupported
);
rejection_test!(
    rejects_tagged_templates,
    "tag`value`;",
    Code::TaggedTemplateUnsupported
);
rejection_test!(rejects_super, "super();", Code::SuperUnsupported);
rejection_test!(
    rejects_meta_properties,
    "const x = import.meta;",
    Code::MetaPropertyUnsupported
);
rejection_test!(rejects_bigint, "const x = 1n;", Code::BigIntUnsupported);
rejection_test!(
    rejects_sequence,
    "const x = (1, 2);",
    Code::SequenceUnsupported
);
rejection_test!(
    rejects_instanceof,
    "const x = value instanceof Type;",
    Code::InstanceOfUnsupported
);
rejection_test!(rejects_debugger, "debugger;", Code::DebuggerUnsupported);
rejection_test!(
    rejects_reserved_generated_identifier,
    "const __typescript_0_a = 1;",
    Code::ReservedIdentifier
);

#[test]
fn agent_iteration_await_and_ecma_method_arities_are_accepted() {
    for source in [
        "let x = 0; x++; finish(x);",
        "let x = 1; const old = x++; finish(old);",
        "var x = 1; const {a} = {a:2}; finish([x,a]);",
        "const x = { ...{a:1}, ['b']:2, f(){return 3;} }; finish(x?.a);",
        "switch(1){case 1: break;} do {} while(false);",
        "for (let i = 0; i < 1; i++) {}",
        "const values = [1]; for (const value of values) { print(value); }",
        "await web.fetch({ url: 'https://example.com' });",
        "finish(`${'abc'.startsWith('bc', 1)}`);",
        "finish(`${'abc'.includes('b', 2)}`);",
        "finish(`${'abc'.endsWith('b', 2)}`);",
        "finish(`${'abc'.charCodeAt(0, 99)}`);",
    ] {
        lash_typescript::validate(source).expect(source);
    }
}

/// The register must name exactly what the lowerer accepts.
///
/// The documented inventory was hand-maintained and fell nine methods behind
/// the allowlist, so the register told a guest that `slice` rejects when it
/// does not. This derives the expected sentence from the lowerer and compares,
/// which is the only way a prose inventory stays true.
#[test]
fn instance_method_inventory_matches_the_lowerer() {
    let register = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))
        .expect("the register is readable");
    let documented = register
        .split("The shipped instance names are ")
        .nth(1)
        .expect("the register names its instance methods")
        .split('.')
        .next()
        .expect("the sentence ends");
    let documented = documented
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>();

    // Set equality in both directions. Documented-implies-accepted alone
    // leaves the direction that actually drifted — the allowlist growing while
    // the register stands still — unchecked, which is how the register came to
    // be nine methods behind.
    let accepted = lash_typescript::accepted_instance_methods()
        .iter()
        .map(|method| (*method).to_string())
        .collect::<std::collections::BTreeSet<_>>();
    let undocumented = accepted.difference(&documented).collect::<Vec<_>>();
    assert!(
        undocumented.is_empty(),
        "the lowerer accepts {undocumented:?}, which the register does not document"
    );
    let unaccepted = documented.difference(&accepted).collect::<Vec<_>>();
    assert!(
        unaccepted.is_empty(),
        "the register documents {unaccepted:?}, which the lowerer does not accept"
    );
    assert_eq!(documented.len(), 89);
    assert_eq!(lash_typescript::stdlib_name_count(), 149);
    for candidate in ["substr", "localeCompare", "toLocaleString", "normalize"] {
        assert!(
            !lash_typescript::accepts_instance_method(candidate),
            "`{candidate}` is not documented, so it must not be accepted"
        );
    }
    for method in lash_typescript::accepted_instance_methods() {
        assert!(
            lash_typescript::accepts_instance_method(method),
            "`{method}` is accepted by the inventory, so `accepts_instance_method` must return true"
        );
    }
}

#[test]
fn base64_globals_keep_the_dom_exception_repair_diagnostic() {
    for source in ["finish(btoa('x'));", "finish(atob('eA=='));"] {
        let error = lash_typescript::compile(source)
            .expect_err("base64 globals remain outside the exact runtime surface");
        assert_eq!(
            error.code,
            lash_typescript::DiagnosticCode::MethodUnsupported
        );
        assert!(error.to_string().contains("DOMException"), "{error}");
        assert!(error.to_string().contains("host tool"), "{error}");
    }
}

#[test]
fn retained_stdlib_rejections_carry_exact_repairs() {
    for (source, repair) in [
        (
            "finish('a'.localeCompare('b'));",
            "a < b ? -1 : a > b ? 1 : 0",
        ),
        ("finish((1).toLocaleString());", "toFixed(digits)"),
        (
            "finish('e'.normalize());",
            "Normalize text in a deterministic host tool",
        ),
        (
            "finish(JSON.parse('{}',(k,v)=>v));",
            "Parse first, then walk the returned value explicitly",
        ),
    ] {
        let error = lash_typescript::validate(source).expect_err("call remains rejected");
        assert_eq!(error.code.as_str(), "TS_METHOD_UNSUPPORTED", "{source}");
        assert!(error.to_string().contains(repair), "{source}: {error}");
    }
}

#[test]
fn date_rejections_name_the_deterministic_repair() {
    for (source, repair) in [
        ("new Date(0).getFullYear();", "d.getUTCFullYear()"),
        ("new Date(0).getHours();", "d.getUTCHours()"),
        ("new Date(0).setUTCSeconds(1);", "new Date(d.getTime() + n)"),
        ("new Date(0).toDateString();", "toISOString()"),
        ("new Date(0).toLocaleString();", "d.toISOString()"),
    ] {
        let error = lash_typescript::validate(source).expect_err("Date call remains rejected");
        assert_eq!(error.code, Code::MethodUnsupported, "{source}: {error}");
        assert!(error.to_string().contains(repair), "{source}: {error}");
    }
}

// The prototype chain. The census has claimed
// `TS_PROTOTYPE_MUTATION_UNSUPPORTED` for `__proto__` and the accessor family
// since it was written, while the guard matched only the literal name
// `prototype` — so `o.__proto__ = base` compiled and ran, landing as an
// ordinary data key that nothing ever reads through. These are the four static
// shapes; the computed one is only knowable at the access and is covered in
// `ecma_regressions.rs`.
rejection_test!(
    rejects_proto_member_write,
    "const o: any = {}; o.__proto__ = { x: 1 };",
    Code::PrototypeMutationUnsupported
);
rejection_test!(
    rejects_proto_member_read,
    "const o: any = { a: 1 }; finish(o.__proto__);",
    Code::PrototypeMutationUnsupported
);
rejection_test!(
    rejects_proto_string_property,
    "const o: any = {}; o['__proto__'] = { x: 1 };",
    Code::PrototypeMutationUnsupported
);
rejection_test!(
    rejects_proto_object_literal_key,
    "const o: any = { __proto__: { x: 1 } };",
    Code::PrototypeMutationUnsupported
);
rejection_test!(
    rejects_proto_quoted_object_literal_key,
    "const o: any = { '__proto__': { x: 1 } };",
    Code::PrototypeMutationUnsupported
);
rejection_test!(
    rejects_define_getter,
    "const o: any = {}; o.__defineGetter__('x', () => 1);",
    Code::PrototypeMutationUnsupported
);
rejection_test!(
    rejects_define_setter,
    "const o: any = {}; o.__defineSetter__('x', (v: number) => v);",
    Code::PrototypeMutationUnsupported
);
rejection_test!(
    rejects_lookup_getter,
    "const o: any = {}; finish(o.__lookupGetter__('x'));",
    Code::PrototypeMutationUnsupported
);

// An unknown static on an ECMA global is a missing method, not a tool call.
// These reported `TS_AWAIT_REQUIRED` — an instruction to add `await` to a
// method that does not exist — because the receiver was not on the list of
// names that can never be a tool module. The diagnostic now names the owner
// too, so `isError` and `fromBase64` are attributed to `Error` and
// `Uint8Array` rather than floating free.
rejection_test!(
    rejects_unknown_error_static,
    "finish(Error.isError(new Error('x')));",
    Code::MethodUnsupported
);
rejection_test!(
    rejects_unknown_typed_array_static,
    "finish(Uint8Array.fromBase64('AAA='));",
    Code::MethodUnsupported
);
rejection_test!(
    rejects_unknown_reflect_static,
    "finish(Reflect.ownKeys({}));",
    Code::MethodUnsupported
);

#[test]
fn an_unknown_ecma_static_names_its_owner() {
    let error = lash_typescript::validate("finish(Error.isError(new Error('x')));")
        .expect_err("an unknown static must reject");
    assert!(
        error.to_string().contains("`Error.isError`"),
        "the diagnostic must name the owner: {error}"
    );
}

/// A rejection the model cannot locate costs it a guess. Lashlang has echoed
/// the offending line with a caret since it shipped; TypeScript dropped the
/// span on the floor and sent `TS_CODE: message` alone.
#[test]
fn a_rejection_points_at_the_line_the_model_wrote() {
    let source = "const rows = [1, 2, 3];\nconst total = 0;\nclass Accumulator {}\n";
    let error = lash_typescript::validate(source).expect_err("classes are refused");
    let rendered = lash_typescript::format_diagnostic(source, &error);

    assert!(
        rendered.contains("--> line 3, column 1"),
        "the model must be given its own line numbers: {rendered}"
    );
    assert!(
        rendered.contains("class Accumulator {}"),
        "the offending line must be echoed back: {rendered}"
    );
    assert!(
        rendered.contains("hint: "),
        "the repair stays on its own line: {rendered}"
    );
}

/// A code used at more than one site cannot rely on the per-code table alone.
///
/// `TS_AWAIT_UNSUPPORTED` is the clearest case: every site that emits it is
/// about *what* may be awaited, and the table answered about *where* `await`
/// may appear — advice for a problem the model does not have. `TS_METHOD_
/// UNSUPPORTED` covers both "no such method" and "wrong arguments", and the
/// table's "use a method the contract lists" is actively wrong for the second:
/// the method is listed, the call shape is not.
#[test]
fn a_multi_use_code_gives_advice_that_matches_the_actual_refusal() {
    for (source, must_contain, must_not_contain) in [
        // What may be awaited, not where await may appear.
        (
            "const x = 1; finish(await x);",
            "already settled",
            "top level",
        ),
        (
            "finish(await Promise.all('nope'));",
            "build the array first",
            "top level",
        ),
        // Arity, not availability.
        (
            "finish([1].map());",
            "callback",
            "the dialect's standard-library contract lists",
        ),
        (
            "finish(JSON.stringify(1, null, 2, 3));",
            "JSON.stringify(value, replacer, space)",
            "the dialect's standard-library contract lists",
        ),
        // The iterator-sink refusal names the wrap, not "use another method".
        (
            "const it = [1].values(); finish(it);",
            "[...expr]",
            "the dialect's standard-library contract lists",
        ),
    ] {
        let error = lash_typescript::validate(source).expect_err(source);
        let rendered = error.to_string();
        assert!(
            rendered.contains(must_contain),
            "{source}: expected advice naming `{must_contain}`, got:\n{rendered}"
        );
        assert!(
            !rendered.contains(must_not_contain),
            "{source}: advice about `{must_not_contain}` does not fit this refusal:\n{rendered}"
        );
    }
}
