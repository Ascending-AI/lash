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
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
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
    /// The engine's handle on the stopped execution, when the engine parked
    /// the root itself (its retry loop ran out, recorded by reconcile): the
    /// handle a redrive resumes and a cancel releases. `None` from the
    /// execution's own park write, which keeps any handle already stored.
    pub engine: Option<EnginePark>,
}

impl TurnParkWrite {
    /// The park the aborting execution itself writes for `root`: no engine
    /// handle.
    #[must_use]
    pub fn refusal(session_id: SessionId, root: TurnId, reason: ParkReason, at_ms: u64) -> Self {
        Self {
            session_id,
            turn_id: root,
            reason,
            at_ms,
            engine: None,
        }
    }
}

/// The stored park a [`TurnParkWrite`] is decided against: the session's
/// current park, as its transaction read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredTurnParkHead {
    /// The root the stored park names.
    pub root: TurnId,
    /// The engine handle the stored park carries.
    pub engine: Option<EnginePark>,
    /// Whether the stored park's `resume_intent` names a redrive that is
    /// still open (pending, or failed and retryable): its engine half has
    /// not resumed the execution yet.
    pub redrive_open: bool,
    /// Whether the stored park names a `resume_intent` at all.
    pub redrive_requested: bool,
}

/// What a [`TurnParkWrite`] does to the session's park (D2 §1.3), decided in
/// the write's own transaction after the root's terminal evidence was found
/// absent (P2 refuses before this).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnParkWriteDecision {
    /// No park: open one.
    Open,
    /// Another root's park: close it `Superseded`, then open this root's.
    Supersede,
    /// The same root refused again, or its engine stopped again after a
    /// redrive resumed it: keep `park_id` and `since_ms`, refresh the reason,
    /// count the attempt, clear `resume_intent` (P3), and store the write's
    /// engine handle if it carries one.
    Repark,
    /// The engine stopped the root's execution and the root already holds
    /// a park without a handle: keep its reason and attempts, store the
    /// handle (P4, `AttachedToExisting`).
    AttachEngine,
    /// Nothing to write: the park already carries this handle and no
    /// redrive ran since, or a redrive is still on its way to resuming it.
    Unchanged,
}

