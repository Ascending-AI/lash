use super::*;
use pretty_assertions::assert_eq;

/// Raw signal appends cannot bypass the identity-derived append key.
#[expect(clippy::expect_used, reason = "conformance-law fixture")]
pub(super) async fn raw_signal_appends_are_refused(registry: Arc<dyn ProcessRegistry>) {
    let base = registry
        .register_process(
            ProcessRegistration::new(
                ProcessInput::Engine {
                    kind: "raw-signal-refusal".to_string(),
                    payload: serde_json::Value::Null,
                },
                ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_execution_env_ref(Some(ProcessExecutionEnvRef::new(
                "process-env:raw-signal-refusal",
            )))
            .with_extra_event_types([
                plain_event_type("signal.ready"),
                plain_event_type("producer.note"),
            ]),
        )
        .await
        .expect("register signal target");
    let signal = lash_core::ProcessSignal::new(
        lash_core::ProcessSignalIdentity::new(base.id.clone(), "ready", "one")
            .expect("signal identity"),
        serde_json::json!(1),
    );
    for request in [
        ProcessEventAppendRequest::new("signal.ready", signal.payload.clone()),
        ProcessEventAppendRequest::new("signal.ready", signal.payload.clone())
            .with_replay_key("arbitrary-key"),
        ProcessEventAppendRequest::new("signal.ready", signal.payload.clone())
            .with_replay_key(signal.identity.append_key()),
    ] {
        let error = registry
            .append_event(&base.id, request)
            .await
            .expect_err("raw signal must be refused, even with the derived key");
        assert!(
            matches!(error, PluginError::ReservedProcessEvent { ref event_type } if event_type == "signal.ready"),
            "raw signal refusal must be typed: {error:?}"
        );
        assert_eq!(
            registry.get_process(&base.id).await.expect("record"),
            Some(base.clone())
        );
        assert!(
            registry
                .full_event_window(&base.id, 0)
                .await
                .expect("events")
                .is_empty()
        );
    }
    let first = registry
        .append_event(&base.id, signal.append_request())
        .await
        .expect("typed signal");
    let error = registry
        .append_event(
            &base.id,
            ProcessEventAppendRequest::new("signal.ready", signal.payload.clone())
                .with_replay_key(signal.identity.append_key()),
        )
        .await
        .expect_err("raw replay must be refused before key lookup coalesces it");
    assert!(matches!(error, PluginError::ReservedProcessEvent { .. }));
    let replay = registry
        .append_event(&base.id, signal.append_request())
        .await
        .expect("typed replay");
    assert_eq!(replay.realization, lash_core::StoreRealization::Coalesced);
    assert_eq!(replay.event.sequence, first.event.sequence);

    let round_trip = serde_json::from_value(
        serde_json::to_value(signal.append_request()).expect("encode typed append"),
    )
    .expect("decode typed append");
    assert_eq!(
        registry
            .append_event(&base.id, round_trip)
            .await
            .expect("typed round trip")
            .event
            .sequence,
        first.event.sequence
    );

    let mut changed_type = signal.append_request();
    changed_type.event_type = "producer.note".to_string();
    let mut missing_key = signal.append_request();
    missing_key.replay = None;
    let wrong_target = lash_core::ProcessSignal::new(
        lash_core::ProcessSignalIdentity::new(ProcessId::fixture("other-process"), "ready", "one")
            .expect("another signal target"),
        signal.payload.clone(),
    )
    .append_request();
    let before = registry
        .get_process(&base.id)
        .await
        .expect("record preimage");
    let events = serde_json::to_value(
        registry
            .full_event_window(&base.id, 0)
            .await
            .expect("event preimage"),
    )
    .expect("encode events");
    for request in [
        changed_type,
        missing_key,
        wrong_target,
        signal.append_request().with_replay_key("changed-key"),
        signal.append_request().without_wake(),
        ProcessEventAppendRequest::new("signal.", serde_json::Value::Null),
        ProcessEventAppendRequest::new("signal.invalid.name", serde_json::Value::Null),
    ] {
        assert!(matches!(
            registry.append_event(&base.id, request).await,
            Err(PluginError::ReservedProcessEvent { .. })
        ));
    }
    assert_eq!(
        registry
            .get_process(&base.id)
            .await
            .expect("unchanged record"),
        before
    );
    assert_eq!(
        serde_json::to_value(
            registry
                .full_event_window(&base.id, 0)
                .await
                .expect("unchanged events")
        )
        .expect("encode events"),
        events
    );

    let authority =
        crate::ProcessExecutionWriteAuthority::invocation(base.id.clone(), "raw-signal-worker")
            .bind_attempt(1);
    registry
        .record_first_started_with_authority(
            &base.id,
            authority.invocation_started().expect("bound attempt"),
            &authority,
        )
        .await
        .expect("start signal target");
    let before = registry
        .get_process(&base.id)
        .await
        .expect("boundary preimage");
    let events = serde_json::to_value(
        registry
            .full_event_window(&base.id, 0)
            .await
            .expect("boundary events"),
    )
    .expect("encode events");
    let raw = || ProcessEventAppendRequest::new("signal.ready", serde_json::json!(2));
    let batch = || {
        vec![
            ProcessEventAppendRequest::new("producer.note", serde_json::json!("rollback")),
            raw(),
        ]
    };
    assert!(matches!(
        registry
            .append_event_with_authority(&base.id, raw(), &authority)
            .await,
        Err(PluginError::ReservedProcessEvent { .. })
    ));
    assert!(matches!(
        registry.append_events(&base.id, batch(), &authority).await,
        Err(PluginError::ReservedProcessEvent { .. })
    ));
    assert!(matches!(
        registry
            .set_process_wait_with_authority(
                &base.id,
                WaitState {
                    since_ms: base.created_at_ms,
                    kind: WaitKind::Signal {
                        name: "ready".to_string(),
                        event_type: "signal.ready".to_string(),
                        key: lash_core::runtime::process_signal_wait_key(&base.id, "ready", 2),
                        ordinal: 2,
                    },
                },
                batch(),
                &authority,
            )
            .await,
        Err(PluginError::ReservedProcessEvent { .. })
    ));
    assert!(matches!(
        registry
            .complete_process_with_prelude(
                &base.id,
                settled_success(serde_json::Value::Null),
                batch(),
                ProcessCompletionAuthority::workflow_key("raw-signal-refusal"),
            )
            .await,
        Err(PluginError::ReservedProcessEvent { .. })
    ));
    assert_eq!(
        registry
            .get_process(&base.id)
            .await
            .expect("boundary record unchanged"),
        before
    );
    assert_eq!(
        serde_json::to_value(
            registry
                .full_event_window(&base.id, 0)
                .await
                .expect("boundary events unchanged")
        )
        .expect("encode events"),
        events
    );
}

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
