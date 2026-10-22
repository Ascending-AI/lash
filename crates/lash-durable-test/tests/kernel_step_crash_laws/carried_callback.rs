//! Shared callback pin/carry properties on SQLite.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_preexisting_callback_authorizes_after_carry_and_a_cold_reopen_on_sqlite() {
    super::callback_carry::a_preexisting_callback_authorizes_after_carry_and_a_cold_reopen_on_sqlite().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_negative_resolution_stays_pinned_across_carry_and_a_cold_reopen_on_sqlite() {
    super::callback_carry::a_negative_resolution_stays_pinned_across_carry_and_a_cold_reopen_on_sqlite().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resolved_callback_keeps_its_capture_and_target_across_carry_and_a_cold_reopen_on_sqlite()
{
    super::callback_carry::a_resolved_callback_keeps_its_capture_and_target_across_carry_and_a_cold_reopen_on_sqlite().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_code_only_imported_callback_keeps_publication_provenance_across_carry_and_cold_reopen() {
    Box::pin(super::callback_carry::a_code_only_imported_callback_keeps_publication_provenance_across_carry_and_cold_reopen()).await;
}