/// Decide `write` against the session's stored park `stored`.
#[must_use]
pub fn decide_turn_park_write(
    stored: Option<&StoredTurnParkHead>,
    write: &TurnParkWrite,
) -> TurnParkWriteDecision {
    let Some(stored) = stored else {
        return TurnParkWriteDecision::Open;
    };
    if stored.root != write.turn_id {
        return TurnParkWriteDecision::Supersede;
    }
    let Some(engine) = write.engine.as_ref() else {
        // The execution itself refused again.
        return TurnParkWriteDecision::Repark;
    };
    if stored.redrive_open {
        // A redrive owns the stopped execution until it resumes it.
        return TurnParkWriteDecision::Unchanged;
    }
    if stored.redrive_requested {
        // A redrive resumed the execution, and the engine stopped it again
        // without the execution refusing first.
        return TurnParkWriteDecision::Repark;
    }
    match stored.engine.as_ref() {
        None => TurnParkWriteDecision::AttachEngine,
        Some(stored) if stored == engine => TurnParkWriteDecision::Unchanged,
        Some(_) => TurnParkWriteDecision::Repark,
    }
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
    /// The engine's handle on the stopped execution, once reconcile recorded
    /// one: what a redrive resumes and a cancel or fork releases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<EnginePark>,
    /// The redrive requested since this park last refused, if any. A re-park
    /// of the same root clears it (P3), so an operator can redrive again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_intent: Option<super::ControlIntentId>,
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
    /// The turn was admitted under another executable generation than the
    /// one this build runs (FIG-3571): its cells would compile, key or meter
    /// differently from the journal's, so its redrive was refused before any
    /// effect, and it holds its claims until a build of its own generation
    /// redrives it, an operator forks it onto the new generation, or cancels
    /// it. The generation is carried (and projected onto the store's
    /// `park_executable_generation` column) so drain status counts parks per generation.
    RetiredGeneration {
        /// The generation the turn's admission recorded; `None` for an
        /// admission a build without the stamp journaled.
        generation: Option<crate::executable_generation::ExecutableGeneration>,
        /// The refusal message, naming the recorded and the current
        /// generation.
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
    /// The engine ran out of attempts on work that kept failing live and
    /// stopped retrying it, keeping its execution for an operator to resume
    /// (FIG-3675). Nothing was settled: the work holds its claims, and a
    /// resume under a fixed deployment retries it where it stopped.
    EngineRetryExhausted {
        /// The attempts the engine made before it stopped.
        attempts: u32,
        /// The engine's code for the last failure, when it named one.
        last_failure_code: Option<String>,
        /// The last failure as the engine recorded it.
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
    /// See [`ParkReason::RetiredGeneration`].
    RetiredGeneration,
    /// See [`ParkReason::BindingDrift`].
    BindingDrift,
    /// See [`ParkReason::EffectReplayDivergence`].
    EffectReplayDivergence,
    /// See [`ParkReason::SessionStateGenerationRefused`].
    SessionStateGenerationRefused,
    /// See [`ParkReason::EngineRetryExhausted`].
    EngineRetryExhausted,
}

impl ParkReasonCode {
    /// Every code, in declaration order. Metrics record each one — zero
    /// included — so a cleared reason drops to 0 instead of going stale.
    pub const ALL: &[Self] = &[
        Self::ReplayDivergence,
        Self::RetiredGeneration,
        Self::BindingDrift,
        Self::EffectReplayDivergence,
        Self::SessionStateGenerationRefused,
        Self::EngineRetryExhausted,
    ];

    /// The serde tag the matching [`ParkReason`] arm serializes under, and
    /// the `reason_code` column value it is stored and filtered by.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReplayDivergence => "replay_divergence",
            Self::RetiredGeneration => "retired_generation",
            Self::BindingDrift => "binding_drift",
            Self::EffectReplayDivergence => "effect_replay_divergence",
            Self::SessionStateGenerationRefused => "session_state_generation_refused",
            Self::EngineRetryExhausted => "engine_retry_exhausted",
        }
    }

    /// Parse a stored `reason_code`.
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "replay_divergence" => Some(Self::ReplayDivergence),
            "retired_generation" => Some(Self::RetiredGeneration),
            "binding_drift" => Some(Self::BindingDrift),
            "effect_replay_divergence" => Some(Self::EffectReplayDivergence),
            "session_state_generation_refused" => Some(Self::SessionStateGenerationRefused),
            "engine_retry_exhausted" => Some(Self::EngineRetryExhausted),
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

    /// The park of a turn redriven under another executable generation than
    /// its admission recorded ([`ParkReason::RetiredGeneration`]).
    #[must_use]
    pub fn retired_generation(
        refusal: crate::executable_generation::ExecutableGenerationRefusal,
    ) -> Self {
        Self::of_retired_generation(&RuntimeError::retired_generation(refusal))
    }

    /// The park of a process incarnation redriven under another executable
    /// generation than its start recorded ([`ParkReason::RetiredGeneration`]).
    #[must_use]
    pub fn retired_process_generation(
        refusal: crate::executable_generation::ExecutableGenerationRefusal,
    ) -> Self {
        Self::of_retired_generation(&RuntimeError::retired_process_generation(refusal))
    }

    /// The park a [`RuntimeErrorCode::RetiredGeneration`] refusal carries: the
    /// refusal's message, and the generation its admission recorded. An error
    /// read back from storage no longer carries the generation.
    fn of_retired_generation(error: &RuntimeError) -> Self {
        Self::RetiredGeneration {
            generation: error
                .executable_generation_refusal()
                .and_then(|refusal| refusal.found.clone()),
            message: error.message.clone(),
        }
    }

    /// The retired generation this park counts under, when it is a
    /// [`RetiredGeneration`](Self::RetiredGeneration) park that names one:
    /// what the store projects onto its `park_executable_generation` column.
    #[must_use]
    pub fn retired_executable_generation_key(&self) -> Option<&str> {
        match self {
            Self::RetiredGeneration {
                generation: Some(generation),
                ..
            } => Some(generation.as_str()),
            _ => None,
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
            RuntimeErrorCode::RetiredGeneration => Some(Self::of_retired_generation(error)),
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
            Self::RetiredGeneration { .. } => ParkReasonCode::RetiredGeneration,
            Self::BindingDrift { .. } => ParkReasonCode::BindingDrift,
            Self::EffectReplayDivergence { .. } => ParkReasonCode::EffectReplayDivergence,
            Self::SessionStateGenerationRefused { .. } => {
                ParkReasonCode::SessionStateGenerationRefused
            }
            Self::EngineRetryExhausted { .. } => ParkReasonCode::EngineRetryExhausted,
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
            | Self::RetiredGeneration { message, .. }
            | Self::BindingDrift { message }
            | Self::EffectReplayDivergence { message, .. }
            | Self::SessionStateGenerationRefused { message, .. }
            | Self::EngineRetryExhausted { message, .. } => message,
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
    /// The parked process appended a lifecycle fact past its refusal: a rerun
    /// got past replay and made progress (NOW-B).
    ProcessProgressed,
    /// The parked process reached a terminal status other than `Cancelled`.
    ProcessTerminal {
        /// The terminal status it reached.
        status: crate::ProcessStatus,
    },
}

/// What ended a park by cancelling the work it held.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ParkCancelCause {
    /// An input withdrawal released the parked turn's last held work.
    InputWithdrawn,
    /// The session was deleted, single or batch.
    SessionDeleted,
    /// The parked process reached its terminal `Cancelled` status.
    ProcessCancelled {
        /// The origin of the cancel request it recorded, when it recorded one.
        origin: Option<lash_sansio::CancelOrigin>,
    },
    /// An operator cancelled the parked root (its `Cancel` intent).
    Operator {
        /// The intent that cancelled it.
        intent: super::ControlIntentId,
    },
    /// An operator forked the parked root: its held inputs now drive
    /// `new_root` (`None` for a queued root, whose next admission mints it).
    Forked {
        /// The intent that forked it.
        intent: super::ControlIntentId,
        /// The root the held inputs drive under.
        new_root: Option<TurnId>,
    },
}

/// One transition of a park, as both park feeds record it (FIG-3659): the
/// durable transition ledger every park write and every park clear appends
/// to in the same transaction, for turns and processes alike.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ParkEventKind {
    /// A park opened: `reason` is what the work refused with.
    Parked {
        /// Why the work parked.
        reason: ParkReason,
    },
    /// A park ended without cancelling the work it held.
    Unparked {
        /// What settled the park.
        cause: UnparkCause,
    },
    /// A park ended because the work it held was cancelled.
    Cancelled {
        /// What cancelled the park.
        cause: ParkCancelCause,
    },
    /// An operator asked to redrive the parked root; the park stays until
    /// the resumed root commits or parks again.
    RedriveRequested {
        /// The redrive's intent.
        intent: super::ControlIntentId,
    },
}

