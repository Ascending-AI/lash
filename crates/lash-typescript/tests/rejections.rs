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
rejection_test!(
    rejects_async_functions,
    "async function f() {}",
    Code::AsyncUnsupported
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
rejection_test!(rejects_var, "var x = 1;", Code::VarUnsupported);
rejection_test!(rejects_using, "using x = resource;", Code::UsingUnsupported);
rejection_test!(
    rejects_array_destructuring,
    "const [x] = value;",
    Code::DestructuringUnsupported
);
rejection_test!(
    rejects_object_destructuring,
    "const {x} = value;",
    Code::DestructuringUnsupported
);
rejection_test!(
    rejects_object_spread,
    "const x = { ...value };",
    Code::SpreadUnsupported
);
rejection_test!(rejects_call_spread, "f(...value);", Code::SpreadUnsupported);
rejection_test!(
    rejects_optional_chaining,
    "const x = value?.field;",
    Code::OptionalChainingUnsupported
);
rejection_test!(rejects_new, "new Date();", Code::NewUnsupported);
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
    rejects_switch,
    "switch (x) { case 1: break; }",
    Code::SwitchUnsupported
);
rejection_test!(
    rejects_do_while,
    "do {} while (false);",
    Code::DoWhileUnsupported
);
rejection_test!(
    rejects_for_in,
    "for (const key in value) {}",
    Code::ForInUnsupported
);
rejection_test!(
    rejects_delete,
    "delete value.field;",
    Code::DeleteUnsupported
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
rejection_test!(
    rejects_computed_properties,
    "const x = { [key]: 1 };",
    Code::ComputedPropertyUnsupported
);
rejection_test!(
    rejects_object_methods,
    "const x = { method() {} };",
    Code::ObjectMethodUnsupported
);
rejection_test!(rejects_super, "super();", Code::SuperUnsupported);
rejection_test!(
    rejects_meta_properties,
    "const x = import.meta;",
    Code::MetaPropertyUnsupported
);
rejection_test!(rejects_bigint, "const x = 1n;", Code::BigIntUnsupported);
rejection_test!(
    rejects_compound_assignment,
    "x += 1;",
    Code::AssignmentOperatorUnsupported
);
rejection_test!(
    rejects_sequence,
    "const x = (1, 2);",
    Code::SequenceUnsupported
);
rejection_test!(
    rejects_bitwise,
    "const x = 1 | 2;",
    Code::BitwiseUnsupported
);
rejection_test!(
    rejects_exponentiation,
    "const x = 2 ** 3;",
    Code::ExponentiationUnsupported
);
rejection_test!(
    rejects_in_operator,
    "const x = 'a' in value;",
    Code::InOperatorUnsupported
);
rejection_test!(
    rejects_instanceof,
    "const x = value instanceof Type;",
    Code::InstanceOfUnsupported
);
rejection_test!(rejects_debugger, "debugger;", Code::DebuggerUnsupported);
rejection_test!(
    rejects_empty_catch_binding,
    "try {} catch {}",
    Code::EmptyCatchBindingUnsupported
);

rejection_test!(
    rejects_compound_index_assignment,
    "const a = [1]; a[0] += 5;",
    Code::AssignmentOperatorUnsupported
);
rejection_test!(
    rejects_reserved_generated_identifier,
    "const __typescript_0_a = 1;",
    Code::ReservedIdentifier
);

#[test]
fn agent_iteration_await_and_ecma_method_arities_are_accepted() {
    for source in [
        "let x = 0; x++; finish(x);",
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

rejection_test!(
    rejects_update_in_value_position,
    "let x = 1; const old = x++;",
    Code::UpdateUnsupported
);
