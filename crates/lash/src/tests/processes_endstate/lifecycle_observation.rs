use super::*;

pub(super) fn assert_working_process_lifecycle(
    events: &[lash_core::ProcessEvent],
    process_id: &ProcessId,
    session_events: &[Arc<lash_core::SessionObservationEvent>],
) {
    let first_started = events
        .iter()
        .position(|event| event.event_type == "process.first_started")
        .unwrap_or_else(|| panic!("missing process.first_started: {events:?}"));
    let completed = events
        .iter()
        .position(|event| event.event_type == "process.completed")
        .unwrap_or_else(|| panic!("missing process.completed: {events:?}"));
    assert!(
        first_started < completed,
        "process.first_started must precede process.completed: {events:?}"
    );
    let durable_kinds = events
        .iter()
        .filter_map(|event| {
            lash_core::SessionProcessEventKind::from_durable_event(
                &event.event_type,
                event.sequence,
            )
        })
        .collect::<Vec<_>>();
    assert!(
        matches!(
            durable_kinds.as_slice(),
            [
                lash_core::SessionProcessEventKind::Started { .. },
                lash_core::SessionProcessEventKind::Waiting { .. },
                lash_core::SessionProcessEventKind::Resumed { .. },
                lash_core::SessionProcessEventKind::Completed { .. },
            ]
        ),
        "working engine lifecycle must have the exact four transitions: {durable_kinds:?}"
    );
    let observed_kinds = session_events
        .iter()
        .filter_map(|event| match &event.payload {
            lash_core::SessionObservationEventPayload::ProcessChanged { kind, process_ids }
                if process_ids.as_slice() == std::slice::from_ref(process_id) =>
            {
                Some(*kind)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        observed_kinds, durable_kinds,
        "session lifecycle must keep durable sequences"
    );
}
