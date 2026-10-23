//! L-PIN: scoped publications remain immutable and retain linked code across
//! installer loss. Current advertisements and immutable Links determine resolution.
use super::*;
use lash_core::{ArtifactReferrer, HostArtifactPin};
use lash_core_store::artifact_referrer::ResolvedArtifactCleanup;
use lash_vocabulary::names::*;

#[tokio::test]
async fn a_publication_retains_its_link_after_the_installer_ends() {
    let stores = lash_sqlite_store::SqliteStoreSet::memory().await.unwrap();
    let store = stores.open_store().await.unwrap();
    let installer =
        ReferrerClaim::unguarded(ArtifactReferrer::HostPin(HostArtifactPin::mint())).unwrap();
    let reader =
        ReferrerClaim::unguarded(ArtifactReferrer::HostPin(HostArtifactPin::mint())).unwrap();
    let context = PublicationContext {
        domain: PublicationDomain(stores.identity().into()),
        scope: PublicationScope::Host,
        publisher: PublisherInstance("fixture".into()),
        root: Segment::new("fixture").unwrap(),
    };
    let function = lash_kernel_doc::FunctionDefinition {
        kernel: lash_kernel_doc::KERNEL_VERSION,
        name: lash_vocabulary::names::FunctionName::new("fixture.run").unwrap(),
        signature: lash_vocabulary::Signature {
            params: Vec::new(),
            result: lash_vocabulary::Type::Null,
        },
        errors: Default::default(),
        guard: None,
        implementation: lash_kernel_doc::Implementation::Guest {
            body: lash_kernel_doc::FunctionBody {
                functions: Default::default(),
                imports: Default::default(),
                block: Vec::new(),
            },
            metering: lash_kernel_doc::Metering::Statements,
        },
        native_version: 1,
    };
    let export = Name::new("run");
    let document = lash_kernel_doc::Document {
        imports: Default::default(),
        exports: [(
            export.clone(),
            lash_kernel_doc::Export::Function(lash_kernel_doc::FunctionExport {
                function: function.identity(),
                signature: function.signature.clone(),
                convention: lash_kernel_doc::StartConvention::Direct,
                asynchronous: false,
                rest: false,
            }),
        )]
        .into(),
        main: None,
        functions: [(function.identity(), function)].into(),
        requires: lash_vocabulary::claims::NodeRequirements {
            kernels: [lash_kernel_doc::KERNEL_VERSION].into(),
            ..Default::default()
        },
        numbers: lash_kernel_doc::NumberPolicy::BySpelling,
        library: Default::default(),
        source_evidence: Default::default(),
        private_bindings: Default::default(),
    };
    let documents = lash_vm_runtime::KernelDocuments::new(
        store.clone(),
        store.clone(),
        std::sync::Arc::clone(
            lash_vm_client::packages::WorkerLibrary::built()
                .unwrap()
                .helpers(),
        ),
    );
    let request = lash_vm_runtime::WorkflowPublishRequest {
        document: lash_kernel_doc::DocumentBytes::from(document),
        selector: StartSelector::Export(export.clone()),
        environment: lash_vm_runtime::WorkflowEnvironmentRequest {
            snapshot: NamespaceSnapshot::default(),
            context: context.clone(),
            attachment: Attachment {
                members: Default::default(),
                packages: Default::default(),
                deferred: Default::default(),
            },
        },
    };
    let start = documents.publish(&installer, &request).await.unwrap();
    let package = Package {
        path: PackagePath::parse("fixture").unwrap(),
        members: [(
            Segment::new("run").unwrap(),
            Member {
                visibility: Visibility::Public,
                kind: MemberKind::Export {
                    link: start.link,
                    export: ExportName {
                        name: export,
                        member: None,
                    },
                },
            },
        )]
        .into(),
    };
    let id = package.identity();
    store
        .put_package(&installer, &PackageRecord { id, package })
        .await
        .unwrap();
    let publication = PublicationRecord {
        context,
        path: PackagePath::parse("fixture").unwrap(),
        content: PublishedContent::Package(id),
        code_links: [start.link].into(),
        imported_publications: Vec::new(),
    };
    let key = store.publish(&installer, &publication).await.unwrap();
    store.acquire_publication(&reader, &key).await.unwrap();
    store
        .end_package_referrer(&ResolvedArtifactCleanup {
            referrer: installer.referrer().clone(),
            carries: Vec::new(),
        })
        .await
        .unwrap();
    drop(documents);
    drop(store);
    let cold = stores.reopen().await.unwrap().open_store().await.unwrap();
    assert_eq!(cold.publication(&key).await.unwrap(), Some(publication));
    assert!(cold.link(&start.link).await.unwrap().is_some());
    assert_eq!(cold.package(&id).await.unwrap().unwrap().id, id);
    assert!(
        cold.acquire_publication(&installer, &key).await.is_err(),
        "the ended installer cannot resurrect its hold"
    );
}

