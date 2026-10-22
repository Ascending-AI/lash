//! Immutable message parts and the reservation owned by their actual Arc.

use super::Part;
use crate::allocation::{AllocationBudget, AllocationError, AllocationHold};
use std::sync::Arc;

/// Message-part storage. Its wire form is the plain list of parts.
/// Readers clone the Arc, keeping its capacity obligation alive independently
/// of a graph node or message that first exposed the parts.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct MessageParts {
    parts: Vec<Part>,
    #[serde(skip)]
    allocation: AllocationHold,
    #[serde(skip)]
    edits: Vec<AllocationHold>,
    #[serde(skip)]
    budget: AllocationBudget,
}

impl MessageParts {
    /// Independently constructed input; owner adoption uses `clone_in`.
    pub fn new(parts: Vec<Part>) -> Self {
        Self {
            parts,
            allocation: AllocationHold::default(),
            edits: Vec::new(),
            budget: AllocationBudget::unowned(),
        }
    }

    /// Reserve before cloning owned part strings and their vector capacity.
    pub fn clone_in(
        parts: &[Part],
        budget: &AllocationBudget,
    ) -> Result<Arc<Self>, AllocationError> {
        let allocation = budget.reserve(crate::allocation::payload_capacity(
            &parts,
            std::mem::size_of::<Self>() + 16 + std::mem::size_of_val(parts),
        ))?;
        Ok(Arc::new(Self {
            parts: parts.to_vec(),
            allocation,
            edits: Vec::new(),
            budget: budget.clone(),
        }))
    }

    /// A reader already admitted by this ledger stays shared. Independent
    /// cold or foreign input is copied only after the new claim admits it.
    pub fn adopt_in(
        parts: &Arc<Self>,
        budget: &AllocationBudget,
    ) -> Result<Arc<Self>, AllocationError> {
        if parts.budget.identity() == budget.identity() {
            return Ok(Arc::clone(parts));
        }
        Self::clone_in(parts.as_slice(), budget)
    }

    pub(crate) fn allocation_budget(&self) -> &AllocationBudget {
        &self.budget
    }

    /// Transfer a decode's preallocation obligation into its fresh parts.
    pub fn decoded_in(&mut self, allocation: AllocationHold, budget: AllocationBudget) {
        self.allocation = allocation;
        self.budget = budget;
    }

    /// Prepare a private editor under the source ledger. Every editor method
    /// reserves its actual proposed growth before changing the draft. A failed
    /// operation or an unwinding callback publishes none of the draft.
    pub fn edit<R>(
        parts: &mut Arc<Self>,
        edit: impl FnOnce(&mut MessagePartsEditor) -> Result<R, AllocationError>,
    ) -> Result<R, AllocationError> {
        let budget = parts.budget.clone();
        let allocation = budget.reserve(crate::allocation::payload_capacity(
            &parts.parts,
            std::mem::size_of::<Self>() + 16 + std::mem::size_of_val(parts.as_slice()),
        ))?;
        let mut editor = MessagePartsEditor(Self {
            parts: parts.parts.to_vec(),
            allocation,
            edits: Vec::new(),
            budget,
        });
        let result = edit(&mut editor)?;
        *parts = Arc::new(editor.0);
        Ok(result)
    }
}
impl From<Vec<Part>> for MessageParts {
    fn from(parts: Vec<Part>) -> Self {
        Self::new(parts)
    }
}
impl std::ops::Deref for MessageParts {
    type Target = Vec<Part>;
    fn deref(&self) -> &Self::Target {
        &self.parts
    }
}
impl AsRef<[Part]> for MessageParts {
    fn as_ref(&self) -> &[Part] {
        &self.parts
    }
}

impl PartialEq for MessageParts {
    fn eq(&self, other: &Self) -> bool {
        self.parts == other.parts
    }
}

impl<'a> IntoIterator for &'a MessageParts {
    type Item = &'a Part;
    type IntoIter = std::slice::Iter<'a, Part>;
    fn into_iter(self) -> Self::IntoIter {
        self.parts.iter()
    }
}

