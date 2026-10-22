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

pub(super) async fn prepare(
    documents: &lash_vm_runtime::KernelDocuments,
    start: &LinkedStart,
    held: &Arc<FunctionRegistry>,
    authored: &std::collections::BTreeSet<lash_vocabulary::LinkId>,
) -> Result<PreparedExecution, String> {
    let (mut content, mut identified) = documents
        .load_linked_identified(start.link)
        .await
        .map_err(|error| error.to_string())?;
    let mut reach = content
        .inspect(
            start.link,
            &lash_kernel_package::ReachSelection::Start(start.selector.clone()),
        )
        .map_err(|error| error.to_string())?;
    // Saved cell functions are roots too: their values arrive through the
    // session heap rather than an Import in the new cell's document.
    for link in authored.iter().filter(|link| **link != start.link) {
        let (saved, saved_documents) = documents
            .load_linked_identified(*link)
            .await
            .map_err(|error| error.to_string())?;
        let saved_reach = saved
            .inspect(*link, &lash_kernel_package::ReachSelection::AllExports)
            .map_err(|error| error.to_string())?;
        content.documents.extend(saved.documents);
        identified.extend(saved_documents);
        content.links.extend(saved.links);
        content.packages.extend(saved.packages);
        for (link, functions) in saved_reach.functions {
            reach.functions.entry(link).or_default().extend(functions);
        }
        reach.machine.kernels.extend(saved_reach.machine.kernels);
        reach.machine.natives.extend(saved_reach.machine.natives);
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
            .map(|(id, document)| (*id, Arc::new(document.clone())))
            .collect(),
        &reach.functions,
        &reach.machine,
        held,
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
        );
        let release = lash_vm_releases::helper_release().unwrap();
        let link = *release.content.links.keys().next().unwrap();
        let document = &release.content.documents[&release.content.links[&link].document];
        let name = document
            .exports
            .keys()
            .find(|name| lash_kernel_check::startable_export(document, name).is_ok())
            .unwrap();
        let start = LinkedStart {
            link,
            selector: lash_vocabulary::StartSelector::Export(name.clone()),
        };
        let held = Arc::new(release.functions.clone());
        for _ in 0..2 {
            reads.store(0, Ordering::Relaxed);
            let prepared = prepare(&documents, &start, &held, &Default::default())
                .await
                .unwrap();
            assert_eq!(
                reads.load(Ordering::Relaxed),
                prepared.documents.len(),
                "each current document must be read once per preparation, including subsequent preparations"
            );
        }
    }
}
