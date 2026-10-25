//! Load retained linked reach without fulfilling against current advertisements.
use lash_kernel_doc::FunctionRegistry;

use lash_vocabulary::names::LinkedStart;
use std::sync::Arc;

pub(super) struct PreparedExecution {
    pub content: lash_kernel_package::LinkedContent,
    pub documents: std::collections::BTreeMap<
        lash_vocabulary::DocumentId,
        lash_kernel_doc::IdentifiedDocument,
    >,
    pub loaded: lash_vm_broker::kernel::loader::Assembled,
}

pub(super) struct LoadedCode {
    pub content: lash_kernel_package::LinkedContent,
    pub documents: std::collections::BTreeMap<
        lash_vocabulary::DocumentId,
        lash_kernel_doc::IdentifiedDocument,
    >,
}

pub(super) async fn load(
    documents: &lash_vm_runtime::KernelDocuments,
    start: &LinkedStart,
    authored: &std::collections::BTreeSet<lash_vocabulary::LinkId>,
) -> Result<LoadedCode, String> {
    let (mut content, mut identified) = documents
        .load_linked_identified(start.link)
        .await
        .map_err(|error| error.to_string())?;
    for link in authored.iter().filter(|link| **link != start.link) {
        let (saved, saved_documents) = documents
            .load_linked_identified(*link)
            .await
            .map_err(|error| error.to_string())?;
        content.documents.extend(saved.documents);
        identified.extend(saved_documents);
        content.links.extend(saved.links);
        content.packages.extend(saved.packages);
    }
    Ok(LoadedCode {
        content,
        documents: identified,
    })
}

pub(super) fn prepare(
    loaded: LoadedCode,
    start: &LinkedStart,
    held: &Arc<FunctionRegistry>,
    authored: &std::collections::BTreeSet<lash_vocabulary::LinkId>,
    documents: &lash_vm_runtime::KernelDocuments,
    selection: &lash_kernel_doc::Selection,
) -> Result<PreparedExecution, String> {
    let helpers = documents.helpers().as_ref();
    let LoadedCode {
        content,
        documents: identified,
    } = loaded;
    let selected = |link, selector: &lash_kernel_package::ReachSelection| {
        match selection {
            lash_kernel_doc::Selection::Fresh => documents.inspect(&content, link, selector),
            lash_kernel_doc::Selection::Restored(_) => content.inspect_ordinary(link, selector),
        }
        .map_err(|error| error.to_string())
    };
    let mut reach = selected(
        start.link,
        &lash_kernel_package::ReachSelection::Start(start.selector.clone()),
    )?;
    for link in authored.iter().filter(|link| **link != start.link) {
        let saved_reach = selected(*link, &lash_kernel_package::ReachSelection::AllExports)?;
        for (link, functions) in saved_reach.functions {
            reach.functions.entry(link).or_default().extend(functions);
        }
        reach.machine.extend(&saved_reach.machine);
    }
    if let lash_kernel_doc::Selection::Restored(restored) = selection {
        reach.machine = content
            .inspect_restored(start.link, restored, helpers)
            .map_err(|error| error.to_string())?
            .machine;
    }
    let link = content
        .links
        .get(&start.link)
        .ok_or("the root Link is absent")?;
    let root = content
        .documents
        .get(&link.document)
        .ok_or("the root Document is absent")?;
    let loaded = lash_vm_broker::kernel::loader::declare_reach(
        root,
        &content
            .documents
            .iter()
            .map(|(id, document)| (*id, Arc::clone(document)))
            .collect(),
        &reach.functions,
        &reach.machine,
        held,
        helpers,
        selection,
    )
    .map_err(|error| error.to_string())?;
    Ok(PreparedExecution {
        content,
        loaded,
        documents: identified,
    })
}

