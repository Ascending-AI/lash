//! L-PIN: scoped publications remain immutable and retain linked code across
//! installer loss. No fleet, installation sequence or package-set pin exists.
use super::*;
use lash_core::{ArtifactReferrer, HostArtifactPin};
use lash_core_store::artifact_referrer::ResolvedArtifactCleanup;

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
        requires: Default::default(),
        numbers: lash_kernel_doc::NumberPolicy::BySpelling,
        private_bindings: Default::default(),
    };
    let documents = lash_vm_runtime::KernelDocuments::new(store.clone(), store.clone());
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
    let document = lash_kernel_doc::Document {
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
        requires: Default::default(),
        numbers: lash_kernel_doc::NumberPolicy::BySpelling,
        private_bindings: Default::default(),
    };
    let documents = Arc::new(lash_vm_runtime::KernelDocuments::new(
        store.clone(),
        store.clone(),
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
    let package = Package {
        path,
        members: [
            (Segment::new("private").unwrap(), private),
            (Segment::new("run").unwrap(), public.clone()),
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
    let snapshot = NamespaceSnapshot {
        advertisements: vec![AcceptedAdvertisement {
            publication: publication.clone(),
            advertisement: PackageAdvertisement::Enumerated(package),
        }],
    };
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
