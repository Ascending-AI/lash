use std::sync::Arc;
#[tokio::test]
async fn in_memory_cross_owner_attachment_adoption_conformance() {
    Box::pin(crate::cross_owner_attachment_adoption_conformance(
        Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
    ))
    .await;
}

#[tokio::test]
async fn in_memory_attachment_condemnation_enumeration_conformance() {
    crate::attachment_condemnation_enumeration_conformance(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .await;
}

#[tokio::test]
async fn in_memory_store_satisfies_the_maintenance_outcome_contract() {
    crate::conformance::store_maintenance_outcome_contract(
        "in-memory",
        || {
            Arc::new(crate::InMemorySessionStoreFactory::new())
                as Arc<dyn crate::SessionStoreFactory>
        },
        // The in-memory sweep reads only process memory under the write
        // transaction: it has no failure path to inject.
        None,
    )
    .await;
}

#[tokio::test]
async fn in_memory_terminal_evidence_retention_conformance() {
    crate::retention_conformance(std::sync::Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .await;
}
