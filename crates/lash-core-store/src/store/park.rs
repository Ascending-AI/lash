//! The parked state of a driver-run turn (FIG-3586, FIG-3600, FIG-3659).
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
//!
//! Every transition the record takes — the first park, a different turn
//! superseding it, and each clear — is appended to the store's turn park
//! feed in the same transaction, so a host can list parked turns and follow
//! their transitions durably (FIG-3659). A same-turn re-park writes no event;
//! it bumps `attempts` and `last_refused_ms` while keeping `since_ms` and
//! `park_id`.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};

use crate::{RuntimeError, RuntimeErrorCode, SessionId, TurnId};

/// The durable identity of one park: the feed sequence of the `Parked` event
/// that opened it. A same-turn re-park keeps it; a superseding park mints a
/// new one. The operator verbs take it as their CAS token.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct ParkId(u64);

impl ParkId {
    /// Construct from the feed sequence the issuing store allocated.
    #[must_use]
    pub fn from_feed_sequence(sequence: u64) -> Self {
        Self(sequence)
    }

    /// The feed sequence this park opened with.
    #[must_use]
    pub fn feed_sequence(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for ParkId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// What a caller asks the store to record: a park is written by the aborting
/// turn's own path, which knows the session, the turn, the refusal and the
/// wall clock. The store allocates `park_id`, `since_ms`, `last_refused_ms`
/// and `attempts`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnParkWrite {
    /// The session whose turn parked.
    pub session_id: SessionId,
    /// The turn that parked.
    pub turn_id: TurnId,
    /// Why it parked.
    pub reason: ParkReason,
    /// Host-clock epoch milliseconds at which the refusal was recorded.
    pub at_ms: u64,
}

/// The stored parked state of one session's turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnPark {
    /// The session whose turn parked.
    pub session_id: SessionId,
    /// The turn that parked: a redrive of this turn is what resumes it.
    pub turn_id: TurnId,
    /// Why it parked.
    pub reason: ParkReason,
    /// The feed sequence of the `Parked` event that opened this park.
    pub park_id: ParkId,
    /// Host-clock epoch milliseconds of the first park of this turn; kept
    /// across same-turn re-parks and reset only when a different turn parks.
    pub since_ms: u64,
    /// Host-clock epoch milliseconds of the most recent refusal.
    pub last_refused_ms: u64,
    /// Refusals of this turn since it parked (1 on the first park).
    pub attempts: u32,
}

/// Why a turn parked. Each arm carries the refusal's operator-facing message:
/// what the journal holds, who wrote it, and the remedies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ParkReason {
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
    /// The turn was in flight when its session's state generation left the
    /// range this build admits (FIG-3571, FIG-3735): the build that runs the
    /// redrive refuses the session at admission, before any effect, and the
    /// turn holds its claims until a build of the generation that wrote them
    /// redrives it.
    SessionStateGenerationRefused {
        /// The generation the session's marker holds.
        found: u32,
        /// The generation this build admits.
        current: u32,
        /// The refusal message, naming both generations.
        message: String,
    },
}

/// The reason a park carries, as a plain code: the metric label and the query
/// filter the free-form [`ParkReason`] payload is projected onto.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ParkReasonCode {
    /// See [`ParkReason::ReplayDivergence`].
    ReplayDivergence,
    /// See [`ParkReason::KeyFormatCutover`].
    KeyFormatCutover,
    /// See [`ParkReason::BindingDrift`].
    BindingDrift,
    /// See [`ParkReason::EffectReplayDivergence`].
    EffectReplayDivergence,
    /// See [`ParkReason::SessionStateGenerationRefused`].
    SessionStateGenerationRefused,
}

impl ParkReasonCode {
    /// Every code, in declaration order. Metrics record each one — zero
    /// included — so a cleared reason drops to 0 instead of going stale.
    pub const ALL: &[Self] = &[
        Self::ReplayDivergence,
        Self::KeyFormatCutover,
        Self::BindingDrift,
        Self::EffectReplayDivergence,
        Self::SessionStateGenerationRefused,
    ];

    /// The serde tag the matching [`ParkReason`] arm serializes under, and
    /// the `reason_code` column value it is stored and filtered by.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReplayDivergence => "replay_divergence",
            Self::KeyFormatCutover => "key_format_cutover",
            Self::BindingDrift => "binding_drift",
            Self::EffectReplayDivergence => "effect_replay_divergence",
            Self::SessionStateGenerationRefused => "session_state_generation_refused",
        }
    }

    /// Parse a stored `reason_code`.
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "replay_divergence" => Some(Self::ReplayDivergence),
            "key_format_cutover" => Some(Self::KeyFormatCutover),
            "binding_drift" => Some(Self::BindingDrift),
            "effect_replay_divergence" => Some(Self::EffectReplayDivergence),
            "session_state_generation_refused" => Some(Self::SessionStateGenerationRefused),
            _ => None,
        }
    }
}

