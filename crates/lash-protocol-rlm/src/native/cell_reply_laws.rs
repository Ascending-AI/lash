//! FIG-5302: a response carrying a code mode cell either runs it or is refused with
//! a correction the model sees, and no shape of it is committed as the turn's
//! reply with the cell unexecuted. Each law answers one scripted provider
//! response on a natural turn, where prose may end the turn, through the real
//! cell-channel and native-tool-channel drivers.

use super::tests::{call, config, drain, reply_with_reason, text};
use lash_core::facade_support::{TurnFinish, TurnOutcome, TurnStop};
use lash_core::sansio::Response;
use lash_core::session_model::SessionStreamEvent;
use lash_core::{Effect, LlmOutputPart, LlmTerminalReason, TurnMachine};
use lash_rlm_types::RlmProtocolEvent;

macro_rules! code {
    () => {
        "print(documents.view_image(\"chart.png\"));"
    };
}

const CODE: &str = code!();
const LONE_BLOCK: &str = concat!("<typescript>\n", code!(), "\n</typescript>");
const LONE_INLINE: &str = concat!("<typescript>", code!(), "</typescript>");
const PROSE_THEN_BLOCK: &str = concat!(
    "Viewing the chart.\n<typescript>\n",
    code!(),
    "\n</typescript>"
);
const BLOCK_THEN_PROSE: &str = concat!(
    "<typescript>\n",
    code!(),
    "\n</typescript>\nThat prints the chart."
);
const MARKDOWN_BLOCK: &str = concat!(
    "```typescript\n<typescript>\n",
    code!(),
    "\n</typescript>\n```"
);
const MARKDOWN_INLINE: &str = concat!("```typescript\n<typescript>", code!(), "</typescript>\n```");
/// A one-line cell with prose after its closing tag on the same line.
const INLINE_THEN_PROSE: &str = concat!("<typescript>", code!(), "</typescript> That prints it.");
/// Source begun on the open tag's own line.
const SOURCE_ON_OPEN_LINE: &str =
    "<typescript>const image = documents.view_image(\"chart.png\");\nprint(image);\n</typescript>";
/// The closing tag glued to the last source line.
const CLOSE_ON_SOURCE_LINE: &str = concat!("<typescript>\n", code!(), "</typescript>");

/// What the driver did with one response.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    /// The response's program ran.
    Ran(String),
    /// Nothing ran, and the model is asked again under this extraction decision.
    Corrected(String),
    /// The turn finished with this reply text.
    Replied(String),
    /// The turn stopped.
    Stopped(TurnStop),
}

fn answer(native: bool, parts: Vec<LlmOutputPart>, reason: LlmTerminalReason) -> Next {
    let mut machine = TurnMachine::new(
        config(native, lash_core::TerminationMode::Natural),
        Vec::new(),
        Default::default(),
        0,
    );
    let initial = drain(&mut machine);
    let mut effects = reply_with_reason(&mut machine, &initial, parts, reason);
    loop {
        if let Some(code) = effects.iter().find_map(|effect| match effect {
            Effect::ExecCode { code, .. } => Some(code.clone()),
            _ => None,
        }) {
            return Next::Ran(code);
        }
        if let Some(outcome) = effects.iter().find_map(|effect| match effect {
            Effect::Emit(SessionStreamEvent::TurnOutcome { outcome }) => Some(outcome),
            _ => None,
        }) {
            return match outcome {
                TurnOutcome::Finished(TurnFinish::AssistantMessage { text }) => {
                    Next::Replied(text.clone())
                }
                TurnOutcome::Stopped(stop) => Next::Stopped(stop.clone()),
                other => panic!("a scripted reply cannot finish with a value: {other:?}"),
            };
        }
        if effects
            .iter()
            .any(|effect| matches!(effect, Effect::LlmCall { .. }))
        {
            return Next::Corrected(last_decision(&machine, native));
        }
        let id = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::Checkpoint { id, .. } => Some(*id),
                _ => None,
            })
            .unwrap_or_else(|| panic!("a response leads somewhere: {effects:#?}"));
        machine.handle_response(Response::Checkpoint {
            id,
            delivery: Default::default(),
        });
        effects = drain(&mut machine);
    }
}

