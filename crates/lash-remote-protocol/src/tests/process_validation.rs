use super::*;

#[test]
fn remote_terminal_semantics_cannot_name_a_non_terminal_status() {
    for (status, terminal) in [
        ("running", false),
        ("waiting", false),
        ("caller_departed", false),
        ("completed", true),
        ("failed", true),
        ("cancelled", true),
        ("abandoned", true),
    ] {
        let decoded = serde_json::from_value::<RemoteProcessTerminalSpec>(serde_json::json!({
            "status": status,
            "await_output": "payload",
        }));
        assert_eq!(
            decoded.is_ok(),
            terminal,
            "`{status}` as a terminal event status"
        );
    }
}

/// A record carries one tagged lifecycle state. The flat `wait`, `park`,
/// `status` and `outcome` it replaced could contradict each other; none of
/// those contradictions decodes.
#[test]
fn remote_process_record_decodes_one_lifecycle_state() {
    let record = serde_json::to_value(remote_process_record()).expect("encode record");
    let with_lifecycle = |lifecycle: serde_json::Value| {
        let mut record = record.clone();
        record["lifecycle"] = lifecycle;
        serde_json::from_value::<RemoteProcessRecord>(record)
    };
    let outcome = serde_json::to_value(settled_success()).expect("encode outcome");
    let wait = record["lifecycle"]["wait"].clone();
    assert!(wait.is_object(), "the fixture record is waiting");

    for (status, lifecycle) in [
        (
            RemoteProcessStatus::Running,
            serde_json::json!({"state": "running"}),
        ),
        (
            RemoteProcessStatus::Waiting,
            serde_json::json!({"state": "waiting", "wait": wait}),
        ),
        (
            RemoteProcessStatus::CallerDeparted,
            serde_json::json!({"state": "caller_departed"}),
        ),
        (
            RemoteProcessStatus::Completed,
            serde_json::json!({"state": "terminal", "outcome": outcome}),
        ),
    ] {
        assert_eq!(
            with_lifecycle(lifecycle)
                .expect("a lifecycle state decodes")
                .status(),
            status
        );
    }

    for (case, lifecycle) in [
        (
            "a terminal state without an outcome",
            serde_json::json!({"state": "terminal"}),
        ),
        (
            "a running state with an outcome",
            serde_json::json!({"state": "running", "outcome": outcome}),
        ),
        (
            "a terminal state with a wait",
            serde_json::json!({"state": "terminal", "outcome": outcome, "wait": wait}),
        ),
        (
            "a waiting state without a wait",
            serde_json::json!({"state": "waiting"}),
        ),
        (
            "a caller-departed state with an outcome",
            serde_json::json!({"state": "caller_departed", "outcome": outcome}),
        ),
        (
            "a pruned answer as an outcome",
            serde_json::json!({"state": "terminal", "outcome": {
                "type": "no_longer_retained",
                "terminal_label": "completed",
                "pruned_at_ms": 1,
            }}),
        ),
        (
            "a status beside the state",
            serde_json::json!({"state": "running", "status": "completed"}),
        ),
    ] {
        assert!(with_lifecycle(lifecycle).is_err(), "{case} must not decode");
    }

    let mut flat = record.clone();
    let object = flat.as_object_mut().expect("a record is an object");
    object.remove("lifecycle");
    object.insert("status".to_string(), serde_json::json!("completed"));
    object.insert("outcome".to_string(), outcome.clone());
    assert!(
        serde_json::from_value::<RemoteProcessRecord>(flat).is_err(),
        "the flat status and outcome a record used to carry must not decode"
    );
}

fn settled_success() -> RemoteProcessAwaitOutput {
    RemoteProcessAwaitOutput::Settled {
        output: RemoteProcessToolCallOutput {
            outcome: RemoteProcessToolCallOutcome::Success(serde_json::Value::Null),
            control: None,
            view: None,
            projection_value: None,
        },
    }
}

fn settled_cancelled() -> RemoteProcessAwaitOutput {
    RemoteProcessAwaitOutput::Settled {
        output: RemoteProcessToolCallOutput {
            outcome: RemoteProcessToolCallOutcome::Cancelled(RemoteProcessToolCancellation {
                origin: None,
                message: "cancelled".to_string(),
                source: RemoteProcessToolFailureSource::Cancellation,
                raw: None,
            }),
            control: None,
            view: None,
            projection_value: None,
        },
    }
}