impl ParkReason {
    /// The park of an in-flight turn whose session the generation gate
    /// refused ([`ParkReason::SessionStateGenerationRefused`]).
    #[must_use]
    pub fn session_state_generation_refused(
        refusal: crate::runtime_error::SessionStateVersionRefusal,
    ) -> Self {
        let crate::runtime_error::SessionStateVersionRefusal { found, current } = refusal;
        Self::SessionStateGenerationRefused {
            found,
            current,
            message: format!(
                "the turn was in flight on session-state generation {found}, and this build \
                 admits only generation {current}: its redrive was refused before any effect; \
                 redrive it under a build of generation {found}, cancel it, or fork from before it"
            ),
        }
    }

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
            RuntimeErrorCode::EffectReplayDivergence
            | RuntimeErrorCode::SqliteEffectReplayHashConflict => {
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

    /// The reason's code: the stored `reason_code` and the metric label.
    #[must_use]
    pub fn code(&self) -> ParkReasonCode {
        match self {
            Self::ReplayDivergence { .. } => ParkReasonCode::ReplayDivergence,
            Self::KeyFormatCutover { .. } => ParkReasonCode::KeyFormatCutover,
            Self::BindingDrift { .. } => ParkReasonCode::BindingDrift,
            Self::EffectReplayDivergence { .. } => ParkReasonCode::EffectReplayDivergence,
            Self::SessionStateGenerationRefused { .. } => {
                ParkReasonCode::SessionStateGenerationRefused
            }
        }
    }

    /// The diverged effect kind an [`EffectReplayDivergence`](Self::EffectReplayDivergence)
    /// names, when the reason carries one.
    #[must_use]
    pub fn effect_kind(&self) -> Option<&str> {
        match self {
            Self::EffectReplayDivergence { effect_kind, .. } => Some(effect_kind.as_str()),
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
            | Self::EffectReplayDivergence { message, .. }
            | Self::SessionStateGenerationRefused { message, .. } => message,
        }
    }
}

/// What ended a park without cancelling the work it held.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum UnparkCause {
    /// The parked turn's own commit settled it.
    TurnCommitted,
    /// The queued run the parked turn belonged to settled.
    RunSettled,
    /// A different turn parked over it in the same session.
    Superseded,
}

/// What ended a park by cancelling the work it held.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ParkCancelCause {
    /// An input withdrawal released the parked turn's last held work.
    InputWithdrawn,
    /// The session was deleted, single or batch (NOW-B adds the process
    /// causes).
    SessionDeleted,
}

/// One entry of a store's turn park feed: the durable transition ledger every
/// park write and every park clear appends to in the same transaction
/// (FIG-3659).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TurnParkEventKind {
    /// A park opened: `reason` is what the turn refused with.
    Parked {
        /// Why the turn parked.
        reason: ParkReason,
    },
    /// A park ended without cancelling the turn's held work.
    Unparked {
        /// What settled the park.
        cause: UnparkCause,
    },
    /// A park ended because the work it held was cancelled.
    Cancelled {
        /// What cancelled the park.
        cause: ParkCancelCause,
    },
}

impl UnparkCause {
    /// The stored `cause` column value: the serde tag for a unit cause, its
    /// JSON body for a payload-carrying one (NOW-B).
    pub fn encode(&self) -> String {
        match self {
            Self::TurnCommitted => "turn_committed".to_string(),
            Self::RunSettled => "run_settled".to_string(),
            Self::Superseded => "superseded".to_string(),
        }
    }
}

impl ParkCancelCause {
    /// The stored `cause` column value; see [`UnparkCause::encode`].
    pub fn encode(&self) -> String {
        match self {
            Self::InputWithdrawn => "input_withdrawn".to_string(),
            Self::SessionDeleted => "session_deleted".to_string(),
        }
    }
}

