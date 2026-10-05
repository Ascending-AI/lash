//! The relay prompt: the committed context as content blocks, then the
//! harness message.
//!
//! The harness message is everything a step needs that is not its own
//! working memory: the turn's user input (on every step of the turn), the
//! last step's code, output and error, the calls of steps that ran and
//! committed nothing (they happened; the repair step must not redo them),
//! the vars the last commit kept and the ones the last step dropped, and the
//! context's size, budget and cache cost. It is rebuilt every step from
//! durable records, so a replay renders it again byte for byte.

use std::fmt::Write as _;
use std::sync::Arc;

use lash_core::llm::types::{LlmContentBlock, LlmMessage, LlmRole};
use lash_rlm_types::RlmTrajectoryEntry;

use super::{RelaySettings, RelayView, context_chars, json_summary};

/// What one relay prompt renders from.
pub(crate) struct RelayHarnessInput<'a> {
    pub(crate) view: &'a RelayView,
    pub(crate) settings: RelaySettings,
    pub(crate) step: usize,
    pub(crate) cell_noun: &'static str,
    pub(crate) turn_causes: &'a [lash_core::TurnCause],
    /// The relay render of the variables the last cell left, as
    /// [`crate::plugin::runtime_state`] records it at the iteration sync.
    pub(crate) left_variables: Option<&'a str>,
    /// The previous turn's committed prompt usage, the cache estimate's input.
    pub(crate) prompt_usage: Option<&'a lash_core::TokenUsage>,
}

/// A variable the last cell left, as the relay bound-variables render lists
/// it: name and summary, never the value.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct LeftVariable {
    pub(crate) name: String,
    pub(crate) summary: String,
}

/// The relay bound-variables render: the variables a cell left.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct LeftVariables {
    pub(crate) relay_left_variables: Vec<LeftVariable>,
}

/// The messages of one relay request: the context, one block per entry with
/// a cache breakpoint on the last, then the harness message.
pub(crate) fn build_relay_messages(input: RelayHarnessInput<'_>) -> Vec<LlmMessage> {
    let context = input
        .view
        .committed
        .as_ref()
        .map(|baton| baton.context.as_slice())
        .unwrap_or_default();
    let mut blocks = context
        .iter()
        .filter(|entry| !entry.trim().is_empty())
        .map(|entry| text_block(entry.clone(), false))
        .collect::<Vec<_>>();
    // The one message breakpoint the provider layer honours goes at the end
    // of the context: the system prompt has its own, and the harness after it
    // changes every step.
    if let Some(LlmContentBlock::Text {
        cache_breakpoint, ..
    }) = blocks.last_mut()
    {
        *cache_breakpoint = true;
    }
    let mut messages = Vec::new();
    if !blocks.is_empty() {
        messages.push(LlmMessage::new(LlmRole::User, blocks));
    }
    messages.push(LlmMessage::new(
        LlmRole::User,
        vec![text_block(harness_text(&input), false)],
    ));
    messages
}

fn text_block(text: impl Into<Arc<str>>, cache_breakpoint: bool) -> LlmContentBlock {
    LlmContentBlock::Text {
        text: text.into(),
        response_meta: None,
        cache_breakpoint,
    }
}

