//! FIG-7237: source admission is total; erasure preserves explicit observations.

#[path = "../src/tests/linked.rs"]
mod linked;
#[expect(
    dead_code,
    reason = "shared machine fixture also supports leaf and reopen laws"
)]
#[path = "../src/tests/pages.rs"]
mod pages;
#[expect(
    dead_code,
    reason = "this differential uses only ordinary source execution"
)]
#[path = "../shared/helper_differential/runner.rs"]
mod runner;
#[path = "../shared/source_admission.rs"]
mod source_admission;

use runner::{Case, Runner};
use serde_json::json;

/// FIG-7237 / K-DOC-005: bounded malformed source either crosses the typed
/// refusal boundary or produces a structurally valid, serializable document.
#[test]
fn malformed_and_ambiguous_source_has_typed_admission_outcomes() {
    let mut harness = source_admission::Harness::new().expect("source admission library");
    let mut state = 7237_u64;
    for feature in 0_u16..256 {
        let mut data = vec![u8::try_from(feature).expect("one byte feature")];
        for _ in 0..31 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            data.push(state.to_le_bytes()[0]);
        }
        harness.check(&data);
    }
    println!("256 inputs, {:?}", harness.counts);
    assert!(harness.counts.admitted > 0);
    assert!(harness.counts.source_refusals > 0);
}

fn case(id: &str, source: String) -> Case {
    Case {
        id: id.into(),
        dialect: "typescript".into(),
        source,
        bits: 0,
        stride: 0,
        nanoseconds: 0,
    }
}

/// FIG-7237 / ADR 0044: compare only this deterministic admitted subset.
/// The JavaScript side is authored erasure, never regex-stripped TypeScript.
/// Literal observations independently pin both sides; Node has no enum role.
#[test]
fn deterministic_typescript_erasure_preserves_observations() {
    let runner = Runner::new("typescript").expect("machine fixture");
    let mut pairs = 0;
    for n in 0..16 {
        let variants = [
            (
                format!("type N = number; const x: N = {n}; await host.observe(x);"),
                format!("const x = {n}; await host.observe(x);"),
            ),
            (
                format!(
                    "function id<T>(x: T): T {{ return x; }} await host.observe(id<number>({n}));"
                ),
                format!("function id(x) {{ return x; }} await host.observe(id({n}));"),
            ),
            (
                format!(
                    "const x = ({{v: {n}}} satisfies {{v: number}}); await host.observe((x as {{v: number}})!.v);"
                ),
                format!("const x = {{v: {n}}}; await host.observe(x.v);"),
            ),
            (
                format!(
                    "const x: {{v?: number}} | null = {{v: {n}}}; await host.observe(x?.v ?? 0);"
                ),
                format!("const x = {{v: {n}}}; await host.observe(x?.v ?? 0);"),
            ),
            (
                format!("const id = <T,>(x: T): T => x; await host.observe(id<number>({n}));"),
                format!("const id = (x) => x; await host.observe(id({n}));"),
            ),
            (
                format!(
                    "interface Box<T> {{ v: T }} const x: Box<number> = {{v: {n}}}; await host.observe(x.v);"
                ),
                format!("const x = {{v: {n}}}; await host.observe(x.v);"),
            ),
        ];
        let expected = json!({"type":"observations", "value":[{
            "type":"float", "bits":format!("{:016x}", f64::from(n).to_bits())
        }]});
        for (typed, erased) in variants {
            for source in [typed, erased] {
                let case = case("erasure", source);
                let observed = runner.run(&case).expect("admitted deterministic source");
                assert_eq!(observed.result, expected, "{}", case.source);
            }
            pairs += 1;
        }
        // TS parses a < b > (c) as a generic call, even with these spaces.
        // Its erased JS is a(c), not the same comparison-looking tokens.
        let generic = format!(
            "function a<T>(x: T): T {{ return x; }} type b = number; const c = {n}; await host.observe(a < b > (c));"
        );
        let erased =
            format!("function a(x) {{ return x; }} const c = {n}; await host.observe(a(c));");
        for source in [generic, erased] {
            let case = case("generic-ambiguity", source);
            let observed = runner.run(&case).expect("admitted generic call");
            assert_eq!(observed.result, expected, "{}", case.source);
        }
        pairs += 1;
        // Parentheses force a comparison; both authored forms keep them.
        let comparison = format!(
            "const a: number = {n}; const b = 20; const c = 0; await host.observe((a < b) > c);"
        );
        let erased =
            format!("const a = {n}; const b = 20; const c = 0; await host.observe((a < b) > c);");
        for source in [comparison, erased] {
            let case = case("comparison", source);
            let observed = runner.run(&case).expect("admitted comparison");
            assert_eq!(
                observed.result,
                json!({"type":"observations", "value":[{"type":"bool", "value":true}]}),
                "{}",
                case.source
            );
        }
        pairs += 1;
    }
    println!("{pairs} erasure pairs, {} machine executions", pairs * 2);
}