/// The stored `cause` column value: the serde tag for a unit cause, its JSON
/// body for a payload-carrying one.
fn encode_cause<T: Serialize>(cause: &T) -> String {
    let value = serde_json::to_value(cause).unwrap_or_default();
    match value.as_object() {
        Some(object) if object.len() == 1 => object
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| value.to_string(), str::to_string),
        _ => value.to_string(),
    }
}

/// Decode a stored `cause` column: a bare tag is a unit cause, anything else
/// is the JSON body of a payload-carrying one.
fn decode_cause<T: serde::de::DeserializeOwned>(cause: &str) -> Option<T> {
    if cause.starts_with('{') {
        serde_json::from_str(cause).ok()
    } else {
        serde_json::from_value(serde_json::json!({ "type": cause })).ok()
    }
}

impl UnparkCause {
    /// The stored `cause` column value: the serde tag for a unit cause, its
    /// JSON body for a payload-carrying one.
    pub fn encode(&self) -> String {
        encode_cause(self)
    }
}

impl ParkCancelCause {
    /// The stored `cause` column value; see [`UnparkCause::encode`]. A
    /// payload-carrying cause stores its JSON body.
    pub fn encode(&self) -> String {
        encode_cause(self)
    }
}

impl ParkEventKind {
    /// The stored `kind` column value.
    #[must_use]
    pub fn kind_code(&self) -> &'static str {
        match self {
            Self::Parked { .. } => "parked",
            Self::Unparked { .. } => "unparked",
            Self::Cancelled { .. } => "cancelled",
            Self::RedriveRequested { .. } => "redrive_requested",
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
            Self::RedriveRequested { intent } => (Some(intent.to_string()), None),
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
            record_kind: "ParkEvent",
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
                    cause: decode_cause(cause).ok_or_else(|| {
                        corrupt(format!("unparked event cause `{cause}` is unknown"))
                    })?,
                })
            }
            "cancelled" => {
                let cause =
                    cause.ok_or_else(|| corrupt("cancelled event carries no cause".to_string()))?;
                Ok(Self::Cancelled {
                    cause: decode_cause(cause).ok_or_else(|| {
                        corrupt(format!("cancelled event cause `{cause}` is unknown"))
                    })?,
                })
            }
            "redrive_requested" => {
                let intent = cause
                    .and_then(|cause| cause.parse::<u64>().ok())
                    .ok_or_else(|| {
                        corrupt("redrive_requested event carries no intent".to_string())
                    })?;
                Ok(Self::RedriveRequested {
                    intent: super::ControlIntentId::from_sequence(intent),
                })
            }
            other => Err(corrupt(format!("park event kind `{other}` is unknown"))),
        }
    }
}

