//! Host process definitions written as kernel documents.

/// Publishes the kernel document `text` under a host pin on `backend`, and
/// answers the start payload of its entry `entry`, without arguments.
pub async fn payload(backend: &lash::Backend, text: &str, entry: &str) -> serde_json::Value {
    let document = lash::workflow::document::parse_document(text).expect("the document parses");
    let identity = lash_vm_runtime::KernelDocuments::new(backend.module_artifacts())
        .publish(
            &lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
                lash_core::HostArtifactPin::mint(),
            ))
            .expect("a host pin is unguarded"),
            &lash_kernel_doc::IdentifiedDocument::new(document.clone()),
            &lash_vm_runtime::lash_packages()
                .map(|packages| std::sync::Arc::clone(packages.functions()))
                .ok()
                .and_then(|functions| {
                    lash::workflow::document::CheckedDeclarations::required(
                        &document,
                        functions.as_ref(),
                    )
                    .ok()
                })
                .expect("the document's helpers are shipped"),
        )
        .await
        .expect("the document publishes");
    serde_json::to_value(lash_vm_runtime::KernelProcessInput {
        packages: lash_vm_runtime::lash_packages()
            .expect("lash's packages")
            .digest(),
        document: identity,
        functions: document.manifest.functions.keys().copied().collect(),
        entry: lash::workflow::document::Name::new(entry),
        args: serde_json::Map::new(),
    })
    .expect("the input encodes")
}
