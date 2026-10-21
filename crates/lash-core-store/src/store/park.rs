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
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{RuntimeError, RuntimeErrorCode};

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
    /// A recorded callback key is unavailable.
    RecordedCallbackUnavailable {
        refusal: Box<crate::store::plugin_writers::RecordedCallbackUnavailable>,
        message: String,
    },
    /// The receiving plugin cannot read the recorded callback's stream state.
    StreamStateFormatUnreadable {
        refusal: Box<crate::store::plugin_writers::StreamStateFormatRefusal>,
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
        fault: lash_protocol_types::engine_fault::WorkerDeploymentFault,
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
    RecordedCallbackUnavailable,
    StreamStateFormatUnreadable,
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
        Self::RecordedCallbackUnavailable,
        Self::StreamStateFormatUnreadable,
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
            Self::RecordedCallbackUnavailable => "recorded_callback_unavailable",
            Self::StreamStateFormatUnreadable => "stream_state_format_unreadable",
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
            "recorded_callback_unavailable" => Some(Self::RecordedCallbackUnavailable),
            "stream_state_format_unreadable" => Some(Self::StreamStateFormatUnreadable),
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
            && cause.plugin_failure_class() == Some(lash_protocol_types::PluginFailureClass::Parked)
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
            RuntimeErrorCode::LashVmCellReplayDivergence => {
                Some(Self::ReplayDivergence { message })
            }
            RuntimeErrorCode::RetiredGeneration => Some(Self::of_retired_generation(error)),
            RuntimeErrorCode::LashVmCellBindingDrift => Some(Self::BindingDrift { message }),
            RuntimeErrorCode::RecordedCallbackUnavailable => match &error.cause {
                Some(crate::RuntimeErrorCause::RecordedCallbackUnavailable { refusal }) => {
                    Some(Self::RecordedCallbackUnavailable {
                        refusal: refusal.clone(),
                        message,
                    })
                }
                _ => None,
            },
            RuntimeErrorCode::StreamStateFormatUnreadable => match &error.cause {
                Some(crate::RuntimeErrorCause::StreamStateFormatUnreadable { refusal }) => {
                    Some(Self::StreamStateFormatUnreadable {
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
            Self::RecordedCallbackUnavailable { .. } => ParkReasonCode::RecordedCallbackUnavailable,
            Self::StreamStateFormatUnreadable { .. } => ParkReasonCode::StreamStateFormatUnreadable,
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
            | Self::RecordedCallbackUnavailable { message, .. }
            | Self::StreamStateFormatUnreadable { message, .. }
            | Self::BindingDrift { message }
            | Self::EffectReplayDivergence { message, .. }
            | Self::SessionStateGenerationRefused { message, .. }
            | Self::EngineRetryExhausted { message, .. } => message,
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