/// Mutable private message parts. Read access is immutable; each operation
/// admits its own proposed allocation before it can grow or rewrite a field.
pub struct MessagePartsEditor(MessageParts);
impl MessagePartsEditor {
    pub fn len(&self) -> usize {
        self.0.parts.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.parts.is_empty()
    }
    fn reserve(&mut self, bytes: u64) -> Result<(), AllocationError> {
        // Per-operation slack covers amortized growth of the hold vector,
        // including its old and new backing during a reallocation.
        let hold = self.0.budget.reserve(bytes.saturating_add(128))?;
        self.0.edits.push(hold);
        Ok(())
    }
    pub fn push(&mut self, part: &Part) -> Result<(), AllocationError> {
        let slots = if self.0.parts.len() == self.0.parts.capacity() {
            self.0.parts.capacity().saturating_mul(2).max(4)
        } else {
            0
        };
        self.reserve(crate::allocation::payload_capacity(
            part,
            slots.saturating_mul(std::mem::size_of::<Part>()),
        ))?;
        self.0.parts.push(part.clone());
        Ok(())
    }
    pub fn replace(&mut self, index: usize, part: &Part) -> Result<bool, AllocationError> {
        if index >= self.len() {
            return Ok(false);
        }
        self.reserve(crate::allocation::payload_capacity(part, 0))?;
        self.0.parts[index] = part.clone();
        Ok(true)
    }
    pub fn omit_attachments(
        &mut self,
        index: usize,
        placeholder: &str,
    ) -> Result<bool, AllocationError> {
        let Some(part) = self.0.parts.get(index) else {
            return Ok(false);
        };
        let count = part.tool_result_content().map_or_else(
            || usize::from(part.attachment().is_some()),
            |blocks| {
                blocks
                    .iter()
                    .filter(|block| block.attachment().is_some())
                    .count()
            },
        );
        if count == 0 {
            return Ok(false);
        }
        self.reserve((placeholder.len() as u64 + 32).saturating_mul(count as u64))?;
        Ok(omit_part_attachments(&mut self.0.parts[index], placeholder))
    }
    /// Flatten only under admission, then elide when the producer's borrowed
    /// content predicate says to. No mutable source escapes the editor.
    pub fn elide_if(
        &mut self,
        index: usize,
        keep_chars: usize,
        placeholder: &str,
        should_elide: impl FnOnce(&str) -> bool,
    ) -> Result<bool, AllocationError> {
        let Some(part) = self.0.parts.get(index) else {
            return Ok(false);
        };
        let bound = crate::allocation::payload_capacity(
            part,
            std::mem::size_of::<crate::ModelToolReturnPart>(),
        )
        .saturating_add((placeholder.len() as u64).saturating_mul(4));
        self.reserve(bound)?;
        let part = &mut self.0.parts[index];
        let mut text = part.content().into_owned();
        if !should_elide(&text) {
            return Ok(false);
        }
        if let Some((index, _)) = text.char_indices().nth(keep_chars) {
            text.truncate(index);
        }
        text.push_str(placeholder);
        match part.tool_result_content_mut() {
            Some(blocks) => *blocks = vec![crate::ModelToolReturnPart::text(text)],
            None => {
                if let Some(content) = part.content_mut() {
                    *content = text;
                }
            }
        }
        Ok(true)
    }
}

/// Replace a part's attachments in place, preserving their occurrence order.
/// Independently owned parts use this directly; retained parts use the editor.
pub fn omit_part_attachments(part: &mut Part, placeholder: &str) -> bool {
    if let Some(blocks) = part.tool_result_content_mut() {
        let mut changed = false;
        for block in blocks.iter_mut() {
            if block.attachment().is_some() {
                *block = crate::ModelToolReturnPart::text(placeholder);
                changed = true;
            }
        }
        return changed;
    }
    if !matches!(part.kind(), super::PartKind::Attachment) || part.attachment().is_none() {
        return false;
    }
    if let Some(slot) = part.attachment_mut() {
        *slot = None;
    }
    if let Some(content) = part.content_mut() {
        *content = placeholder.to_string();
    }
    true
}
