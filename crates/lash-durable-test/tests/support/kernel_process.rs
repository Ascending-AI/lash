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
            &document,
        )
        .await
        .expect("the document publishes");
    serde_json::to_value(lash_vm_runtime::KernelProcessInput {
        document: identity,
        entry: lash::workflow::document::Name::new(entry),
        args: serde_json::Map::new(),
    })
    .expect("the input encodes")
}
