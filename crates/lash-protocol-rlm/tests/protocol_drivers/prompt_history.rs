use super::support::*;
use std::collections::BTreeSet;

// Prompt/history rendering scenarios: visible prose, reasoning lanes, and RLM trajectory projection.

#[derive(Clone, Copy, Debug)]
struct RlmPromptHistoryFocusedCheck {
    test_name: &'static str,
    declared_test: fn(),
    display_name: &'static str,
    focused_boundary: &'static str,
}

macro_rules! rlm_prompt_history_coverage {
    ($test_fn:ident, $display_name:literal, $focused_boundary:literal) => {
        RlmPromptHistoryFocusedCheck {
            test_name: stringify!($test_fn),
            declared_test: $test_fn,
            display_name: $display_name,
            focused_boundary: $focused_boundary,
        }
    };
}

const TEXT_ONLY_CELL_TRAJECTORY: RlmPromptHistoryFocusedCheck = rlm_prompt_history_coverage!(
    rlm_prompt_history_text_only_cell_records_code_without_reasoning_or_prose,
    "text-only lashlang cell trajectory",
    "Trajectory projection records code without synthetic reasoning or visible prose."
);
const PROVIDER_REASONING_SEPARATED: RlmPromptHistoryFocusedCheck = rlm_prompt_history_coverage!(
    rlm_prompt_history_provider_reasoning_is_recorded_separately_from_lashlang_text,
    "provider reasoning separated from lashlang text",
    "Provider reasoning remains separate from Lashlang cell text in history."
);
const VISIBLE_PROSE_BEFORE_CELL: RlmPromptHistoryFocusedCheck = rlm_prompt_history_coverage!(
    rlm_prompt_history_visible_text_before_cell_is_recorded_as_prose_not_reasoning,
    "visible prose before cell stays visible",
    "Visible prose before a Lashlang cell is recorded as visible prose, not reasoning."
);
const SUFFIX_REPAIR: RlmPromptHistoryFocusedCheck = rlm_prompt_history_coverage!(
    rlm_prompt_history_text_after_closing_tag_is_silently_cut_and_executes,
    "cut suffix text",
    "Text after a closing cell tag is silently cut before executing the cell."
);
const REASONING_VISIBLE_LANES: RlmPromptHistoryFocusedCheck = rlm_prompt_history_coverage!(
    rlm_prompt_history_reasoning_and_visible_prose_are_independent_lanes,
    "reasoning and visible prose lanes",
    "Reasoning and visible prose stay independent in rendered prompt/history lanes."
);
const MARKDOWN_BEFORE_CELL: RlmPromptHistoryFocusedCheck = rlm_prompt_history_coverage!(
    rlm_prompt_history_markdown_code_block_remains_visible_prose_before_real_lashlang_cell,
    "markdown block remains visible prose before lashlang cell",
    "Markdown code fences remain visible prose before the real Lashlang cell."
);
const EXEC_ERROR_EXACT_HISTORY: RlmPromptHistoryFocusedCheck = rlm_prompt_history_coverage!(
    rlm_prompt_history_exec_error_keeps_reasoning_prose_and_code_exact,
    "exec error keeps reasoning prose and code",
    "Exec error trajectory preserves reasoning, prose, code, and error text exactly."
);
const FINISH_FINAL_VALUE_EXACT_HISTORY: RlmPromptHistoryFocusedCheck = rlm_prompt_history_coverage!(
    rlm_prompt_history_finish_final_value_keeps_reasoning_prose_and_code_exact,
    "finish final value keeps reasoning prose and code",
    "Finish final-value trajectory preserves reasoning, prose, code, and final value exactly."
);
const STREAMED_REASONING_TRAJECTORY: RlmPromptHistoryFocusedCheck = rlm_prompt_history_coverage!(
    rlm_prompt_history_reasoning_part_is_preserved_in_trajectory,
    "streamed reasoning is preserved in trajectory",
    "Streamed reasoning parts are preserved in the final RLM trajectory."
);

