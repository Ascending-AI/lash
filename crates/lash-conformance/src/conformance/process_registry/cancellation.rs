use super::*;
use lash_core::{CancelOrigin, CancelRequest};
use pretty_assertions::assert_eq;

fn owned_registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::Engine {
            kind: "cancel-conformance".to_string(),
            payload: serde_json::Value::Null,
        },
        RecoveryContract::Rerunnable,
        ProcessProvenance::host(),
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Host,
            lash_core::OnParentEnd::Abandon,
        ),
    )
    .with_execution_env_ref(Some(ProcessExecutionEnvRef::new(format!(
        "process-env:{id}"
    ))))
}

async fn read(
    reader: &Arc<dyn crate::ConformanceProcessRegistry>,
    process_ref: &ProcessRef,
) -> ProcessRecord {
    reader
        .get_process_ref(process_ref)
        .await
        .expect("read cancellation fold")
        .expect("retained process")
}

pub(super) async fn contract(
    writer: Arc<dyn crate::ConformanceProcessRegistry>,
    reader: Arc<dyn crate::ConformanceProcessRegistry>,
) {
    let base = writer
        .register_process(registration("cancel-first-wins"))
        .await
        .expect("register cancel target");
    let process_ref = ProcessRef::from_record(&base);
    assert!(!base.is_terminal());
    assert!(base.cancel_request.is_none());
    let first = CancelRequest::new(CancelOrigin::OperatorRequested, "actor:shared", 11);
    let second = CancelRequest::new(CancelOrigin::ModelRequested, "actor:shared", 11);
    assert_eq!(
        first.requester, second.requester,
        "both requests carry the identical requester text"
    );
    assert_ne!(first.origin, second.origin, "the request origins differ");
    let first_append = ProcessEventAppendRequest::cancel_requested(&process_ref, &first);
    let second_append = ProcessEventAppendRequest::cancel_requested(&process_ref, &second);
    assert_ne!(
        first_append.replay, second_append.replay,
        "different origins must not alias before the fold"
    );
    assert_ne!(
        first_append.payload, second_append.payload,
        "the two proposed facts must remain distinct"
    );
    let receipt = writer
        .append_event_ref(&process_ref, first_append)
        .await
        .expect("accept first cancel");
    let accepted = read(&reader, &process_ref).await;
    assert_eq!(accepted.cancel_request.as_deref(), Some(&first));
    assert_eq!(
        accepted.cancel_request.as_ref().unwrap().requested_at_ms,
        11
    );
    assert!(
        matches!(
            writer.append_event_ref(&process_ref, second_append).await,
            Err(PluginError::ProcessCancelConflict { existing, requested, .. })
                if existing.origin == CancelOrigin::OperatorRequested && requested.origin == CancelOrigin::ModelRequested
        ),
        "a different origin must reach the typed fold refusal, not a replay-payload conflict"
    );
    assert_eq!(
        read(&reader, &process_ref).await.cancel_request.as_deref(),
        Some(&first)
    );
    let retry = CancelRequest {
        requested_at_ms: 99,
        ..first.clone()
    };
    assert_ne!(retry.requested_at_ms, first.requested_at_ms);
    let replay = writer
        .append_event_ref(
            &process_ref,
            ProcessEventAppendRequest::cancel_requested(&process_ref, &retry),
        )
        .await
        .expect("fresh-clock retry replays the first fact");
    assert_eq!(replay.event.sequence, receipt.event.sequence);
    assert_eq!(replay.event.payload, serde_json::json!(first));
    let unchanged = writer
        .request_process_cancel(&process_ref, first.origin, first.requester.clone(), None)
        .await
        .expect("registry cancellation retry returns the first record");
    assert_eq!(unchanged.last_event_sequence, accepted.last_event_sequence);
    assert_eq!(
        unchanged.cancel_request.as_ref().unwrap().requested_at_ms,
        11
    );
    assert!(matches!(
        writer
            .request_process_cancel(&process_ref, first.origin, "actor:other".to_string(), None)
            .await,
        Err(PluginError::ProcessCancelConflict { .. })
    ));
    assert_eq!(
        reader
            .events_after_ref(&process_ref, 0)
            .await
            .expect("read event tail")
            .len(),
        1,
        "conflicts and retries append no second cancellation fact"
    );
    let wrong_lifetime = ProcessRef::new(
        base.id.clone(),
        lash_core::ProcessIncarnation::from_registration_sequence(
            base.incarnation.registration_sequence() + 1,
        ),
    );
    assert_ne!(wrong_lifetime.incarnation, process_ref.incarnation);
    assert!(matches!(
        writer
            .request_process_cancel(&wrong_lifetime, first.origin, first.requester.clone(), None)
            .await,
        Err(PluginError::ProcessIncarnationSuperseded { .. })
    ));

    let proposed = ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::cancelled(
        lash_core::ToolCancellation::runtime("runner stopped"),
    ));
    let completed = writer
        .complete_process(
            &base.id,
            proposed.clone(),
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("settle the standing cancellation");
    assert!(matches!(
        completed,
        crate::ProcessCompletionOutcome::Committed(_)
    ));
    let settled = read(&reader, &process_ref).await;
    assert!(
        matches!(
            settled.outcome,
            Some(ProcessAwaitOutput::Settled { ref output })
                if matches!(&output.outcome, lash_core::ToolCallOutcome::Cancelled(cancellation)
                    if cancellation.origin == Some(first.origin))
        ),
        "settlement inherits the durable first origin"
    );
    let repeated = writer
        .complete_process(
            &base.id,
            proposed,
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("completion retry without a caller-supplied origin");
    assert!(
        matches!(
            repeated,
            crate::ProcessCompletionOutcome::AlreadyApplied { .. }
        ),
        "origin enrichment must not turn identical completion into Superseded"
    );

    let terminal = writer
        .register_process(registration("cancel-terminal-refusal"))
        .await
        .expect("register terminal target");
    writer
        .complete_process(
            &terminal.id,
            settled_success(serde_json::Value::Null),
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete before cancellation");
    let terminal_ref = ProcessRef::from_record(&terminal);
    assert!(read(&reader, &terminal_ref).await.is_terminal());
    assert!(matches!(
        writer
            .request_process_cancel(
                &terminal_ref,
                CancelOrigin::OperatorRequested,
                "operator".to_string(),
                None
            )
            .await,
        Err(PluginError::ProcessAlreadyTerminal { .. })
    ));
    assert!(read(&reader, &terminal_ref).await.cancel_request.is_none());

    let unrun = writer
        .register_process(owned_registration("cancel-unrun-start-failed"))
        .await
        .expect("register unrun target");
    assert!(unrun.first_started.is_none());
    assert!(unrun.external_ref.is_none());
    assert!(!unrun.is_terminal());
    let unrun_ref = ProcessRef::from_record(&unrun);
    let failed = writer
        .request_process_cancel(
            &unrun_ref,
            CancelOrigin::StartFailed,
            "start:unrun".to_string(),
            None,
        )
        .await
        .expect("fold failed start");
    assert_eq!(failed.status, ProcessStatus::Cancelled);
    let output = failed
        .outcome
        .clone()
        .expect("terminal outcome")
        .into_tool_output();
    assert!(
        matches!(output.outcome, crate::ToolCallOutcome::Cancelled(cancellation)
        if cancellation.origin == Some(CancelOrigin::StartFailed))
    );
    assert_eq!(read(&reader, &unrun_ref).await, failed);
    let events = reader
        .events_after_ref(&unrun_ref, 0)
        .await
        .expect("read failed-start event");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0]
            .semantics
            .terminal
            .as_ref()
            .expect("terminal event semantics")
            .status,
        ProcessStatus::Cancelled
    );
    let standing = failed
        .cancel_request
        .as_deref()
        .expect("standing StartFailed request");
    let retry = CancelRequest {
        requested_at_ms: standing
            .requested_at_ms
            .checked_add(1)
            .expect("clock range"),
        ..standing.clone()
    };
    assert_ne!(retry.requested_at_ms, standing.requested_at_ms);
    let replay = writer
        .append_event_ref(
            &unrun_ref,
            ProcessEventAppendRequest::cancel_requested(&unrun_ref, &retry),
        )
        .await
        .expect("stored terminal StartFailed event remains replayable");
    assert_eq!(replay.event.sequence, events[0].sequence);
    assert_eq!(read(&reader, &unrun_ref).await, failed);

    let started = writer
        .register_process(owned_registration("cancel-started-start-failed"))
        .await
        .expect("register started target");
    writer
        .record_first_started(
            &started.id,
            crate::ProcessStarted {
                owner: crate::LeaseOwnerIdentity::opaque("cancel-worker", "cancel-worker:started"),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: started.created_at_ms,
            },
        )
        .await
        .expect("record actual first start");
    let started_ref = ProcessRef::from_record(&started);
    assert!(read(&reader, &started_ref).await.first_started.is_some());
    let pending = writer
        .request_process_cancel(
            &started_ref,
            CancelOrigin::StartFailed,
            "start:started".to_string(),
            None,
        )
        .await
        .expect("started work receives a pending request");
    assert!(
        !pending.is_terminal(),
        "started work must observe cancellation itself"
    );

    let external = writer
        .register_process(owned_registration("cancel-external-start-failed"))
        .await
        .expect("register submitted target");
    writer
        .set_external_ref(
            &external.id,
            crate::ProcessExternalRef {
                backend: "cancel-conformance".to_string(),
                id: "invocation".to_string(),
                metadata: None,
            },
        )
        .await
        .expect("record submission reference");
    let external_ref = ProcessRef::from_record(&external);
    assert!(read(&reader, &external_ref).await.external_ref.is_some());
    let pending = writer
        .request_process_cancel(
            &external_ref,
            CancelOrigin::StartFailed,
            "start:submitted".to_string(),
            None,
        )
        .await
        .expect("submitted work receives a pending request");
    assert!(
        !pending.is_terminal(),
        "submitted work must settle through its owner"
    );

    let mut custom_type = plain_event_type("custom.finished");
    custom_type.semantics.terminal = Some(lash_core::ProcessTerminalSpec {
        status: ProcessStatus::Completed,
        await_output: Some(lash_core::ProcessValueSelector::Pointer(
            "/await_output".to_string(),
        )),
    });
    let custom = writer
        .register_process(
            registration("cancel-custom-projector").with_extra_event_types([custom_type]),
        )
        .await
        .expect("register custom terminal producer");
    let custom_ref = ProcessRef::from_record(&custom);
    writer
        .append_event_ref(
            &custom_ref,
            ProcessEventAppendRequest::new(
                "custom.finished",
                serde_json::json!({
                    "await_output": settled_success(serde_json::json!("custom payload")),
                }),
            )
            .with_replay_key("custom:finished"),
        )
        .await
        .expect("append custom terminal event");
    let custom = read(&reader, &custom_ref).await;
    assert_eq!(custom.status, ProcessStatus::Completed);
    assert_eq!(
        custom.outcome,
        Some(settled_success(serde_json::json!("custom payload")))
    );
    assert!(
        custom.cancel_request.is_none(),
        "custom projection must not synthesize a cancellation"
    );
}
