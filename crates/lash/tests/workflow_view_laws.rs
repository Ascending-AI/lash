//! Source-language workflow views over complete Documents and public Exports.
#![expect(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "test laws"
)]
use lash::package::*;
use lash::workflow::document as k;
use std::collections::BTreeMap;

fn namespaces(
    effects: BTreeMap<k::EffectName, lash::schema::Signature>,
) -> lash::dialect::Namespaces {
    let mut packages: BTreeMap<PackagePath, BTreeMap<Segment, lash::dialect::NamespaceMember>> =
        BTreeMap::new();
    for (name, signature) in effects {
        let operation = lash_kernel_value::operation_text::parse(name.as_str()).unwrap();
        packages.entry(operation.package).or_default().insert(
            operation.name,
            lash::dialect::NamespaceMember {
                visibility: Visibility::Public,
                kind: lash::dialect::NamespaceKind::Operation(
                    OperationOffer {
                        requirement: lash_kernel_value::OperationRequirement::new(
                            signature,
                            k::ContractId::new("1"),
                        ),
                        intents: Default::default(),
                        controls: Default::default(),
                        mode: OperationMode::Waited,
                    }
                    .into(),
                ),
            },
        );
    }
    lash::dialect::Namespaces::new(packages, Default::default(), &|_| false).unwrap()
}
fn holder(
    document: k::Document,
) -> (
    lash::workflow::WorkflowDocument,
    lash::workflow::view::ViewContext,
) {
    let release = lash::vm::helper_release().expect("the authored helper release");
    let mut content = lash_kernel_package::LinkedContent::default();
    for package in lash::vm::DIALECT_SERVICE_PACKAGES.iter() {
        content.packages.insert(package.identity(), package.clone());
    }
    let fills = document
        .imports
        .iter()
        .map(|(slot, import)| {
            let fill = match &import.kind {
                lash_kernel_value::ImportKind::Operation { signature } => {
                    let offered = content
                        .packages
                        .values()
                        .find(|package| package.path == import.name.package)
                        .and_then(|package| package.members.get(&import.name.member))
                        .and_then(|member| match &member.kind {
                            MemberKind::Operation(offer) => Some(offer.requirement.clone()),
                            _ => None,
                        });
                    Fill::Operation(offered.unwrap_or_else(|| {
                        lash_kernel_value::OperationRequirement::new(
                            signature.clone(),
                            k::ContractId::new("1"),
                        )
                    }))
                }
                _ => {
                    let member = content
                        .packages
                        .values()
                        .find(|package| package.path == import.name.package)
                        .and_then(|package| package.members.get(&import.name.member))
                        .expect("an authored helper member");
                    let MemberKind::Export { link, export } = &member.kind else {
                        panic!("code helper")
                    };
                    Fill::Export {
                        link: *link,
                        export: export.clone(),
                    }
                }
            };
            (*slot, fill)
        })
        .collect();
    let link = Link {
        document: document.identity(),
        fills,
    };
    content
        .documents
        .insert(document.identity(), document.clone().into());
    content.links.insert(link.identity(), link.clone());
    let start = LinkedStart {
        link: link.identity(),
        selector: StartSelector::Main,
    };
    let mut context = lash::workflow::view::ViewContext::new(
        start.clone().into(),
        content,
        BTreeMap::new(),
        std::sync::Arc::clone(&release.helpers),
    )
    .expect("retained context")
    .with_timer_operations(lash::vm::TIMER_OPERATIONS.iter().cloned());
    context
        .add_helper_labels(&release.labels)
        .expect("build helper labels");
    (
        lash::workflow::WorkflowDocument::entered(document, start, release.helpers.as_ref())
            .expect("complete document"),
        context,
    )
}
fn export_unit(document: &k::Document, name: &str) -> k::Unit {
    let k::Export::Function(export) = &document.exports[&lash::schema::Name::new(name)] else {
        panic!("function export")
    };
    k::Unit::Library(export.function)
}
#[path = "workflow_laws/entry_comparison.rs"]
mod entry_comparison;
#[path = "workflow_laws/value_names.rs"]
mod value_names;
#[path = "workflow_laws/view_links.rs"]
mod view_links;