const RLM_PROMPT_HISTORY_FOCUSED_CHECKS: &[RlmPromptHistoryFocusedCheck] = &[
    TEXT_ONLY_CELL_TRAJECTORY,
    PROVIDER_REASONING_SEPARATED,
    VISIBLE_PROSE_BEFORE_CELL,
    SUFFIX_REPAIR,
    REASONING_VISIBLE_LANES,
    MARKDOWN_BEFORE_CELL,
    EXEC_ERROR_EXACT_HISTORY,
    FINISH_FINAL_VALUE_EXACT_HISTORY,
    STREAMED_REASONING_TRAJECTORY,
];

#[test]
fn rlm_prompt_history_focused_check_metadata_is_unique_and_complete() {
    assert_eq!(RLM_PROMPT_HISTORY_FOCUSED_CHECKS.len(), 9);
    let mut names = BTreeSet::new();
    for check in RLM_PROMPT_HISTORY_FOCUSED_CHECKS {
        let _declared_test = check.declared_test;
        assert!(
            check.test_name.starts_with("rlm_prompt_history_"),
            "prompt-history focused checks must use their focused prefix, got {}",
            check.test_name
        );
        assert!(!check.display_name.trim().is_empty());
        assert!(!check.focused_boundary.trim().is_empty());
        assert!(
            names.insert(check.test_name),
            "duplicate RLM prompt-history focused check metadata for {}",
            check.test_name
        );
    }
}

