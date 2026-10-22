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

struct CountedDocuments {
    inner: Arc<dyn lash_execution_contracts::module_artifacts::ModuleArtifactStore>,
    holds: Arc<std::sync::Mutex<BTreeMap<String, usize>>>,
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
        self.inner.get_module_artifact(module_ref).await
    }
}

/// FIG-7135: namespace retention walks the full helper graph once, regardless
/// of the number of advertised packages, under each receiving claim.
#[tokio::test]
async fn a_full_helper_namespace_holds_each_document_once() {
    use crate::tests::host::{
        explicit_ephemeral_facets, runtime_lease_owner, sqlite_memory_store_backend,
    };
    let backend = sqlite_memory_store_backend().await;
    let holds = Arc::new(std::sync::Mutex::new(BTreeMap::new()));
    let counted = holds.clone();
    let backend = crate::testing::LayeredBackend::over(backend)
        .map_module_artifacts(move |inner| {
            Arc::new(CountedDocuments {
                inner,
                holds: counted,
            })
        })
        .into_backend();
    let core = explicit_ephemeral_facets(crate::LashCore::standard_builder(backend.clone()))
        .build(runtime_lease_owner())
        .unwrap();
    let env = lash_core::ProcessEnv::builder(crate::MaxToolCalls::new(16))
        .component(lash_vm_runtime::KERNEL_SERVICES_CONFIG, &())
        .build()
        .unwrap();
    let context = PublicationContext {
        domain: PublicationDomain(backend.binding_identity().to_string()),
        scope: PublicationScope::Host,
        publisher: PublisherInstance("fixture".into()),
        root: Segment::new("fixture").unwrap(),
    };
    let release = lash_vm_releases::helper_release().unwrap();
    assert!(release.content.packages.len() > 1);
    let pin = HostArtifactPin::mint();
    let before = std::time::Instant::now();
    let packages = core
        .host_artifacts()
        .process_namespace(&pin, &env, context)
        .await
        .unwrap();
    eprintln!(
        "full helper namespace setup: {:?}; packages={}; documents={}",
        before.elapsed(),
        release.content.packages.len(),
        release.content.documents.len()
    );
    let expected: BTreeMap<_, _> = release
        .content
        .documents
        .keys()
        .map(|id| (id.to_string(), 1))
        .collect();
    assert_eq!(
        *holds.lock().unwrap(),
        expected,
        "each helper Document must be held once per namespace"
    );
    let store = backend.package_store();
    for accepted in &packages.snapshot.advertisements {
        assert_eq!(
            store
                .publication(&accepted.publication.key())
                .await
                .unwrap(),
            Some(accepted.publication.clone())
        );
    }
    // Keep one helper publication on a second claim while ending the namespace
    // owner: shared code survives exactly as long as a publication needs it.
    let retained = packages
        .snapshot
        .advertisements
        .iter()
        .find(|accepted| !accepted.publication.code_links.is_empty())
        .unwrap();
    let reader =
        ReferrerClaim::unguarded(ArtifactReferrer::HostPin(HostArtifactPin::mint())).unwrap();
    store
        .acquire_publication(&reader, &retained.publication.key())
        .await
        .unwrap();
    let installer = ReferrerClaim::unguarded(ArtifactReferrer::HostPin(pin)).unwrap();
    store
        .end_package_referrer(&ResolvedArtifactCleanup {
            referrer: installer.referrer().clone(),
            carries: Vec::new(),
        })
        .await
        .unwrap();
    for accepted in &packages.snapshot.advertisements {
        assert_eq!(
            store
                .publication(&accepted.publication.key())
                .await
                .unwrap()
                .is_some(),
            accepted.publication.key() == retained.publication.key()
        );
    }
    for id in release.content.documents.keys() {
        assert!(
            backend
                .module_artifacts()
                .get_module_artifact(&id.to_string())
                .await
                .unwrap()
                .is_some()
        );
    }
    assert!(
        store
            .acquire_publication(&installer, &retained.publication.key())
            .await
            .is_err()
    );
    store
        .end_package_referrer(&ResolvedArtifactCleanup {
            referrer: reader.referrer().clone(),
            carries: Vec::new(),
        })
        .await
        .unwrap();
    for id in release.content.documents.keys() {
        assert!(
            backend
                .module_artifacts()
                .get_module_artifact(&id.to_string())
                .await
                .unwrap()
                .is_none()
        );
    }
    for id in release.content.links.keys() {
        assert!(store.link(id).await.unwrap().is_none());
    }
    for id in release.content.packages.keys() {
        assert!(store.package(id).await.unwrap().is_none());
    }
}
