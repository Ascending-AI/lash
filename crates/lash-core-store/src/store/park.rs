//! The parked state of an engine-executed turn (FIG-3586, FIG-3600, FIG-3659).
//!
//! A turn *parks* when it aborts on a refusal no redrive of the same build can
//! get past and no live retry may paper over: its journal cannot be replayed
//! by the code now running it. Parking is neither failing (nothing is settled,
//! so the turn's claims stay held and a redrive under the right build finishes
//! it) nor retrying live (a live retry would re-issue effects the journal
//! already holds).
//!
//! One record per session: a session shifts one turn at a time, and a parked
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

use serde::{Deserialize, Serialize};

use crate::{RuntimeError, RuntimeErrorCode, TurnId};

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

/// The result of an owning store write. The disposition is ephemeral and
/// cannot be serialized into a journal as a freshness claim.
#[derive(Debug)]
pub struct StoreTransition<T> {
    pub record: T,
    pub changed: bool,
}

impl<T> StoreTransition<T> {
    pub fn changed(record: T) -> Self {
        Self {
            record,
            changed: true,
        }
    }
    pub fn unchanged(record: T) -> Self {
        Self {
            record,
            changed: false,
        }
    }
    pub fn into_record(self) -> T {
        self.record
    }
    pub fn permit(&self) -> Option<lash_trace::EmissionPermit> {
        self.changed
            .then(lash_trace::EmissionPermit::new_transition)
    }
}

/// Why a turn parked. Each arm carries the refusal's operator-facing message:
/// what the journal holds, who wrote it, and the remedies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ParkReason {
    /// The exact recorded plugin composition or tool owner is unavailable.
    PluginRevisionUnavailable {
        refusal: Box<crate::store::plugin_writers::PluginExecutionRefusal>,
        message: String,
    },
    /// A declared plugin failure needs operator repair before execution can resume.
    PluginFailure {
        cause: Box<crate::RuntimeErrorCause>,
        message: String,
    },
    /// The deployment must supply the configured worker before redrive can run it.
    WorkerDeployment {
        executable: std::path::PathBuf,
        fault: lash_vm_protocol::WorkerDeploymentFault,
        message: String,
    },
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
        /// The recorded model key the failing attempts could not bind, when
        /// that is why they failed (FIG-4404): a deployment that serves the
        /// key again lets a resume proceed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile_key: Option<crate::LlmProfileKey>,
    },
}

impl ParkReason {
    /// The park of work whose engine retries ran out, from what the engine
    /// kept of its last failure. A typed fault the failure text carries is
    /// decoded into the park's own fields, never read out of the prose.
    #[must_use]
    pub fn engine_retry_exhausted(
        attempts: u32,
        last_failure_code: Option<String>,
        message: String,
    ) -> Self {
        let profile_key = crate::runtime_error::RuntimeEffectControllerError::in_text(&message)
            .and_then(|fault| fault.profile_key().cloned());
        Self::EngineRetryExhausted {
            attempts,
            last_failure_code,
            message,
            profile_key,
        }
    }

    /// The recorded model key the parked work could not bind, when that is
    /// why its engine retries ran out.
    #[must_use]
    pub fn profile_key(&self) -> Option<&crate::LlmProfileKey> {
        match self {
            Self::EngineRetryExhausted { profile_key, .. } => profile_key.as_ref(),
            _ => None,
        }
    }
}

/// The reason a park carries, as a plain code: the metric label and the query
/// filter the free-form [`ParkReason`] payload is projected onto.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ParkReasonCode {
    PluginRevisionUnavailable,
    /// See [`ParkReason::PluginFailure`].
    PluginFailure,
    /// See [`ParkReason::WorkerDeployment`].
    WorkerDeployment,
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
        Self::PluginFailure,
        Self::WorkerDeployment,
        Self::ReplayDivergence,
        Self::RetiredGeneration,
        Self::PluginRevisionUnavailable,
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
            Self::PluginFailure => "plugin_failure",
            Self::WorkerDeployment => "worker_deployment",
            Self::ReplayDivergence => "replay_divergence",
            Self::RetiredGeneration => "retired_generation",
            Self::BindingDrift => "binding_drift",
            Self::PluginRevisionUnavailable => "plugin_revision_unavailable",
            Self::EffectReplayDivergence => "effect_replay_divergence",
            Self::SessionStateGenerationRefused => "session_state_generation_refused",
            Self::EngineRetryExhausted => "engine_retry_exhausted",
        }
    }

    /// Parse a stored `reason_code`.
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "plugin_failure" => Some(Self::PluginFailure),
            "worker_deployment" => Some(Self::WorkerDeployment),
            "replay_divergence" => Some(Self::ReplayDivergence),
            "retired_generation" => Some(Self::RetiredGeneration),
            "binding_drift" => Some(Self::BindingDrift),
            "plugin_revision_unavailable" => Some(Self::PluginRevisionUnavailable),
            "effect_replay_divergence" => Some(Self::EffectReplayDivergence),
            "session_state_generation_refused" => Some(Self::SessionStateGenerationRefused),
            "engine_retry_exhausted" => Some(Self::EngineRetryExhausted),
            _ => None,
        }
    }
}

