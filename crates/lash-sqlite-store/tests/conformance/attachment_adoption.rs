use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_cross_owner_attachment_adoption_conformance() {
    let dir = tempfile::tempdir().unwrap();
    Box::pin(
        lash_conformance::cross_owner_attachment_adoption_conformance(Arc::new(
            SqliteSessionStoreFactory::new(dir.path()),
        )),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_attachment_condemnation_enumeration_conformance() {
    let dir = tempfile::tempdir().unwrap();
    lash_conformance::attachment_condemnation_enumeration_conformance(Arc::new(
        SqliteSessionStoreFactory::new(dir.path()),
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_attachment_condemnation_delete_crash_survives_cold_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    lash_conformance::attachment_condemnation_delete_crash_survives_cold_reopen(
        Arc::new(SqliteSessionStoreFactory::new(&root)),
        move || async move {
            Arc::new(SqliteSessionStoreFactory::new(root)) as Arc<dyn SessionStoreFactory>
        },
    )
    .await;
}
