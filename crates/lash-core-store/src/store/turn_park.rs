//! The parked state of a driver-run turn (FIG-3586, FIG-3600).
//!
//! A turn *parks* when it aborts on a refusal no redrive of the same build can
//! get past and no live retry may paper over: its journal cannot be replayed
//! by the code now running it. Parking is neither failing (nothing is settled,
//! so the turn's claims stay held and a redrive under the right build finishes
//! it) nor retrying live (a live retry would re-issue effects the journal
//! already holds).
//!
//! One record per session: a session drives one turn at a time, and a parked
//! turn holds the claims that would start the next, so a second park in the
//! same session replaces the first only when the same turn parks again. The
//! record is generic over why the turn parked; the operator verbs (redrive,
//! cancel, fork) act on the turn it names, and `drain_status` counts it.

use serde::{Deserialize, Serialize};

use crate::{RuntimeError, RuntimeErrorCode, SessionId, TurnId};

/// The parked state of one session's turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnPark {
    /// The session whose turn parked.
    pub session_id: SessionId,
    /// The turn that parked: a redrive of this turn is what resumes it.
    pub turn_id: TurnId,
    /// Why it parked.
    pub reason: TurnParkReason,
    /// Host-clock epoch milliseconds at which the turn parked.
    pub parked_at_ms: u64,
}

/// Why a turn parked. Each arm carries the refusal's operator-facing message:
/// what the journal holds, who wrote it, and the remedies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TurnParkReason {
    /// A code cell's re-execution issued a command its journal does not hold
    /// where it was issued: the running build is not the one that wrote the
    /// journal.
    ReplayDivergence {
        /// The refusal message.
        message: String,
    },
    /// A code cell's journal was written under a replay-key grammar this
    /// build does not mint.
    KeyFormatCutover {
        /// The refusal message.
        message: String,
    },
    /// A code cell needed a host tool binding its journal names, and the live
    /// tool for it is missing or changed since the pass that wrote the
    /// journal (FIG-3587).
    BindingDrift {
        /// The refusal message, naming the binding and how it drifted.
        message: String,
    },
    /// A recorded effect's envelope no longer matches the one the redrive
    /// reconstructs — a model call built from another prompt surface, a tool
    /// attempt with other arguments — and serving its recorded outcome would
    /// answer a different request (FIG-3587). Any recorded effect's replay
    /// hash conflict parks instead of failing on every redrive.
    EffectReplayDivergence {
        /// The diverged effect's kind (its command `type`, e.g. `llm_call`),
        /// or `unknown` when the substrate did not name it.
        effect_kind: String,
        /// The refusal message, with the divergent envelope paths.
        message: String,
    },
}

impl TurnParkReason {
    /// The park reason `error` carries, when it is a refusal that parks the
    /// turn ([`RuntimeErrorCode::parks_turn`]).
    #[must_use]
    pub fn of_error(error: &RuntimeError) -> Option<Self> {
        let message = error.message.clone();
        match error.code {
            RuntimeErrorCode::LashlangCellReplayDivergence => {
                Some(Self::ReplayDivergence { message })
            }
            RuntimeErrorCode::LashlangCellReplayKeyFormatCutover => {
                Some(Self::KeyFormatCutover { message })
            }
            RuntimeErrorCode::LashlangCellBindingDrift => Some(Self::BindingDrift { message }),
            RuntimeErrorCode::SqliteEffectReplayHashConflict
            | RuntimeErrorCode::PostgresEffectReplayHashConflict => {
                Some(Self::EffectReplayDivergence {
                    effect_kind: error
                        .summary
                        .as_ref()
                        .and_then(|summary| summary.effect_kind.clone())
                        .unwrap_or_else(|| "unknown".to_string()),
                    message,
                })
            }
            _ => None,
        }
    }

    /// The refusal message.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::ReplayDivergence { message }
            | Self::KeyFormatCutover { message }
            | Self::BindingDrift { message }
            | Self::EffectReplayDivergence { message, .. } => message,
        }
    }
}

impl TurnPark {
    /// Encode the reason for its storage column.
    ///
    /// # Errors
    /// When the reason fails to serialize.
    pub fn encode_reason(&self) -> Result<String, crate::StoreError> {
        serde_json::to_string(&self.reason).map_err(|error| {
            crate::StoreError::Backend(format!("turn park reason encode failed: {error}"))
        })
    }

    /// Decode a stored park row.
    ///
    /// # Errors
    /// When the stored reason is not a reason this build reads.
    pub fn decode(
        session_id: SessionId,
        turn_id: TurnId,
        reason_json: &str,
        parked_at_ms: u64,
    ) -> Result<Self, crate::StoreError> {
        let reason = serde_json::from_str(reason_json).map_err(|error| {
            crate::StoreError::Backend(format!(
                "stored turn park reason for session `{session_id}` is unreadable: {error}"
            ))
        })?;
        Ok(Self {
            session_id,
            turn_id,
            reason,
            parked_at_ms,
        })
    }
}

/// The turns of a deployment that are not settled yet, for drain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsettledTurnCounts {
    /// Sessions whose turn is parked ([`TurnPark`]).
    pub parked_turns: usize,
    /// Sessions with a turn in flight: a pending queued run, a claimed turn
    /// input that is not settled, or a parked turn. Parked turns are
    /// in-flight turns too — they hold their claims — so this is never less
    /// than [`parked_turns`](Self::parked_turns).
    pub in_flight_turns: usize,
}
