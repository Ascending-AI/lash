use lash::tracing::{TraceContext, TraceEvent, TraceRecord};

#[test]
fn host_owned_trace_records_supply_fresh_identity_and_timestamp() {
    let context = TraceContext {
        session_id: Some("host-session".into()),
        ..Default::default()
    };
    let event = TraceEvent::TurnStarted {
        metadata: Default::default(),
    };
    let before = std::time::SystemTime::now();
    let first = TraceRecord::host_owned(context.clone(), event.clone()).unwrap();
    let second = TraceRecord::host_owned(context.clone(), event.clone()).unwrap();
    assert_ne!(first.id, second.id);
    assert_eq!(first.id.len(), 32);
    assert_eq!(first.context, context);
    assert_eq!(first.event, event);
    assert!(first.timestamp >= lash::tracing::TraceTimestamp::from_system_time(before).unwrap());
    assert_eq!(
        serde_json::from_value::<TraceRecord>(serde_json::to_value(&first).unwrap()).unwrap(),
        first
    );
}
