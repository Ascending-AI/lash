use super::*;
use pretty_assertions::assert_eq;

/// `count_events_through` counts events whose sequence is at or below the
/// bound, for every `u64` bound a caller can pass.
///
/// The SQL backends store sequences as signed 64-bit integers, so every stored
/// sequence is at most `i64::MAX` and any bound at or above it means "through
/// the end". A bound past the signed range must still count every event, not
/// wrap to a negative value that counts none (FIG-3601).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn count_events_through_counts_every_event_at_any_top_bound(
    registry: Arc<dyn ProcessRegistry>,
) {
    let process_id = ProcessId::from("count-events-through-top-bound");
    let counted = "signal.counted";
    let record =
        registry
            .register_process(registration(&process_id).with_extra_event_types([
                plain_event_type(counted),
                plain_event_type("signal.other"),
            ]))
            .await
            .expect("register count-bound process");
    let process_ref = ProcessRef::from_record(&record);
    let mut sequences = Vec::new();
    for (index, event_type) in [counted, "signal.other", counted, counted]
        .into_iter()
        .enumerate()
    {
        let appended = registry
            .append_event(
                &process_id,
                ProcessEventAppendRequest::new(event_type, serde_json::json!({ "index": index })),
            )
            .await
            .expect("append count-bound event");
        sequences.push(appended.event.sequence);
    }

    let through_second_counted = sequences[2];
    let bounds = [
        (through_second_counted, 2),
        (i64::MAX as u64, 3),
        (i64::MAX as u64 + 1, 3),
        (u64::MAX, 3),
    ];
    for (bound, expected) in bounds {
        assert_eq!(
            registry
                .count_events_through(&process_id, counted, bound)
                .await
                .expect("count through bound"),
            expected,
            "count_events_through({bound}) must count every `{counted}` event at or below it"
        );
        assert_eq!(
            registry
                .count_events_through_ref(&process_ref, counted, bound)
                .await
                .expect("count through bound by reference"),
            expected,
            "count_events_through_ref({bound}) must count every `{counted}` event at or below it"
        );
    }
}