/// DOC-MAP: a refused sidecar must publish no executable content; successful
/// publication retains the exact checked map and source under the host pin.
#[tokio::test]
async fn a_refused_source_map_publishes_no_document_or_link() {
    use std::sync::Arc;
    let stores = Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap());
    let backend = lash_conformance::backend_over(stores);
    let workers = lash::vm::WorkerService::default();
    let bounds = workers.config().run_bounds;
    let core = lash::LashCore::standard_builder(backend)
        .plugin(Arc::new(lash::vm::KernelProcessPluginFactory::new(
            workers,
            "typescript",
            bounds,
        )))
        .serve_sessions(false)
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash_core::LeaseOwnerIdentity::opaque(
            lash_core::LeaseOwnerId::new("source-map"),
            lash_core::LeaseIncarnationId::new("source-map-boot"),
        ))
        .unwrap();
    let source = "const answer = 42;";
    let lowered = lash::typescript::lower(
        source,
        &lash::dialect::Environment::new(&lash::dialect::NamedLibrary::new(), &Default::default()),
        lash::dialect::Mode::Cell(lash::dialect::CellEnvironment::first()),
    )
    .unwrap();
    let document = lowered.document.into_document();
    let map = lowered.source_map.unwrap();
    let pin = lash::process::HostArtifactPin::mint();
    let context = PublicationContext {
        domain: PublicationDomain(core.backend().binding_identity().to_string()),
        scope: PublicationScope::Host,
        publisher: PublisherInstance("source-map-law".into()),
        root: Segment::new("law").unwrap(),
    };
    let namespace = lash::vm::WorkflowEnvironmentRequest {
        snapshot: Default::default(),
        context,
        attachment: Attachment {
            packages: Default::default(),
            members: Default::default(),
            deferred: Default::default(),
        },
    };
    let documents = lash_vm_runtime::KernelDocuments::new(
        core.backend().module_artifacts(),
        core.backend().package_store(),
        std::sync::Arc::clone(lash::vm::WorkerLibrary::built().unwrap().helpers()),
    );
    let other = k::parse_document("kernel 1\nnumbers float\nmain { return 7 }").unwrap();
    let missing = k::Site::new(k::Unit::Main, [999]);
    let mut stray = map.clone();
    stray.sites.push(k::SourceSite {
        site: missing.clone(),
        location: k::SourceLocation::Implicit { offset: 0 },
    });
    for (candidate, sidecar, text, expected) in [
        (
            &document,
            &map,
            "different source",
            k::SourceMapRefusal::Digest,
        ),
        (&other, &map, source, k::SourceMapRefusal::Document),
        (
            &document,
            &stray,
            source,
            k::SourceMapRefusal::Site { site: missing },
        ),
    ] {
        let error = core
            .host_artifacts()
            .publish_workflow(
                &pin,
                &candidate.clone().into(),
                &StartSelector::Main,
                &namespace,
                Some((sidecar, text)),
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error,lash::EmbedError::WorkflowSourceMap(ref refusal) if *refusal == expected),
            "map refusal stays typed: {error:?}"
        );
        assert!(
            documents
                .get(&candidate.identity())
                .await
                .unwrap()
                .is_none(),
            "a refused map publishes no Document"
        );
        let link = Link {
            document: candidate.identity(),
            fills: Default::default(),
        };
        assert!(
            core.backend()
                .package_store()
                .link(&link.identity())
                .await
                .unwrap()
                .is_none(),
            "a refused map publishes no Link"
        );
    }
    let start = core
        .host_artifacts()
        .publish_workflow(
            &pin,
            &document.clone().into(),
            &StartSelector::Main,
            &namespace,
            Some((&map, source)),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        documents
            .execution_document(&start)
            .await
            .unwrap()
            .identity(),
        document.identity()
    );
    assert!(
        matches!(core.host_artifacts().read_source_map(&map.identity()).await.unwrap(),lash::workflow::SourceMapRead::Read {map:stored,source} if stored == map && source == "const answer = 42;")
    );
    core.shutdown().await.unwrap();
}

