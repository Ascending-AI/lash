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
            embedding.library(),
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

/// Independently read source is checked by the production exact module oracle,
/// including its existing allowance for private alpha-renaming.
struct CanvasReadback {
    source: String,
    document: Document,
    captions: k::Captions,
    format: SourceFormat,
}
impl lash::dialect::regions::Regions for CanvasReadback {
    fn format(&self) -> SourceFormat {
        self.format
    }
    fn whole(
        &self,
        _: &std::collections::BTreeSet<lash::schema::Name>,
    ) -> Result<lash::dialect::regions::Whole, lash::dialect::PrintError> {
        Ok(lash::dialect::regions::Whole {
            source: self.source.clone(),
            functions: Default::default(),
        })
    }
    fn read(&self, _: &str) -> Result<lash::dialect::regions::RegionRead, Option<usize>> {
        Ok(lash::dialect::regions::RegionRead {
            document: self.document.clone(),
            captions: self.captions.clone(),
            map: None,
        })
    }
}

fn canvas_site(document: &Document, wanted: impl Fn(Node<'_>) -> bool) -> Site {
    fn find(node: Node<'_>, site: Site, wanted: &impl Fn(Node<'_>) -> bool) -> Option<Site> {
        if wanted(node) {
            return Some(site);
        }
        node.children()
            .into_iter()
            .enumerate()
            .find_map(|(i, child)| find(child, site.child(i as u32), wanted))
    }
    document
        .units()
        .find_map(|(unit, body)| find(Node::Block(body), Site::new(unit, []), &wanted))
        .expect("fixture target")
}

