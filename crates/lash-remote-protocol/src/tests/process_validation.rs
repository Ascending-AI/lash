use super::*;

#[test]
fn remote_terminal_semantics_reject_non_terminal_status() {
    let terminal = RemoteProcessTerminalSpec {
        status: RemoteProcessStatus::Running,
        await_output: Some(RemoteProcessValueSelector::Payload),
    };
    assert!(
        terminal
            .validate("RemoteProcessTerminalSpec")
            .expect_err("running terminal semantics must be rejected")
            .to_string()
            .contains("require a terminal status")
    );
}

#[test]
fn remote_process_record_rejects_contradictory_status_and_outcome() {
    let mut terminal_without_outcome = remote_process_record();
    terminal_without_outcome.status = RemoteProcessStatus::Completed;
    assert!(
        terminal_without_outcome
            .validate("RemoteProcessRecord")
            .expect_err("terminal status without outcome must be rejected")
            .to_string()
            .contains("must carry an outcome")
    );

    let mut non_terminal_with_outcome = remote_process_record();
    non_terminal_with_outcome.outcome = Some(RemoteProcessAwaitOutput::Settled {
        output: RemoteProcessToolCallOutput {
            outcome: RemoteProcessToolCallOutcome::Success(serde_json::Value::Null),
            control: None,
        },
    });
    assert!(
        non_terminal_with_outcome
            .validate("RemoteProcessRecord")
            .expect_err("non-terminal status with outcome must be rejected")
            .to_string()
            .contains("must not carry an outcome")
    );

    let mut mismatched = remote_process_record();
    mismatched.status = RemoteProcessStatus::Completed;
    mismatched.outcome = Some(RemoteProcessAwaitOutput::Settled {
        output: RemoteProcessToolCallOutput {
            outcome: RemoteProcessToolCallOutcome::Cancelled(RemoteProcessToolCancellation {
                origin: None,
                message: "cancelled".to_string(),
                source: RemoteProcessToolFailureSource::Cancellation,
                raw: None,
            }),
            control: None,
        },
    });
    assert!(
        mismatched
            .validate("RemoteProcessRecord")
            .expect_err("mismatched terminal status and outcome must be rejected")
            .to_string()
            .contains("contradicts its outcome")
    );
}

fn settled_success() -> RemoteProcessAwaitOutput {
    RemoteProcessAwaitOutput::Settled {
        output: RemoteProcessToolCallOutput {
            outcome: RemoteProcessToolCallOutcome::Success(serde_json::Value::Null),
            control: None,
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
        },
    }
}

#[test]
fn remote_process_event_semantics_reject_contradictory_status_and_outcome() {
    let mismatched = RemoteProcessEventSemantics {
        terminal: Some(RemoteProcessTerminalSemantics {
            status: RemoteProcessStatus::Completed,
            outcome: settled_cancelled(),
        }),
        wake: None,
    };
    assert!(
        mismatched
            .validate("RemoteProcessEventSemantics")
            .expect_err("mismatched terminal status and outcome must be rejected")
            .to_string()
            .contains("contradicts its outcome")
    );

    let nonterminal = RemoteProcessEventSemantics {
        terminal: Some(RemoteProcessTerminalSemantics {
            status: RemoteProcessStatus::Running,
            outcome: settled_success(),
        }),
        wake: None,
    };
    assert!(
        nonterminal
            .validate("RemoteProcessEventSemantics")
            .expect_err("nonterminal status in the terminal slot must be rejected")
            .to_string()
            .contains("must not carry an outcome")
    );

    let no_longer_retained = RemoteProcessEventSemantics {
        terminal: Some(RemoteProcessTerminalSemantics {
            status: RemoteProcessStatus::Completed,
            outcome: RemoteProcessAwaitOutput::NoLongerRetained {
                terminal_label: "completed".to_string(),
                pruned_at_ms: 1,
            },
        }),
        wake: None,
    };
    assert!(
        no_longer_retained
            .validate("RemoteProcessEventSemantics")
            .expect_err("NoLongerRetained must not be a terminal event outcome")
            .to_string()
            .contains("contradicts its outcome")
    );
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
        },
    }
}