/// FIG-7227: literal milliseconds become a typed wait fact; host inputs stay unknown.
#[test]
fn typescript_wait_states_its_static_duration() {
    let embedding = lash_vm_worker::built_embedding(&Default::default()).unwrap();
    let namespaces = Default::default();
    let environment = lash::dialect::Environment::new(embedding.library(), &namespaces);
    let document = lash::typescript::lower(
        "export async function run(x: number): Promise<void> { await sleep(90000); await sleep(x); await sleep(90000.9); await sleep(1e30); }",
        &environment,
        lash::dialect::Mode::Module,
    ).unwrap().document.into_document();
    let unit = export_unit(&document, "run");
    let (holder, context) = holder(document);
    let view = lash::workflow::view::derive_callable(&holder, &unit, &context).unwrap();
    let waits: Vec<_> = view
        .dataflow
        .actions
        .iter()
        .filter(|action| action.role == lash::workflow::view::ActionRole::Wait)
        .map(|action| serde_json::to_value(action).unwrap())
        .collect();
    assert_eq!(waits.len(), 4);
    assert_eq!(
        waits[0]["static_duration"],
        serde_json::json!({"secs": 90, "nanos": 0})
    );
    assert!(waits[1]["static_duration"].is_null());
    assert_eq!(
        waits[2]["static_duration"],
        serde_json::json!({"secs": 90, "nanos": 0})
    );
    assert_eq!(
        waits[3]["static_duration"],
        serde_json::to_value(std::time::Duration::from_millis(i64::MAX as u64)).unwrap()
    );
}

/// FIG-7227: Python seconds become the same typed fact; host inputs stay unknown.
#[test]
fn python_wait_states_its_static_duration() {
    let embedding = lash_vm_worker::built_embedding(&Default::default()).unwrap();
    let namespaces = Default::default();
    let environment = lash::dialect::Environment::new(embedding.library(), &namespaces);
    let document = lash::python::lower(
        "import asyncio\nasync def run(x: float) -> None:\n    await asyncio.sleep(90)\n    await asyncio.sleep(x)\n    await asyncio.sleep(0.0009)\n    await asyncio.sleep(1e30)\n",
        &environment,
        lash::dialect::Mode::Module,
    ).unwrap().document.into_document();
    let unit = export_unit(&document, "run");
    let (holder, context) = holder(document);
    let view = lash::workflow::view::derive_callable(&holder, &unit, &context).unwrap();
    let waits: Vec<_> = view
        .dataflow
        .actions
        .iter()
        .filter(|action| action.role == lash::workflow::view::ActionRole::Wait)
        .map(|action| action.static_duration)
        .collect();
    assert_eq!(
        waits,
        vec![
            Some(std::time::Duration::from_secs(90)),
            None,
            Some(std::time::Duration::ZERO),
            Some(std::time::Duration::from_millis(i64::MAX as u64)),
        ]
    );
}

