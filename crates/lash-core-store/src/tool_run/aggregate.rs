//! Run-owned aggregate membership. Consumer modes choose how far to observe
//! the same recorded settlements; they do not change admission or identity.

use std::collections::BTreeSet;

use lash_sansio::ToolCallId;
use serde::{Deserialize, Serialize};

use super::RunEventRefusal;

/// One unique operation, or an already settled operand whose value stays
/// with the program. Aliases refer to its index in `AggregatePlan::leaves`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "leaf", rename_all = "snake_case", deny_unknown_fields)]
pub enum AggregateLeaf {
    Call {
        call_id: ToolCallId,
    },
    Timer {
        duration_ms: u64,
    },
    Settled {
        fulfilled: bool,
    },
    /// A source request refused before an executable can be bound. Its
    /// canonical admission input stays in this plan; no body is owed.
    Refused {
        input: serde_json::Value,
    },
}

/// The source positions and unique operations admitted by an aggregate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AggregatePlan {
    pub key: String,
    pub leaves: Vec<AggregateLeaf>,
    pub operands: Vec<u32>,
}

impl AggregatePlan {
    /// Every leaf appears, every operand names a leaf, and a call appears
    /// only once among unique leaves. Duplicate operands are aliases.
    ///
    /// # Errors
    /// A typed refusal of the aggregate's mapping.
    pub fn validate(&self) -> Result<(), RunEventRefusal> {
        let used: BTreeSet<_> = self.operands.iter().map(|index| *index as usize).collect();
        let mut calls = BTreeSet::new();
        if self.key.is_empty()
            || used != (0..self.leaves.len()).collect()
            || self.leaves.iter().any(
                |leaf| matches!(leaf, AggregateLeaf::Call { call_id } if !calls.insert(call_id)),
            )
        {
            return Err(RunEventRefusal::AggregateShape {
                key: self.key.clone(),
            });
        }
        Ok(())
    }
}

/// A consumer over the recorded settlement order. ListBatch waits for all
/// operands and reports the first rejection in written source order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateConsumer {
    Race,
    Any,
    All,
    AllSettled,
    ListBatch,
}

impl AggregateConsumer {
    #[must_use]
    pub const fn wake(self) -> lash_sansio::RunAggregateWakePolicy {
        match self {
            Self::Race => lash_sansio::RunAggregateWakePolicy::First,
            Self::Any => lash_sansio::RunAggregateWakePolicy::FirstSuccess,
            Self::All | Self::AllSettled | Self::ListBatch => {
                lash_sansio::RunAggregateWakePolicy::All
            }
        }
    }
}