fn abandoned_outcome() -> RemoteProcessAwaitOutput {
    RemoteProcessAwaitOutput::Abandoned {
        evidence: RemoteAbandonEvidence {
            writer: RemoteAbandonWriter::OwnerDrain,
            owner: None,
            epoch_ms: 1,
        },
        control: None,
    }
}

fn terminal_event(status: RemoteProcessStatus, outcome: RemoteProcessAwaitOutput) {
    RemoteProcessEventSemantics {
        terminal: Some(RemoteProcessTerminalSemantics { status, outcome }),
        wake: None,
    }
    .validate("RemoteProcessEventSemantics")
    .expect("matching terminal status and outcome must be accepted");
}

#[test]
fn remote_process_event_semantics_accept_matching_failed_cancelled_and_abandoned() {
    terminal_event(RemoteProcessStatus::Failed, settled_failed());
    terminal_event(RemoteProcessStatus::Cancelled, settled_cancelled());
    terminal_event(RemoteProcessStatus::Abandoned, abandoned_outcome());
}

#[test]
fn remote_process_cancel_receipt_rejects_status_that_contradicts_its_record() {
    let receipt = RemoteProcessCancelReceipt {
        origin: lash_sansio::CancelOrigin::OperatorRequested,
        process_id: ProcessId::from("process:1"),
        incarnation: 1,
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

fn observed_process(lifecycle: RemoteProcessStatus, terminal: bool) -> RemoteObservedProcess {
    RemoteObservedProcess {
        process_id: ProcessId::from("process:1"),
        incarnation: 1,
        last_event_sequence: 1,
        graph_key: "process:process:1:incarnation:1".to_string(),
        kind: "external".to_string(),
        identity: RemoteProcessIdentity {
            kind: "external".to_string(),
            label: Some("Import".to_string()),
            definition: None,
        },
        lifecycle,
        status_label: match lifecycle {
            RemoteProcessStatus::Running => "running",
            RemoteProcessStatus::Waiting => "waiting",
            RemoteProcessStatus::Completed => "completed",
            RemoteProcessStatus::Failed => "failed",
            RemoteProcessStatus::Cancelled => "cancelled",
            RemoteProcessStatus::Abandoned => "abandoned",
            RemoteProcessStatus::CallerDeparted => "caller_departed",
        }
        .to_string(),
        terminal,
        disposition: RemoteRecoveryContract::ExternallyOwned,
        error: None,
        created_at_ms: 1,
        updated_at_ms: 2,
        first_started: None,
        lease_holder: None,
        lease_expires_at_ms: None,
        abandon_request: None,
        cancel_request: None,
        input: RemoteProcessInput::External {
            metadata: serde_json::json!({ "label": "Import" }),
        },
        originator: RemoteProcessOriginator::Host { scope: None },
        env_ref: None,
        caused_by: None,
        external_ref: None,
        wait: None,
        child_session_id: None,
        label: "Import".to_string(),
    }
}

#[test]
fn remote_observed_process_rejects_terminal_flag_that_contradicts_lifecycle() {
    assert!(
        observed_process(RemoteProcessStatus::Running, true)
            .validate("RemoteObservedProcess")
            .expect_err("non-terminal lifecycle with terminal=true must be rejected")
            .to_string()
            .contains("contradicts lifecycle")
    );
    assert!(
        observed_process(RemoteProcessStatus::Failed, false)
            .validate("RemoteObservedProcess")
            .expect_err("terminal lifecycle with terminal=false must be rejected")
            .to_string()
            .contains("contradicts lifecycle")
    );
    observed_process(RemoteProcessStatus::Failed, true)
        .validate("RemoteObservedProcess")
        .expect("matching terminal flag and lifecycle must be accepted");
}
