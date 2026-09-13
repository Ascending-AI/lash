use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_exact_replay_preserves_different_pending_cancel_authorization_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping PostgreSQL cancellation exact-replay witness: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let factory = Arc::new(storage.session_store_factory())
        as Arc<dyn lash_core::store::ConformanceSessionStoreFactory>;
    lash_conformance::turn_cancel_exact_replay_preserves_different_pending_authorization(factory)
        .await;
}

#[tokio::test]
async fn postgres_real_turn_cancel_closure_survives_every_cold_process_crash_cut_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping PostgreSQL cancellation cold-process matrix: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let url = database_url().expect("configured PostgreSQL database URL");
    let dir = tempfile::tempdir().expect("PostgreSQL cancellation cold-process tempdir");
    cold_process_turn_parent::assert_real_turn_cancel_kill_recovery(
        dir.path(),
        |action, nonce, marker| {
            let mut command = tokio::process::Command::new(lash_conformance::helper_executable(
                "postgres-await-event-helper",
            ));
            command
                .env("LASH_POSTGRES_DATABASE_URL", &url)
                .arg(action)
                .arg(nonce)
                .arg(marker);
            command
        },
    )
    .await;
}
