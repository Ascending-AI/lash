//! Turning a stopped turn's partial output into ordinary input (ADR 0114
//! §5.3).
//!
//! A stopped turn's [`StoppedPartial`] is returned to the host and never fed
//! back by lash. A host that wants it in context selects what to keep, and
//! the pure `build_resubmission` helper renders the selection as one labeled
//! text item ahead of the host's follow-up. The host sends the result with
//! `session.send`, like any other input.

use std::collections::BTreeMap;

use lash_sansio::{
    CaptureCoverage, CutState, PartialItem, PartialItemId, StopReason, StoppedPartial,
    ToolCallOutput, ToolExecutionState, ToolOutputCapture, TurnId,
};

use crate::{InputItem, TurnInput};

/// The fixed label ahead of every resubmitted excerpt.
pub const RESUBMISSION_PREAMBLE: &str = "The previous assistant turn was stopped before it \
finished. The JSON below quotes what it produced. It is not a completed answer: items marked \
interrupted were cut off, tool calls marked not_started never ran, and tool calls marked \
outcome_unknown may have partly run.";

/// Renders the host's selection as ordinary input: one labeled text item
/// quoting the chosen items, then `follow_up`'s items. It is pure: it makes
/// no model request and no store call, and it never emits a tool-use, tool
/// result or assistant message.
///
/// When every item is omitted, the input is `follow_up` alone; when
/// `follow_up` is empty too, there is nothing to send and the answer is
/// [`ResubmissionError::Empty`].
pub fn build_resubmission(
    selection: &ResubmissionSelection<'_>,
    follow_up: TurnInput,
) -> Result<Resubmission, ResubmissionError> {
    let partial = selection.partial;
    if let Some(item) = selection
        .choices
        .keys()
        .find(|id| !partial.items.iter().any(|item| item.id() == *id))
    {
        return Err(ResubmissionError::UnknownItem { item: item.clone() });
    }
    let mut missing = Vec::new();
    let mut items = Vec::new();
    let mut omitted = Vec::new();
    for item in &partial.items {
        let id = item.id();
        let kind = PartialItemKind::of(item);
        let Some(choice) = selection.choice(id) else {
            missing.push(id.clone());
            continue;
        };
        if !kind.allows(choice) {
            return Err(ResubmissionError::ChoiceNotAllowed {
                item: id.clone(),
                choice,
            });
        }
        if choice == ItemChoice::Omit {
            omitted.push(OmittedItem {
                item: id.clone(),
                kind,
            });
            continue;
        }
        items.push(ExcerptItem::of(item));
    }
    if !missing.is_empty() {
        return Err(ResubmissionError::SelectionIncomplete { items: missing });
    }
    let omissions = OmissionReport {
        omitted,
        coverage: partial.coverage,
    };
    let TurnInput {
        items: follow_up_items,
        trace_turn_id,
        turn_context,
    } = follow_up;
    if items.is_empty() && follow_up_items.is_empty() {
        return Err(ResubmissionError::Empty);
    }
    let mut input_items = Vec::with_capacity(follow_up_items.len() + 1);
    if !items.is_empty() {
        let excerpt = ResubmittedExcerpt {
            stopped_turn: &partial.id.turn_id,
            reason: &partial.reason,
            coverage: partial.coverage,
            items,
        };
        let json = serde_json::to_string_pretty(&excerpt).map_err(|error| {
            ResubmissionError::Unencodable {
                message: error.to_string(),
            }
        })?;
        input_items.push(InputItem::text(format!(
            "{RESUBMISSION_PREAMBLE}\n\n{json}"
        )));
    }
    input_items.extend(follow_up_items);
    Ok(Resubmission {
        input: TurnInput {
            items: input_items,
            trace_turn_id,
            turn_context,
        },
        omissions,
    })
}

