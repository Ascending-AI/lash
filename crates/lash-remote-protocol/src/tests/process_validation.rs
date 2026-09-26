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