fn last_decision(machine: &TurnMachine, native: bool) -> String {
    let phase = if native {
        lash_rlm_types::RlmDiagnosticPhase::NativeExtraction
    } else {
        lash_rlm_types::RlmDiagnosticPhase::LlmExtraction
    };
    machine
        .events()
        .iter()
        .filter_map(|record| {
            let lash_core::SessionHistoryRecord::Protocol(event) = record else {
                return None;
            };
            match crate::projection::decode_rlm_protocol_event(event)
                .expect("valid history fixture")
                .as_deref()
                .map(AsRef::as_ref)
            {
                Some(RlmProtocolEvent::RlmDiagnostic(diagnostic)) if diagnostic.phase == phase => {
                    diagnostic.payload["decision"].as_str().map(str::to_string)
                }
                _ => None,
            }
        })
        .next_back()
        .expect("every answered response records its extraction decision")
}

fn reasoning(text: &str) -> LlmOutputPart {
    LlmOutputPart::Reasoning {
        text: text.to_string(),
        replay: None,
    }
}

/// The property every law shares: whatever happened, no committed reply
/// carries a cell that did not run.
fn assert_no_cell_replied(shape: &str, native: bool, next: &Next) {
    if let Next::Replied(reply) = next {
        assert!(
            !reply.contains("<typescript>") && !reply.contains("</typescript>"),
            "native={native} {shape}: an unexecuted cell was committed as the reply: {reply:?}"
        );
    }
}

