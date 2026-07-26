use super::*;

pub(super) async fn process_record_is_the_fold_of_its_event_log(
    registry: Arc<dyn ProcessRegistry>,
) {
    let process_id = "proc-event-fold";
    let base = registry
        .register_process(
            rerunnable_registration(process_id)
                .with_extra_event_types([plain_event_type("signal.ready")]),
        )
        .await
        .expect("register fold process");
    assert!(
        base.event_types.iter().all(|event_type| !matches!(
            event_type.name.as_str(),
            "process.first_started"
                | "process.waiting"
                | "process.resumed"
                | "process.external_ref_set"
                | "process.abandon_requested"
        )),
        "runtime lifecycle types must not be persisted as producer declarations"
    );
    assert_process_record_fold(&registry, &base, "registration").await;
    registry
        .register_process(
            rerunnable_registration(process_id)
                .with_extra_event_types([plain_event_type("signal.ready")]),
        )
        .await
        .expect("replay fold process registration");
    assert_process_record_fold(&registry, &base, "registration replay").await;

    let started = ProcessStarted {
        owner: process_lease_owner("fold-starter"),
        fencing_token: 0,
        attempt: 1,
        started_at_ms: 11,
    };
    registry
        .record_first_started(process_id, started)
        .await
        .expect("append first-started lifecycle event");
    assert_process_record_fold(&registry, &base, "first started").await;
    replay_latest_event(&registry, process_id, "process.first_started").await;
    assert_process_record_fold(&registry, &base, "first-started replay").await;
    registry
        .record_first_started(
            process_id,
            ProcessStarted {
                owner: process_lease_owner("fold-starter-2"),
                fencing_token: 0,
                attempt: 2,
                started_at_ms: 12,
            },
        )
        .await
        .expect("append second execution attempt");
    assert_process_record_fold(&registry, &base, "second execution attempt").await;
    replay_latest_event(&registry, process_id, "process.first_started").await;
    assert_process_record_fold(&registry, &base, "second attempt replay").await;

    let wait = WaitState {
        kind: WaitKind::Signal {
            name: "ready".to_string(),
            event_type: "signal.ready".to_string(),
            key: format!("process:{process_id}:signal.ready:1"),
            ordinal: 1,
        },
        since_ms: 22,
    };
    registry
        .set_process_wait(process_id, wait.clone())
        .await
        .expect("append wait-entered lifecycle event");
    assert_process_record_fold(&registry, &base, "wait entered").await;
    replay_latest_event(&registry, process_id, "process.waiting").await;
    assert_process_record_fold(&registry, &base, "wait-entered replay").await;

    registry
        .clear_process_wait(process_id)
        .await
        .expect("append wait-cleared lifecycle event");
    assert_process_record_fold(&registry, &base, "wait cleared").await;
    let after_first_wait_cycle = registry
        .get_process(process_id)
        .await
        .expect("load process after first wait cycle");
    let first_cycle_events = registry
        .events_after(process_id, 0)
        .await
        .expect("load first wait-cycle events");

    registry
        .set_process_wait(process_id, wait.clone())
        .await
        .expect("replay wait-entered lifecycle event");
    registry
        .clear_process_wait(process_id)
        .await
        .expect("replay wait-cleared lifecycle event");
    assert_process_record_fold(&registry, &base, "wait-cycle replay").await;
    let after_replay = registry
        .get_process(process_id)
        .await
        .expect("load process after wait-cycle replay");
    assert_eq!(
        serde_json::to_value(&after_replay).expect("serialize replayed wait-cycle record"),
        serde_json::to_value(&after_first_wait_cycle).expect("serialize first wait-cycle record"),
        "re-executing the same wait cycle must leave the record unchanged"
    );
    assert_eq!(
        registry
            .events_after(process_id, 0)
            .await
            .expect("load events after wait-cycle replay")
            .len(),
        first_cycle_events.len(),
        "re-executing the same wait cycle must not append events"
    );

    let second_wait = WaitState {
        kind: WaitKind::Signal {
            name: "ready".to_string(),
            event_type: "signal.ready".to_string(),
            key: format!("process:{process_id}:signal.ready:2"),
            ordinal: 2,
        },
        since_ms: 22,
    };
    registry
        .set_process_wait(process_id, second_wait)
        .await
        .expect("append distinct second wait-entered lifecycle event");
    registry
        .clear_process_wait(process_id)
        .await
        .expect("append distinct second wait-cleared lifecycle event");
    assert_process_record_fold(&registry, &base, "second wait cycle").await;
    let wait_events = registry
        .events_after(process_id, 0)
        .await
        .expect("load both wait cycles")
        .into_iter()
        .filter(|event| {
            matches!(
                event.event_type.as_str(),
                "process.waiting" | "process.resumed"
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        wait_events.len(),
        4,
        "two legitimate wait cycles must produce four lifecycle events"
    );
    assert_eq!(
        wait_events
            .iter()
            .filter_map(|event| event.invocation.replay.as_ref())
            .map(|replay| replay.key.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len(),
        4,
        "legitimate wait cycles must have distinct replay identities"
    );

    let external_ref = ProcessExternalRef {
        backend: "fold".to_string(),
        id: "external-1".to_string(),
        metadata: Some(serde_json::json!({ "source": "conformance" })),
    };
    registry
        .set_external_ref(process_id, external_ref)
        .await
        .expect("append external-ref lifecycle event");
    assert_process_record_fold(&registry, &base, "external ref").await;
    replay_latest_event(&registry, process_id, "process.external_ref_set").await;
    assert_process_record_fold(&registry, &base, "external-ref replay").await;

    let abandon_request = AbandonRequest {
        requested_by: "fold-conformance".to_string(),
        requested_at_ms: 33,
        reason: Some("exercise lifecycle fold".to_string()),
    };
    registry
        .request_process_abandon(process_id, abandon_request)
        .await
        .expect("append abandon-requested lifecycle event");
    assert_process_record_fold(&registry, &base, "abandon request").await;
    replay_latest_event(&registry, process_id, "process.abandon_requested").await;
    assert_process_record_fold(&registry, &base, "abandon-request replay").await;

    assert!(
        registry
            .append_event(
                process_id,
                ProcessEventAppendRequest::new(
                    "process.first_started",
                    serde_json::json!({ "started": {
                        "owner": process_lease_owner("fold-starter"),
                        "started_at_ms": 11,
                    }}),
                ),
            )
            .await
            .is_err(),
        "a lifecycle append without its deterministic replay key must fail"
    );
    assert_process_record_fold(&registry, &base, "failed append").await;

    let signal = ProcessEventAppendRequest::new("signal.ready", serde_json::json!({ "value": 1 }))
        .with_replay_key(format!("process:{process_id}:signal.ready:1"));
    registry
        .append_event(process_id, signal.clone())
        .await
        .expect("append signal event");
    assert_process_record_fold(&registry, &base, "signal").await;
    registry
        .append_event(process_id, signal)
        .await
        .expect("replay signal event");
    assert_process_record_fold(&registry, &base, "signal replay").await;

    let cancel = ProcessEventAppendRequest::cancel_requested(
        process_id,
        Some("fold conformance".to_string()),
    );
    registry
        .append_event(process_id, cancel.clone())
        .await
        .expect("append cancel-requested event");
    assert_process_record_fold(&registry, &base, "cancel request").await;
    registry
        .append_event(process_id, cancel)
        .await
        .expect("replay cancel-requested event");
    assert_process_record_fold(&registry, &base, "cancel-request replay").await;

    complete_and_assert_fold(
        &registry,
        process_id,
        &base,
        ProcessAwaitOutput::Success {
            value: serde_json::json!({ "ok": true }),
            control: None,
        },
        "completed",
    )
    .await;

    for (suffix, output, label) in [
        (
            "failed",
            ProcessAwaitOutput::Failure {
                class: crate::ToolFailureClass::Execution,
                code: "fold_failed".to_string(),
                message: "fold failure".to_string(),
                raw: None,
                control: None,
            },
            "failed",
        ),
        (
            "cancelled",
            ProcessAwaitOutput::Cancelled {
                message: "fold cancelled".to_string(),
                raw: None,
                control: None,
            },
            "cancelled",
        ),
        (
            "abandoned",
            ProcessAwaitOutput::Abandoned {
                evidence: Box::new(crate::AbandonEvidence {
                    writer: crate::AbandonWriter::ReconciledRequest,
                    owner: None,
                    epoch_ms: 44,
                }),
                control: None,
            },
            "abandoned",
        ),
    ] {
        let terminal_process_id = format!("proc-event-fold-{suffix}");
        let terminal_base = registry
            .register_process(registration(&terminal_process_id))
            .await
            .expect("register terminal fold process");
        assert_process_record_fold(&registry, &terminal_base, "terminal registration").await;
        complete_and_assert_fold(
            &registry,
            &terminal_process_id,
            &terminal_base,
            output,
            label,
        )
        .await;
    }
}

async fn replay_latest_event(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &str,
    event_type: &str,
) {
    let event = registry
        .events_after(process_id, 0)
        .await
        .expect("load lifecycle event for replay")
        .into_iter()
        .rev()
        .find(|event| event.event_type == event_type)
        .expect("lifecycle event exists");
    let replayed = registry
        .append_event(
            process_id,
            ProcessEventAppendRequest::new(event.event_type, event.payload)
                .with_optional_replay(event.invocation.replay),
        )
        .await
        .expect("replay lifecycle event");
    assert!(
        replayed.wake_delivery.is_none(),
        "lifecycle events must be wake-inert"
    );
}

async fn complete_and_assert_fold(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &str,
    base: &ProcessRecord,
    output: ProcessAwaitOutput,
    label: &str,
) {
    let authority = if base.disposition == RecoveryDisposition::ExternallyOwned {
        ProcessCompletionAuthority::external_owner()
    } else {
        ProcessCompletionAuthority::workflow_key("fold-conformance")
    };
    registry
        .complete_process(process_id, output.clone(), authority.clone())
        .await
        .expect("append terminal event");
    assert_process_record_fold(registry, base, label).await;
    registry
        .complete_process(process_id, output, authority)
        .await
        .expect("replay terminal event");
    assert_process_record_fold(registry, base, &format!("{label} replay")).await;
}

async fn assert_process_record_fold(
    registry: &Arc<dyn ProcessRegistry>,
    base: &ProcessRecord,
    transition: &str,
) {
    let events = registry
        .events_after(&base.id, 0)
        .await
        .expect("load process fold events");
    let folded = crate::fold_process_record(base.clone(), &events).expect("fold process events");
    let stored = registry
        .get_process(&base.id)
        .await
        .expect("load stored process record");
    assert_eq!(
        serde_json::to_value(&folded).expect("serialize folded process record"),
        serde_json::to_value(&stored).expect("serialize stored process record"),
        "stored process record must equal its event fold after {transition}"
    );
}
