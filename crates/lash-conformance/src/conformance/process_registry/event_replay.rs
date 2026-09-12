use super::*;
use pretty_assertions::assert_eq;

pub(super) async fn canonical_process_event_payload_replay(registry: Arc<dyn ProcessRegistry>) {
    let process_id = ProcessId::from("canonical-process-event-payload-replay");
    registry
        .register_process(
            registration(&process_id).with_extra_event_types([plain_event_type("signal.zero")]),
        )
        .await
        .expect("register canonical-payload process");
    let replay_key = format!("process:{process_id}:signal.zero:1");
    let first = registry
        .append_event(
            &process_id,
            ProcessEventAppendRequest::new("signal.zero", serde_json::json!({"value": -0.0}))
                .with_replay_key(&replay_key),
        )
        .await
        .expect("append negative-zero payload");
    let replay = registry
        .append_event(
            &process_id,
            ProcessEventAppendRequest::new("signal.zero", serde_json::json!({"value": 0.0}))
                .with_replay_key(replay_key),
        )
        .await
        .expect("canonical positive-zero retry must be idempotent");
    assert_eq!(
        replay.event.sequence, first.event.sequence,
        "canonically equal zero payloads must share the replayed event"
    );
}

pub(super) async fn long_cancellation_reason_replay_is_backend_safe(
    registry: Arc<dyn ProcessRegistry>,
) {
    let process_id = ProcessId::from("long-cancellation-reason-replay");
    registry
        .register_process(registration(&process_id))
        .await
        .expect("register long-cancellation process");
    let reason = (0..800)
        .map(|index| format!("{index:08x}"))
        .collect::<String>();
    let request = ProcessEventAppendRequest::cancel_requested(&process_id, Some(reason));
    let first = registry
        .append_event(&process_id, request.clone())
        .await
        .expect("append cancellation with long reason");
    let replay = registry
        .append_event(&process_id, request)
        .await
        .expect("replay cancellation with long reason");
    assert_eq!(
        replay.event.sequence, first.event.sequence,
        "long cancellation reason retries must remain idempotent on every backend"
    );
}
