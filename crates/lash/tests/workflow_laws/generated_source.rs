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
        SourceFormat::TypeScriptV1 => document.typescript(options),
        SourceFormat::PythonV1 => document.python(options),
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
    assert_eq!(
        original.canonical_bytes().expect("canonical bytes"),
        decoded.canonical_bytes().expect("canonical bytes")
    );
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
        bindings: &Default::default(),
        functions: &Default::default(),
    };
    let ts = lash::typescript::lower("const answer = [1, 2].map(x => x + 1);", &environment)
        .expect("TS origin")
        .document;
    let py = lash::python::lower("answer = [1, 2]\nanswer.append(3)\n", &environment)
        .expect("Python origin")
        .document;
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
        let view =
            WorkflowDocument::derive(document.clone(), WorkflowDocumentEntry::Main, registry)
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
                    assert_eq!(map.document, document.identity().expect("identity"));
                    assert_eq!(map.format, format);
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
                registered.definition.implementation,
                k::Implementation::Native
            )
        })
        .map(|(_, registered)| registered.definition.as_ref().clone())
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
    let lash::workflow::WorkflowImport::Imported {
        document: imported, ..
    } = imported
    else {
        panic!("full identities must bypass name resolution: {imported:?}");
    };
    exact(&document, imported.document());
    core.shutdown().await.expect("shutdown");
}