/// FIG-7476: lowering adapters are owned by the authored send/wait steps,
/// while their exact execution and replay coordinates remain selectable.
#[test]
fn lowering_glue_belongs_to_the_authored_consumer() {
    use k::FunctionCatalog as _;
    let embedding = lash_vm_worker::built_embedding(&Default::default()).unwrap();
    let echo = lash::schema::Signature {
        params: vec![lash::schema::Param {
            name: lash::schema::Name::new("input"),
            ty: lash::schema::Type::Any,
            optional: false,
        }],
        result: lash::schema::Type::Any,
    };
    let namespaces = namespaces(BTreeMap::from([(
        k::EffectName::new("inbox.send_item").unwrap(),
        echo,
    )]));
    let environment = lash::dialect::Environment::new(embedding.library(), &namespaces);
    for (language, source) in [
        (
            k::SourceLanguage::TypeScript,
            "export async function slowReminder(): Promise<void> { await inbox.sendItem({title: \"Started\"}); await sleep(600000); await inbox.sendItem({title: \"Done\"}); }",
        ),
        (
            k::SourceLanguage::Python,
            "async def slow_reminder() -> None:\n    await inbox.send_item({\"title\": \"Started\"})\n    await asyncio.sleep(600)\n    await inbox.send_item({\"title\": \"Done\"})\n",
        ),
    ] {
        let document = match language {
            k::SourceLanguage::TypeScript => {
                lash::typescript::lower(source, &environment, lash::dialect::Mode::Module).unwrap()
            }
            k::SourceLanguage::Python => {
                lash::python::lower(source, &environment, lash::dialect::Mode::Module).unwrap()
            }
        }
        .document
        .into_document();
        let entry = document.exports.keys().next().unwrap().as_str().to_owned();
        let unit = export_unit(&document, &entry);
        let (document, context) = holder(document);
        let mut view = lash::workflow::view::derive_callable(&document, &unit, &context).unwrap();
        assert_eq!(view.nodes.len(), 3, "{language:?}");
        assert_eq!(view.statements.len(), 3, "{language:?}");
        assert_eq!(
            view.dataflow
                .actions
                .iter()
                .filter(|action| action.role == lash::workflow::view::ActionRole::Wait)
                .count(),
            1
        );
        assert!(
            view.dataflow
                .actions
                .iter()
                .any(|action| action.static_duration == Some(std::time::Duration::from_secs(600)))
        );
        for site in view
            .execution_sites
            .iter()
            .filter(|site| site.site.unit == unit)
        {
            assert!(
                view.node_at(&site.site).is_some(),
                "{language:?} {:?}",
                site.site
            );
        }
        assert!(view.nodes.iter().any(|node| !node.folded.is_empty()));
        for node in &view.nodes {
            if let lash::workflow::view::WorkflowNodeBody::Action {
                within: Some(within),
                ..
            } = &node.body
            {
                assert_ne!(within, &node.site);
                assert_eq!(&view.node_at(within).unwrap().site, within);
            }
        }
        let folded: std::collections::BTreeSet<_> =
            view.nodes.iter().flat_map(|node| &node.folded).collect();
        assert!(view.flow.iter().all(|edge| !folded.contains(&edge.from)
            && !matches!(&edge.to, lash::workflow::view::FlowTarget::Site(site) if folded.contains(site))));
        let printed = match language {
            k::SourceLanguage::TypeScript => lash::typescript::print(
                document.document(),
                None,
                embedding.library(),
                lash::dialect::PrintOptions {
                    source_map: true,
                    ..Default::default()
                },
            ),
            k::SourceLanguage::Python => lash::python::print(
                document.document(),
                None,
                embedding.library(),
                lash::dialect::PrintOptions {
                    source_map: true,
                    ..Default::default()
                },
            ),
        }
        .unwrap();
        view.readable_ranges(
            document.document(),
            "readable".into(),
            &printed.source,
            printed.source_map.as_ref().unwrap(),
        )
        .unwrap();
        assert!(view.nodes.iter().all(|node| !node.readable.is_empty()
            && node.readable.iter().all(|range| range.first > 0
                && range.first <= range.last
                && range.last <= printed.source.lines().count())));
        assert!(
            view.statements
                .iter()
                .all(|statement| !statement.readable.is_empty()
                    && statement.readable.iter().all(|range| range.first > 0
                        && range.first <= range.last
                        && range.last <= printed.source.lines().count()))
        );
    }
    let authored = lash::typescript::lower(
        "export async function send(title: string): Promise<void> { await inbox.sendItem({title: title.toUpperCase()}); return; }",
        &environment, lash::dialect::Mode::Module,
    ).unwrap().document.into_document();
    let unit = export_unit(&authored, "send");
    let (document, context) = holder(authored);
    let view = lash::workflow::view::derive_callable(&document, &unit, &context).unwrap();
    assert!(
        view.statements
            .iter()
            .any(|statement| matches!(statement.statement, k::Stmt::ReturnVia { .. })),
        "an explicit return remains authored"
    );
    let authored_calls: Vec<_> = view
        .dataflow
        .actions
        .iter()
        .filter(|action| {
            let Some(lash::workflow::view::CalleeView::Library { id, .. }) = &action.callee else {
                return false;
            };
            embedding
                .library()
                .header(id)
                .is_some_and(|header| header.projection == k::ProjectionRole::Authored)
        })
        .collect();
    assert!(
        !authored_calls.is_empty(),
        "the upper-case call is an authored library call"
    );
    for action in authored_calls {
        assert_eq!(view.node_at(&action.site).unwrap().site, action.site);
    }
}