/// FIG-7378: every canvas operation keeps authored expressions printable.
/// InsertStep covers records/scalars, bound/discarded replies and TypeScript
/// Main (the reported bug); Python inserts inside its async entry.
#[tokio::test]
async fn canvas_expressions_print_and_read_back_in_both_dialects() {
    use lash::schema::{Name, Type};
    use lash::workflow::authoring::{
        Branch, DataRecordEntry, DataValue, Operation, ValueTarget, Wrapper,
    };
    use lash::workflow::edit::{Draft, Position, Transaction};
    let embedding = lash_vm_worker::built_embedding(&Default::default()).unwrap();
    use std::sync::Arc;
    const INBOX: lash::process::ProcessConfigKey<()> =
        lash::process::ProcessConfigKey::new("canvas-inbox", lash::plugins::FormatVersion::ONE);
    let provider = Arc::new(lash::tools::StaticToolProvider::new(
        vec![
            lash::tools::ToolDefinition::raw(
                "tool:inbox_send_item",
                "inbox.send_item",
                "Accepts canvas data.",
                serde_json::json!(true),
                lash::tools::ToolDefinition::any_output(),
            )
            .unwrap()
            .declared_contract(lash_kernel_value::operations::ContractId::new("canvas/1"))
            .with_execution(std::time::Duration::from_secs(120)),
        ],
        super::Echo,
    ));
    let component = lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(INBOX.component()),
        lash::plugins::PluginSpec::new()
            .with_tool_provider(lash::tool_provider_key!("inbox"), provider.clone())
            .with_package_provider(
                lash::tool_provider_key!("inbox"),
                Arc::new(lash::package::ToolPackageProvider::new(
                    INBOX.component(),
                    lash::tool_provider_key!("inbox"),
                    [lash::package::Segment::new("inbox").unwrap()].into(),
                    provider,
                )),
            ),
    )
    .as_component(INBOX.format())
    .unwrap();
    let stores = Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap());
    let core = super::builder_with_process_tools(stores, false, super::EchoComponent::Installed)
        .await
        .process_component(Arc::new(component), INBOX)
        .build(super::node_identity("canvas-source"))
        .unwrap();
    let environment = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(1024))
        .component(INBOX, &())
        .component(lash::vm::KERNEL_SERVICES_CONFIG, &())
        .build()
        .unwrap();
    let pin = lash::process::HostArtifactPin::mint();
    let (_, request) = super::namespace(&core, &pin, &environment).await;
    let checked = core
        .host_artifacts()
        .workflow_environment(&request)
        .await
        .unwrap()
        .unwrap();
    let namespaces = checked.namespaces();
    let service = lash::vm::WorkerService::default();
    let text = |s: &str| DataValue::Literal(k::Literal::Text(s.into()));
    let record = DataValue::Record(vec![
        DataRecordEntry {
            field: "title".into(),
            value: text("Live run v2 extra"),
        },
        DataRecordEntry {
            field: "body".into(),
            value: text("A step added on the canvas."),
        },
    ]);
    let mut failures = Vec::new();
    let mut readbacks = 0;
    for format in [SourceFormat::TypeScriptV1, SourceFormat::PythonV1] {
        let source = match format {
            SourceFormat::TypeScriptV1 => {
                "const seed = 'before'; export async function run(input: string): Promise<any> { if (true) { await inbox.sendItem({title: 'first', body: input}); } await sleep(20000); await inbox.sendItem('second'); return await inbox.sendItem('third'); }"
            }
            SourceFormat::PythonV1 => {
                "seed = 'before'\nasync def run(input: str) -> JsonValue:\n    if True:\n        await inbox.send_item(record.new({'title': 'first', 'body': input}))\n    await inbox.send_item('second')\n    return await inbox.send_item('third')\n"
            }
        };
        let imported = core
            .host_artifacts()
            .import_workflow(format.language(), source, &request, None)
            .await
            .unwrap();
        let lash::workflow::WorkflowImport::Imported { document, .. } = imported else {
            panic!("canvas source imports: {imported:?}");
        };
        let original = document.to_document().unwrap();
        let slot = original
            .imports
            .iter()
            .find(|(_, import)| import.name.member.as_str() == "send_item")
            .map(|(slot, _)| *slot)
            .unwrap();
        let base = original.identity();
        let unit = super::function_unit(&original, "run");
        let block = Site::new(unit.clone(), []);
        let body = original.units().find(|(u, _)| *u == unit).unwrap().1;
        let end = Position::before(block.child(body.len() as u32 - 1)).unwrap();
        let seed = canvas_site(
            &original,
            |node| matches!(node, Node::Stmt(k::Stmt::Let { name, .. }) if name.as_str() == "seed"),
        );
        let condition = canvas_site(&original, |node| {
            matches!(node, Node::Stmt(k::Stmt::If { .. }))
        });
        let first = canvas_site(
            &original,
            |node| matches!(node, Node::Expr(k::Expr::Act(act)) if matches!(act.kind, k::ActKind::Perform { slot: s, .. } if s == slot)),
        );
        let scalar = canvas_site(
            &original,
            |node| matches!(node, Node::Expr(k::Expr::Literal(k::Literal::Text(s))) if s == "second"),
        );
        let nested = canvas_site(
            &original,
            |node| matches!(node, Node::Expr(k::Expr::Literal(k::Literal::Text(s))) if s == "first"),
        );
        let path = nested.path[first.path.len() + 1..].to_vec();
        let mut rows = Vec::new();
        for (value_name, argument) in [("record", record.clone()), ("scalar", text("scalar"))] {
            for binding in [None, Some(Name::new("extra"))] {
                let mut positions = vec![("entry", end.clone())];
                if format == SourceFormat::TypeScriptV1 {
                    positions.push(("main", Position::end(Site::new(k::Unit::Main, []))));
                }
                for (where_, at) in positions {
                    rows.push((
                        format!("insert_{where_}_{value_name}_{binding:?}"),
                        Operation::InsertStep {
                            base,
                            at,
                            slot,
                            args: vec![argument.clone()],
                            result: Type::Any,
                            binding: binding.clone(),
                        },
                    ));
                }
            }
        }
        rows.extend([
            (
                "insert_binding".into(),
                Operation::InsertBinding {
                    base,
                    at: Position::end(Site::new(k::Unit::Main, [])),
                    name: Name::new("added"),
                    value: record.clone(),
                },
            ),
            (
                "rename_binding".into(),
                Operation::RenameBinding {
                    base,
                    declaration: seed.clone(),
                    name: Name::new("renamed_seed"),
                },
            ),
            (
                "remove_binding".into(),
                Operation::RemoveBinding {
                    base,
                    declaration: seed.clone(),
                },
            ),
            (
                "rename_export".into(),
                Operation::RenameExport {
                    base,
                    from: Name::new("run"),
                    to: Name::new("revised_run"),
                },
            ),
            (
                "set_literal".into(),
                Operation::SetValue {
                    base,
                    target: ValueTarget::Literal {
                        expression: seed.child(0),
                    },
                    value: record.clone(),
                },
            ),
            (
                "set_scalar_argument_literal".into(),
                Operation::SetValue {
                    base,
                    target: ValueTarget::Literal { expression: scalar },
                    value: record.clone(),
                },
            ),
            (
                "set_initializer".into(),
                Operation::SetValue {
                    base,
                    target: ValueTarget::Initializer { statement: seed },
                    value: text("changed"),
                },
            ),
            (
                "set_record_argument".into(),
                Operation::SetValue {
                    base,
                    target: ValueTarget::ToolArgument {
                        action: first.clone(),
                        index: 0,
                        path: vec![],
                    },
                    value: record.clone(),
                },
            ),
            (
                "set_scalar_argument".into(),
                Operation::SetValue {
                    base,
                    target: ValueTarget::ToolArgument {
                        action: first.clone(),
                        index: 0,
                        path: vec![],
                    },
                    value: text("changed"),
                },
            ),
            (
                "set_nested_argument".into(),
                Operation::SetValue {
                    base,
                    target: ValueTarget::ToolArgument {
                        action: first.clone(),
                        index: 0,
                        path,
                    },
                    value: text("changed"),
                },
            ),
            (
                "remove_step".into(),
                Operation::RemoveStep {
                    base,
                    action: first.clone(),
                },
            ),
            (
                "move_step".into(),
                Operation::MoveStep {
                    base,
                    action: first.clone(),
                    to: end,
                },
            ),
            (
                "change_condition".into(),
                Operation::ChangeCondition {
                    base,
                    statement: condition.clone(),
                    condition: k::Expr::Literal(k::Literal::Bool(false)),
                },
            ),
            (
                "unwrap".into(),
                Operation::UnwrapSteps {
                    base,
                    wrapper: condition,
                    branch: Branch::Then,
                },
            ),
        ]);
        for (name, wrapper) in [
            (
                "wrap_if",
                Wrapper::If {
                    condition: k::Expr::Literal(k::Literal::Bool(true)),
                },
            ),
            (
                "wrap_for",
                Wrapper::For {
                    binding: Name::new("canvas_item"),
                    iterable: k::Expr::List(vec![k::Expr::Literal(k::Literal::Text("one".into()))]),
                },
            ),
            (
                "wrap_try",
                Wrapper::Try {
                    catch_binding: Name::new("caught"),
                },
            ),
        ] {
            rows.push((
                name.into(),
                Operation::WrapSteps {
                    base,
                    first: first.clone(),
                    last: first.clone(),
                    wrapper,
                },
            ));
        }
        let (replacement, _, _) = service
            .import_workflow(
                format.language(),
                &source.replace("before", "replacement"),
                namespaces,
                None,
            )
            .await
            .unwrap();
        rows.push((
            "replace".into(),
            Operation::ReplaceDocument {
                base,
                document: replacement,
            },
        ));
        for (name, operation) in rows {
            let mut draft = Draft::open(original.clone()).unwrap();
            if let Err(error) = draft.apply(
                &Transaction {
                    base,
                    edits: vec![operation],
                },
                &checked.checker(),
            ) {
                failures.push(format!("{format:?} {name}: edit {error:?}"));
                continue;
            }
            let mut without_map = None;
            for maps in [false, true] {
                let printed = match service
                    .print(
                        draft.document(),
                        None,
                        format,
                        PrintOptions {
                            source_map: maps,
                            ..Default::default()
                        },
                    )
                    .await
                {
                    Ok(printed) => printed,
                    Err(error) => {
                        failures.push(format!("{format:?} {name} maps={maps}: {error:?}"));
                        continue;
                    }
                };
                let (read, captions) = service
                    .read_exported(
                        &printed.source,
                        draft.document(),
                        format,
                        Default::default(),
                        None,
                    )
                    .await
                    .unwrap();
                lash::dialect::regions::print_verified(
                    draft.document(),
                    None,
                    Default::default(),
                    Default::default(),
                    &CanvasReadback {
                        source: printed.source.clone(),
                        document: read,
                        captions,
                        format,
                    },
                    embedding.library(),
                )
                .unwrap_or_else(|error| panic!("{format:?} {name}: exact readback {error:?}"));
                assert_eq!(printed.source_map.is_some(), maps);
                if let Some(map) = printed.source_map {
                    map.validate(draft.document(), &printed.source).unwrap();
                    assert_eq!(without_map.as_ref(), Some(&printed.source));
                } else {
                    without_map = Some(printed.source);
                }
                readbacks += 1;
            }
        }
    }
    core.shutdown().await.unwrap();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    println!("FIG-7378: {readbacks} exact canvas source readbacks");
}