impl TurnParkEventKind {
    /// The stored `kind` column value.
    #[must_use]
    pub fn kind_code(&self) -> &'static str {
        match self {
            Self::Parked { .. } => "parked",
            Self::Unparked { .. } => "unparked",
            Self::Cancelled { .. } => "cancelled",
        }
    }

    /// The `(cause, reason_json)` column pair this kind stores: a `Parked`
    /// carries its reason and no cause; the closing kinds carry a cause and
    /// no reason.
    pub fn encode_columns(&self) -> (Option<String>, Option<String>) {
        match self {
            Self::Parked { reason } => (
                None,
                Some(serde_json::to_string(reason).unwrap_or_default()),
            ),
            Self::Unparked { cause } => (Some(cause.encode()), None),
            Self::Cancelled { cause } => (Some(cause.encode()), None),
        }
    }

    /// Decode a feed row's `(kind, cause, reason_json)` columns.
    ///
    /// # Errors
    /// When the combination is not one the writer produces: a `parked` row
    /// with an unreadable reason, or a closing row whose cause decodes to
    /// neither [`UnparkCause`] nor [`ParkCancelCause`].
    pub fn decode_columns(
        kind: &str,
        cause: Option<&str>,
        reason_json: Option<&str>,
    ) -> Result<Self, crate::StoreError> {
        let corrupt = |message: String| crate::StoreError::StoredDataCorrupt {
            record_kind: "TurnParkEvent",
            message,
        };
        match kind {
            "parked" => {
                let reason: ParkReason = serde_json::from_str(reason_json.unwrap_or_default())
                    .map_err(|error| {
                        corrupt(format!("parked event reason is unreadable: {error}"))
                    })?;
                Ok(Self::Parked { reason })
            }
            "unparked" => {
                let cause =
                    cause.ok_or_else(|| corrupt("unparked event carries no cause".to_string()))?;
                Ok(Self::Unparked {
                    cause: decode_unpark_cause(cause)?,
                })
            }
            "cancelled" => {
                let cause =
                    cause.ok_or_else(|| corrupt("cancelled event carries no cause".to_string()))?;
                Ok(Self::Cancelled {
                    cause: decode_cancel_cause(cause)?,
                })
            }
            other => Err(corrupt(format!(
                "turn park event kind `{other}` is unknown"
            ))),
        }
    }
}

fn decode_unpark_cause(cause: &str) -> Result<UnparkCause, crate::StoreError> {
    let decoded = match cause {
        "turn_committed" => Some(UnparkCause::TurnCommitted),
        "run_settled" => Some(UnparkCause::RunSettled),
        "superseded" => Some(UnparkCause::Superseded),
        _ => None,
    };
    decoded.ok_or_else(|| crate::StoreError::StoredDataCorrupt {
        record_kind: "TurnParkEvent",
        message: format!("unparked event cause `{cause}` is unknown"),
    })
}

fn decode_cancel_cause(cause: &str) -> Result<ParkCancelCause, crate::StoreError> {
    let decoded = match cause {
        "input_withdrawn" => Some(ParkCancelCause::InputWithdrawn),
        "session_deleted" => Some(ParkCancelCause::SessionDeleted),
        _ => None,
    };
    decoded.ok_or_else(|| crate::StoreError::StoredDataCorrupt {
        record_kind: "TurnParkEvent",
        message: format!("cancelled event cause `{cause}` is unknown"),
    })
}

/// One feed row. `seq` is the store's clock sequence; `park_id` is the park
/// the transition applies to (for `Unparked`/`Cancelled` it names the park
/// that closed, which is already gone from `turn_parks`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnParkFeedEvent {
    /// The store clock sequence this event was allocated.
    pub seq: u64,
    /// Host-clock epoch milliseconds the transition happened.
    pub at_ms: u64,
    /// The session the transitioned park belonged to.
    pub session_id: SessionId,
    /// The turn the transitioned park belonged to.
    pub turn_id: TurnId,
    /// The park the transition applies to.
    pub park_id: ParkId,
    /// The transition.
    pub kind: TurnParkEventKind,
}

/// Opaque position in one store's turn park feed.
///
/// The wrapped sequence is meaningful only to the backend that issued it and
/// is not comparable across stores. Backends expose constructors/accessors so
/// store implementations can persist and bind the position; consumers treat
/// values as cursors, never as timestamps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnParkFeedCursor(u64);

impl TurnParkFeedCursor {
    /// The feed's first position: every event follows it.
    #[must_use]
    pub fn initial() -> Self {
        Self(0)
    }

    /// Bind the backend-defined position `sequence`.
    #[must_use]
    pub fn from_store_sequence(sequence: u64) -> Self {
        Self(sequence)
    }

    /// The opaque sequence for the store implementor that issued it.
    #[must_use]
    pub fn store_sequence(self) -> u64 {
        self.0
    }
}

