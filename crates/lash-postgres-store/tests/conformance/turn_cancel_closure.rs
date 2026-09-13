use super::*;

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
