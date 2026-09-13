use super::*;

#[tokio::test]
async fn sqlite_real_turn_cancel_closure_survives_every_cold_process_crash_cut() {
    let dir = tempfile::tempdir().expect("SQLite cancellation cold-process tempdir");
    let database = dir.path().join("cold-process-turn-cancel.db");
    cold_process_turn_parent::assert_real_turn_cancel_kill_recovery(
        dir.path(),
        |action, nonce, marker| {
            let mut command = tokio::process::Command::new(lash_conformance::helper_executable(
                "sqlite-await-event-helper",
            ));
            command.arg(&database).arg(action).arg(nonce).arg(marker);
            command
        },
    )
    .await;
}