#[cfg(test)]
mod work_laws {
    use super::*;
    use lash_core::ReferrerClaim;
    use lash_core_store::artifact_referrer::ResolvedArtifactCleanup;
    use lash_execution_contracts::module_artifacts::{
        ArtifactStoreError, ModuleArtifactHold, ModuleArtifactStore,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counted {
        inner: Arc<dyn ModuleArtifactStore>,
        reads: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl ModuleArtifactStore for Counted {
        async fn publish_module_artifact(
            &self,
            claim: &ReferrerClaim,
            name: &str,
            bytes: &[u8],
        ) -> Result<(), ArtifactStoreError> {
            self.inner.publish_module_artifact(claim, name, bytes).await
        }
        async fn acquire_module_artifact(
            &self,
            claim: &ReferrerClaim,
            name: &str,
        ) -> Result<(), ArtifactStoreError> {
            self.inner.acquire_module_artifact(claim, name).await
        }
        async fn hold_module_artifacts(
            &self,
            claim: &ReferrerClaim,
            artifacts: &[ModuleArtifactHold<'_>],
        ) -> Result<(), ArtifactStoreError> {
            self.inner.hold_module_artifacts(claim, artifacts).await
        }
        async fn end_module_referrer(
            &self,
            cleanup: &ResolvedArtifactCleanup,
        ) -> Result<(), ArtifactStoreError> {
            self.inner.end_module_referrer(cleanup).await
        }
        async fn get_module_artifact(
            &self,
            name: &str,
        ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.inner.get_module_artifact(name).await
        }
    }

    /// FIG-7169: document admission and inspection use the workers' actual
    /// helper image, including a custom image that excludes shipped helpers.
    #[tokio::test]
    async fn document_owner_uses_its_custom_worker_helper_image() {
        use lash_execution_contracts::process::linking::{
            AdmissionRefusal, EnginePortError, FulfilRequest, LinkPurpose, ReachSelector,
        };
        use lash_kernel_doc::{
            ActExpr, ActKind, BuildHelpers, Callee, Document, Expr, FunctionRegistry, NumberPolicy,
            Stmt,
        };
        let host = crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
        let context = host.context();
        let backend = context.backend();
        let definition = lash_kernel_doc::parse_definition(
            "function custom.helper() -> Null\nkernel 1\ncharge 1\nbody { return null }",
        )
        .unwrap();
        let function = definition.identity();
        let mut registry = FunctionRegistry::declarations();
        registry.register(definition, None).unwrap();
        let helpers = Arc::new(BuildHelpers::new(registry.clone(), [function]).unwrap());
        let library = Arc::new(
            lash_vm_client::packages::WorkerLibrary::with_helpers(
                &registry,
                [function],
                &Default::default(),
                helpers,
            )
            .unwrap(),
        );
        let workers =
            lash_vm_client::service::Service::default().with_library(Arc::clone(&library));
        let bounds = workers.config().run_bounds;
        let factory = lash_vm_runtime::KernelProcessPluginFactory::new(
            workers,
            lash_vm_runtime::TYPESCRIPT_DIALECT,
            bounds,
        );
        let documents = factory
            .documents(backend.module_artifacts(), backend.package_store())
            .unwrap();
        assert!(Arc::ptr_eq(documents.helpers(), library.helpers()));
        let documents = documents.bind(backend.module_artifacts(), backend.package_store());
        assert!(Arc::ptr_eq(documents.helpers(), library.helpers()));
        let document = |function| {
            let mut document = Document::new(
                NumberPolicy::BySpelling,
                vec![Stmt::Print {
                    value: Expr::Act(Box::new(ActExpr {
                        kind: ActKind::Call(Callee::Local(function)),
                        args: vec![],
                    })),
                }],
            );
            document.library.insert(function);
            document
        };
        let claim = ReferrerClaim::guarded(lash_core::ReferrerGuard::Journal(
            context.execution_scope().journal_identity().unwrap(),
        ));
        let id = documents
            .admit_document(&claim, &document(function).into())
            .await
            .unwrap();
        let request = |document| FulfilRequest {
            document,
            selector: ReachSelector::Start(lash_vocabulary::StartSelector::Main),
            snapshot: Default::default(),
            context: lash_vocabulary::PublicationContext {
                domain: lash_vocabulary::PublicationDomain("custom-image-law".into()),
                scope: lash_vocabulary::PublicationScope::Host,
                publisher: lash_vocabulary::PublisherInstance("custom-image-law".into()),
                root: lash_vocabulary::Segment::new("custom").unwrap(),
            },
            purpose: LinkPurpose::Cell,
        };
        let admitted = documents.fulfil(&request(id)).await.unwrap();
        assert!(
            admitted
                .inspection
                .machine
                .kernels
                .contains(&lash_kernel_doc::KERNEL_VERSION)
        );
        let shipped = *lash_vm_releases::helper_release()
            .unwrap()
            .helpers
            .members()
            .next()
            .unwrap();
        assert!(!library.helpers().contains(&shipped));
        let refused = document(shipped);
        assert!(matches!(
            documents
                .admit_document(&claim, &refused.clone().into())
                .await,
            Err(EnginePortError::Refused(
                AdmissionRefusal::InvalidDocument { .. }
            ))
        ));
        // Retain structurally valid bytes independently to test the cold
        // inspection boundary, which must use the same custom image too.
        let identified = lash_kernel_doc::IdentifiedDocument::new(refused);
        backend
            .module_artifacts()
            .publish_module_artifact(&claim, &identified.id().to_string(), identified.body())
            .await
            .unwrap();
        assert!(matches!(
            documents.fulfil(&request(identified.id())).await,
            Err(EnginePortError::Refused(
                AdmissionRefusal::InvalidDocument { .. }
            ))
        ));
    }

    /// FIG-7135: linked loading's validated document facts reach execution
    /// preparation intact, without another read just to recover identity.
    #[tokio::test]
    async fn preparing_linked_code_reads_each_document_once() {
        let host = crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
        let context = host.context();
        let stores = context.backend().stores();
        let reads = Arc::new(AtomicUsize::new(0));
        let documents = lash_vm_runtime::KernelDocuments::new(
            Arc::new(Counted {
                inner: stores.module_artifacts(),
                reads: reads.clone(),
            }),
            stores.package_store(),
            std::sync::Arc::clone(
                lash_vm_client::packages::WorkerLibrary::built()
                    .unwrap()
                    .helpers(),
            ),
        );
        let release = lash_vm_releases::helper_release().unwrap();
        let definition = lash_kernel_doc::parse_definition(
            "function test.run() -> Null\nkernel 1\ncharge 1\nbody { return null }",
        )
        .unwrap();
        let mut document =
            lash_kernel_doc::parse_document("kernel 1\nnumbers float\nmain { return null }")
                .unwrap();
        document.exports.insert(
            lash_vocabulary::Name::new("run"),
            lash_kernel_doc::Export::Function(lash_kernel_doc::FunctionExport {
                function: definition.identity(),
                signature: definition.signature.clone(),
                convention: lash_kernel_doc::StartConvention::Direct,
                asynchronous: false,
                rest: false,
            }),
        );
        document.functions.insert(definition.identity(), definition);
        let claim = ReferrerClaim::guarded(lash_core::ReferrerGuard::Journal(
            context.execution_scope().journal_identity().unwrap(),
        ));
        documents
            .admit_document(&claim, &document.clone().into())
            .await
            .unwrap();
        let link = lash_vocabulary::Link {
            document: document.identity(),
            fills: Default::default(),
        };
        stores
            .package_store()
            .put_link(
                &claim,
                &lash_core_store::packages::LinkRecord {
                    id: link.identity(),
                    link: link.clone(),
                },
                &[lash_core::ArtifactName {
                    store: lash_core::ArtifactStoreId::KernelDocument,
                    artifact_ref: document.identity().to_string(),
                }],
            )
            .await
            .unwrap();
        let start = LinkedStart {
            link: link.identity(),
            selector: lash_vocabulary::StartSelector::Export(lash_vocabulary::Name::new("run")),
        };
        let held = Arc::new(release.functions.clone());
        for _ in 0..2 {
            reads.store(0, Ordering::Relaxed);
            let loaded = load(&documents, &start, &Default::default()).await.unwrap();
            let prepared = prepare(
                loaded,
                &start,
                &held,
                &Default::default(),
                &documents,
                &lash_kernel_doc::Selection::Fresh,
            )
            .unwrap();
            assert_eq!(
                reads.load(Ordering::Relaxed),
                prepared.documents.len(),
                "each current document must be read once per preparation, including subsequent preparations"
            );
        }
    }
}
