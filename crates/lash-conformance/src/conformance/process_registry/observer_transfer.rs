//! A failed multi-process observer transfer commits nothing.

use super::*;
use pretty_assertions::assert_eq;

/// Everything a transfer could touch, read through the registry's own
/// surface: both processes' records, event logs, observer edges and leases,
/// every wake delivery, the sender floors, and the change feed.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each read is established by the setup above"
)]
async fn transfer_footprint(
    registry: &Arc<dyn crate::ConformanceProcessRegistry>,
    processes: &[ProcessId],
    sessions: &[SessionId],
) -> serde_json::Value {
    let mut per_process = Vec::new();
    for process_id in processes {
        let mut observed_by = Vec::new();
        for session_id in sessions {
            observed_by.push(
                registry
                    .is_observer(session_id, process_id)
                    .await
                    .expect("read an observer edge"),
            );
        }
        let mut floors = Vec::new();
        for session_id in sessions {
            floors.push(
                registry
                    .wake_allocation_floor_for_testing(session_id, process_id)
                    .await
                    .expect("read a sender floor"),
            );
        }
        per_process.push(serde_json::json!({
            "record": registry.get_process(process_id).await.expect("read a record"),
            "events": registry
                .full_event_window(process_id, 0)
                .await
                .expect("read an event log"),
            "observers": registry
                .observers_for_process(process_id)
                .await
                .expect("read observers"),
            "observed_by": observed_by,
            "lease": registry.get_process_lease(process_id).await.expect("read a lease"),
            "floors": floors,
        }));
    }
    let (changes, _) = registry
        .processes_changed_since(crate::ProcessChangeCursor::initial(), 1_000)
        .await
        .expect("read the change feed");
    serde_json::json!({
        "processes": per_process,
        "wake_deliveries": registry
            .list_wake_deliveries(None)
            .await
            .expect("list wake deliveries"),
        "changes": changes.len(),
    })
}

/// A transfer of several processes that fails part-way commits nothing: the
/// first process's observer move, its audit events, the wake deliveries and
/// the sender floor all roll back with the refusal the second process causes.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_failed_observer_transfer_leaves_no_partial_mutation(
    registry: Arc<dyn crate::ConformanceProcessRegistry>,
) {
    let from_session = SessionId::from("observer-transfer-from");
    let to_session = SessionId::from("observer-transfer-to");
    let first = ProcessId::from("observer-transfer-first");
    let second = ProcessId::from("observer-transfer-second");
    registry
        .register_process(
            registration(first.as_str())
                .with_wake_session_id(Some(from_session.clone()))
                .with_extra_event_types([wake_event_type("producer.wake")]),
        )
        .await
        .expect("register the first process");
    // A real wake delivery, so the footprint carries a non-empty outbox row
    // to compare against after the failed transfer.
    registry
        .append_event(
            &first,
            ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "seed"}),
            )
            .with_replay_key("observer-transfer:seed"),
        )
        .await
        .expect("seed a wake delivery");
    registry
        .register_process(registration(second.as_str()))
        .await
        .expect("register the second process");
    registry
        .add_observer(
            &from_session,
            &first,
            crate::ProcessObserverBy::host("observer-transfer"),
        )
        .await
        .expect("observe the first process");

    let processes = [first.clone(), second.clone()];
    let sessions = [from_session.clone(), to_session.clone()];
    let before = transfer_footprint(&registry, &processes, &sessions).await;

    // The source session does not observe the second process, so the
    // transfer fails after the first process's move was staged.
    let outcome = registry
        .transfer_observers(
            &from_session,
            &to_session,
            &processes,
            crate::ProcessObserverBy::host("observer-transfer"),
        )
        .await;
    assert!(
        outcome.is_err(),
        "the second process id must fail the transfer"
    );

    assert_eq!(
        transfer_footprint(&registry, &processes, &sessions).await,
        before,
        "records, events, observers, leases, deliveries, floors and the change feed must equal \
         the pre-call footprint"
    );
}
