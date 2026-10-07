//! The relay prompt: the committed context under a constant header, then the
//! step message.
//!
//! The step message is everything a step needs that is not its own working
//! memory, in a fixed tagged order: the turn's user request (on every step of
//! the turn), the previous turn's answer (on a turn's first step), the last
//! step's code, output, calls and commit status with the calls of steps that
//! ran and committed nothing (they happened; the repair step must not redo
//! them), the memory's size and the vars kept and dropped, runtime notes, and
//! the next move. It is rebuilt every step from durable records, so a replay
//! renders it again byte for byte.

use std::fmt::Write as _;
use std::sync::Arc;

use lash_core::llm::types::{LlmContentBlock, LlmMessage, LlmRole};
use lash_rlm_types::RlmTrajectoryEntry;

use super::{RelaySettings, RelayView, context_chars, json_summary};

/// The context message's first block. It never changes, so it stays in the
/// cached prefix however the entries after it change.
pub(crate) const CONTEXT_HEADER: &str =
    "Your context: notes you wrote in earlier steps. Only you write here.";

/// The step message's last element: the two reply shapes.
const YOUR_MOVE: &str = "Call execute_code with a program ending in control.next({ context, vars }) to work, or reply in plain text (no tool call) to answer the user and end the turn.";

/// The step message's own tags. Embedded text that would open or close one of
/// them is escaped, so no user, tool or model text can end an element early.
const STEP_TAGS: &[&str] = &[
    "step",
    "user_request",
    "last_reply",
    "last_step",
    "code",
    "output",
    "calls",
    "error",
    "receipts",
    "memory",
    "note",
    "your_move",
];

/// What one relay prompt renders from.
pub(crate) struct RelayHarnessInput<'a> {
    pub(crate) view: &'a RelayView,
    pub(crate) settings: RelaySettings,
    pub(crate) step: usize,
    pub(crate) turn_causes: &'a [lash_core::TurnCause],
    /// The relay render of the variables the last program left, as
    /// [`crate::plugin::runtime_state`] records it at the iteration sync.
    pub(crate) left_variables: Option<&'a str>,
    /// The previous turn's committed prompt usage, the cache estimate's input.
    pub(crate) prompt_usage: Option<&'a lash_core::TokenUsage>,
}

/// A variable the last program left, as the relay bound-variables render
/// lists it: name and summary, never the value.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct LeftVariable {
    pub(crate) name: String,
    pub(crate) summary: String,
}

/// The relay bound-variables render: the variables a program left.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct LeftVariables {
    pub(crate) relay_left_variables: Vec<LeftVariable>,
}

/// The messages of one relay request: the context message (the constant
/// header, then one `[i]` block per entry), then the step message.
///
/// The cache breakpoints, in order, are the last context block unchanged
/// since the previous commit (the end of the prefix an earlier request
/// already wrote) and the end of the context. The system prompt and the
/// `execute_code` tool before them carry the provider's own breakpoints. When
/// the two coincide there is one. The step message is new every step, so it
/// carries none.
pub(crate) fn build_relay_messages(input: RelayHarnessInput<'_>) -> Vec<LlmMessage> {
    let context = input
        .view
        .committed
        .as_ref()
        .map(|baton| baton.context.as_slice())
        .unwrap_or_default();
    let unchanged = input
        .view
        .previous
        .as_ref()
        .map_or(0, |previous| common_prefix(&previous.context, context));
    let mut blocks = vec![text_block(CONTEXT_HEADER, context.is_empty())];
    blocks.extend(context.iter().enumerate().map(|(index, entry)| {
        text_block(
            format!("[{index}] {entry}"),
            index + 1 == unchanged || index + 1 == context.len(),
        )
    }));
    vec![
        LlmMessage::new(LlmRole::User, blocks),
        LlmMessage::new(LlmRole::User, vec![text_block(step_text(&input), false)]),
    ]
}

fn text_block(text: impl Into<Arc<str>>, cache_breakpoint: bool) -> LlmContentBlock {
    LlmContentBlock::Text {
        text: text.into(),
        response_meta: None,
        cache_breakpoint,
    }
}

