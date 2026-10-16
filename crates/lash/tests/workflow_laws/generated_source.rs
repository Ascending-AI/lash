//! Host integration of the independent exact-source operations (FIG-6205).
use lash::dialect::{GeneratedSourceLimits, PrintOptions, PrintedSource, SourceFormat};
use lash::workflow::document::{self as k, Document, Node, Site, Unit};
use lash::workflow::{WorkflowAdmissionRefusal, WorkflowDocument, WorkflowDocumentEntry};

fn local(document: &WorkflowDocument, format: SourceFormat, maps: bool) -> PrintedSource {
    let options = PrintOptions {
        source_map: maps,
        ..Default::default()
    };
    match format {
        SourceFormat::TypeScriptV1 => lash::typescript::print(document.document(), options),
        SourceFormat::PythonV1 => lash::python::print(document.document(), options),
    }
    .expect("local print succeeds")
}

fn decode(source: &str, base: &Document, format: SourceFormat) -> Document {
    match format {
        SourceFormat::TypeScriptV1 => lash::typescript::read(source, base, Default::default())
            .expect("TypeScript reads against its base without a library"),
        SourceFormat::PythonV1 => lash::python::read(source, base, Default::default())
            .expect("Python reads against its base without a library"),
    }
}

fn exact(original: &Document, decoded: &Document) {
    assert_eq!(original.canonical_bytes(), decoded.canonical_bytes());
}

fn sites(document: &Document) -> Vec<Site> {
    fn walk(node: Node<'_>, at: Site, out: &mut Vec<Site>) {
        out.push(at.clone());
        for (index, child) in (0u32..).zip(node.children()) {
            walk(child, at.child(index), out);
        }
    }
    let mut out = Vec::new();
    walk(
        Node::Block(&document.main),
        Site::new(Unit::Main, vec![]),
        &mut out,
    );
    for (name, function) in &document.functions {
        walk(
            Node::Block(&function.body),
            Site::new(Unit::Function(name.clone()), vec![]),
            &mut out,
        );
    }
    out.sort();
    out
}

/// The same TS-origin, Python-origin and host-built documents survive both
/// formats and both transport paths. Requested maps survive IPC unchanged;
/// default printing sends no map and is independent of ordinary cell caps.
#[tokio::test]
async fn both_dialects_and_transports_preserve_exact_documents_and_optional_maps() {
    let embedding = lash_vm_worker::built_dialects(&Default::default()).expect("embedding");
    let registry = embedding.registry().expect("registry");
    let environment = lash::dialect::Environment {
        library: embedding.library(),
        effects: &Default::default(),
        tool_roots: &Default::default(),
        controls: &Default::default(),
    };
    let ts = lash::typescript::lower(
        "const answer = [1, 2].map(x => x + 1);",
        &environment,
        lash::dialect::Mode::Cell(lash::dialect::CellEnvironment::first()),
    )
    .expect("TS origin")
    .document
    .into_document();
    let py = lash::python::lower(
        "answer = [1, 2]\nanswer.append(3)\n",
        &environment,
        lash::dialect::Mode::Cell(lash::dialect::CellEnvironment::first()),
    )
    .expect("Python origin")
    .document
    .into_document();
    let mut host = k::parse_document(super::GUARDED).expect("host tree");
    host.private_bindings.insert(k::Name::new("日本語"));
    // Exceed the ordinary cell cap. This is a whole-workflow operation.
    host.main.insert(
        0,
        k::Stmt::Let {
            name: k::Name::new("日本語"),
            value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Text("λ".repeat(40_000)))),
        },
    );
    let service = lash::vm::WorkerService::default();
    for document in [ts, py, host] {
        let view = WorkflowDocument::derive(
            document.clone(),
            WorkflowDocumentEntry::Main,
            registry.as_ref(),
        )
        .expect("derive");
        for format in [SourceFormat::TypeScriptV1, SourceFormat::PythonV1] {
            let mut without_map = None;
            for maps in [false, true] {
                let printed = local(&view, format, maps);
                exact(&document, &decode(&printed.source, &document, format));
                let remote = service
                    .print(
                        &document,
                        format,
                        PrintOptions {
                            source_map: maps,
                            ..Default::default()
                        },
                    )
                    .await
                    .expect("worker print");
                assert_eq!(
                    remote, printed,
                    "local and worker agree including final spans"
                );
                let decoded = service
                    .read_exported(&remote.source, &document, format, Default::default())
                    .await
                    .expect("worker decode");
                exact(&document, &decoded);
                assert_eq!(printed.format, format);
                if let Some(map) = &printed.source_map {
                    assert!(maps);
                    assert_eq!(map.document, document.identity());
                    assert_eq!(map.language, format.language());
                    map.validate(&document, &printed.source).unwrap();
                    let mut mapped: Vec<_> =
                        map.sites.iter().map(|entry| entry.site.clone()).collect();
                    mapped.sort();
                    assert_eq!(mapped, sites(&document));
                    for entry in &map.sites {
                        assert!(!entry.ranges.is_empty() || entry.implicit.is_some());
                        for span in &entry.ranges {
                            assert!(span.start < span.end);
                            assert!(printed.source.get(span.start..span.end).is_some());
                        }
                    }
                    assert_eq!(without_map.as_ref(), Some(&printed.source));
                } else {
                    assert!(!maps);
                    without_map = Some(printed.source);
                }
            }
        }
    }
}

