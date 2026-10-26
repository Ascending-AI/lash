use super::support::*;

// Prompt/history rendering scenarios: visible prose, reasoning lanes, and RLM trajectory projection.

#[derive(Clone, Copy, Debug)]
struct RlmPromptHistoryFocusedCheck {
    display_name: &'static str,
}

const EXEC_ERROR_EXACT_HISTORY: RlmPromptHistoryFocusedCheck = RlmPromptHistoryFocusedCheck {
    display_name: "exec error keeps reasoning prose and code",
};
const FINISH_FINAL_VALUE_EXACT_HISTORY: RlmPromptHistoryFocusedCheck =
    RlmPromptHistoryFocusedCheck {
        display_name: "finish final value keeps reasoning prose and code",
    };

#[test]
fn rlm_prompt_history_exec_error_keeps_reasoning_prose_and_code_exact() {
    RlmProtocolScenario::new(EXEC_ERROR_EXACT_HISTORY.display_name)
        .termination(lash_core::TerminationMode::TerminalRequired)
        .llm_response(
            [
                vec![reasoning_part("find the failing call")],
                typescript_block_with_prose("Trying it now.", "missing_name"),
            ]
            .concat(),
        )
        .exec_result(exec_response(
            &[],
            Some("unknown binding `missing_name`"),
            None,
        ))
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["missing_name"],
            checkpoints: vec![CheckpointKind::AfterWork],
            assistant_reasoning_texts: Some(vec!["find the failing call"]),
            assistant_visible_texts: Some(vec!["Trying it now."]),
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "missing_name",
                output: Vec::new(),
                outcome: lash_core::CellOutcome::Failed(program_failure(
                    "unknown binding `missing_name`",
                )),
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_prompt_history_finish_final_value_keeps_reasoning_prose_and_code_exact() {
    RlmProtocolScenario::new(FINISH_FINAL_VALUE_EXACT_HISTORY.display_name)
        .termination(lash_core::TerminationMode::TerminalRequired)
        .llm_response(
            [
                vec![reasoning_part("ready to finish")],
                typescript_block_with_prose("Finishting.", "await control.finish(\"done\");"),
            ]
            .concat(),
        )
        .exec_result(exec_response(&[], None, Some(serde_json::json!("done"))))
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["await control.finish(\"done\");"],
            checkpoints: vec![CheckpointKind::BeforeCompletion],
            assistant_reasoning_texts: Some(vec!["ready to finish"]),
            assistant_visible_texts: Some(vec!["Finishting."]),
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "await control.finish(\"done\");",
                output: Vec::new(),
                outcome: finished(serde_json::json!("done")),
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}
