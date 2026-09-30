use super::*;
use pretty_assertions::assert_eq;

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn canonical_process_event_payload_replay(registry: Arc<dyn ProcessRegistry>) {
    let process_id = registry
        .register_process(
            registration("canonical-process-event-payload-replay")
                .with_extra_event_types([plain_event_type("signal.zero")]),
        )
        .await
        .expect("register canonical-payload process")
        .id;
    let first = registry
        .append_event(
            &process_id,
            signal_request(&process_id, "zero", "1", serde_json::json!({"value": -0.0})),
        )
        .await
        .expect("append negative-zero payload");
    let replay = registry
        .append_event(
            &process_id,
            signal_request(&process_id, "zero", "1", serde_json::json!({"value": 0.0})),
        )
        .await
        .expect("canonical positive-zero retry must be idempotent");
    assert_eq!(
        replay.event.sequence, first.event.sequence,
        "canonically equal zero payloads must share the replayed event"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn long_cancellation_requester_replay_is_backend_safe(
    registry: Arc<dyn ProcessRegistry>,
) {
    let record = registry
        .register_process(registration("long-cancellation-requester-replay"))
        .await
        .expect("register long-cancellation process");
    let process_id = record.id.clone();
    let requester = (0..800)
        .map(|index| format!("{index:08x}"))
        .collect::<String>();
    let request = ProcessEventAppendRequest::cancel_requested(
        &record.id.clone(),
        &lash_core::CancelRequest::new(
            lash_core::CancelOrigin::OperatorRequested,
            format!("actor:{requester}"),
            11,
        ),
    );
    let first = registry
        .append_event(&process_id, request.clone())
        .await
        .expect("append cancellation with long requester");
    let replay = registry
        .append_event(&process_id, request)
        .await
        .expect("replay cancellation with long requester");
    assert_eq!(
        replay.event.sequence, first.event.sequence,
        "long cancellation requester retries must remain idempotent on every backend"
    );
}