/// Reading preserves unavailable dependency identities. Both dialects refuse
/// missing and mismatched requirements at normal admission.
#[tokio::test]
async fn exported_reading_preserves_dependency_identities_for_admission() {
    let embedding = lash_vm_worker::built_dialects(&Default::default()).expect("embedding");
    let registry = embedding.registry().expect("registry");
    let mut document = k::parse_document("kernel 1\nnumbers float\nmain { finish null }\n")
        .expect("host document");
    let unavailable = k::FunctionId::from_bytes([0x57; 32]);
    document.manifest.functions.insert(
        unavailable,
        k::FunctionName::new("missing.helper").expect("qualified name"),
    );
    let mismatched = k::parse_document("kernel 1\nnumbers float\neffect remote.answer() -> Int\nmain { let got = perform remote.answer() as Int finish got }\n").expect("mismatched dependency document");
    let mut checker = lash::workflow::graph::Environment::new(registry.as_ref());
    checker.effects.insert(
        k::EffectName::new("remote.answer").expect("effect name"),
        k::Signature {
            params: vec![],
            result: k::Type::Text,
        },
    );
    let service = lash::vm::WorkerService::default();
    for (document, expected_code) in [
        (document, "missing_function"),
        (mismatched, "effect_signature"),
    ] {
        for format in [SourceFormat::TypeScriptV1, SourceFormat::PythonV1] {
            let printed = service
                .print(&document, format, Default::default())
                .await
                .expect("print needs no helpers");
            exact(&document, &decode(&printed.source, &document, format));
            let decoded = service
                .read_exported(
                    &printed.source,
                    &document,
                    format,
                    GeneratedSourceLimits::default(),
                )
                .await
                .expect("decoding needs no helpers");
            exact(&document, &decoded);
            let refusal = WorkflowDocument::admit(decoded, WorkflowDocumentEntry::Main, &checker)
                .expect_err("dependency mismatch refuses admission");
            assert!(
                matches!(refusal, WorkflowAdmissionRefusal::Document(ref refusal) if refusal.errors.iter().any(|error| error.reason.code() == expected_code))
            );
        }
    }
}

