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
    budget: AllocationBudget,
}

impl MessageParts {
    /// Independently constructed input; owner adoption uses `clone_in`.
    pub fn new(parts: Vec<Part>) -> Self {
        Self {
            parts,
            allocation: AllocationHold::default(),
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

    /// Transfer a decode's preallocation obligation into its fresh parts.
    pub fn decoded_in(&mut self, allocation: AllocationHold, budget: AllocationBudget) {
        self.allocation = allocation;
        self.budget = budget;
    }

    /// Reserve the complete private rewrite before copying or calling `edit`.
    /// `additional_bytes` bounds new field allocation (including temporary
    /// growth); `additional_parts` bounds new vector slots. The callback must
    /// not retain mutable references or exceed those declared growth bounds.
    /// Readers keep their original parts and obligations on refusal or unwind.
    pub fn edit<R>(
        parts: &mut Arc<Self>,
        additional_bytes: u64,
        additional_parts: usize,
        edit: impl FnOnce(&mut Vec<Part>) -> R,
    ) -> Result<R, AllocationError> {
        let budget = parts.budget.clone();
        let slots = parts.len().saturating_add(additional_parts);
        let allocation = budget.reserve(
            crate::allocation::payload_capacity(
                &parts.parts,
                std::mem::size_of::<Self>()
                    + 16
                    + slots.saturating_mul(std::mem::size_of::<Part>()),
            )
            .saturating_add(additional_bytes),
        )?;
        let mut next = Vec::with_capacity(slots);
        next.extend(parts.iter().cloned());
        let result = edit(&mut next);
        *parts = Arc::new(Self {
            parts: next,
            allocation,
            budget,
        });
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
