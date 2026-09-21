use super::*;

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn assert_retired_history(
    registry: &Arc<dyn ProcessRegistry>,
    first_ref: &ProcessRef,
    first: &ProcessRecord,
    second: &ProcessRecord,
) {
    let retired_page = registry
        .event_page_ref(
            first_ref,
            std::num::NonZeroUsize::new(8).expect("non-zero page size"),
            crate::ProcessEventQueryMode::Lite,
            None,
        )
        .await
        .expect("a retired history is a typed page outcome");
    assert!(
        matches!(
            retired_page,
            crate::ProcessEventReadOutcome::NoLongerRetained(
                crate::ProcessEventHistoryRetention::Retired {
                    requested_incarnation,
                    current_incarnation,
                }
            ) if requested_incarnation == first.incarnation
                && current_incarnation == second.incarnation
        ),
        "a page token for an old incarnation must report retired history: {retired_page:?}"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn assert_pruned_history(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &ProcessId,
    pruned_at_ms: u64,
) {
    let pruned_page = registry
        .event_page(
            process_id,
            std::num::NonZeroUsize::new(8).expect("non-zero page size"),
            crate::ProcessEventQueryMode::Lite,
            None,
        )
        .await
        .expect("a pruned history is a typed page outcome");
    assert!(
        matches!(
            pruned_page,
            crate::ProcessEventReadOutcome::NoLongerRetained(
                crate::ProcessEventHistoryRetention::Pruned {
                    pruned_at_ms: observed,
                    ..
                }
            ) if observed == pruned_at_ms
        ),
        "a pruned page read must preserve the tombstone timestamp: {pruned_page:?}"
    );
}
