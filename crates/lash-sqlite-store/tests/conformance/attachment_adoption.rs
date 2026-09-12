use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_cross_owner_attachment_adoption_conformance() {
    let dir = tempfile::tempdir().unwrap();
    lash_conformance::cross_owner_attachment_adoption_conformance(Arc::new(
        SqliteSessionStoreFactory::new(dir.path()),
    ))
    .await;
}