/// The work a turn park feed event names: the parked turn of one session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnParkTarget {
    /// The session the transitioned park belonged to.
    pub session_id: SessionId,
    /// The turn the transitioned park belonged to.
    pub turn_id: TurnId,
}

/// The one key a parked process is named by, in its park projection and its
/// park feed.
///
/// It is the process's minted, never-reused id, so a park never outlives or
/// aliases the run it names; every park surface follows this one type.
pub type ProcessParkKey = crate::ProcessId;

/// One park feed row, shared by the turn and the process feeds. `seq` is the
/// feed's own clock sequence; `park_id` is the park the transition applies to
/// (for `Unparked`/`Cancelled` it names the park that closed, which is
/// already gone from the live park projection).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkFeedEvent<Target> {
    /// The feed clock sequence this event was allocated.
    pub seq: u64,
    /// Host-clock epoch milliseconds the transition happened.
    pub at_ms: u64,
    /// The work whose park transitioned.
    pub target: Target,
    /// The park the transition applies to.
    pub park_id: ParkId,
    /// The transition.
    pub kind: ParkEventKind,
}

/// Opaque position in one store's park feed.
///
/// The wrapped sequence is meaningful only to the feed that issued it: a
/// turn park feed position and a process park feed position are not
/// comparable, and neither is comparable across stores. Backends expose
/// constructors/accessors so store implementations can persist and bind the
/// position; consumers treat values as cursors, never as timestamps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ParkFeedCursor(u64);

impl ParkFeedCursor {
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

/// One page of a park feed: the events strictly after the cursor the caller
/// passed, and the cursor that resumes after them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkFeedPage<Target> {
    /// Events in commit order (`seq` ascending).
    pub events: Vec<ParkFeedEvent<Target>>,
    /// Resume position: the last event's sequence, or the caller's cursor
    /// when the page is empty.
    pub next: ParkFeedCursor,
}

impl<Target> Default for ParkFeedPage<Target> {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            next: ParkFeedCursor::default(),
        }
    }
}

/// The engine's own handle on parked work: what its redrive and release find
/// the stopped execution by (an engine-owned, opaque id).
///
/// Opaque to lash: stored beside the park as the engine wrote it and handed
/// back to the same engine, never parsed outside it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct EnginePark(String);