/// Exact workflow import must not construct an ambiguous name catalogue
/// before reading dependencies that already carry their complete identities.
#[tokio::test]
async fn exact_host_import_does_not_resolve_ambiguous_library_labels() {
    let embedding = lash_vm_worker::built_dialects(&Default::default()).expect("embedding");
    let definition = embedding
        .registry()
        .expect("registry")
        .iter()
        .find(|(_, registered)| {
            matches!(
                registered.definition().implementation,
                k::Implementation::Native
            )
        })
        .map(|(_, registered)| registered.definition().as_ref().clone())
        .expect("native definition");
    let mut registry = k::FunctionRegistry::declarations();
    registry
        .register(definition.clone(), None)
        .expect("first identity");
    let mut other = definition;
    other.native_version += 1;
    registry
        .register(other, None)
        .expect("second identity, same label");
    assert!(lash::dialect::NamedLibrary::from_registry(&registry).is_err());
    let stores = std::sync::Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite memory"),
    );
    lash_core::testing::process_execution_env_fixture(stores.process_env_store().as_ref()).await;
    let workers = lash::vm::WorkerService::default();
    let bounds = workers.config().run_bounds;
    let core = lash::LashCore::standard_builder(lash_conformance::backend_over(stores))
        .plugin(std::sync::Arc::new(
            lash::vm::KernelProcessPluginFactory::new(
                workers,
                lash::vm::TYPESCRIPT_DIALECT,
                bounds,
            )
            .with_functions(std::sync::Arc::new(registry)),
        ))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash_core::testing::runtime_lease_owner())
        .expect("core");
    let document =
        k::parse_document("kernel 1\nnumbers float\nmain { finish null }\n").expect("document");
    let printed = lash::typescript::print(&document, Default::default()).expect("print");
    exact(
        &document,
        &decode(&printed.source, &document, SourceFormat::TypeScriptV1),
    );
    let env = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(1))
        .build()
        .expect("environment");
    let imported = core
        .host_artifacts()
        .import_exported_workflow(&printed.source, SourceFormat::TypeScriptV1, &document, &env)
        .await
        .expect("import");
    let lash::workflow::WorkflowImport::Read { document: imported } = imported else {
        panic!("full identities must bypass name resolution: {imported:?}");
    };
    exact(
        &document,
        &imported.to_document().expect("explicit typed read"),
    );
    core.shutdown().await.expect("shutdown");
}

/// FIG-6322, FIG-6378 (HELP-LABEL): a reader holding only a shipped document
/// and the runtime's serialized declarations draws real TypeScript and Python
/// library callees with their labels. It needs no engine, registry, native
/// code or parser: the checked document carries its declarations, and labels
/// join by the callee's use-line name, a removed helper showing its bare name.
#[test]
fn a_reader_draws_labelled_typescript_and_python_callees_from_shipped_declarations() {
    use lash::helper_labels::{labels, title};
    use lash::workflow::view::{CalleeView, derive};
    let embedding = lash_vm_worker::built_dialects(&Default::default()).expect("embedding");
    let registry = embedding.registry().expect("registry");
    let environment = lash::dialect::Environment {
        library: embedding.library(),
        effects: &Default::default(),
        tool_roots: &Default::default(),
        controls: &Default::default(),
    };
    let typescript = lash::typescript::lower(
        "export async function digest(items: string[], count: number) { let total = count; for (const item of items) { total = total + item.length; } return total; }",
        &environment,
        lash::dialect::Mode::Module,
    ).expect("import TypeScript workflow").document.into_document();
    let python = lash::python::lower(
        "answer = [1, 2]\nanswer.append(3)\nshown = str(answer)\n",
        &environment,
        lash::dialect::Mode::Cell(lash::dialect::CellEnvironment::first()),
    )
    .expect("lower Python")
    .document
    .into_document();
    let shipped = [
        (
            serde_json::to_vec(&typescript).expect("ship TypeScript document"),
            WorkflowDocumentEntry::Entry {
                function: k::Name::new("digest"),
            },
            "TypeScript ",
        ),
        (
            serde_json::to_vec(&python).expect("ship Python document"),
            WorkflowDocumentEntry::Main,
            "Python ",
        ),
    ];
    let library_wire = serde_json::to_vec(&registry.export_declarations()).expect("ship library");
    drop(embedding);

    let functions: k::FunctionDeclarations =
        serde_json::from_slice(&library_wire).expect("read library declarations");
    let labels = labels().expect("the shipped labels read without a dialect");
    for (wire, entry, labelled) in shipped {
        let document: Document = serde_json::from_slice(&wire).expect("read document");
        assert!(
            WorkflowDocument::derive(
                document.clone(),
                entry.clone(),
                &k::FunctionRegistry::default()
            )
            .is_err(),
            "use lines alone do not link"
        );
        let document =
            WorkflowDocument::derive(document, entry, &functions).expect("link without engine");
        let view = derive(&document).expect("canvas without engine");
        assert!(!view.statements.is_empty());
        assert!(!view.execution_sites.is_empty());
        let titles: Vec<&str> = view
            .dataflow
            .actions
            .iter()
            .filter_map(|action| match &action.callee {
                Some(CalleeView::Library { name, .. }) => Some(title(&labels, name)),
                _ => None,
            })
            .collect();
        assert!(
            titles.iter().any(|title| title.starts_with(labelled)),
            "a real {labelled}callee is drawn with its label: {titles:?}"
        );
        let serialized = serde_json::to_value(&view).expect("view serializes");
        let decoded: lash::workflow::view::WorkflowView =
            serde_json::from_value(serialized).expect("view decodes");
        assert_eq!(view, decoded);
    }
    let removed = k::FunctionName::new("ts.removed_helper").expect("qualified name");
    assert_eq!(title(&labels, &removed), "ts.removed_helper");
}

