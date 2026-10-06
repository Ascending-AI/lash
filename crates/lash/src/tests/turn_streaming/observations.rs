use super::*;

#[tokio::test]
pub(super) async fn incarnation_change_invalidates_cursor() {
    let original = crate::observe::InMemoryLiveReplayStore::default();
    let preserved = crate::observe::InMemoryLiveReplayStore::reopen_preserving_history(&original);
    lash_conformance::incarnation_change_invalidates_cursor(
        Arc::new(original),
        Arc::new(crate::observe::InMemoryLiveReplayStore::default()),
        Arc::new(preserved),
    )
    .await;
}