/// The quoted items, in the partial's order.
#[derive(serde::Serialize)]
struct ResubmittedExcerpt<'a> {
    stopped_turn: &'a TurnId,
    reason: &'a StopReason,
    coverage: CaptureCoverage,
    items: Vec<ExcerptItem<'a>>,
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ExcerptItem<'a> {
    AssistantText {
        state: CutState,
        text: &'a str,
    },
    QuotedReasoning {
        state: CutState,
        summary: &'a str,
    },
    ToolCall {
        call_id: &'a str,
        tool_name: &'a str,
        arguments: &'a serde_json::Value,
        result: AbortedToolResult<'a>,
    },
    QuotedInvalidArguments {
        call_id: Option<&'a str>,
        tool_name: Option<&'a str>,
        raw_arguments: &'a str,
    },
}

impl<'a> ExcerptItem<'a> {
    /// The quotation of an item the host kept. Which choice kept it is
    /// already checked: each kind has exactly one keeping choice.
    fn of(item: &'a PartialItem) -> Self {
        match item {
            PartialItem::Text { state, text, .. } => Self::AssistantText {
                state: *state,
                text,
            },
            PartialItem::Reasoning { state, summary, .. } => Self::QuotedReasoning {
                state: *state,
                summary,
            },
            PartialItem::ToolCall {
                call, execution, ..
            } => Self::ToolCall {
                call_id: &call.call_id,
                tool_name: &call.tool_name,
                arguments: &call.arguments,
                result: match execution {
                    ToolExecutionState::NotStarted => AbortedToolResult::NotStarted,
                    ToolExecutionState::Running(running) => AbortedToolResult::OutcomeUnknown {
                        captured_output: &running.output,
                    },
                    ToolExecutionState::Settled { output } => AbortedToolResult::Settled { output },
                },
            },
            PartialItem::ArgumentFragment {
                call_id,
                tool_name,
                raw_arguments,
                ..
            } => Self::QuotedInvalidArguments {
                call_id: call_id.as_deref(),
                tool_name: tool_name.as_deref(),
                raw_arguments,
            },
        }
    }
}

/// The typed result every quoted call is paired with.
#[derive(serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum AbortedToolResult<'a> {
    NotStarted,
    OutcomeUnknown {
        captured_output: &'a ToolOutputCapture,
    },
    Settled {
        output: &'a ToolCallOutput,
    },
}

/// The host's choice for each item of one partial.
#[derive(Clone, Debug)]
pub struct ResubmissionSelection<'p> {
    partial: &'p StoppedPartial,
    choices: BTreeMap<PartialItemId, ItemChoice>,
}

impl<'p> ResubmissionSelection<'p> {
    /// `Include` for text, calls and running tools. No choice for reasoning
    /// or fragments, which must be chosen explicitly.
    pub fn defaults(partial: &'p StoppedPartial) -> Self {
        let choices = partial
            .items
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    PartialItem::Text { .. } | PartialItem::ToolCall { .. }
                )
            })
            .map(|item| (item.id().clone(), ItemChoice::Include))
            .collect();
        Self { partial, choices }
    }

    pub fn choose(&mut self, item: &PartialItemId, choice: ItemChoice) -> &mut Self {
        self.choices.insert(item.clone(), choice);
        self
    }

    /// The partial this selection chooses from.
    pub fn partial(&self) -> &'p StoppedPartial {
        self.partial
    }

    /// The choice made for `item`, if any.
    pub fn choice(&self, item: &PartialItemId) -> Option<ItemChoice> {
        self.choices.get(item).copied()
    }

    /// Every choice made, by item.
    pub fn choices(&self) -> &BTreeMap<PartialItemId, ItemChoice> {
        &self.choices
    }
}

/// What the host does with one item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemChoice {
    /// Text, a complete call or a running tool.
    Include,
    /// A reasoning summary or a fragment's raw text, quoted and labeled.
    Quote,
    Omit,
}