#[test]
fn rlm_prompt_history_text_only_cell_records_code_without_reasoning_or_prose() {
    RlmProtocolScenario::new(TEXT_ONLY_CELL_TRAJECTORY.display_name)
        .termination(RlmTermination::FinishRequired { schema: None })
        .llm_response(vec![text_part(&lashlang_block("print \"hi\""))])
        .exec_result(exec_response(&["hi\n"], None, None))
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["print \"hi\""],
            checkpoints: vec![CheckpointKind::AfterWork],
            assistant_message_count: Some(0),
            assistant_reasoning_texts: Some(Vec::new()),
            assistant_visible_texts: Some(Vec::new()),
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "print \"hi\"",
                output: vec!["hi\n".to_string()],
                error: None,
                final_output: None,
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_prompt_history_provider_reasoning_is_recorded_separately_from_lashlang_text() {
    RlmProtocolScenario::new(PROVIDER_REASONING_SEPARATED.display_name)
        .termination(RlmTermination::FinishRequired { schema: None })
        .llm_response(vec![
            reasoning_part("hidden plan"),
            text_part(&lashlang_block("print \"hi\"")),
        ])
        .exec_result(exec_response(&["hi\n"], None, None))
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["print \"hi\""],
            checkpoints: vec![CheckpointKind::AfterWork],
            assistant_reasoning_texts: Some(vec!["hidden plan"]),
            assistant_visible_texts: Some(Vec::new()),
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "print \"hi\"",
                output: vec!["hi\n".to_string()],
                error: None,
                final_output: None,
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_prompt_history_visible_text_before_cell_is_recorded_as_prose_not_reasoning() {
    RlmProtocolScenario::new(VISIBLE_PROSE_BEFORE_CELL.display_name)
        .termination(RlmTermination::FinishRequired { schema: None })
        .llm_response(vec![text_part(&lashlang_block_with_prose(
            "I will inspect first.\n",
            "print \"hi\"",
        ))])
        .exec_result(exec_response(&["hi\n"], None, None))
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["print \"hi\""],
            checkpoints: vec![CheckpointKind::AfterWork],
            assistant_reasoning_texts: Some(Vec::new()),
            assistant_visible_texts: Some(vec!["I will inspect first."]),
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "print \"hi\"",
                output: vec!["hi\n".to_string()],
                error: None,
                final_output: None,
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_prompt_history_text_after_closing_tag_is_silently_cut_and_executes() {
    RlmProtocolScenario::new(SUFFIX_REPAIR.display_name)
        .termination(RlmTermination::FinishRequired { schema: None })
        .llm_response(vec![text_part(
            "Before\n<lashlang>\nprint \"hi\"\n</lashlang>\nIgnored suffix",
        )])
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["print \"hi\""],
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_prompt_history_reasoning_and_visible_prose_are_independent_lanes() {
    RlmProtocolScenario::new(REASONING_VISIBLE_LANES.display_name)
        .termination(RlmTermination::FinishRequired { schema: None })
        .llm_response(vec![
            reasoning_part("private chain"),
            text_part(&lashlang_block_with_prose("visible status", "print \"hi\"")),
        ])
        .exec_result(exec_response(&["hi\n"], None, None))
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["print \"hi\""],
            checkpoints: vec![CheckpointKind::AfterWork],
            assistant_reasoning_texts: Some(vec!["private chain"]),
            assistant_visible_texts: Some(vec!["visible status"]),
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "print \"hi\"",
                output: vec!["hi\n".to_string()],
                error: None,
                final_output: None,
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_prompt_history_markdown_code_block_remains_visible_prose_before_real_lashlang_cell() {
    RlmProtocolScenario::new(MARKDOWN_BEFORE_CELL.display_name)
        .termination(RlmTermination::FinishRequired { schema: None })
        .llm_response(vec![text_part(&lashlang_block_with_prose(
            "Example:\n```python\nprint('hi')\n```",
            "print \"done\"",
        ))])
        .exec_result(exec_response(&["done\n"], None, None))
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["print \"done\""],
            checkpoints: vec![CheckpointKind::AfterWork],
            assistant_reasoning_texts: Some(Vec::new()),
            assistant_visible_texts: Some(vec!["Example:\n```python\nprint('hi')\n```"]),
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "print \"done\"",
                output: vec!["done\n".to_string()],
                error: None,
                final_output: None,
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_prompt_history_exec_error_keeps_reasoning_prose_and_code_exact() {
    RlmProtocolScenario::new(EXEC_ERROR_EXACT_HISTORY.display_name)
        .termination(RlmTermination::FinishRequired { schema: None })
        .llm_response(vec![
            reasoning_part("find the failing call"),
            text_part(&lashlang_block_with_prose("Trying it now.", "missing_name")),
        ])
        .exec_result(exec_response(
            &[],
            Some("unknown variable `missing_name`"),
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
                error: Some("unknown variable `missing_name`".to_string()),
                final_output: None,
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_prompt_history_finish_final_value_keeps_reasoning_prose_and_code_exact() {
    RlmProtocolScenario::new(FINISH_FINAL_VALUE_EXACT_HISTORY.display_name)
        .termination(RlmTermination::FinishRequired { schema: None })
        .llm_response(vec![
            reasoning_part("ready to finish"),
            text_part(&lashlang_block_with_prose("Finishting.", "finish \"done\"")),
        ])
        .exec_result(exec_response(&[], None, Some(serde_json::json!("done"))))
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["finish \"done\""],
            checkpoints: vec![CheckpointKind::BeforeCompletion],
            no_final_message_event: true,
            assistant_reasoning_texts: Some(vec!["ready to finish"]),
            assistant_visible_texts: Some(vec!["Finishting."]),
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "finish \"done\"",
                output: Vec::new(),
                error: None,
                final_output: Some(serde_json::json!("done")),
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}

#[test]
fn rlm_prompt_history_reasoning_part_is_preserved_in_trajectory() {
    RlmProtocolScenario::new(STREAMED_REASONING_TRAJECTORY.display_name)
        .user_message("say hi")
        .termination(RlmTermination::FinishRequired { schema: None })
        .streamed_llm_response(vec![
            reasoning_part("I'll answer directly."),
            text_part(&lashlang_block("finish \"Hi.\"")),
        ])
        .exec_result(exec_response(&[], None, Some(serde_json::json!("Hi."))))
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["finish \"Hi.\""],
            checkpoints: vec![CheckpointKind::BeforeCompletion],
            no_final_message_event: true,
            assistant_reasoning_texts: Some(vec!["I'll answer directly."]),
            assistant_visible_texts: Some(Vec::new()),
            trajectory_last: Some(RlmTrajectoryExpectation {
                code: "finish \"Hi.\"",
                output: Vec::new(),
                error: None,
                final_output: Some(serde_json::json!("Hi.")),
            }),
            ..RlmProtocolExpectations::default()
        })
        .run();
}