/// FIG-7237: ordinary enum initializers execute once in source order, const-enum member reads inline constants.
#[test]
fn enum_initializer_order_and_const_enum_inlining_are_observable() {
    let runner = Runner::new("typescript").expect("machine fixture");
    let case = case(
        "enum-order",
        r#"
        let log = '';
        function mark(label: string, n: number): number { log += label; return n; }
        enum E { A = mark('a', 2), B = mark('b', 3) }
        const enum C { A = 4, B }
        await host.observe(log === 'ab' && E.A === 2 && E.B === 3 && C.B === 5);
    "#
        .into(),
    );
    let observed = runner.run(&case).expect("admitted enum fixture");
    assert_eq!(
        observed.result,
        json!({"type":"observations", "value":[{"type":"bool", "value":true}]})
    );
}

/// FIG-7237 regression: an enum is a record, so numeric reverse lookup must
/// not emit list.len/list.get even in a statically unreachable fast path.
#[test]
fn enum_reverse_lookup_uses_record_semantics() {
    let runner = Runner::new("typescript").expect("machine fixture");
    let case = case(
        "enum-reverse",
        "enum E { A = 2 } await host.observe(E[2] === 'A');".into(),
    );
    let observed = runner.run(&case).expect("enum reverse lookup is admitted");
    assert_eq!(
        observed.result,
        json!({"type":"observations", "value":[{"type":"bool", "value":true}]})
    );
}

