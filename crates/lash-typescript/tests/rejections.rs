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
    assert!(
        error.message.contains("[...text.matchAll(regexp)]"),
        "{error}"
    );
}

#[test]
fn match_all_rejects_non_contract_iterable_sinks() {
    for source in [
        "new Set('a'.matchAll(/a/g));",
        "new Map('a'.matchAll(/a/g));",
        "Object.fromEntries('a'.matchAll(/a/g));",
    ] {
        let error = lash_typescript::validate(source).expect_err(source);
        assert_eq!(error.code, Code::RegexIteratorPosition, "{error}");
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
    assert_eq!(documented.len(), 85);
    assert_eq!(lash_typescript::stdlib_name_count(), 144);
    for candidate in ["pop", "push", "shift", "unshift", "substr"] {
        assert!(
            !lash_typescript::accepts_instance_method(candidate),
            "`{candidate}` is not documented, so it must not be accepted"
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
        assert!(error.message.contains("DOMException"), "{error}");
        assert!(error.message.contains("host tool"), "{error}");
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
