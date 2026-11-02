#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_producer_wakes_no_absent_or_deleted_session() {
    let backend = TestBackend::open(SUBSTRATE).await;
    lash_conformance::a_producer_wakes_no_absent_or_deleted_session(stores(&backend), "sqlite")
        .await;
}

include!("session_mail_reopen.rs");
