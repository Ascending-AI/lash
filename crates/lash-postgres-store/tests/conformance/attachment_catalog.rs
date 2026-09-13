use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_cross_owner_attachment_adoption_conformance() {
    let Some((_database_lock, storage)) = storage().await else {
        return;
    };
    reset(&storage).await;
    Box::pin(
        lash_conformance::cross_owner_attachment_adoption_conformance(Arc::new(
            storage.session_store_factory(),
        )),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_attachment_condemnation_enumeration_conformance() {
    let Some((_database_lock, storage)) = storage().await else {
        return;
    };
    reset(&storage).await;
    lash_conformance::attachment_condemnation_enumeration_conformance(Arc::new(
        storage.session_store_factory(),
    ))
    .await;
}
