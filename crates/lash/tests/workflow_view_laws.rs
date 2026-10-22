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

fn namespaces(effects: BTreeMap<k::EffectName, k::Signature>) -> lash::dialect::Namespaces {
    let mut packages: BTreeMap<PackagePath, BTreeMap<Segment, lash::dialect::NamespaceMember>> =
        BTreeMap::new();
    for (name, signature) in effects {
        let operation = lash_vocabulary::operation_text::parse(name.as_str()).unwrap();
        packages.entry(operation.package).or_default().insert(
            operation.name,
            lash::dialect::NamespaceMember {
                visibility: Visibility::Public,
                kind: lash::dialect::NamespaceKind::Operation(
                    OperationOffer {
                        requirement: lash_vocabulary::OperationRequirement::new(
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
    let mut content = release.content.clone();
    for package in lash::vm::DIALECT_SERVICE_PACKAGES.iter() {
        content.packages.insert(package.identity(), package.clone());
    }
    let fills = document
        .imports
        .iter()
        .map(|(slot, import)| {
            let fill = match &import.kind {
                lash_vocabulary::ImportKind::Operation { signature } => {
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
                        lash_vocabulary::OperationRequirement::new(
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
        .insert(document.identity(), document.clone());
    content.links.insert(link.identity(), link.clone());
    let start = LinkedStart {
        link: link.identity(),
        selector: StartSelector::Main,
    };
    let mut labels = BTreeMap::new();
    // These are the fixture's pinned sidecars, independent of executable hashes.
    for (id, text) in &release.labels {
        let package = &content.packages[id];
        let sidecars = lash_kernel_package::read_labels(&content, package, text)
            .expect("valid authored labels");
        for (member, display) in sidecars.members {
            let MemberKind::Export { link, export } =
                &package.members[&Segment::new(member).unwrap()].kind
            else {
                continue;
            };
            let function = content.function_export(*link, export).unwrap().function;
            labels.insert(
                lash_vocabulary::CallableId {
                    link: *link,
                    function,
                },
                display,
            );
        }
        for (name, display) in sidecars.definitions {
            for (link, source) in &content.links {
                for (function, definition) in &content.documents[&source.document].functions {
                    if definition.name.as_str() == name {
                        labels.insert(
                            lash_vocabulary::CallableId {
                                link: *link,
                                function: *function,
                            },
                            display.clone(),
                        );
                    }
                }
            }
        }
    }
    let context = lash::workflow::view::ViewContext::new(start.clone(), content, labels)
        .expect("retained context")
        .with_timer_operations(lash::vm::TIMER_OPERATIONS.iter().cloned());
    (
        lash::workflow::WorkflowDocument::entered(document, start).expect("complete document"),
        context,
    )
}
fn export_unit(document: &k::Document, name: &str) -> k::Unit {
    let k::Export::Function(export) = &document.exports[&k::Name::new(name)] else {
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
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash_core::LeaseOwnerIdentity::opaque(
            lash_core::LeaseOwnerId::new("source-map"),
            lash_core::LeaseIncarnationId::new("source-map-boot"),
        ))
        .unwrap();
    let document = k::parse_document("kernel 1\nnumbers float\nmain { return 42 }").unwrap();
    let source = "return 42;";
    let map = k::SourceMap::new(
        &document,
        source,
        k::SourceLanguage::TypeScript,
        vec![k::SourceSite {
            site: k::Site::new(k::Unit::Main, [0]),
            ranges: vec![k::Span {
                start: 0,
                end: source.len(),
            }],
            implicit: None,
        }],
    )
    .unwrap();
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
    );
    let other = k::parse_document("kernel 1\nnumbers float\nmain { return 7 }").unwrap();
    let missing = k::Site::new(k::Unit::Main, [999]);
    let mut stray = map.clone();
    stray.sites.push(k::SourceSite {
        site: missing.clone(),
        ranges: Vec::new(),
        implicit: None,
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
        matches!(core.host_artifacts().read_source_map(&map.identity()).await.unwrap(),lash::workflow::SourceMapRead::Read {map:stored,source} if stored == map && source == "return 42;")
    );
    core.shutdown().await.unwrap();
}
