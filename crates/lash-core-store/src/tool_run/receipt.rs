//! K7: logical business receipts and observation permits (FIG-4830
//! implements them).
//!
//! A call has at most one accepted and one terminal business receipt, both
//! under its original [`ToolCallId`], across replay, retries, routes and
//! handover. A Deferred park is pending, not a terminal. Observations are
//! minted from recorded events only: an attempt observation needs a
//! recorded attempt, and an observer cannot mint a permit itself.

use lash_sansio::ToolCallId;
use serde::{Deserialize, Serialize};

use super::run_event::{AttemptOrdinal, CallDecision, RunEvent, RunEventOrdinal};

/// A logical call's terminal, as its business receipt states it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogicalTerminal {
    Final,
    Denied,
    Cancelled,
    Aborted,
}

/// One business receipt of a logical call.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "receipt", rename_all = "snake_case", deny_unknown_fields)]
pub enum BusinessReceipt {
    Accepted {
        call_id: ToolCallId,
    },
    Terminal {
        call_id: ToolCallId,
        terminal: LogicalTerminal,
    },
}

impl BusinessReceipt {
    /// The receipts a recorded event emits.
    #[must_use]
    pub fn for_event(event: &RunEvent) -> Vec<Self> {
        match event {
            RunEvent::Admitted { round } => round
                .members
                .iter()
                .map(|member| Self::Accepted {
                    call_id: member.call_id.clone(),
                })
                .collect(),
            RunEvent::Decided {
                call_id, decision, ..
            } => vec![Self::Terminal {
                call_id: call_id.clone(),
                terminal: match decision {
                    CallDecision::Final { .. } => LogicalTerminal::Final,
                    CallDecision::Denied => LogicalTerminal::Denied,
                    CallDecision::CheckCancelled | CallDecision::Cancelled => {
                        LogicalTerminal::Cancelled
                    }
                    CallDecision::Aborted => LogicalTerminal::Aborted,
                },
            }],
            _ => Vec::new(),
        }
    }
}

/// What a permit lets an observer report.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ObservedFact {
    /// One attempt that actually executed and recorded a result.
    Attempt {
        call_id: ToolCallId,
        attempt: AttemptOrdinal,
    },
    /// A logical receipt.
    Logical(BusinessReceipt),
}

/// The permission to emit one observation, minted only from a recorded
/// event at its stable ordinal. It has no public constructor.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ObservationPermit {
    recorded: RunEventOrdinal,
    fact: ObservedFact,
}

impl ObservationPermit {
    /// The permits a recorded event at `recorded` grants.
    #[must_use]
    pub fn for_recorded(recorded: RunEventOrdinal, event: &RunEvent) -> Vec<Self> {
        let attempt = match event {
            RunEvent::AttemptRecorded {
                call_id, attempt, ..
            } => Some(ObservedFact::Attempt {
                call_id: call_id.clone(),
                attempt: *attempt,
            }),
            _ => None,
        };
        attempt
            .into_iter()
            .chain(
                BusinessReceipt::for_event(event)
                    .into_iter()
                    .map(ObservedFact::Logical),
            )
            .map(|fact| Self { recorded, fact })
            .collect()
    }

    #[must_use]
    pub fn recorded(&self) -> RunEventOrdinal {
        self.recorded
    }

    #[must_use]
    pub fn fact(&self) -> &ObservedFact {
        &self.fact
    }
}
