//! Host integration of the independent exact-source operations (FIG-6205).
use lash::dialect::{PrintOptions, PrintedSource, SourceFormat};
use lash::workflow::WorkflowDocument;
use lash::workflow::document::{self as k, Document, Node, Site};

fn local(
    document: &WorkflowDocument,
    format: SourceFormat,
    maps: bool,
    library: &dyn lash::dialect::Library,
) -> Result<PrintedSource, lash::dialect::PrintError> {
    let options = PrintOptions {
        source_map: maps,
        ..Default::default()
    };
    match format {
        SourceFormat::TypeScriptV1 => {
            lash::typescript::print(document.document(), None, library, options)
        }
        SourceFormat::PythonV1 => lash::python::print(document.document(), None, library, options),
    }
}

fn decode(
    source: &str,
    base: &Document,
    format: SourceFormat,
    library: &dyn lash::dialect::Library,
) -> Document {
    match format {
        SourceFormat::TypeScriptV1 => {
            lash::typescript::read(source, base, library, Default::default(), None)
                .expect("TypeScript reads against its base")
                .0
        }
        SourceFormat::PythonV1 => {
            lash::python::read(source, base, library, Default::default(), None)
                .expect("Python reads against its base without a library")
                .0
        }
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
    for (unit, block) in document.units() {
        walk(Node::Block(block), Site::new(unit, vec![]), &mut out);
    }
    out.sort();
    out
}

/// FIG-6205: Cell documents round-trip exactly through their authored dialect
/// and both transports, with complete optional maps and whole-source caps.
/// FIG-7109: a foreign helper or number policy (TS Float, Python BySpelling)
/// and a raw kernel tree have no faithful authored representation: both
/// transports must preserve the typed PrintError::Unprintable refusal.
#[tokio::test]
async fn both_dialects_and_transports_preserve_exact_documents_and_optional_maps() {
    let embedding = lash_vm_worker::built_embedding(&Default::default()).expect("embedding");
    let environment_namespaces =
        lash::dialect::Namespaces::new(Default::default(), Default::default(), &|_| false).unwrap();
    let environment = lash::dialect::Environment::new(embedding.library(), &environment_namespaces);
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
    let mut host = k::parse_document("kernel 1\nnumbers float\nmain { let out = \"start\" try { set out = \"operator\" } catch e { set out = \"caught\" } return out }").expect("host tree");
    host.private_bindings
        .insert(lash::schema::Name::new("日本語"));
    // Exceed the ordinary cell cap. This is a whole-workflow operation.
    host.main.as_mut().unwrap().insert(
        0,
        k::Stmt::Let {
            name: lash::schema::Name::new("日本語"),
            value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Text("λ".repeat(40_000)))),
        },
    );
    // Large authored cells retain the independent whole-source cap proof;
    // the raw kernel fixture above independently pins refusal.
    let large = "λ".repeat(40_000);
    let large_ts = decode(
        &format!("const 日本語 = {large:?};"),
        &ts,
        SourceFormat::TypeScriptV1,
        embedding.library(),
    );
    let large_py = decode(
        &format!("日本語 = {large:?}\n"),
        &py,
        SourceFormat::PythonV1,
        embedding.library(),
    );
    let service = lash::vm::WorkerService::default();
    for (document, authored) in [
        (ts, Some(SourceFormat::TypeScriptV1)),
        (py, Some(SourceFormat::PythonV1)),
        (large_ts, Some(SourceFormat::TypeScriptV1)),
        (large_py, Some(SourceFormat::PythonV1)),
        (host, None),
    ] {
        let link = lash::package::Link {
            document: document.identity(),
            fills: Default::default(),
        };
        let view = WorkflowDocument::entered(
            document.clone(),
            lash::package::LinkedStart {
                link: link.identity(),
                selector: lash::package::StartSelector::Main,
            },
        )
        .expect("derive");
        for format in [SourceFormat::TypeScriptV1, SourceFormat::PythonV1] {
            let mut without_map = None;
            for maps in [false, true] {
                let printed = local(&view, format, maps, embedding.library());
                if authored != Some(format) {
                    let Err(lash::dialect::PrintError::Unprintable { site }) = printed else {
                        panic!(
                            "foreign helpers/policy or raw kernel must refuse: {format:?}: {printed:?}"
                        );
                    };
                    let remote = service
                        .print(
                            &document,
                            None,
                            format,
                            PrintOptions {
                                source_map: maps,
                                ..Default::default()
                            },
                        )
                        .await;
                    assert!(
                        matches!(remote,
                        Err(lash::vm::SourceServiceError::Print(lash::dialect::PrintError::Unprintable { site: actual })) if actual == site),
                        "worker retains the local refusal site"
                    );
                    continue;
                }
                let printed = printed.unwrap_or_else(|error| panic!("{format:?}: {error:?}"));
                exact(
                    &document,
                    &decode(&printed.source, &document, format, embedding.library()),
                );
                let remote = service
                    .print(
                        &document,
                        None,
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
                    .read_exported(&remote.source, &document, format, Default::default(), None)
                    .await
                    .expect("worker decode")
                    .0;
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