/// Answers every `(shape, reply, reason, expected)` case and reports each one
/// that differs, so one run names every failing shape.
fn assert_answers(native: bool, cases: Vec<(&str, &str, LlmTerminalReason, Next)>) {
    let failures = cases
        .into_iter()
        .filter_map(|(shape, reply, reason, expected)| {
            let next = answer(native, vec![text(reply)], reason);
            (next != expected).then(|| {
                format!("native={native} {shape} at {reason:?}: {next:?}, expected {expected:?}")
            })
        })
        .collect::<Vec<_>>();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Shapes (a)–(d) and (h) on the cell channel: a cell the grammar reads runs,
/// whether the model stopped on its own or at the output limit.
#[test]
fn a_well_formed_cell_runs_in_every_position_the_cell_grammar_allows() {
    let mut cases = Vec::new();
    for (shape, reply) in [
        ("lone block", LONE_BLOCK),
        ("lone one-line cell", LONE_INLINE),
        ("prose then block", PROSE_THEN_BLOCK),
        ("block then prose", BLOCK_THEN_PROSE),
        ("block inside a markdown fence", MARKDOWN_BLOCK),
        ("one-line cell inside a markdown fence", MARKDOWN_INLINE),
    ] {
        for reason in [LlmTerminalReason::Stop, LlmTerminalReason::OutputLimit] {
            cases.push((shape, reply, reason, Next::Ran(CODE.to_string())));
        }
    }
    assert_answers(false, cases);
}

/// FIG-6432 provider replay: a model predicts another cell or the answer
/// immediately after the first block's close. Only the first program runs.
#[test]
fn a_provider_reply_glued_to_the_first_close_runs_only_the_first_cell() {
    assert_answers(
        false,
        vec![
            (
                "glued next cell",
                concat!(
                    "<typescript>\n",
                    code!(),
                    "\n</typescript><typescript>\nprint(\"predicted\");\n</typescript>"
                ),
                LlmTerminalReason::Stop,
                Next::Ran(CODE.to_string()),
            ),
            (
                "glued answer",
                concat!("<typescript>\n", code!(), "\n</typescript>Sent a joke…"),
                LlmTerminalReason::Stop,
                Next::Ran(CODE.to_string()),
            ),
        ],
    );
}

/// FIG-6432 / S1: observed tails are typed evidence, and only the executed
/// cell enters the next request. The journaled stream and full reply agree.
#[test]
fn discarded_continuations_are_recorded_and_excluded_from_history_in_both_dialects() {
    for (dialect, open, close) in [
        (
            crate::dialect::CellDialect::typescript(),
            "<typescript>",
            "</typescript>",
        ),
        (
            crate::dialect::CellDialect::python(),
            "<python>",
            "</python>",
        ),
    ] {
        let first = format!("Before.\n{open}\nprint(1)\n{close}");
        for tail in [
            String::new(),
            format!("{open}\nprint(999)\n{close}"),
            "Sent a joke…".to_string(),
            "\nPredicted results.".to_string(),
        ] {
            let raw = format!("{first}{tail}");
            for streamed in [false, true] {
                let config = || {
                    super::tests::config_with_dialect(
                        false,
                        lash_core::TerminationMode::Natural.into(),
                        dialect.clone(),
                    )
                };
                let mut machine = TurnMachine::new(config(), Vec::new(), Default::default(), 0);
                let initial = drain(&mut machine);
                let id = initial
                    .iter()
                    .find_map(|e| match e {
                        Effect::LlmCall { id, .. } => Some(*id),
                        _ => None,
                    })
                    .unwrap();
                let reply = if streamed {
                    crate::stream_mask::streamed_reply_for_test(dialect.clone(), &[&raw])
                } else {
                    lash_core::LlmResponse {
                        parts: vec![text(&raw)],
                        terminal_reason: LlmTerminalReason::Stop,
                        ..Default::default()
                    }
                };
                machine.handle_response(Response::LlmComplete {
                    id,
                    text_streamed: streamed,
                    result: Ok(reply),
                });
                let effects = drain(&mut machine);
                let execs = effects
                    .iter()
                    .filter_map(|e| match e {
                        Effect::ExecCode { id, code, .. } => Some((*id, code)),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert_eq!(execs.len(), 1);
                assert_eq!(execs[0].1, "print(1)");
                assert_eq!(machine.cell_trailing_discarded().unwrap(), tail.len());
                let diagnostic = machine
                    .events()
                    .iter()
                    .find_map(|record| {
                        let lash_core::SessionHistoryRecord::Protocol(event) = record else {
                            return None;
                        };
                        match crate::projection::decode_rlm_protocol_event(event)
                            .unwrap()
                            .as_deref()
                            .map(AsRef::as_ref)
                        {
                            Some(RlmProtocolEvent::RlmDiagnostic(d)) => Some(d.payload.clone()),
                            _ => None,
                        }
                    })
                    .unwrap();
                if tail.is_empty() {
                    assert!(diagnostic.get("discarded_after_cell").is_none());
                } else {
                    let note: lash_core::llm::types::DiscardedAfterCell =
                        serde_json::from_value(diagnostic["discarded_after_cell"].clone()).unwrap();
                    assert_eq!(note.bytes, tail.len());
                    assert_eq!(
                        note.reason,
                        lash_core::llm::types::DiscardReason::PredictedContinuation
                    );
                }
                let id = execs[0].0;
                // The note also survives the cell's checkpoint boundary.
                machine =
                    TurnMachine::restore_from_checkpoint(config(), machine.checkpoint(), None)
                        .unwrap();
                drain(&mut machine);
                machine.handle_response(Response::ExecResult {
                    id,
                    result: Ok(super::tests::response(None)),
                });
                let mut effects = drain(&mut machine);
                let request = loop {
                    if let Some(request) = effects.iter().find_map(|e| match e {
                        Effect::LlmCall { request, .. } => Some(request),
                        _ => None,
                    }) {
                        break request.clone();
                    }
                    let id = effects
                        .iter()
                        .find_map(|e| match e {
                            Effect::Checkpoint { id, .. } => Some(*id),
                            _ => None,
                        })
                        .expect("continuation checkpoint");
                    machine.handle_response(Response::Checkpoint {
                        id,
                        delivery: Default::default(),
                    });
                    effects = drain(&mut machine);
                };
                let history = serde_json::to_string(&request.messages).unwrap();
                assert!(history.contains("print(1)"));
                assert!(
                    !history.contains("999")
                        && !history.contains("Sent a joke")
                        && !history.contains("Predicted results"),
                    "{history}"
                );
            }
        }
    }
}

/// A cell the grammar refuses is refused out loud on a natural turn too, rather
/// than read as the prose answer and committed verbatim.
///
/// Red before FIG-5302: on a natural turn the driver consulted the fence check
/// only where a cell was required, so the first two shapes finished the turn
/// with the cell's own text as the reply.
#[test]
fn a_cell_the_grammar_refuses_is_corrected_and_never_replied() {
    let mut cases = Vec::new();
    for (shape, reply, at_stop, at_limit) in [
        (
            "one-line cell with trailing prose",
            INLINE_THEN_PROSE,
            "retry_malformed_cell_fence",
            "retry_output_limit_prose",
        ),
        (
            "source on the open-tag line",
            SOURCE_ON_OPEN_LINE,
            "retry_malformed_cell_fence",
            "retry_output_limit_prose",
        ),
        (
            "close tag on the last source line",
            CLOSE_ON_SOURCE_LINE,
            "retry_unclosed_cell",
            "retry_output_limit_cell",
        ),
    ] {
        for (reason, decision) in [
            (LlmTerminalReason::Stop, at_stop),
            (LlmTerminalReason::OutputLimit, at_limit),
        ] {
            cases.push((shape, reply, reason, Next::Corrected(decision.to_string())));
        }
    }
    assert_answers(false, cases);
}

/// Shape (g): the native channel runs code only through `execute_code`, and
/// its prompt teaches no cell tags (`native/prompt.rs`). A cell in reply text,
/// well-formed or not, is outside that contract: it is corrected toward the
/// tool, never committed as the answer.
///
/// Red before FIG-5302: a native reply with no tool call was prose, and a
/// natural turn committed it whole.
#[test]
fn a_text_cell_on_the_native_channel_is_corrected_toward_execute_code() {
    let mut cases = Vec::new();
    for (shape, reply) in [
        ("lone block", LONE_BLOCK),
        ("lone one-line cell", LONE_INLINE),
        ("prose then block", PROSE_THEN_BLOCK),
        ("block then prose", BLOCK_THEN_PROSE),
        ("block inside a markdown fence", MARKDOWN_BLOCK),
        ("one-line cell inside a markdown fence", MARKDOWN_INLINE),
        ("one-line cell with trailing prose", INLINE_THEN_PROSE),
        ("source on the open-tag line", SOURCE_ON_OPEN_LINE),
        ("close tag on the last source line", CLOSE_ON_SOURCE_LINE),
    ] {
        for (reason, decision) in [
            (LlmTerminalReason::Stop, "retry_text_cell"),
            (LlmTerminalReason::OutputLimit, "retry_output_limit_prose"),
        ] {
            cases.push((shape, reply, reason, Next::Corrected(decision.to_string())));
        }
    }
    assert_answers(true, cases);
}

/// The native correction names the tool the program belongs in, in the very
/// next request the model sees.
#[test]
fn the_native_text_cell_correction_reaches_the_next_request() {
    let mut machine = TurnMachine::new(
        config(true, lash_core::TerminationMode::Natural),
        Vec::new(),
        Default::default(),
        0,
    );
    let initial = drain(&mut machine);
    let mut effects = reply_with_reason(
        &mut machine,
        &initial,
        vec![text(LONE_BLOCK)],
        LlmTerminalReason::Stop,
    );
    let request = loop {
        if let Some(request) = effects.iter().find_map(|effect| match effect {
            Effect::LlmCall { request, .. } => Some(request.clone()),
            _ => None,
        }) {
            break request;
        }
        let id = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::Checkpoint { id, .. } => Some(*id),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the correction continues the turn: {effects:#?}"));
        machine.handle_response(Response::Checkpoint {
            id,
            delivery: Default::default(),
        });
        effects = drain(&mut machine);
    };
    let rendered = serde_json::to_string(&request.messages).unwrap();
    assert!(
        rendered.contains("No code executed")
            && rendered.contains("is not executed")
            && rendered.contains("`code` argument of an `execute_code` call"),
        "{rendered}"
    );
}

/// Shape (f): a cell beside a provider tool call. The cell channel declares no
/// tools, so the stray call is repaired and nothing runs (FIG-2777); the native
/// channel runs the call's program, and the text cell beside it stays prose
/// that ends nothing.
#[test]
fn a_cell_beside_a_tool_call_runs_the_channel_program_or_nothing() {
    let parts = |tool: &str, args: &str| vec![text(LONE_BLOCK), call("provider-id", tool, args)];
    let cell = answer(
        false,
        parts("execute_code", r#"{"code":"finish(1);"}"#),
        LlmTerminalReason::Stop,
    );
    assert_eq!(cell, Next::Corrected("retry_native_tool_call".to_string()));
    let native = answer(
        true,
        parts("execute_code", r#"{"code":"finish(1);"}"#),
        LlmTerminalReason::ToolUse,
    );
    assert_eq!(native, Next::Ran("finish(1);".to_string()));
}

/// Shape (e): the cell contract reads cells from reply text only
/// (`cell_scan.rs`: "visible assistant prose, then a line ..."). A cell written
/// only in the reasoning channel is outside it and runs nothing; the turn ends
/// with an empty reply on the cell channel and a typed provider stop on the
/// native channel, never with the cell as its reply. With the text channel
/// carrying the cell as well, the text's cell runs.
#[test]
fn a_cell_in_the_reasoning_channel_alone_runs_nothing_and_is_never_replied() {
    for native in [false, true] {
        let next = answer(native, vec![reasoning(LONE_BLOCK)], LlmTerminalReason::Stop);
        assert_no_cell_replied("reasoning-only cell", native, &next);
        let expected = if native {
            Next::Stopped(TurnStop::ProviderError {
                cause: lash_protocol_types::TurnErrorCause::new(
                    lash_protocol_types::TurnErrorCode::InvalidResponse,
                ),
            })
        } else {
            Next::Replied(String::new())
        };
        assert_eq!(next, expected, "native={native}");
    }
    let next = answer(
        false,
        vec![reasoning(LONE_BLOCK), text(LONE_BLOCK)],
        LlmTerminalReason::Stop,
    );
    assert_eq!(next, Next::Ran(CODE.to_string()));
}