/// Ordinary input built from a selection, and what it left out.
#[derive(Clone, Debug)]
pub struct Resubmission {
    /// Ordinary input: one labeled text item, then `follow_up`'s items.
    /// `trace_turn_id` and `turn_context` come from `follow_up`.
    pub input: TurnInput,
    pub omissions: OmissionReport,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OmissionReport {
    pub omitted: Vec<OmittedItem>,
    pub coverage: CaptureCoverage,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OmittedItem {
    pub item: PartialItemId,
    pub kind: PartialItemKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartialItemKind {
    Text,
    Reasoning,
    ToolCall,
    ArgumentFragment,
}

impl PartialItemKind {
    pub fn of(item: &PartialItem) -> Self {
        match item {
            PartialItem::Text { .. } => Self::Text,
            PartialItem::Reasoning { .. } => Self::Reasoning,
            PartialItem::ToolCall { .. } => Self::ToolCall,
            PartialItem::ArgumentFragment { .. } => Self::ArgumentFragment,
        }
    }

    /// Text and calls are included or omitted; reasoning and fragments are
    /// quoted or omitted.
    pub fn allows(self, choice: ItemChoice) -> bool {
        match (self, choice) {
            (_, ItemChoice::Omit) => true,
            (Self::Text | Self::ToolCall, ItemChoice::Include) => true,
            (Self::Reasoning | Self::ArgumentFragment, ItemChoice::Quote) => true,
            (_, ItemChoice::Include | ItemChoice::Quote) => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResubmissionError {
    #[error("items need an explicit choice: {items:?}")]
    SelectionIncomplete { items: Vec<PartialItemId> },
    #[error("{choice:?} is not allowed for {item:?}")]
    ChoiceNotAllowed {
        item: PartialItemId,
        choice: ItemChoice,
    },
    #[error("no such item: {item:?}")]
    UnknownItem { item: PartialItemId },
    #[error("nothing to resubmit")]
    Empty,
    /// The excerpt did not encode as JSON. A sealed partial always encodes
    /// (its digest is computed over the same encoding), so this names a
    /// partial that was built by hand.
    #[error("the excerpt does not encode: {message}")]
    Unencodable { message: String },
}

#[cfg(test)]
mod tests {
    use lash_sansio::{
        CompleteToolCall, FragmentState, InterruptedToolOutcome, PartialItemKey, RunningTool,
        SessionId, StoppedPartialId, ToolOutputChunk,
    };

    use super::*;

    fn seal(coverage: CaptureCoverage, items: Vec<PartialItem>) -> StoppedPartial {
        let id = StoppedPartialId {
            session_id: SessionId::from("session-1"),
            root: TurnId::from("root-1"),
            turn_id: TurnId::from("turn-1"),
            base: lash_sansio::CaptureBase(0),
            sealed_through: 7,
        };
        match StoppedPartial::seal(id, StopReason::UserCancel, false, coverage, items) {
            Ok(partial) => partial,
            Err(error) => panic!("seal: {error}"),
        }
    }

    fn item_id(key: PartialItemKey<'_>) -> PartialItemId {
        PartialItemId::new("llm", 1, key)
    }

    fn text(key: &str, state: CutState, text: &str) -> PartialItem {
        PartialItem::Text {
            id: item_id(PartialItemKey::Text(key)),
            state,
            text: text.to_string(),
        }
    }

    fn reasoning(key: &str) -> PartialItem {
        PartialItem::Reasoning {
            id: item_id(PartialItemKey::Reasoning(key)),
            state: CutState::Interrupted,
            summary: "Checking the READ".to_string(),
        }
    }

    fn call(key: &str, execution: ToolExecutionState) -> PartialItem {
        PartialItem::ToolCall {
            id: item_id(PartialItemKey::Tool(key)),
            call: CompleteToolCall {
                call_id: key.to_string(),
                tool_name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "README.md"}),
            },
            execution,
        }
    }

    fn fragment(state: FragmentState) -> PartialItem {
        PartialItem::ArgumentFragment {
            id: item_id(PartialItemKey::Tool("#0")),
            call_id: Some("call-2".to_string()),
            tool_name: Some("read_file".to_string()),
            raw_arguments: r#"{"path":"READ"#.to_string(),
            state,
        }
    }

    fn only_text(input: &TurnInput) -> Vec<&str> {
        input
            .items
            .iter()
            .map(|item| match item {
                InputItem::Text { text } => text.as_str(),
                InputItem::Attachment { .. } => panic!("the resubmission is text only"),
            })
            .collect()
    }

    fn build(
        selection: &ResubmissionSelection<'_>,
        follow_up: TurnInput,
    ) -> Result<Resubmission, ResubmissionError> {
        build_resubmission(selection, follow_up)
    }

    #[test]
    fn a_text_cut_resubmits_as_the_pinned_labeled_excerpt() {
        let partial = seal(
            CaptureCoverage::Complete,
            vec![text("b0", CutState::Interrupted, "The answer is fort")],
        );
        assert!(partial.safe_to_resubmit());
        let resubmission = match build(
            &ResubmissionSelection::defaults(&partial),
            TurnInput::text("go on"),
        ) {
            Ok(resubmission) => resubmission,
            Err(error) => panic!("build: {error}"),
        };
        let golden = "The previous assistant turn was stopped before it finished. The JSON \
below quotes what it produced. It is not a completed answer: items marked interrupted were cut \
off, tool calls marked not_started never ran, and tool calls marked outcome_unknown may have \
partly run.

{
  \"stopped_turn\": \"turn-1\",
  \"reason\": {
    \"kind\": \"user_cancel\"
  },
  \"coverage\": \"complete\",
  \"items\": [
    {
      \"kind\": \"assistant_text\",
      \"state\": \"interrupted\",
      \"text\": \"The answer is fort\"
    }
  ]
}";
        assert_eq!(only_text(&resubmission.input), vec![golden, "go on"]);
        assert_eq!(
            resubmission.omissions,
            OmissionReport {
                omitted: Vec::new(),
                coverage: CaptureCoverage::Complete,
            }
        );
    }

    #[test]
    fn follow_up_carrier_fields_survive() {
        let partial = seal(
            CaptureCoverage::Complete,
            vec![text("b0", CutState::Complete, "done")],
        );
        let mut follow_up = TurnInput::text("next");
        follow_up.trace_turn_id = Some(TurnId::from("trace-1"));
        let resubmission = match build(&ResubmissionSelection::defaults(&partial), follow_up) {
            Ok(resubmission) => resubmission,
            Err(error) => panic!("build: {error}"),
        };
        assert_eq!(
            resubmission.input.trace_turn_id,
            Some(TurnId::from("trace-1"))
        );
    }

    #[test]
    fn reasoning_needs_an_explicit_quote() {
        let partial = seal(CaptureCoverage::Complete, vec![reasoning("r0")]);
        let reasoning_id = item_id(PartialItemKey::Reasoning("r0"));
        assert!(!partial.safe_to_resubmit());
        let mut selection = ResubmissionSelection::defaults(&partial);
        assert_eq!(
            build(&selection, TurnInput::empty()).err(),
            Some(ResubmissionError::SelectionIncomplete {
                items: vec![reasoning_id.clone()],
            })
        );
        selection.choose(&reasoning_id, ItemChoice::Include);
        assert_eq!(
            build(&selection, TurnInput::empty()).err(),
            Some(ResubmissionError::ChoiceNotAllowed {
                item: reasoning_id.clone(),
                choice: ItemChoice::Include,
            })
        );
        selection.choose(&reasoning_id, ItemChoice::Quote);
        let resubmission = match build(&selection, TurnInput::empty()) {
            Ok(resubmission) => resubmission,
            Err(error) => panic!("build: {error}"),
        };
        let texts = only_text(&resubmission.input);
        assert_eq!(texts.len(), 1);
        assert!(texts[0].contains("\"kind\": \"quoted_reasoning\""));
        assert!(texts[0].contains("\"summary\": \"Checking the READ\""));
        for opaque in ["signature", "encrypted", "provider_item"] {
            assert!(!texts[0].contains(opaque), "{opaque} leaked");
        }
    }

    #[test]
    fn a_complete_call_is_paired_with_its_aborted_result() {
        let running = ToolExecutionState::Running(RunningTool {
            output: ToolOutputCapture::Captured {
                chunks: vec![ToolOutputChunk {
                    text: "line 1".to_string(),
                }],
                omitted_bytes: 0,
            },
            outcome: InterruptedToolOutcome::OutcomeUnknown,
        });
        let partial = seal(
            CaptureCoverage::Complete,
            vec![
                text("b0", CutState::Complete, "Reading it."),
                call("call-1", ToolExecutionState::NotStarted),
                call("call-3", running),
            ],
        );
        let resubmission = match build(
            &ResubmissionSelection::defaults(&partial),
            TurnInput::empty(),
        ) {
            Ok(resubmission) => resubmission,
            Err(error) => panic!("build: {error}"),
        };
        let texts = only_text(&resubmission.input);
        assert_eq!(texts.len(), 1);
        let Some((_, json)) = texts[0].split_once("\n\n") else {
            panic!("the excerpt follows a blank line");
        };
        let excerpt: serde_json::Value = match serde_json::from_str(json) {
            Ok(excerpt) => excerpt,
            Err(error) => panic!("excerpt json: {error}"),
        };
        assert_eq!(
            excerpt["items"][1],
            serde_json::json!({
                "kind": "tool_call",
                "call_id": "call-1",
                "tool_name": "read_file",
                "arguments": {"path": "README.md"},
                "result": {"status": "not_started"},
            })
        );
        assert_eq!(
            excerpt["items"][2]["result"],
            serde_json::json!({
                "status": "outcome_unknown",
                "captured_output": {
                    "capture": "captured",
                    "chunks": [{"text": "line 1"}],
                    "omitted_bytes": 0,
                },
            })
        );
    }

    #[test]
    fn a_fragment_is_quoted_never_included() {
        for state in [
            FragmentState::Interrupted,
            FragmentState::Invalid {
                parse_error: "EOF while parsing a string".to_string(),
            },
        ] {
            let partial = seal(CaptureCoverage::Complete, vec![fragment(state)]);
            let fragment_id = item_id(PartialItemKey::Tool("#0"));
            assert!(partial.cut_mid_tool_call());
            let mut selection = ResubmissionSelection::defaults(&partial);
            selection.choose(&fragment_id, ItemChoice::Include);
            assert_eq!(
                build(&selection, TurnInput::empty()).err(),
                Some(ResubmissionError::ChoiceNotAllowed {
                    item: fragment_id.clone(),
                    choice: ItemChoice::Include,
                })
            );
            selection.choose(&fragment_id, ItemChoice::Quote);
            let resubmission = match build(&selection, TurnInput::empty()) {
                Ok(resubmission) => resubmission,
                Err(error) => panic!("build: {error}"),
            };
            let texts = only_text(&resubmission.input);
            assert!(texts[0].contains("\"kind\": \"quoted_invalid_arguments\""));
            assert!(texts[0].contains(r#""raw_arguments": "{\"path\":\"READ""#));
        }
    }

    #[test]
    fn omissions_are_reported_and_an_empty_resubmission_is_refused() {
        let partial = seal(
            CaptureCoverage::AcknowledgedPrefix,
            vec![text("b0", CutState::Interrupted, "half")],
        );
        let text_id = item_id(PartialItemKey::Text("b0"));
        let mut selection = ResubmissionSelection::defaults(&partial);
        selection.choose(&text_id, ItemChoice::Omit);
        assert_eq!(
            build(&selection, TurnInput::empty()).err(),
            Some(ResubmissionError::Empty)
        );
        let resubmission = match build(&selection, TurnInput::text("only this")) {
            Ok(resubmission) => resubmission,
            Err(error) => panic!("build: {error}"),
        };
        assert_eq!(only_text(&resubmission.input), vec!["only this"]);
        assert_eq!(
            resubmission.omissions,
            OmissionReport {
                omitted: vec![OmittedItem {
                    item: text_id,
                    kind: PartialItemKind::Text,
                }],
                coverage: CaptureCoverage::AcknowledgedPrefix,
            }
        );

        let empty = seal(CaptureCoverage::Complete, Vec::new());
        assert_eq!(
            build(&ResubmissionSelection::defaults(&empty), TurnInput::empty()).err(),
            Some(ResubmissionError::Empty)
        );
    }

    #[test]
    fn a_choice_for_a_foreign_item_is_refused() {
        let partial = seal(
            CaptureCoverage::Complete,
            vec![text("b0", CutState::Complete, "done")],
        );
        let stranger = item_id(PartialItemKey::Text("elsewhere"));
        let mut selection = ResubmissionSelection::defaults(&partial);
        selection.choose(&stranger, ItemChoice::Omit);
        assert_eq!(
            build(&selection, TurnInput::empty()).err(),
            Some(ResubmissionError::UnknownItem { item: stranger })
        );
    }
}
