//! Turning a stopped turn's partial output into ordinary input (ADR 0114
//! §5.3).
//!
//! A stopped turn's [`StoppedPartial`] is returned to the host and never fed
//! back by lash. A host that wants it in context selects what to keep, and
//! the pure `build_resubmission` helper renders the selection as one labeled
//! text item ahead of the host's follow-up. The host sends the result with
//! `session.send`, like any other input.

use std::collections::BTreeMap;

use lash_sansio::{CaptureCoverage, PartialItem, PartialItemId, StoppedPartial};

use crate::TurnInput;

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
}