/// A terminal event carries its outcome only: a status beside it, or an
/// answer that is not an outcome, does not decode.
#[test]
fn remote_process_event_semantics_decode_an_outcome_only() {
    let outcome = serde_json::to_value(settled_cancelled()).expect("encode outcome");
    let decoded = serde_json::from_value::<RemoteProcessEventSemantics>(serde_json::json!({
        "terminal": {"outcome": outcome},
    }))
    .expect("an outcome decodes");
    assert_eq!(
        decoded.terminal.expect("terminal").outcome.status(),
        RemoteTerminalProcessStatus::Cancelled
    );
    for (case, terminal) in [
        (
            "a status beside the outcome",
            serde_json::json!({"status": "completed", "outcome": outcome}),
        ),
        (
            "a pruned answer as the outcome",
            serde_json::json!({"outcome": {
                "type": "no_longer_retained",
                "terminal_label": "completed",
                "pruned_at_ms": 1,
            }}),
        ),
    ] {
        assert!(
            serde_json::from_value::<RemoteProcessEventSemantics>(
                serde_json::json!({"terminal": terminal})
            )
            .is_err(),
            "{case} must not decode"
        );
    }
}

fn settled_failed() -> RemoteProcessAwaitOutput {
    RemoteProcessAwaitOutput::Settled {
        output: RemoteProcessToolCallOutput {
            outcome: RemoteProcessToolCallOutcome::Failure(RemoteProcessToolFailure {
                class: RemoteToolFailureClass::Execution,
                code: "failed".to_string(),
                message: "failed".to_string(),
                source: RemoteProcessToolFailureSource::Tool,
                retry: RemoteProcessToolRetryStatus::Never,
                raw: None,
            }),
            control: None,
            view: None,
            projection_value: None,
        },
    }
}

fn abandoned_outcome() -> RemoteProcessAwaitOutput {
    RemoteProcessAwaitOutput::Abandoned {
        evidence: RemoteAbandonEvidence {
            writer: RemoteAbandonWriter::Producer,
            owner: None,
            epoch_ms: 1,
        },
        control: None,
    }
}

fn terminal_event(status: RemoteTerminalProcessStatus, outcome: RemoteProcessAwaitOutput) {
    let outcome = RemoteProcessTerminal::try_from(outcome).expect("a terminal outcome");
    assert_eq!(outcome.status(), status);
    RemoteProcessEventSemantics {
        terminal: Some(RemoteProcessTerminalSemantics { outcome }),
        wake: None,
        signal_wait: None,
    }
    .validate("RemoteProcessEventSemantics")
    .expect("a terminal outcome must be accepted");
}

#[test]
fn remote_process_event_semantics_derive_failed_cancelled_and_abandoned() {
    terminal_event(RemoteTerminalProcessStatus::Completed, settled_success());
    terminal_event(RemoteTerminalProcessStatus::Failed, settled_failed());
    terminal_event(RemoteTerminalProcessStatus::Cancelled, settled_cancelled());
    terminal_event(RemoteTerminalProcessStatus::Abandoned, abandoned_outcome());
}

#[test]
fn remote_process_cancel_receipt_rejects_status_that_contradicts_its_record() {
    let receipt = RemoteProcessCancelReceipt {
        origin: lash_sansio::CancelOrigin::OperatorRequested,
        process_id: lash_sansio::ProcessId::fixture("process:1"),
        status: RemoteProcessStatus::Cancelled,
        record: Some(remote_process_record()),
    };
    assert!(
        receipt
            .validate()
            .expect_err("cancel receipt status must match the embedded record")
            .to_string()
            .contains("contradicts its record status")
    );
}

// FIG-2965 deleted the observed-process terminal-flag agreement test with
// the flag it policed: `terminal` and `status_label` were encoded copies of
// the lifecycle beside them, so a record could contradict itself on the wire
// and the validator existed only to catch that. They are derived from the
// lifecycle on the reader side now, and a contradiction has no way to be
// expressed, so there is nothing left to reject.