impl EnginePark {
    /// The engine's handle `value`.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The handle as the engine wrote it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The parked state a process record carries (NOW-B).
///
/// A process parks when its body refuses to replay its journal: nothing is
/// settled and nothing was dispatched, so the process stays non-terminal and
/// holds what it holds until an operator acts. The park lives on the record
/// for as long as the process makes no progress: a rerun that refuses again
/// re-parks it (`attempts += 1`, `since_ms` and `park_id` kept), and the
/// first lifecycle fact past the refusal — progress or a terminal — clears it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProcessPark {
    /// Why the process parked, as its latest refusal said.
    pub reason: ParkReason,
    /// The process event sequence of the `process.parked` fact that opened
    /// this park: the CAS token the operator verbs take.
    pub park_id: ParkId,
    /// Host-clock epoch milliseconds of the first refusal of this park.
    pub since_ms: u64,
    /// Host-clock epoch milliseconds of the most recent refusal.
    pub last_refused_ms: u64,
    /// Refusals since the park opened (1 on the first).
    pub attempts: u32,
    /// Whether the latest run refused: a rerun under way clears it, and the
    /// run's own refusal sets it again. Only a refusing park exempts the
    /// process's next start from its attempt budget, so a rerun that gets
    /// past replay and then fails live spends its budget as usual.
    pub refusing: bool,
    /// The engine's handle on the stopped execution, when the engine parked
    /// the process itself (an exhausted retry loop, FIG-3675).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<EnginePark>,
}

/// What a process park write records: the refusal, and the engine's handle
/// on the stopped execution when the engine parked the work itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessParkWrite {
    /// Why the process parked.
    pub reason: ParkReason,
    /// The engine's handle on the stopped execution, if any.
    pub engine: Option<EnginePark>,
}

impl From<ParkReason> for ProcessParkWrite {
    fn from(reason: ParkReason) -> Self {
        Self {
            reason,
            engine: None,
        }
    }
}

/// The filter a `list_parked_processes` read applies.
#[derive(Clone, Debug)]
pub struct ProcessParkQuery {
    /// Restrict to these reason codes; `None` (or an empty set) means all.
    pub reasons: Option<BTreeSet<ParkReasonCode>>,
    /// Age filter: only parks whose `since_ms` is at or before this instant.
    pub parked_at_or_before_ms: Option<u64>,
    /// Keyset: rows strictly after `(since_ms, process key)` in the
    /// `(since_ms, process key)` ordering.
    pub after: Option<(u64, ProcessParkKey)>,
    /// Page size.
    pub limit: NonZeroUsize,
}

/// The live parks of one kind of work, for drain and metrics.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkSummary {
    /// Live parks per reason code; codes with no park are absent.
    pub by_reason: BTreeMap<ParkReasonCode, usize>,
    /// The oldest live park's `since_ms`, `None` when nothing is parked.
    pub oldest_since_ms: Option<u64>,
    /// Live [`RetiredGeneration`](ParkReason::RetiredGeneration) parks per
    /// the executable generation their admission recorded (FIG-3571), read off
    /// the store's projected `park_executable_generation` column.
    #[serde(default)]
    pub retired_by_executable_generation:
        BTreeMap<crate::executable_generation::ExecutableGeneration, usize>,
}

impl ParkSummary {
    /// Every live park, over all reasons.
    #[must_use]
    pub fn total(&self) -> usize {
        self.by_reason.values().sum()
    }
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
        engine_ref: Option<String>,
        resume_intent: Option<u64>,
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
            engine: engine_ref.map(EnginePark::new),
            resume_intent: resume_intent.map(super::ControlIntentId::from_sequence),
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
    /// Live [`RetiredGeneration`](ParkReason::RetiredGeneration) parks per
    /// the generation their admission recorded (FIG-3571): what an old-build
    /// drain of each retired generation still has to redrive. A park whose
    /// admission recorded no generation is counted under
    /// [`parked_by_reason`](Self::parked_by_reason) only.
    pub retired_by_executable_generation:
        BTreeMap<crate::executable_generation::ExecutableGeneration, usize>,
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