fn step_text(input: &RelayHarnessInput<'_>) -> String {
    let view = input.view;
    let mut out = format!("<step n=\"{}\">", input.step);

    let request = view
        .turn_input
        .iter()
        .map(|&index| view.transcript[index].text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    element(&mut out, "user_request", &request);

    match &view.last_step {
        None => {
            if let Some(index) = view.last_reply {
                element(&mut out, "last_reply", &view.transcript[index].text);
            }
        }
        Some(step) => {
            let status = if view.last_step_committed {
                "committed".to_string()
            } else {
                format!(
                    "not committed: {}",
                    view.last_step_refusal
                        .as_deref()
                        .unwrap_or(match step.outcome {
                            lash_rlm_types::CellOutcome::Failed(_) => "the program failed",
                            _ => "it never reached a successful control.next",
                        })
                )
            };
            let _ = write!(
                out,
                "\n<last_step status=\"{}\">",
                escape_attribute(&status)
            );
            render_step(&mut out, step, input.settings.max_output_chars);
            let receipts = view
                .uncommitted
                .iter()
                .filter(|step| !step.calls.is_empty() || step.calls_omitted > 0)
                .map(|step| {
                    let mut line = format!("step {}:", step.protocol_iteration + 1);
                    if step.calls_omitted > 0 {
                        let _ = write!(line, " ({} earlier calls omitted)", step.calls_omitted);
                    }
                    let _ = write!(line, " {}", calls_line(&step.calls));
                    line
                })
                .collect::<Vec<_>>();
            if !view.last_step_committed && !receipts.is_empty() {
                element(
                    &mut out,
                    "receipts",
                    &format!(
                        "These calls ran in steps that did not commit, and their effects stand. Check their results instead of repeating them.\n{}",
                        receipts.join("\n")
                    ),
                );
            }
            out.push_str("\n</last_step>");
        }
    }

    element(&mut out, "memory", &memory_line(input));

    for note in notes(input) {
        element(&mut out, "note", &note);
    }

    let _ = write!(out, "\n<your_move>{YOUR_MOVE}</your_move>\n</step>");
    out
}

/// `<tag>` + escaped `text` + `</tag>`, on lines of their own when the text
/// spans several.
fn element(out: &mut String, tag: &str, text: &str) {
    let text = escape_tags(text.trim());
    if text.contains('\n') {
        let _ = write!(out, "\n<{tag}>\n{text}\n</{tag}>");
    } else {
        let _ = write!(out, "\n<{tag}>{text}</{tag}>");
    }
}

/// The memory line: the context's size against its budget, the vars the last
/// commit kept, the variables the last step left and dropped, and the cache
/// estimate when it is cold.
fn memory_line(input: &RelayHarnessInput<'_>) -> String {
    let view = input.view;
    let context = view
        .committed
        .as_ref()
        .map(|baton| baton.context.as_slice())
        .unwrap_or_default();
    let mut line = format!(
        "context: {} entries, {} of {} chars",
        context.len(),
        thousands(context_chars(context)),
        thousands(input.settings.context_budget_chars)
    );
    let kept = view
        .committed
        .as_ref()
        .map(|baton| baton.vars.iter().collect::<Vec<_>>())
        .unwrap_or_default();
    line.push_str(" · vars kept: ");
    if kept.is_empty() {
        line.push_str("none");
    } else {
        line.push_str(
            &kept
                .iter()
                .map(|(name, value)| format!("{name} ({})", json_summary(value)))
                .collect::<Vec<_>>()
                .join(", "),
        );
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
            .map(|variable| format!("{} ({})", variable.name, variable.summary))
            .collect::<Vec<_>>();
        if !dropped.is_empty() {
            let _ = write!(line, " · dropped: {}", dropped.join(", "));
        }
    }
    if let Some(usage) = input.prompt_usage
        && usage.cache_read_input_tokens == 0
        && usage.input_tokens + usage.cache_write_input_tokens > 1024
    {
        line.push_str(" · cache: cold");
    }
    line
}

/// The runtime notes, in order: the turn's causes, what a host injected into
/// this request, and protocol feedback since the last step. A note repeated
/// by consecutive replies is shown once, with its count.
fn notes(input: &RelayHarnessInput<'_>) -> Vec<String> {
    let mut notes = Vec::new();
    if let Some(events) = lash_core::facade_support::render_turn_causes_prompt(input.turn_causes) {
        notes.push(events);
    }
    notes.extend(
        input
            .view
            .host_notes
            .iter()
            .map(|note| note.trim().to_string())
            .filter(|note| !note.is_empty()),
    );
    let mut feedback: Vec<(&str, usize)> = Vec::new();
    for note in &input.view.feedback {
        match feedback.last_mut() {
            Some((last, count)) if *last == note.trim() => *count += 1,
            _ => feedback.push((note.trim(), 1)),
        }
    }
    notes.extend(feedback.into_iter().map(|(note, count)| {
        if count > 1 {
            format!("{note}\n(repeated for your last {count} replies)")
        } else {
            note.to_string()
        }
    }));
    notes
}

fn render_step(out: &mut String, step: &RlmTrajectoryEntry, max_output_chars: usize) {
    element(out, "code", &step.code);
    let printed = step
        .output
        .iter()
        .map(|print| print.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if !printed.is_empty() {
        let (preview, raw_len) =
            lash_core::facade_support::head_tail_truncate(&printed, max_output_chars);
        let mut output = preview;
        if raw_len > max_output_chars {
            let _ = write!(
                output,
                "\n(truncated from {raw_len} characters; it is gone unless you kept it)"
            );
        }
        element(out, "output", &output);
    } else if let Some(archive) = &step.output_archive {
        element(
            out,
            "output",
            &format!("(too long to show)\n{}", archive.witness),
        );
    }
    if !step.calls.is_empty() {
        element(out, "calls", &calls_line(&step.calls));
    }
    if let lash_rlm_types::CellOutcome::Failed(failure) = &step.outcome {
        element(out, "error", &crate::feedback::render(failure, "program"));
    }
}

/// `inbox.work.list → ok · control.next → ok`.
fn calls_line(calls: &[lash_rlm_types::RlmExecutedCall]) -> String {
    calls
        .iter()
        .map(|call| format!("{} → {}", call.operation, call.outcome.as_str()))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// `text` with every `<` that would open or close a step tag written as
/// `&lt;`. Everything else, code included, stays as written.
pub(crate) fn escape_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('<') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let name = after.strip_prefix('/').unwrap_or(after);
        let opens_tag = STEP_TAGS.iter().any(|tag| {
            name.strip_prefix(tag).is_some_and(|tail| {
                !tail
                    .chars()
                    .next()
                    .is_some_and(|next| next.is_alphanumeric() || next == '_')
            })
        });
        out.push_str(if opens_tag { "&lt;" } else { "<" });
        rest = after;
    }
    out.push_str(rest);
    out
}

/// An attribute value: tag-escaped, with no `"` to end it.
fn escape_attribute(text: &str) -> String {
    escape_tags(text).replace('"', "&quot;").replace('\n', " ")
}

/// `400000` → `400,000`.
fn thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// How many leading entries `previous` and `current` share.
fn common_prefix(previous: &[String], current: &[String]) -> usize {
    previous
        .iter()
        .zip(current)
        .take_while(|(before, after)| before == after)
        .count()
}