/// FIG-7714 / K-ADM-005: literals are checked after TypeScript lowering.
#[test]
fn typescript_enum_literals_are_checked_at_admission() {
    enum_admission(
        "typescript",
        &[
            (r#"apps.linear.choose("wrong@x");"#, Some("/account")),
            (
                r#"await apps.linear.search({account: "wrong@x"});"#,
                Some("/input/account"),
            ),
            (r#"apps.linear.as("wrong@x");"#, Some("/account")),
            (
                r#"apps.linear.Account.choose("wrong@x");"#,
                Some("/account"),
            ),
            (
                r#"apps.linear.as("a@x").route("wrong@x");"#,
                Some("/account"),
            ),
            (r#"apps.linear.choose("a@x");"#, None),
            (
                r#"const value = "wrong@x"; apps.linear.choose(value);"#,
                None,
            ),
            (r#"apps.linear.choose("wrong" + "@x");"#, None),
            (r#"await apps.linear.search({account: "b@x"});"#, None),
            (
                r#"const value = "wrong@x"; await apps.linear.search({account: value});"#,
                None,
            ),
            (
                r#"const input = {account: "wrong@x"}; await apps.linear.search(input);"#,
                None,
            ),
            (
                r#"function dynamic(value: any) { apps.linear.choose(value); }"#,
                None,
            ),
        ],
    );
}

/// FIG-7714 / K-ADM-005: the same rule checks Python's lowered call forms.
#[test]
fn python_enum_literals_are_checked_at_admission() {
    enum_admission(
        "python",
        &[
            (r#"apps.linear.choose("wrong@x")"#, Some("/account")),
            (
                r#"await apps.linear.search(account="wrong@x")"#,
                Some("/input/account"),
            ),
            (r#"apps.linear.for_account("wrong@x")"#, Some("/account")),
            (r#"apps.linear.Account.choose("wrong@x")"#, Some("/account")),
            (
                r#"apps.linear.for_account("a@x").route("wrong@x")"#,
                Some("/account"),
            ),
            (r#"apps.linear.choose("a@x")"#, None),
            ("value = 'wrong@x'\napps.linear.choose(value)", None),
            (r#"apps.linear.choose("wrong" + "@x")"#, None),
            (r#"await apps.linear.search(account="b@x")"#, None),
            (
                "value = 'wrong@x'\nawait apps.linear.search(account=value)",
                None,
            ),
            (
                "input = {'account': 'wrong@x'}\nawait apps.linear.search(input)",
                None,
            ),
            ("def dynamic(value):\n    apps.linear.choose(value)", None),
        ],
    );
}

#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "scripted law fixtures fail immediately when their setup or saved execution is invalid"
)]
fn enum_admission(dialect: &str, cases: &[(&str, Option<&str>)]) {
    use lash_kernel_dialect::{
        CellEnvironment, Environment, FunctionMember, Mode, NamespaceKind, NamespaceMember,
        Namespaces, OperationSignature, TypeMember, ValueType,
    };
    use lash_kernel_value::package::Visibility;
    use lash_kernel_value::{
        Convention, Name, OperationMode, PackagePath, Param, QualifiedName, RecordRest, RecordType,
        RecordTypeField, Segment, Signature, TurnControls, Type,
    };
    use std::collections::BTreeMap;
    let built = lash_ext_helper_compiler::library::Library::of(match dialect {
        "typescript" => lash_ext_helper_compiler::library::Dialect::TypeScript,
        "python" => lash_ext_helper_compiler::library::Dialect::Python,
        _ => unreachable!(),
    })
    .unwrap();
    let library = &built.names;
    let param = |name: &str, ty| Param {
        name: Name::new(name),
        ty,
        optional: false,
    };
    let account = Type::Enum(vec!["a@x".into(), "b@x".into()]);
    let signature = Signature {
        params: vec![param("account", account.clone())],
        result: Type::Any,
    };
    let brand = lash_kernel_dialect::brand_type("apps.linear", "account");
    let constructor = Signature {
        result: brand.clone(),
        ..signature.clone()
    };
    let method = Signature {
        params: vec![
            param("self", brand.clone()),
            param("account", account.clone()),
        ],
        result: Type::Any,
    };
    let function = |member: &str, type_member: Option<&str>, signature: Signature| FunctionMember {
        name: QualifiedName {
            package: PackagePath::parse("apps.linear").unwrap(),
            member: Segment::new(member).unwrap(),
            type_member: type_member.map(|name| Segment::new(name).unwrap()),
        },
        convention: Convention::Direct,
        signature,
        asynchronous: false,
        rest: false,
    };
    let alias = if dialect == "typescript" {
        "as"
    } else {
        "for_account"
    };
    let members = BTreeMap::from([
        (
            Segment::new("choose").unwrap(),
            NamespaceKind::Function(function("choose", None, signature.clone())),
        ),
        (
            Segment::new(alias).unwrap(),
            NamespaceKind::Function(function(alias, None, constructor)),
        ),
        (
            Segment::new("account").unwrap(),
            NamespaceKind::Type(ValueType {
                representation: brand,
                members: BTreeMap::from([
                    (
                        Segment::new("choose").unwrap(),
                        TypeMember::Static(function("account", Some("choose"), signature)),
                    ),
                    (
                        Segment::new("route").unwrap(),
                        TypeMember::Method(function("account", Some("route"), method)),
                    ),
                ]),
            }),
        ),
        (
            Segment::new("search").unwrap(),
            NamespaceKind::Operation(OperationSignature {
                signature: Signature {
                    params: vec![param(
                        "input",
                        Type::Record(RecordType {
                            fields: vec![RecordTypeField {
                                name: "account".into(),
                                ty: account.clone(),
                                optional: false,
                            }],
                            rest: RecordRest::Closed,
                        }),
                    )],
                    result: Type::Any,
                },
                controls: TurnControls::none(),
                mode: OperationMode::Waited,
            }),
        ),
    ])
    .into_iter()
    .map(|(name, kind)| {
        (
            name,
            NamespaceMember {
                visibility: Visibility::Public,
                kind,
            },
        )
    })
    .collect();
    let namespaces = Namespaces::new(
        BTreeMap::from([(PackagePath::parse("apps.linear").unwrap(), members)]),
        Default::default(),
        &|_| false,
    )
    .unwrap();
    let environment = Environment::new(library, &namespaces);
    let mut missed = Vec::new();
    let mut retained = None;
    for &(source, path) in cases {
        let mode = Mode::Cell(CellEnvironment::first());
        let lowered = match dialect {
            "typescript" => lash_dialect_typescript::lower(source, &environment, mode)
                .map_err(|error| error.to_string()),
            "python" => lash_dialect_python::lower(source, &environment, mode)
                .map_err(|error| error.to_string()),
            _ => unreachable!(),
        }
        .unwrap_or_else(|error| panic!("{source}: {error}"));
        let admission = lash_kernel_check::admit(
            &lowered.document,
            &lash_kernel_check::Environment::new(library),
        );
        match (path, admission) {
            (Some(path), Err(refusal)) => {
                assert!(
                    refusal.errors.iter().any(|error| matches!(&error.reason,
                    lash_kernel_check::RefusalReason::ArgumentType { param, expected, .. }
                    if param.as_str() == path && expected == &account)),
                    "{source}: {refusal}"
                );
                println!("refused {source}: {refusal}");
            }
            (Some(_), Ok(_)) => {
                println!("WRONGLY ADMITTED {source}");
                missed.push(source);
            }
            (None, result) => {
                result.unwrap_or_else(|refusal| panic!("{source}: {refusal}"));
            }
        }
        // Retained document readback derives a view, without fresh admission.
        let saved =
            lash_kernel_doc::parse_document(&lash_kernel_doc::print_document(&lowered.document))
                .unwrap();
        lash_kernel_check::derive(&saved, library).expect("saved document remains inspectable");
        if source == cases[0].0 {
            retained = Some(saved);
        }
    }
    assert!(
        missed.is_empty(),
        "wrong enum literals admitted: {missed:?}"
    );
    retained_enum_call(dialect, retained.unwrap(), &built, &namespaces);
}

#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "scripted law fixtures fail immediately when their setup or saved execution is invalid"
)]
fn retained_enum_call(
    dialect: &str,
    saved: lash_kernel_doc::Document,
    built: &lash_ext_helper_compiler::library::Library,
    namespaces: &lash_kernel_dialect::Namespaces,
) {
    use lash_kernel_doc::{Datum, Document, Export, FunctionExport, StartConvention};
    use lash_kernel_value::package::{Fill, Link};
    use lash_kernel_value::{ExportName, LinkedStart, Name, StartSelector};
    use lash_kernel_vm::{
        Bounds, CellStart, End, Host, Machine, PreparedLibrary, Program, Step, Target,
    };
    use std::{collections::BTreeMap, sync::Arc};
    let printed = match dialect {
        "typescript" => lash_dialect_typescript::print(
            &saved,
            None,
            &built.names,
            namespaces,
            Default::default(),
        ),
        "python" => {
            lash_dialect_python::print(&saved, None, &built.names, namespaces, Default::default())
        }
        _ => unreachable!(),
    }
    .expect("saved wrong literal still prints");
    let read = match dialect {
        "typescript" => lash_dialect_typescript::read(
            &printed.source,
            &saved,
            &built.names,
            namespaces,
            Default::default(),
            None,
        )
        .map_err(|error| error.to_string()),
        "python" => lash_dialect_python::read(
            &printed.source,
            &saved,
            &built.names,
            namespaces,
            Default::default(),
            None,
        )
        .map_err(|error| error.to_string()),
        _ => unreachable!(),
    }
    .expect("saved wrong literal still reads")
    .0;
    assert_eq!(read.identity(), saved.identity());
    let definition = lash_kernel_doc::parse_definition(
        "function choose(account: Enum(\"a@x\", \"b@x\")) -> Any\nkernel 1\nmetering statements\nbody { return null }"
    ).unwrap();
    let mut provider = Document::new(saved.numbers, vec![]);
    provider.exports.insert(
        Name::new("choose"),
        Export::Function(FunctionExport {
            function: definition.identity(),
            signature: definition.signature.clone(),
            convention: StartConvention::Direct,
            asynchronous: false,
            rest: false,
        }),
    );
    provider.functions.insert(definition.identity(), definition);
    let leaf = Link {
        document: provider.identity(),
        fills: BTreeMap::new(),
        helper_fills: BTreeMap::new(),
    };
    let root = Link {
        document: saved.identity(),
        helper_fills: BTreeMap::new(),
        fills: saved
            .imports
            .keys()
            .map(|slot| {
                (
                    *slot,
                    Fill::Export {
                        link: leaf.identity(),
                        export: ExportName {
                            name: Name::new("choose"),
                            member: None,
                        },
                    },
                )
            })
            .collect(),
    };
    lash_kernel_check::WorkflowDocument::entered(
        saved.clone(),
        LinkedStart {
            link: root.identity(),
            selector: StartSelector::Main,
        },
        &built.names,
    )
    .expect("stored workflow opens without fresh literal admission");
    let program = Program::prepare(
        root.identity(),
        BTreeMap::from([
            (saved.identity(), Arc::new(saved)),
            (provider.identity(), Arc::new(provider)),
        ]),
        &BTreeMap::from([(root.identity(), root), (leaf.identity(), leaf)]),
        pages::CLOSURE,
        PreparedLibrary::new(Arc::new(built.registry.clone())),
    )
    .unwrap();
    let mut machine = pages::start(
        program,
        Bounds {
            memory_meter: lash_kernel_vm::MemoryMeter::On,
            charge: lash_kernel_vm::Charge::Exact,
            charge_limit: 1_000_000,
            memory: 64 << 20,
            call_depth: Bounds::DEFAULT_CALL_DEPTH,
            live_tasks: 10,
            requests_per_park: 10,
            join_members: 10,
        },
        CellStart {
            target: Target::Main,
            args: vec![],
            session: None,
            owner: pages::OWNER,
            modes: Default::default(),
            host: vec![],
        },
    )
    .unwrap();
    struct Quiet;
    impl Host for Quiet {
        fn print(&mut self, _: &Datum) {}
        fn cancel_requested(&mut self) -> bool {
            false
        }
    }
    let end = loop {
        match machine.run(&mut Quiet, u64::MAX).unwrap() {
            Step::Slice => {}
            Step::Ended(end) => break end,
            other => panic!("wrong account reached an operation: {other:?}"),
        }
    };
    let End::Error(error) = end else {
        panic!("wrong account ran: {end:?}");
    };
    let message = format!("{error:?}");
    assert!(
        message.contains("function_argument") && message.contains("account"),
        "{message}"
    );
    println!("{dialect} saved call: {message}");
}