/// One page of the turn park feed: the events strictly after the cursor the
/// caller passed, and the cursor that resumes after them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnParkFeedPage {
    /// Events in commit order (`seq` ascending).
    pub events: Vec<TurnParkFeedEvent>,
    /// Resume position: the last event's sequence, or the caller's cursor
    /// when the page is empty.
    pub next: TurnParkFeedCursor,
}

/// The filter a `list_turn_parks` read applies.
#[derive(Clone, Debug)]
pub struct TurnParkQuery {
    /// Restrict to these reason codes; `None` (or an empty set) means all.
    pub reasons: Option<BTreeSet<ParkReasonCode>>,
    /// Restrict to one session.
    pub session: Option<SessionId>,
    /// Age filter: only parks whose `since_ms` is at or before this instant.
    pub parked_at_or_before_ms: Option<u64>,
    /// Keyset: rows strictly after `(since_ms, session_id)` in the
    /// `(since_ms, session_id)` ordering.
    pub after: Option<(u64, SessionId)>,
    /// Page size.
    pub limit: NonZeroUsize,
}

impl TurnPark {
    /// Decode a stored park row, checking that the stored `reason_code`
    /// agrees with the decoded reason's [`ParkReason::code`].
    ///
    /// # Errors
    /// When the stored reason is not a reason this build reads, or the stored
    /// code disagrees with it.
    #[allow(clippy::too_many_arguments)]
    pub fn decode(
        session_id: SessionId,
        turn_id: TurnId,
        park_id: ParkId,
        reason_code: &str,
        reason_json: &str,
        since_ms: u64,
        last_refused_ms: u64,
        attempts: u32,
    ) -> Result<Self, crate::StoreError> {
        let reason: ParkReason = serde_json::from_str(reason_json).map_err(|error| {
            crate::StoreError::StoredDataCorrupt {
                record_kind: "TurnPark",
                message: format!(
                    "stored turn park reason for session `{session_id}` is unreadable: {error}"
                ),
            }
        })?;
        if reason.code().as_str() != reason_code {
            return Err(crate::StoreError::StoredDataCorrupt {
                record_kind: "TurnPark",
                message: format!(
                    "stored turn park for session `{session_id}` names reason code \
                     `{reason_code}` but its reason decodes as `{}`",
                    reason.code().as_str()
                ),
            });
        }
        Ok(Self {
            session_id,
            turn_id,
            reason,
            park_id,
            since_ms,
            last_refused_ms,
            attempts,
        })
    }
}

/// The turns of a deployment that are not settled yet, for drain.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsettledTurnCounts {
    /// Sessions whose turn is parked ([`TurnPark`]).
    pub parked_turns: usize,
    /// Sessions with a turn in flight: a pending queued run, a claimed turn
    /// input that is not settled, or a parked turn. Parked turns are
    /// in-flight turns too — they hold their claims — so this is never less
    /// than [`parked_turns`](Self::parked_turns).
    pub in_flight_turns: usize,
    /// The oldest live park's `since_ms`: the first park of the oldest
    /// still-parked turn, `None` when nothing is parked.
    pub oldest_parked_since_ms: Option<u64>,
    /// Live parks per reason code; codes with no park are absent.
    pub parked_by_reason: BTreeMap<ParkReasonCode, usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `effect_replay_divergence` — and the substrate hash-conflict code it
    /// replaced — must map to a parking reason: without the arm, a Restate
    /// effect-journal divergence aborts instead of parking (FIG-3587).
    #[test]
    fn effect_replay_divergence_errors_park() {
        for code in [
            RuntimeErrorCode::EffectReplayDivergence,
            RuntimeErrorCode::SqliteEffectReplayHashConflict,
        ] {
            let mut error = RuntimeError::new(code.clone(), "the recorded envelope diverged");
            error.summary = Some(Box::new(crate::RuntimeEffectReplayMismatchReport {
                divergent_path_count: 1,
                first_divergent_paths: vec!["$.args".to_string()],
                effect_kind: Some("llm_call".to_string()),
            }));
            let Some(ParkReason::EffectReplayDivergence { effect_kind, .. }) =
                ParkReason::of_error(&error)
            else {
                panic!("{code:?} must park as an effect replay divergence");
            };
            assert_eq!(effect_kind, "llm_call");

            let bare = RuntimeError::new(code.clone(), "the recorded envelope diverged");
            assert_eq!(
                ParkReason::of_error(&bare),
                Some(ParkReason::EffectReplayDivergence {
                    effect_kind: "unknown".to_string(),
                    message: "the recorded envelope diverged".to_string(),
                }),
                "a summary-less {code:?} still parks"
            );
        }
    }
}