/// L-INTERNAL: an admitted public alias retains the defining Link; presenting
/// its bytes as newly authored code in a sibling package cannot borrow internals.
#[tokio::test]
async fn a_public_alias_retains_internal_fills_without_lending_their_authority() {
    use lash_core::plugin::packages::operation_publication;
    use lash_execution_contracts::process::linking::{FulfilRequest, LinkPurpose, ReachSelector};
    use lash_kernel_doc::{Action, Expr, Stmt};

    let stores = lash_sqlite_store::SqliteStoreSet::memory().await.unwrap();
    let store = stores.open_store().await.unwrap();
    let claim =
        ReferrerClaim::unguarded(ArtifactReferrer::HostPin(HostArtifactPin::mint())).unwrap();
    let context = PublicationContext {
        domain: PublicationDomain(stores.identity().into()),
        scope: PublicationScope::Host,
        publisher: PublisherInstance("owner".into()),
        root: Segment::new("fixture").unwrap(),
    };
    let path = PackagePath::parse("fixture").unwrap();
    let signature = lash_vocabulary::Signature {
        params: Vec::new(),
        result: lash_vocabulary::Type::Null,
    };
    let offer = OperationOffer {
        requirement: lash_vocabulary::OperationRequirement {
            signature: signature.clone(),
            contract: lash_vocabulary::ContractId::new("fixture.private"),
        },
        intents: Default::default(),
        controls: Default::default(),
        mode: OperationMode::Waited,
    };
    let private = Member {
        visibility: Visibility::Internal,
        kind: MemberKind::Operation(offer.clone()),
    };
    let operations = Package {
        path: path.clone(),
        members: [(Segment::new("private").unwrap(), private.clone())].into(),
    };
    let advertisement = PackageAdvertisement::Enumerated(operations.clone());
    let operation_record = operation_publication(&context, &advertisement).unwrap();
    store
        .put_package(
            &claim,
            &PackageRecord {
                id: operations.identity(),
                package: operations,
            },
        )
        .await
        .unwrap();
    store.publish(&claim, &operation_record).await.unwrap();
    let slot = ImportSlot(0);
    let function = lash_kernel_doc::FunctionDefinition {
        kernel: lash_kernel_doc::KERNEL_VERSION,
        name: lash_vocabulary::names::FunctionName::new("fixture.run").unwrap(),
        signature: signature.clone(),
        errors: Default::default(),
        guard: None,
        native_version: 1,
        implementation: lash_kernel_doc::Implementation::Guest {
            metering: lash_kernel_doc::Metering::Statements,
            body: lash_kernel_doc::FunctionBody {
                imports: [(
                    slot,
                    Import {
                        name: QualifiedName {
                            package: path.clone(),
                            member: Segment::new("private").unwrap(),
                            type_member: None,
                        },
                        kind: ImportKind::Operation {
                            signature: signature.clone(),
                        },
                    },
                )]
                .into(),
                functions: Default::default(),
                block: vec![Stmt::Return {
                    value: Expr::from(Action::Perform {
                        slot,
                        args: Vec::new(),
                        result: lash_vocabulary::Type::Null,
                        then_yield: false,
                    }),
                }],
            },
        },
    };
    let mut document = lash_kernel_doc::Document {
        imports: function.body().unwrap().imports.clone(),
        exports: [(
            Name::new("run"),
            lash_kernel_doc::Export::Function(lash_kernel_doc::FunctionExport {
                function: function.identity(),
                signature,
                convention: lash_kernel_doc::StartConvention::Direct,
                asynchronous: false,
                rest: false,
            }),
        )]
        .into(),
        functions: [(function.identity(), function)].into(),
        main: None,
        requires: lash_vocabulary::claims::NodeRequirements {
            kernels: [lash_kernel_doc::KERNEL_VERSION].into(),
            ..Default::default()
        },
        numbers: lash_kernel_doc::NumberPolicy::BySpelling,
        library: Default::default(),
        source_evidence: Default::default(),
        private_bindings: Default::default(),
    };
    document.exports.insert(
        Name::new("other"),
        document.exports[&Name::new("run")].clone(),
    );
    let documents = Arc::new(lash_vm_runtime::KernelDocuments::new(
        store.clone(),
        store.clone(),
        std::sync::Arc::clone(
            lash_vm_client::packages::WorkerLibrary::built()
                .unwrap()
                .helpers(),
        ),
    ));
    let id = documents
        .admit_document(&claim, &lash_kernel_doc::DocumentBytes::from(document))
        .await
        .unwrap();
    let fulfilled = documents
        .fulfil(&FulfilRequest {
            document: id,
            selector: ReachSelector::AllExports,
            snapshot: NamespaceSnapshot {
                advertisements: vec![AcceptedAdvertisement {
                    publication: operation_record,
                    advertisement,
                }],
            },
            context: context.clone(),
            purpose: LinkPurpose::PackageCode { path: path.clone() },
        })
        .await
        .unwrap();
    let public = Member {
        visibility: Visibility::Public,
        kind: MemberKind::Export {
            link: fulfilled.content.identity(),
            export: ExportName {
                name: Name::new("run"),
                member: None,
            },
        },
    };
    let other = Member {
        visibility: Visibility::Public,
        kind: MemberKind::Export {
            link: fulfilled.content.identity(),
            export: ExportName {
                name: Name::new("other"),
                member: None,
            },
        },
    };
    let package = Package {
        path,
        members: [
            (Segment::new("private").unwrap(), private),
            (Segment::new("run").unwrap(), public.clone()),
            (Segment::new("other").unwrap(), other.clone()),
        ]
        .into(),
    };
    let engine = lash_vm_runtime::KernelProcessEngine::new(
        (*documents).clone(),
        "typescript",
        lash_vm_client::service::Service::new(lash_vm_client::PoolConfig::standard(
            lash_vm_client::WorkerEntry::helper("unused"),
        )),
        lash_vm_runtime::KernelBounds {
            memory_meter: Default::default(),
            charge: Default::default(),
            charge_limit: 1_000_000,
            memory: 64 << 20,
            call_depth: 64,
            live_tasks: 64,
            requests_per_park: 64,
            join_members: 64,
        },
    )
    .unwrap();
    let engines = lash_core::ProcessEngineRegistry::new()
        .with_registration(lash_vm_runtime::kernel_process_engine_registration(engine));
    let provider = engines
        .document_provider(lash_vm_runtime::LASH_VM_ENGINE_KIND)
        .unwrap();
    let publisher = ScopedPackagePublisher::new(
        context.clone(),
        NamespaceSnapshot::default(),
        provider.clone(),
        store.clone(),
    );
    let publication = publisher
        .publish(
            &claim,
            &context,
            PublishPackage {
                package: package.clone(),
                code: vec![fulfilled.clone()],
            },
        )
        .await
        .unwrap();
    // L-ALIAS: another valid publication of this Link offers a different
    // Export. Only the publication offering `run` supplies this alias's source.
    let other_alias = Package {
        path: PackagePath::parse("fixture.shadow").unwrap(),
        members: [(Segment::new("other").unwrap(), other)].into(),
    };
    let other_publication = ScopedPackagePublisher::new(
        context.clone(),
        NamespaceSnapshot {
            advertisements: vec![AcceptedAdvertisement {
                publication: publication.clone(),
                advertisement: PackageAdvertisement::Enumerated(package.clone()),
            }],
        },
        provider.clone(),
        store.clone(),
    )
    .publish(
        &claim,
        &context,
        PublishPackage {
            package: other_alias.clone(),
            code: Vec::new(),
        },
    )
    .await
    .unwrap();
    // L-ALIAS: a second package may keep the same public Export internally.
    // Its local visibility cannot revoke the original package's public offer.
    let internal_alias = Package {
        path: PackagePath::parse("fixture.hidden").unwrap(),
        members: [(
            Segment::new("run").unwrap(),
            Member {
                visibility: Visibility::Internal,
                kind: public.kind.clone(),
            },
        )]
        .into(),
    };
    let internal_publication = ScopedPackagePublisher::new(
        context.clone(),
        NamespaceSnapshot {
            advertisements: vec![AcceptedAdvertisement {
                publication: publication.clone(),
                advertisement: PackageAdvertisement::Enumerated(package.clone()),
            }],
        },
        provider.clone(),
        store.clone(),
    )
    .publish(
        &claim,
        &context,
        PublishPackage {
            package: internal_alias.clone(),
            code: Vec::new(),
        },
    )
    .await
    .unwrap();
    let mut snapshot = NamespaceSnapshot {
        advertisements: vec![
            AcceptedAdvertisement {
                publication: publication.clone(),
                advertisement: PackageAdvertisement::Enumerated(package),
            },
            AcceptedAdvertisement {
                publication: other_publication,
                advertisement: PackageAdvertisement::Enumerated(other_alias),
            },
        ],
    };
    snapshot.advertisements.push(AcceptedAdvertisement {
        publication: internal_publication,
        advertisement: PackageAdvertisement::Enumerated(internal_alias),
    });
    let publisher = ScopedPackagePublisher::new(context.clone(), snapshot, provider, store.clone());
    let alias = Package {
        path: PackagePath::parse("fixture.alias").unwrap(),
        members: [(Segment::new("run").unwrap(), public)].into(),
    };
    let aliased = publisher
        .publish(
            &claim,
            &context,
            PublishPackage {
                package: alias.clone(),
                code: Vec::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(aliased.code_links, publication.code_links);
    assert_eq!(aliased.imported_publications, vec![publication.key()]);
    let refused = publisher
        .publish(
            &claim,
            &context,
            PublishPackage {
                package: alias,
                code: vec![fulfilled],
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(
        refused,
        EnginePortError::Refused(AdmissionRefusal::NotVisible { .. })
    ));
}

struct CountedDocuments {
    inner: Arc<dyn lash_execution_contracts::module_artifacts::ModuleArtifactStore>,
    holds: Arc<std::sync::Mutex<BTreeMap<String, usize>>>,
    reads: Arc<std::sync::Mutex<BTreeMap<String, usize>>>,
}
#[async_trait::async_trait]
impl lash_execution_contracts::module_artifacts::ModuleArtifactStore for CountedDocuments {
    async fn publish_module_artifact(
        &self,
        claim: &ReferrerClaim,
        module_ref: &str,
        bytes: &[u8],
    ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
        self.inner
            .publish_module_artifact(claim, module_ref, bytes)
            .await
    }
    async fn acquire_module_artifact(
        &self,
        claim: &ReferrerClaim,
        module_ref: &str,
    ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
        self.inner.acquire_module_artifact(claim, module_ref).await
    }
    async fn hold_module_artifacts(
        &self,
        claim: &ReferrerClaim,
        artifacts: &[lash_execution_contracts::module_artifacts::ModuleArtifactHold<'_>],
    ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
        {
            let mut holds = self.holds.lock().unwrap();
            for artifact in artifacts {
                *holds.entry(artifact.module_ref().to_owned()).or_default() += 1;
            }
        }
        self.inner.hold_module_artifacts(claim, artifacts).await
    }
    async fn end_module_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
        self.inner.end_module_referrer(cleanup).await
    }
    async fn get_module_artifact(
        &self,
        module_ref: &str,
    ) -> Result<Option<Vec<u8>>, lash_execution_contracts::module_artifacts::ArtifactStoreError>
    {
        *self
            .reads
            .lock()
            .unwrap()
            .entry(module_ref.to_owned())
            .or_default() += 1;
        self.inner.get_module_artifact(module_ref).await
    }
}

/// FIG-7135: many package names for one Link validate its current document
/// once per hold, rather than hydrating the same retained code per member.
#[tokio::test]
async fn holding_package_aliases_loads_each_distinct_link_once() {
    use crate::tests::host::sqlite_memory_store_backend;
    let backend = sqlite_memory_store_backend().await;
    let reads = Arc::new(std::sync::Mutex::new(BTreeMap::new()));
    let documents = lash_vm_runtime::KernelDocuments::new(
        Arc::new(CountedDocuments {
            inner: backend.module_artifacts(),
            holds: Default::default(),
            reads: reads.clone(),
        }),
        backend.package_store(),
        std::sync::Arc::clone(
            lash_vm_client::packages::WorkerLibrary::built()
                .unwrap()
                .helpers(),
        ),
    );
    let function = lash_kernel_doc::parse_definition(
        "function fixture.run() -> Null\nkernel 1\ncharge 1\nbody { return null }",
    )
    .unwrap();
    let mut document =
        lash_kernel_doc::parse_document("kernel 1\nnumbers float\nmain { return null }").unwrap();
    document.main = None;
    document.exports.insert(
        Name::new("run"),
        lash_kernel_doc::Export::Function(lash_kernel_doc::FunctionExport {
            function: function.identity(),
            signature: function.signature.clone(),
            convention: lash_kernel_doc::StartConvention::Direct,
            asynchronous: false,
            rest: false,
        }),
    );
    document.functions.insert(function.identity(), function);
    let document_id = document.identity();
    let link = Link {
        document: document_id,
        fills: Default::default(),
    };
    let link_id = link.identity();
    let package = Package {
        path: PackagePath::parse("fixture").unwrap(),
        members: (0..8)
            .map(|i| {
                (
                    Segment::new(format!("alias{i}")).unwrap(),
                    Member {
                        visibility: Visibility::Public,
                        kind: MemberKind::Export {
                            link: link_id,
                            export: ExportName {
                                name: Name::new("run"),
                                member: None,
                            },
                        },
                    },
                )
            })
            .collect(),
    };
    let package_id = package.identity();
    let content = lash_kernel_package::LinkedContent {
        documents: [(document_id, document)].into(),
        links: [(link_id, link)].into(),
        packages: [(package_id, package)].into(),
    };
    let claim =
        ReferrerClaim::unguarded(ArtifactReferrer::HostPin(HostArtifactPin::mint())).unwrap();
    documents
        .admit_document(
            &claim,
            &lash_kernel_doc::DocumentBytes::from(content.documents[&document_id].clone()),
        )
        .await
        .unwrap();
    backend
        .package_store()
        .put_link(
            &claim,
            &LinkRecord {
                id: link_id,
                link: content.links[&link_id].clone(),
            },
            &[],
        )
        .await
        .unwrap();
    backend
        .package_store()
        .put_package(
            &claim,
            &PackageRecord {
                id: package_id,
                package: content.packages[&package_id].clone(),
            },
        )
        .await
        .unwrap();
    for _ in 0..2 {
        reads.lock().unwrap().clear();
        documents
            .hold(&claim, &ContentRef::Package(package_id))
            .await
            .unwrap();
        assert_eq!(
            reads.lock().unwrap().get(&document_id.to_string()),
            Some(&1),
            "a package hold must read each distinct Link's current document once, including subsequent holds"
        );
    }
    // The same package aliases imported in distinct document-local slots
    // still name one dependency. Publishing the new root must not start
    // another complete cold load for each fill.
    let context = PublicationContext {
        domain: PublicationDomain(backend.binding_identity().to_string()),
        scope: PublicationScope::Host,
        publisher: PublisherInstance("fixture".into()),
        root: Segment::new("fixture").unwrap(),
    };
    let package = content.packages[&package_id].clone();
    let publication = PublicationRecord {
        context: context.clone(),
        path: package.path.clone(),
        content: PublishedContent::Package(package_id),
        code_links: [link_id].into(),
        imported_publications: Vec::new(),
    };
    backend
        .package_store()
        .publish(&claim, &publication)
        .await
        .unwrap();
    let snapshot = NamespaceSnapshot {
        advertisements: vec![AcceptedAdvertisement {
            publication,
            advertisement: PackageAdvertisement::Enumerated(package.clone()),
        }],
    };
    let signature = match &content.documents[&document_id].exports[&Name::new("run")] {
        lash_kernel_doc::Export::Function(function) => function.signature.clone(),
        _ => unreachable!("fixture function"),
    };
    let mut root =
        lash_kernel_doc::parse_document("kernel 1\nnumbers float\nmain { return null }").unwrap();
    root.imports = package
        .members
        .keys()
        .enumerate()
        .map(|(slot, name)| {
            (
                ImportSlot(slot as u32),
                Import {
                    name: QualifiedName {
                        package: package.path.clone(),
                        member: name.clone(),
                        type_member: None,
                    },
                    kind: ImportKind::Code {
                        signature: signature.clone(),
                        convention: Convention::Direct,
                    },
                },
            )
        })
        .collect();
    let root_id = documents
        .admit_document(&claim, &root.into())
        .await
        .unwrap();
    let fulfilled = documents
        .fulfil(&lash_execution_contracts::process::linking::FulfilRequest {
            document: root_id,
            selector: lash_execution_contracts::process::linking::ReachSelector::AllExports,
            snapshot,
            context,
            purpose: lash_execution_contracts::process::linking::LinkPurpose::Cell,
        })
        .await
        .unwrap();
    for _ in 0..2 {
        reads.lock().unwrap().clear();
        documents
            .hold(&claim, &ContentRef::Link(fulfilled.inspection.link))
            .await
            .unwrap();
        assert_eq!(
            reads.lock().unwrap().get(&document_id.to_string()),
            Some(&1),
            "a linked document must read a shared dependency once per hold, rather than once per import"
        );
    }
}
