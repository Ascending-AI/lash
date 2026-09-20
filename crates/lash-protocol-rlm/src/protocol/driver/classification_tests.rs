use super::*;

/// FIG-1788: the whole non-cell decision space, pinned byte for byte.
/// Decision strings, retained-projection purposes and correction message
/// ids are durable history — a drift in any of them fails here.
#[test]
fn non_cell_reply_classification_table_is_byte_identical() {
    #[derive(Debug, PartialEq, Eq)]
    enum Outcome {
        Cell,
        Finish,
        Repair {
            decision: &'static str,
            assistant_message: Option<(String, &'static str)>,
            correction_id: String,
        },
    }

    let driver = RlmDriver::new();
    let turn_id = TurnId::from("turn-9");
    let iteration = 4;
    let attempt = AttemptContext {
        turn_id: &turn_id,
        protocol_iteration: iteration,
        output_token_cap: Some(512),
    };
    let finish_required = RlmTermination::FinishRequired { schema: None };
    let natural = RlmTermination::Natural;
    let malformed = "<typescript >\nfinish(1)\n</typescript>";
    let reasoning = [RlmReasoningPart {
        text: "thinking".to_string(),
        replay: None,
    }];
    fn reply<'a>(
        assistant_text: &'a str,
        visible_prose: &'a str,
        visible_assistant_text: &'a str,
        reasoning: &'a [RlmReasoningPart],
    ) -> ReplyProjections<'a> {
        ReplyProjections {
            assistant_text,
            visible_prose,
            visible_assistant_text,
            reasoning,
        }
    }
    let classify =
        |extraction, terminal_reason, termination: &RlmTermination, reply: ReplyProjections<'_>| {
            match driver.classify_reply(&attempt, extraction, terminal_reason, termination, reply) {
                ReplyClass::Cell(_) => Outcome::Cell,
                ReplyClass::Finish => Outcome::Finish,
                ReplyClass::Repair(prompt) => Outcome::Repair {
                    decision: prompt.decision,
                    assistant_message: prompt
                        .assistant_message
                        .map(|(text, purpose)| (text.to_string(), purpose)),
                    correction_id: prompt.correction.id,
                },
            }
        };
    let repair = |decision,
                  assistant_message: Option<(&'static str, &'static str)>,
                  purpose: &str| {
        Outcome::Repair {
            decision,
            assistant_message: assistant_message.map(|(text, purpose)| (text.to_string(), purpose)),
            correction_id: format!("m_rlm_{turn_id}_{iteration}_{purpose}"),
        }
    };

    let cases: Vec<(&str, Outcome, Outcome)> = vec![
        (
            "unclosed cell cut by the output limit",
            classify(
                Err(CellExtractionError::UnclosedCell),
                LlmTerminalReason::OutputLimit,
                &finish_required,
                reply("raw", "tag-stripped prose", "normalized prose", &reasoning),
            ),
            repair(
                "retry_output_limit_cell",
                Some(("tag-stripped prose", "assistant_response")),
                "invalid_cell",
            ),
        ),
        (
            "unclosed cell at a clean stop",
            classify(
                Err(CellExtractionError::UnclosedCell),
                LlmTerminalReason::Stop,
                &finish_required,
                reply("raw", "tag-stripped prose", "normalized prose", &reasoning),
            ),
            repair(
                "retry_unclosed_cell",
                Some(("tag-stripped prose", "assistant_response")),
                "invalid_cell",
            ),
        ),
        (
            "prose reply cut by the output limit",
            classify(
                Ok(None),
                LlmTerminalReason::OutputLimit,
                &finish_required,
                reply("raw", "tag-stripped prose", "normalized prose", &reasoning),
            ),
            repair(
                "retry_output_limit_prose",
                Some(("normalized prose", "truncated_assistant_response")),
                "output_limit_retry",
            ),
        ),
        (
            "malformed fence when a cell is required",
            classify(
                Ok(None),
                LlmTerminalReason::Stop,
                &finish_required,
                reply(
                    malformed,
                    "tag-stripped prose",
                    "normalized prose",
                    &reasoning,
                ),
            ),
            repair(
                "retry_malformed_cell_fence",
                Some(("tag-stripped prose", "assistant_response")),
                "malformed_cell_fence",
            ),
        ),
        (
            "prose answer on a natural turn",
            classify(
                Ok(None),
                LlmTerminalReason::Stop,
                &natural,
                reply("raw", "tag-stripped prose", "normalized prose", &reasoning),
            ),
            Outcome::Finish,
        ),
        (
            "a malformed fence stays a finish on a natural turn",
            classify(
                Ok(None),
                LlmTerminalReason::Stop,
                &natural,
                reply(
                    malformed,
                    "tag-stripped prose",
                    "normalized prose",
                    &reasoning,
                ),
            ),
            Outcome::Finish,
        ),
        (
            "finish-required with visible prose",
            classify(
                Ok(None),
                LlmTerminalReason::Stop,
                &finish_required,
                reply("raw", "tag-stripped prose", "normalized prose", &reasoning),
            ),
            repair(
                "request_finish",
                Some(("normalized prose", "assistant_prose")),
                "finish_reminder",
            ),
        ),
        (
            "finish-required with reasoning only",
            classify(
                Ok(None),
                LlmTerminalReason::Stop,
                &finish_required,
                reply("raw", "tag-stripped prose", "", &reasoning),
            ),
            repair(
                "request_finish",
                Some(("", "assistant_reasoning")),
                "finish_reminder",
            ),
        ),
        (
            "finish-required with nothing to retain",
            classify(
                Ok(None),
                LlmTerminalReason::Stop,
                &finish_required,
                reply("raw", "tag-stripped prose", "", &[]),
            ),
            repair("request_finish", None, "finish_reminder"),
        ),
        (
            "a usable cell stays on the mainline",
            classify(
                extract_cell(
                    "<typescript>\nfinish(1)\n</typescript>",
                    driver.dialect.cell_tags(),
                ),
                LlmTerminalReason::Stop,
                &finish_required,
                reply("raw", "tag-stripped prose", "normalized prose", &reasoning),
            ),
            Outcome::Cell,
        ),
    ];
    let mut failures = Vec::new();
    for (name, actual, expected) in cases {
        if actual != expected {
            failures.push(format!("{name}: {actual:?} != {expected:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));

    let native = driver.native_tool_call_prompt(&attempt, "bash");
    assert_eq!(native.decision, "retry_native_tool_call");
    assert_eq!(native.assistant_message, None);
    assert_eq!(
        native.correction.id,
        format!("m_rlm_{turn_id}_{iteration}_native_tool_call")
    );
}