impl ParkReason {
    /// Whether the engine stopped retrying work whose last attempt admission
    /// refused only because the session's park named a redrive that had not
    /// settled (D15): the work waited behind that redrive, and once it
    /// settles nothing of the work's own stops it. The engine records the
    /// refusal as it rendered it, which names the typed code either way it
    /// renders: its wire form or its variant.
    #[must_use]
    pub fn stopped_behind_unsettled_redrive(&self) -> bool {
        let code = crate::runtime_error::RuntimeErrorCode::SessionRedriveUnsettled;
        matches!(
            self,
            Self::EngineRetryExhausted { message, .. }
                if message.contains(code.as_str()) || message.contains(&format!("{code:?}"))
        )
    }

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
        if let Some(cause) = &error.cause
            && cause.plugin_failure_class() == Some(lash_sansio::PluginFailureClass::Parked)
        {
            return Some(Self::PluginFailure {
                cause: Box::new(cause.clone()),
                message,
            });
        }
        if let Some(crate::RuntimeErrorCause::VmWorker { outcome }) = &error.cause
            && let Some((executable, fault)) = outcome.deployment_fault()
        {
            return Some(Self::WorkerDeployment {
                executable: executable.to_owned(),
                fault,
                message,
            });
        }
        match error.code {
            RuntimeErrorCode::LashlangCellReplayDivergence => {
                Some(Self::ReplayDivergence { message })
            }
            RuntimeErrorCode::RetiredGeneration => Some(Self::of_retired_generation(error)),
            RuntimeErrorCode::LashlangCellBindingDrift => Some(Self::BindingDrift { message }),
            RuntimeErrorCode::PluginRevisionUnavailable => match &error.cause {
                Some(crate::RuntimeErrorCause::PluginExecution { refusal }) => {
                    Some(Self::PluginRevisionUnavailable {
                        refusal: refusal.clone(),
                        message,
                    })
                }
                _ => None,
            },
            RuntimeErrorCode::EffectReplayDivergence => Some(Self::EffectReplayDivergence {
                effect_kind: error
                    .summary
                    .as_ref()
                    .and_then(|summary| summary.effect_kind.clone())
                    .unwrap_or_else(|| "unknown".to_string()),
                message,
            }),
            _ => None,
        }
    }

    /// The reason's code: the stored `reason_code` and the metric label.
    #[must_use]
    pub fn code(&self) -> ParkReasonCode {
        match self {
            Self::PluginFailure { .. } => ParkReasonCode::PluginFailure,
            Self::WorkerDeployment { .. } => ParkReasonCode::WorkerDeployment,
            Self::ReplayDivergence { .. } => ParkReasonCode::ReplayDivergence,
            Self::RetiredGeneration { .. } => ParkReasonCode::RetiredGeneration,
            Self::BindingDrift { .. } => ParkReasonCode::BindingDrift,
            Self::PluginRevisionUnavailable { .. } => ParkReasonCode::PluginRevisionUnavailable,
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
            Self::PluginFailure { message, .. }
            | Self::WorkerDeployment { message, .. }
            | Self::ReplayDivergence { message }
            | Self::RetiredGeneration { message, .. }
            | Self::PluginRevisionUnavailable { message, .. }
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
    /// A different turn parked over it in the same session.
    Superseded,
    /// The parked command run applied the session's command lane until it
    /// was empty and ended: a command run commits no turn, so its end is
    /// what settles its park (FIG-4780).
    CommandsApplied,
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
    /// An operator cancelled the parked run (its `Cancel` intent).
    Operator {
        /// The intent that cancelled it.
        intent: super::ControlIntentId,
    },
    /// An operator forked the parked run: its held inputs now shift
    /// `new_run` (`None` for a queued run, whose next admission mints it).
    Forked {
        /// The intent that forked it.
        intent: super::ControlIntentId,
        /// The run the held inputs shift under.
        new_run: Option<TurnId>,
    },
    /// The engine ended the parked run's only run without a Lash outcome,
    /// and the run ended with it (FIG-4780).
    RunLost {
        /// The cancellation request the run had recorded, when it had one.
        cancelled_by: Option<String>,
    },
    /// The parked run's execution ended with a typed refusal no retry could
    /// change, and the run ended with it (FIG-4780).
    RunRefused {
        /// The refusal's code.
        code: RuntimeErrorCode,
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
    /// An operator asked to redrive the parked run; the park stays until
    /// the resumed run commits or parks again.
    RedriveRequested {
        /// The redrive's intent.
        intent: super::ControlIntentId,
    },
}

/// Encode a closing cause as its complete tagged JSON body.
fn encode_cause<T: Serialize>(cause: &T) -> Result<String, crate::StoreError> {
    serde_json::to_string(cause).map_err(|error| crate::StoreError::RecordEncodingFailed {
        record_kind: "ParkEvent".to_string(),
        message: error.to_string(),
    })
}

impl UnparkCause {
    /// The complete tagged JSON of this cause.
    pub fn encode(&self) -> Result<String, crate::StoreError> {
        encode_cause(self)
    }
}

impl ParkCancelCause {
    /// The complete tagged JSON of this cause.
    pub fn encode(&self) -> Result<String, crate::StoreError> {
        encode_cause(self)
    }
}

/// Encoded `(cause_json, reason_json, redrive_intent)` values of a park event.
pub type ParkEventColumns = (Option<String>, Option<String>, Option<i64>);

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

    /// Encode `(cause_json, reason_json, redrive_intent)`, with exactly one populated.
    pub fn encode_columns(&self) -> Result<ParkEventColumns, crate::StoreError> {
        Ok(match self {
            Self::Parked { reason } => (None, Some(encode_cause(reason)?), None),
            Self::Unparked { cause } => (Some(cause.encode()?), None, None),
            Self::Cancelled { cause } => (Some(cause.encode()?), None, None),
            Self::RedriveRequested { intent } => (
                None,
                None,
                Some(i64::try_from(intent.sequence()).map_err(|error| {
                    crate::StoreError::RecordEncodingFailed {
                        record_kind: "ParkEvent".to_string(),
                        message: error.to_string(),
                    }
                })?),
            ),
        })
    }

    /// Decode a feed row's discriminant and exclusive variant columns.
    pub fn decode_columns(
        kind: &str,
        cause_json: Option<&str>,
        reason_json: Option<&str>,
        redrive_intent: Option<i64>,
    ) -> Result<Self, crate::StoreError> {
        let corrupt = |message: String| crate::StoreError::StoredDataCorrupt {
            record_kind: "ParkEvent",
            message,
        };
        match (kind, cause_json, reason_json, redrive_intent) {
            ("parked", None, Some(reason), None) => Ok(Self::Parked {
                reason: serde_json::from_str(reason)
                    .map_err(|error| corrupt(format!("park reason is unreadable: {error}")))?,
            }),
            ("unparked", Some(cause), None, None) => Ok(Self::Unparked {
                cause: serde_json::from_str(cause)
                    .map_err(|error| corrupt(format!("unpark cause is unreadable: {error}")))?,
            }),
            ("cancelled", Some(cause), None, None) => Ok(Self::Cancelled {
                cause: serde_json::from_str(cause)
                    .map_err(|error| corrupt(format!("cancel cause is unreadable: {error}")))?,
            }),
            ("redrive_requested", None, None, Some(intent)) => {
                Ok(Self::RedriveRequested {
                    intent: super::ControlIntentId::from_sequence(u64::try_from(intent).map_err(
                        |error| corrupt(format!("redrive intent is unreadable: {error}")),
                    )?),
                })
            }
            _ => Err(corrupt(format!(
                "park event `{kind}` has invalid variant columns"
            ))),
        }
    }
}

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
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
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

/// The live parks of one kind of work, for drain and metrics.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkReport {
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

impl ParkReport {
    /// Every live park, over all reasons.
    #[must_use]
    pub fn total(&self) -> usize {
        self.by_reason.values().sum()
    }
}

/// The turns of a deployment that are not settled yet, for drain.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsettledTurnCounts {
    /// Sessions with a turn in flight: a pending queued run or an admitted
    /// turn input that is not settled.
    pub in_flight_turns: usize,
    /// Sessions whose turn in flight their stalled close holds: the
    /// session's `CloseSession` intent is still pending, so the claim stays
    /// until the close retires it. Never more than `in_flight_turns`.
    pub held_by_stalled_close: usize,
}
