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
