use super::*;

#[expect(
    clippy::chunks_exact_to_as_chunks,
    clippy::expect_used,
    reason = "conformance-law fixture: the helper mutates a token produced by the implementation"
)]
fn token_with_sequence(token: &crate::ProcessEventPageToken, after_sequence: u64) -> String {
    let encoded = serde_json::to_string(token).expect("encode valid process-event page token");
    let encoded: String = serde_json::from_str(&encoded).expect("decode token string envelope");
    let hex = encoded
        .strip_prefix("process-event-page:v1:")
        .expect("known process-event page token version");
    let bytes = hex
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).expect("ASCII token hex");
            u8::from_str_radix(pair, 16).expect("valid token hex")
        })
        .collect::<Vec<_>>();
    let mut wire: serde_json::Value =
        serde_json::from_slice(&bytes).expect("decode token wire payload");
    wire["after_sequence"] = serde_json::json!(after_sequence);
    let bytes = serde_json::to_vec(&wire).expect("encode forged token wire payload");
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    serde_json::to_string(&format!("process-event-page:v1:{hex}"))
        .expect("encode forged token string envelope")
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn assert_out_of_range_tokens_are_rejected(registry: Arc<dyn ProcessRegistry>) {
    let process_id = ProcessId::from("process-event-page-sql-cursor-range");
    registry
        .register_process(
            registration(&process_id).with_extra_event_types([plain_event_type("cursor.event")]),
        )
        .await
        .expect("register cursor-range process");
    for index in 0..2 {
        registry
            .append_event(
                &process_id,
                ProcessEventAppendRequest::new(
                    "cursor.event",
                    serde_json::json!({ "index": index }),
                ),
            )
            .await
            .expect("append cursor-range event");
    }
    let first = registry
        .event_page(
            &process_id,
            std::num::NonZeroUsize::MIN,
            crate::ProcessEventQueryMode::Full,
            None,
        )
        .await
        .expect("read first cursor-range page");
    let crate::ProcessEventReadOutcome::Retained(first) = first else {
        panic!("new cursor-range history must be retained");
    };
    let crate::ProcessEventPageMore::More { continuation } = first.more else {
        panic!("cursor-range fixture must issue a continuation");
    };

    for after_sequence in [i64::MAX as u64 + 1, u64::MAX] {
        let encoded = token_with_sequence(&continuation, after_sequence);
        let mut returned_events = Vec::new();
        let result: Result<(), String> = async {
            let token: crate::ProcessEventPageToken =
                serde_json::from_str(&encoded).map_err(|error| error.to_string())?;
            let outcome = registry
                .event_page(
                    &process_id,
                    std::num::NonZeroUsize::MIN,
                    crate::ProcessEventQueryMode::Full,
                    Some(token),
                )
                .await
                .map_err(|error| error.to_string())?;
            if let crate::ProcessEventReadOutcome::Retained(crate::ProcessEventPage {
                events: crate::ProcessEventPageEvents::Full(events),
                ..
            }) = outcome
            {
                returned_events.extend(events);
            }
            Ok(())
        }
        .await;
        assert!(
            result.is_err(),
            "SQL-unrepresentable cursor {after_sequence} must be rejected"
        );
        assert!(
            returned_events.is_empty(),
            "a rejected cursor must return no process events"
        );
    }
}

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
