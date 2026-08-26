//! Cross-backend contract for the two arms of a process-event append.

use super::*;

/// The two arms of a process-event append leave different durable footprints,
/// and every entry point into the append sequence must produce the same one.
///
/// The insert arm writes exactly one event row and advances the wake allocation
/// floor to that event's sequence. The replay arm writes no event row and must
/// leave the floor alone: re-advancing it there would push a later incarnation's
/// sequences past a wake that was already allocated and delivered, for a call
/// that persisted nothing.
///
/// Each backend spells the sequence once and reaches it from three entry points
/// — the unfenced host append, unleased completion under an explicit authority,
/// and leased completion. All three are exercised here. The completion paths
/// settle their repeat call on the already-terminal row rather than the replay
/// arm proper; the observable contract is the same either way, and asserting it
/// per entry point is what catches a floor advance or an event row escaping
/// onto a path that persisted nothing.
pub(super) async fn process_event_append_arms_are_ordered(registry: Arc<dyn ProcessRegistry>) {
    let target_session_id = "append-arm-ordering-target";

    // Entry point 1: the unfenced host append, which reaches the replay arm
    // proper through a repeated replay key.
    let host_id = "append-arm-host";
    registry
        .register_process(
            registration(host_id)
                .with_extra_event_types([wake_event_type("producer.wake")])
                .with_wake_session_id(Some(target_session_id.to_string())),
        )
        .await
        .expect("register host-append arm process");
    let host_request = || {
        ProcessEventAppendRequest::new(
            "producer.wake",
            serde_json::json!({"wake_input": "append arm ordering"}),
        )
        .with_replay_key("append-arm-host:wake:1")
    };
    let baseline = append_arm_footprint(&registry, host_id, target_session_id).await;
    assert_eq!(
        baseline,
        (0, None),
        "a registered process has no events and no sender floor yet"
    );
    let inserted = registry
        .append_event(host_id, host_request())
        .await
        .expect("host append takes the insert arm");
    assert_eq!(
        append_arm_footprint(&registry, host_id, target_session_id).await,
        (1, Some(inserted.event.sequence)),
        "the insert arm writes one event row and advances the floor to it"
    );
    let replayed = registry
        .append_event(host_id, host_request())
        .await
        .expect("host append takes the replay arm");
    assert_eq!(replayed.event.sequence, inserted.event.sequence);
    assert_eq!(
        append_arm_footprint(&registry, host_id, target_session_id).await,
        (1, Some(inserted.event.sequence)),
        "the replay arm writes no event row and leaves the floor where the insert put it"
    );

    // Entry point 2: unleased completion under an explicit authority.
    let unleased_id = "append-arm-unleased-completion";
    registry
        .register_process(
            registration(unleased_id).with_wake_session_id(Some(target_session_id.to_string())),
        )
        .await
        .expect("register unleased-completion arm process");
    let unleased_output = ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
        serde_json::json!({"append_arm": "unleased"}),
    ));
    assert!(matches!(
        registry
            .complete_process(
                unleased_id,
                unleased_output.clone(),
                ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("unleased completion takes the insert arm"),
        crate::ProcessCompletionOutcome::Committed(_)
    ));
    let unleased_footprint = append_arm_footprint(&registry, unleased_id, target_session_id).await;
    assert_eq!(
        unleased_footprint.0, 1,
        "unleased completion writes exactly one terminal event row"
    );
    assert_eq!(
        unleased_footprint.1,
        Some(terminal_sequence(&registry, unleased_id).await),
        "unleased completion advances the floor to its terminal event"
    );
    assert!(matches!(
        registry
            .complete_process(
                unleased_id,
                unleased_output,
                ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("unleased completion is idempotent"),
        crate::ProcessCompletionOutcome::AlreadyApplied { .. }
    ));
    assert_eq!(
        append_arm_footprint(&registry, unleased_id, target_session_id).await,
        unleased_footprint,
        "a repeated unleased completion writes no event row and does not move the floor"
    );

    // Entry point 3: leased completion.
    let leased_id = "append-arm-leased-completion";
    registry
        .register_process(
            registration(leased_id).with_wake_session_id(Some(target_session_id.to_string())),
        )
        .await
        .expect("register leased-completion arm process");
    let lease = registry
        .claim_process_lease(
            leased_id,
            &crate::LeaseOwnerIdentity::opaque("append-arm-owner", "append-arm-owner:i"),
            60_000,
        )
        .await
        .expect("claim leased-completion arm lease")
        .acquired()
        .expect("leased-completion arm lease acquired");
    let leased_output = ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
        serde_json::json!({"append_arm": "leased"}),
    ));
    assert!(matches!(
        registry
            .complete_process_with_lease(&lease, leased_output.clone())
            .await
            .expect("leased completion takes the insert arm"),
        crate::ProcessCompletionOutcome::Committed(_)
    ));
    let leased_footprint = append_arm_footprint(&registry, leased_id, target_session_id).await;
    assert_eq!(
        leased_footprint.0, 1,
        "leased completion writes exactly one terminal event row"
    );
    assert_eq!(
        leased_footprint.1,
        Some(terminal_sequence(&registry, leased_id).await),
        "leased completion advances the floor to its terminal event"
    );
    assert!(matches!(
        registry
            .complete_process_with_lease(&lease, leased_output)
            .await
            .expect("leased completion is idempotent"),
        crate::ProcessCompletionOutcome::AlreadyApplied { .. }
    ));
    assert_eq!(
        append_arm_footprint(&registry, leased_id, target_session_id).await,
        leased_footprint,
        "a repeated leased completion writes no event row and does not move the floor"
    );
}

/// The durable footprint of a process's appends: how many event rows exist, and
/// where the sender floor for `target_session_id` stands.
async fn append_arm_footprint(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &str,
    target_session_id: &str,
) -> (usize, Option<u64>) {
    let events = registry
        .events_after(process_id, 0)
        .await
        .expect("read append-arm event rows")
        .len();
    let floor = registry
        .wake_allocation_floor_for_testing(target_session_id, process_id)
        .await
        .expect("read append-arm sender floor");
    (events, floor)
}

async fn terminal_sequence(registry: &Arc<dyn ProcessRegistry>, process_id: &str) -> u64 {
    registry
        .events_after(process_id, 0)
        .await
        .expect("read append-arm event rows")
        .last()
        .expect("a completed process has a terminal event")
        .sequence
}
