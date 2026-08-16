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
    rejects_regular_expressions,
    "const r = /x/;",
    Code::RegExpUnsupported
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
rejection_test!(rejects_enums, "enum E { A }", Code::EnumUnsupported);
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
    rejects_array_from_mapping_callback,
    "Array.from('ab', value => value.toUpperCase());",
    Code::MethodUnsupported
);
rejection_test!(
    rejects_missing_runtime_property,
    "finish(Math.PI);",
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
        .split("The shipped instance methods are ")
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
    let claimed = register
        .split("37 static methods and\n")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|count| count.parse::<usize>().ok())
        .expect("the register states an instance-method count");
    assert_eq!(
        claimed,
        documented.len(),
        "the stated instance-method count must match the documented list"
    );
    for candidate in [
        "sort", "pop", "push", "shift", "unshift", "reduce", "flat", "substr",
    ] {
        assert!(
            !lash_typescript::accepts_instance_method(candidate),
            "`{candidate}` is not documented, so it must not be accepted"
        );
    }
}