fn harness_text(input: &RelayHarnessInput<'_>) -> String {
    let view = input.view;
    let mut out = format!("=== HARNESS · step {} ===", input.step);

    out.push_str("\n\n--- User message (this turn) ---\n");
    if view.turn_input.is_empty() {
        out.push_str("(none)");
    }
    for (position, &index) in view.turn_input.iter().enumerate() {
        if position > 0 {
            out.push_str("\n\n");
        }
        let entry = &view.transcript[index];
        let (preview, raw_len) = lash_core::facade_support::head_tail_truncate(
            &entry.text,
            input.settings.max_output_chars,
        );
        out.push_str(&preview);
        if raw_len > input.settings.max_output_chars {
            let _ = write!(
                out,
                "\n(preview only: the full {raw_len} characters are in `transcript[{index}].text`)"
            );
        }
    }

    if let Some(events) = lash_core::facade_support::render_turn_causes_prompt(input.turn_causes) {
        out.push_str("\n\n");
        out.push_str(&events);
    }

    match &view.last_step {
        None => out.push_str("\n\n--- Last step ---\nNone yet this turn."),
        Some(step) => {
            let _ = write!(
                out,
                "\n\n--- Last step ({}) ---\n",
                if view.last_step_committed {
                    "committed"
                } else {
                    "NOT committed: nothing it passed to next, no var and no output was kept"
                }
            );
            render_step(
                &mut out,
                step,
                input.settings.max_output_chars,
                input.cell_noun,
            );
        }
    }

    for note in &view.feedback {
        out.push_str("\n\n--- Harness note ---\n");
        out.push_str(note.trim());
    }

    let receipts = view
        .uncommitted
        .iter()
        .filter(|step| !step.calls.is_empty() || step.calls_omitted > 0)
        .collect::<Vec<_>>();
    if !receipts.is_empty() {
        out.push_str(
            "\n\n--- Effects of steps that did not commit ---\nThese calls ran and their effects stand. Do not redo them; check their results instead.",
        );
        for step in receipts {
            let _ = write!(out, "\n- step {}:", step.protocol_iteration + 1);
            if step.calls_omitted > 0 {
                let _ = write!(out, " ({} earlier calls omitted)", step.calls_omitted);
            }
            for call in &step.calls {
                let _ = write!(out, "\n  - {} → {}", call.operation, call.outcome.as_str());
            }
        }
    }

    out.push_str("\n\n--- Variables ---\n");
    let kept = view
        .committed
        .as_ref()
        .map(|baton| baton.vars.iter().collect::<Vec<_>>())
        .unwrap_or_default();
    if kept.is_empty() {
        out.push_str("Kept by your last commit: none.");
    } else {
        out.push_str("Kept by your last commit (bound now):");
        for (name, value) in &kept {
            let _ = write!(out, "\n- `{name}`: {}", json_summary(value));
        }
    }
    if view.last_step.is_some() {
        let left = input
            .left_variables
            .and_then(|text| serde_json::from_str::<LeftVariables>(text).ok())
            .unwrap_or_default();
        let dropped = left
            .relay_left_variables
            .iter()
            .filter(|variable| !kept.iter().any(|(name, _)| **name == variable.name))
            .collect::<Vec<_>>();
        if !dropped.is_empty() {
            out.push_str("\nLeft by your last step and dropped (not in vars):");
            for variable in dropped {
                let _ = write!(out, "\n- `{}`: {}", variable.name, variable.summary);
            }
        }
    }
    let _ = write!(
        out,
        "\nAlways bound: `{}` (your context, as above) and `{}` (committed user inputs and outputs: {} entries).",
        super::CONTEXT_VAR,
        super::TRANSCRIPT_VAR,
        view.transcript.len(),
    );

    let context = view
        .committed
        .as_ref()
        .map(|baton| baton.context.as_slice())
        .unwrap_or_default();
    let chars = context_chars(context);
    let _ = write!(
        out,
        "\n\n--- Context ---\n{} entries, {chars} of {} characters.",
        context.len(),
        input.settings.context_budget_chars
    );
    if let Some(previous) = &view.previous {
        match first_change(&previous.context, context) {
            Some(index) => {
                let reread = context_chars(&context[index.min(context.len())..]);
                let _ = write!(
                    out,
                    " Last edit changed entry {index} onward: about {} tokens re-read uncached.",
                    reread.div_ceil(4)
                );
            }
            None => out.push_str(" Last commit left the context unchanged."),
        }
    }
    if let Some(usage) = input.prompt_usage
        && usage.cache_read_input_tokens == 0
        && usage.input_tokens + usage.cache_write_input_tokens > 1024
    {
        out.push_str(
            "\ncache: cold (estimate: the last turn's request read nothing from the provider cache)",
        );
    }
    out
}

/// The first entry at which `current` differs from `previous`, or `None` when
/// they are equal.
fn first_change(previous: &[String], current: &[String]) -> Option<usize> {
    let common = previous
        .iter()
        .zip(current)
        .take_while(|(before, after)| before == after)
        .count();
    (common != previous.len() || common != current.len()).then_some(common)
}

fn render_step(
    out: &mut String,
    step: &RlmTrajectoryEntry,
    max_output_chars: usize,
    cell_noun: &str,
) {
    out.push_str("Code:\n");
    out.push_str(step.code.trim());
    let printed = step
        .output
        .iter()
        .map(|print| print.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if !printed.is_empty() {
        let (preview, raw_len) =
            lash_core::facade_support::head_tail_truncate(&printed, max_output_chars);
        out.push_str("\nOutput:\n");
        out.push_str(&preview);
        if raw_len > max_output_chars {
            let _ = write!(
                out,
                "\n(truncated from {raw_len} characters; it is gone unless you kept it)"
            );
        }
    } else if let Some(archive) = &step.output_archive {
        out.push_str("\nOutput (too long to show):\n");
        out.push_str(&archive.witness);
    }
    if !step.calls.is_empty() {
        out.push_str("\nCalls:");
        for call in &step.calls {
            let _ = write!(out, "\n- {} → {}", call.operation, call.outcome.as_str());
        }
    }
    if let lash_rlm_types::CellOutcome::Failed(failure) = &step.outcome {
        out.push_str("\nError:\n");
        out.push_str(&crate::feedback::render(failure, cell_noun));
    }
}