/// FIG-6340: entry publication and main admission retain the same typed
/// output-signature difference rather than reducing it to an effect name.
#[test]
fn an_entry_signature_refusal_retains_the_same_output_difference_as_main() {
    use lash::workflow::graph::{Environment, RefusalReason, SignatureSide};
    let document = k::parse_document(
        "kernel 1\nnumbers float\neffect notify() -> Record{message: Text}\nentry worker() -> Any\nfn worker() { let sent = perform notify() as Any return sent }\nmain { finish null }\n",
    ).expect("document");
    let functions = k::FunctionRegistry::default();
    let mut environment = Environment::new(&functions);
    environment.effects.insert(
        k::EffectName::new("notify").expect("effect"),
        k::Signature {
            params: vec![],
            result: k::Type::Any,
        },
    );
    for entry in [
        WorkflowDocumentEntry::Main,
        WorkflowDocumentEntry::Entry {
            function: k::Name::new("worker"),
        },
    ] {
        let refusal = WorkflowDocument::admit(document.clone(), entry, &environment)
            .expect_err("open output refuses the declared object");
        let WorkflowAdmissionRefusal::Document(refusal) = refusal else {
            panic!("signature refusal")
        };
        let reason = refusal
            .errors
            .iter()
            .find(|error| error.reason.code() == "effect_signature")
            .expect("signature mismatch");
        let RefusalReason::EffectSignature { mismatch, .. } = &reason.reason else {
            unreachable!()
        };
        assert_eq!(mismatch.side, SignatureSide::Output);
        assert_eq!(mismatch.path, "/output");
        assert_eq!(mismatch.provided, k::Type::Any);
        assert!(matches!(&mismatch.expected, k::Type::Record(record) if record.rest.is_none()));
        let message = reason.to_string();
        assert!(
            message.contains("/output")
                && message.contains("closed object")
                && message.contains("open/unconstrained"),
            "{message}"
        );
    }
}

/// FIG-6361: only code the entry executes must link. A passed function
/// remains a template; calling or spawning it makes its faults relevant.
#[test]
fn a_definition_links_its_executed_closure_without_main_or_passed_templates() {
    let document = k::parse_document(
        r#"kernel 1
numbers float
effect count() -> Int
entry worker() -> Any
entry broken() -> Any
fn broken() { return missing }
fn worker() { return &broken }
main { let count = perform count() as Text finish count }
"#,
    )
    .expect("document");
    let functions = k::FunctionRegistry::default();
    let entry = WorkflowDocumentEntry::Entry {
        function: k::Name::new("worker"),
    };
    WorkflowDocument::derive(document.clone(), entry.clone(), &functions)
        .expect("main and the passed template are not executed by worker");
    WorkflowDocument::admit(
        document.clone(),
        entry.clone(),
        &lash::workflow::graph::Environment::new(&functions),
    )
    .expect("entry admission uses the same closure");
    for action in ["call broken()", "spawn call broken()"] {
        let mut called = document.clone();
        called.functions.get_mut(&k::Name::new("worker")).expect("worker").body =
            k::parse_document(&format!("kernel 1\nnumbers float\nfn broken() {{ return null }}\nmain {{ let got = {action} finish got }}"))
                .expect("action document").main;
        let refusal = WorkflowDocument::derive(called, entry.clone(), &functions)
            .expect_err("an executed function must link");
        assert!(
            refusal
                .to_string()
                .contains("variable `missing` is not bound here"),
            "the called template's scope fault is retained for {action}: {refusal}"
        );
    }
    assert!(WorkflowDocument::derive(document, WorkflowDocumentEntry::Main, &functions).is_err());
}
