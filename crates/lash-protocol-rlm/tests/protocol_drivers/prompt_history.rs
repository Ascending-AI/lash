use super::support::*;

// Prompt/history rendering scenarios: visible prose, reasoning lanes, and RLM trajectory projection.

#[derive(Clone, Copy, Debug)]
struct RlmPromptHistoryFocusedCheck {
    display_name: &'static str,
}

const TEXT_ONLY_CELL_TRAJECTORY: RlmPromptHistoryFocusedCheck = RlmPromptHistoryFocusedCheck {
    display_name: "text-only lash_vm cell trajectory",
};

const MARKDOWN_BEFORE_CELL: RlmPromptHistoryFocusedCheck = RlmPromptHistoryFocusedCheck {
    display_name: "markdown block remains visible prose before lash_vm cell",
};
const EXEC_ERROR_EXACT_HISTORY: RlmPromptHistoryFocusedCheck = RlmPromptHistoryFocusedCheck {
    display_name: "exec error keeps reasoning prose and code",
};
const FINISH_FINAL_VALUE_EXACT_HISTORY: RlmPromptHistoryFocusedCheck =
    RlmPromptHistoryFocusedCheck {
        display_name: "finish final value keeps reasoning prose and code",
    };
const STREAMED_REASONING_TRAJECTORY: RlmPromptHistoryFocusedCheck = RlmPromptHistoryFocusedCheck {
    display_name: "streamed reasoning is preserved in trajectory",
};

#[test]
fn rlm_prompt_history_text_only_cell_records_code_without_reasoning_or_prose() {
    RlmProtocolScenario::new(TEXT_ONLY_CELL_TRAJECTORY.display_name)
        .termination(lash_core::TerminationMode::TerminalRequired)
        .llm_response(vec![text_part(&typescript_block("console.log(\"hi\");"))])
        .exec_result(exec_response(&["hi\n"], None, None))
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["console.log(\"hi\");"],
            checkpoints: vec![CheckpointKind::AfterWork],
            assistant_message_count: Some(0),
            assistant_reasoning_texts: Some(Vec::new()),
            assistant_visible_texts: Some(Vec::new()),
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "console.log(\"hi\");",
                output: vec!["hi\n".to_string()],
                outcome: lash_core::CellOutcome::Completed,
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_prompt_history_markdown_code_block_remains_visible_prose_before_real_lash_vm_cell() {
    RlmProtocolScenario::new(MARKDOWN_BEFORE_CELL.display_name)
        .termination(lash_core::TerminationMode::TerminalRequired)
        .llm_response(vec![text_part(&typescript_block_with_prose(
            "Example:\n```python\nprint('hi')\n```",
            "console.log(\"done\")",
        ))])
        .exec_result(exec_response(&["done\n"], None, None))
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["console.log(\"done\")"],
            checkpoints: vec![CheckpointKind::AfterWork],
            assistant_reasoning_texts: Some(Vec::new()),
            assistant_visible_texts: Some(vec!["Example:\n```python\nprint('hi')\n```"]),
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "console.log(\"done\")",
                output: vec!["done\n".to_string()],
                outcome: lash_core::CellOutcome::Completed,
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_prompt_history_exec_error_keeps_reasoning_prose_and_code_exact() {
    RlmProtocolScenario::new(EXEC_ERROR_EXACT_HISTORY.display_name)
        .termination(lash_core::TerminationMode::TerminalRequired)
        .llm_response(vec![
            reasoning_part("find the failing call"),
            text_part(&typescript_block_with_prose(
                "Trying it now.",
                "missing_name",
            )),
        ])
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
        .llm_response(vec![
            reasoning_part("ready to finish"),
            text_part(&typescript_block_with_prose(
                "Finishting.",
                "await control.finish(\"done\");",
            )),
        ])
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

#[test]
fn rlm_prompt_history_reasoning_part_is_preserved_in_trajectory() {
    RlmProtocolScenario::new(STREAMED_REASONING_TRAJECTORY.display_name)
        .user_message("say hi")
        .termination(lash_core::TerminationMode::TerminalRequired)
        .streamed_llm_response(vec![
            reasoning_part("I'll answer directly."),
            text_part(&typescript_block("await control.finish(\"Hi.\");")),
        ])
        .exec_result(exec_response(&[], None, Some(serde_json::json!("Hi."))))
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["await control.finish(\"Hi.\");"],
            checkpoints: vec![CheckpointKind::BeforeCompletion],
            assistant_reasoning_texts: Some(vec!["I'll answer directly."]),
            assistant_visible_texts: Some(Vec::new()),
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "await control.finish(\"Hi.\");",
                output: Vec::new(),
                outcome: finished(serde_json::json!("Hi.")),
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}
