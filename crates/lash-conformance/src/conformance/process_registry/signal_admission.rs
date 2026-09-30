use super::*;
use pretty_assertions::assert_eq;

/// A signal's append is its admission (FIG-4298, FIG-4299).
///
/// The append request a signal makes is derived from its identity alone, so
/// the store keys it by that identity: the same signal appended again, after
/// its process moved on, is served the event its first append stored, and
/// the same identity under changed content is a typed conflict. The first
/// append selects the wait the signal resolves in the transaction that
/// inserts it, and retains it on the event: the declared ordinal of the wait
/// the process is parked on for the name, or else the signal's position among
/// the events of its type.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn signal_admission_retains_its_identity_and_selected_wait(
    registry: Arc<dyn ProcessRegistry>,
) {
    let record = registry
        .register_process(
            ProcessRegistration::new(
                ProcessInput::Engine {
                    kind: "signal-admission".to_string(),
                    payload: serde_json::Value::Null,
                },
                ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_execution_env_ref(Some(ProcessExecutionEnvRef::new(
                "process-env:signal-admission",
            )))
            .with_extra_event_types([plain_event_type("signal.ready")]),
        )
        .await
        .expect("register the signal target");
    let process_id = record.id.clone();
    registry
        .record_first_started(
            &process_id,
            crate::ProcessStarted {
                owner: lash_core::LeaseOwnerIdentity::engine_process_execution(
                    &process_id,
                    "signal-admission",
                ),
                attempt: 1,
                started_at_ms: record.created_at_ms,
                generation: None,
                build_generation: None,
            },
        )
        .await
        .expect("start the signal target");
    let signal = |signal_id: &str, payload: serde_json::Value| {
        lash_core::ProcessSignal::new(
            lash_core::ProcessSignalIdentity::new(process_id.clone(), "ready", signal_id)
                .expect("valid signal identity"),
            payload,
        )
    };
    let park_at = |ordinal: u64| WaitState {
        since_ms: record.created_at_ms,
        kind: WaitKind::Signal {
            name: "ready".to_string(),
            event_type: "signal.ready".to_string(),
            key: lash_core::runtime::process_signal_wait_key(&process_id, "ready", ordinal),
            ordinal,
        },
    };
    let binding = |ordinal| Some(lash_core::ProcessSignalWaitBinding { ordinal });

    // Parked on no wait: the signal's position among its type is its wait.
    let early = registry
        .append_event(
            &process_id,
            signal("early", serde_json::json!(0)).append_request(),
        )
        .await
        .expect("append the early signal");
    assert_eq!(early.event.semantics.signal_wait, binding(1));
    assert_eq!(
        early
            .event
            .invocation
            .replay
            .as_ref()
            .map(|replay| replay.key.clone()),
        Some(signal("early", serde_json::json!(0)).identity.append_key()),
        "the append is keyed by the signal's identity"
    );

    // Parked on a declared ordinal: the declared wait wins over the count.
    registry
        .set_process_wait(&process_id, park_at(7))
        .await
        .expect("park on ordinal seven");
    let a = signal("a", serde_json::json!({"signal": "a"}));
    let first = registry
        .append_event(&process_id, a.append_request())
        .await
        .expect("admit A");
    assert_eq!(first.realization, lash_core::StoreRealization::Realized);
    assert_eq!(first.event.semantics.signal_wait, binding(7));

    // The process moves on; A again is served its admission, wait included.
    registry
        .set_process_wait(&process_id, park_at(8))
        .await
        .expect("park on ordinal eight");
    let again = registry
        .append_event(&process_id, a.append_request())
        .await
        .expect("A again");
    assert_eq!(again.realization, lash_core::StoreRealization::Coalesced);
    assert_eq!(again.event.sequence, first.event.sequence);
    assert_eq!(again.event.payload, first.event.payload);
    assert_eq!(again.event.semantics, first.event.semantics);

    // A's identity under changed content is a conflict, not a second event.
    let changed = registry
        .append_event(
            &process_id,
            signal("a", serde_json::json!({"signal": "changed"})).append_request(),
        )
        .await
        .expect_err("A's identity under changed content conflicts");
    assert!(
        crate::is_durable_identity_conflict(&changed),
        "the conflict is typed: {changed:?}"
    );

    // A distinct signal is what the ordinal-8 wait selects.
    let b = registry
        .append_event(
            &process_id,
            signal("b", serde_json::json!({"signal": "b"})).append_request(),
        )
        .await
        .expect("admit B");
    assert_eq!(b.event.semantics.signal_wait, binding(8));
    let signals = registry
        .full_event_window(&process_id, 0)
        .await
        .expect("read the event log")
        .into_iter()
        .filter(|event| event.event_type == "signal.ready")
        .count();
    assert_eq!(signals, 3, "early, A and B, each appended once");
}
