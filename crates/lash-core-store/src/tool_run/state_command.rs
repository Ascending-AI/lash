//! K10: plugin-state commands and their durable publication (binding Q5;
//! FIG-4878 implements it).
//!
//! No callback holds a writable state handle. A permitted callback returns
//! a bounded, ordered batch of commands owned by its plugin revision. One
//! owner-level coordinator reduces the batch privately against the last
//! published state, records the resolution with its predecessor, and
//! exposes it only after durable acceptance. Replay installs recorded
//! resolutions without running a body, hook, reducer or converter. One
//! refusal rejects the whole batch.
//!
//! Only the sequential before-turn, after-turn, checkpoint and after-tool
//! (result-check) callbacks may return commands ([`CallbackSlot`]); every
//! other callback is decision-only.

use serde::{Deserialize, Serialize};

use super::run_event::{AttemptOrdinal, SegmentOrdinal};
use super::tool_hooks::{HookCause, HookOccurrence};
use crate::plugin_state::FormatRefusal;
use crate::store::plugin_writers::{PluginCallbackIdentity, PluginRevision};

/// Generates [`CallbackSlot`] from one table, so a slot cannot exist
/// without its key prefix and its state authority.
macro_rules! callback_slots {
    ($($(#[$doc:meta])* $variant:ident => $prefix:literal, $authority:ident;)+) => {
        /// Every callback slot the plugin registrar names, by the key prefix
        /// of its [`PluginCallbackIdentity`] (FIG-4854).
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum CallbackSlot {
            $($(#[$doc])* $variant,)+
        }

        impl CallbackSlot {
            /// Every slot, in table order.
            pub const ALL: &'static [Self] = &[$(Self::$variant,)+];

            /// The prefix of the slot's callback keys.
            #[must_use]
            pub const fn key_prefix(self) -> &'static str {
                match self {
                    $(Self::$variant => $prefix,)+
                }
            }

            /// What the slot may do to plugin state.
            #[must_use]
            pub const fn state_authority(self) -> StateAuthority {
                match self {
                    $(Self::$variant => StateAuthority::$authority,)+
                }
            }
        }
    };
}

/// What a callback may do to plugin state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StateAuthority {
    /// May return a command batch, recorded with its decision.
    Commands,
    /// Returns a decision only; any command it carried is refused.
    DecisionOnly,
}

callback_slots! {
    ToolProvider => "tool_provider", DecisionOnly;
    ToolCatalog => "tool_catalog", DecisionOnly;
    BeforeTurn => "before_turn", Commands;
    /// Argument transforms, chained before provider preparation.
    ToolArgsTransform => "tool_args_transform", DecisionOnly;
    /// Before-checks over the immutable prepared call.
    ToolArgsCheck => "tool_args_check", DecisionOnly;
    /// Result transforms, chained over the original and preceding candidate.
    ToolResultTransform => "tool_result_transform", DecisionOnly;
    /// The after-tool result check, on the Run's sequential path.
    ToolResultCheck => "tool_result_check", Commands;
    AfterTurn => "after_turn", Commands;
    Checkpoint => "checkpoint", Commands;
    AssistantStream => "assistant_stream", DecisionOnly;
    AssistantResponse => "assistant_response", DecisionOnly;
    AssistantStreamFinished => "assistant_stream_finished", DecisionOnly;
    PresentationStep => "presentation_step", DecisionOnly;
    PresentationPresenter => "presentation_presenter", DecisionOnly;
    RuntimeEvent => "runtime_event", DecisionOnly;
    Operation => "operation", DecisionOnly;
    TurnContextTransform => "turn_context_transform", DecisionOnly;
    ContextCompactor => "context_compactor", DecisionOnly;
    ContextPressure => "context_pressure", DecisionOnly;
    ProtocolSession => "protocol_session", DecisionOnly;
    ProtocolDriver => "protocol_driver", DecisionOnly;
    CodeExecutor => "code_executor", DecisionOnly;
    AssistantProseProjector => "assistant_prose_projector", DecisionOnly;
}

impl CallbackSlot {
    /// The slot a callback identity's key names: the key itself for a
    /// singleton, or the part before its first `:`.
    #[must_use]
    pub fn of(callback: &PluginCallbackIdentity) -> Option<Self> {
        let prefix = callback
            .key
            .split_once(':')
            .map_or(callback.key.as_str(), |(prefix, _)| prefix);
        Self::ALL
            .iter()
            .copied()
            .find(|slot| slot.key_prefix() == prefix)
    }
}

/// One declared command against the owning plugin's namespace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum StateCommand {
    /// Last writer wins.
    Set {
        key: String,
        value: serde_json::Value,
    },
    /// Last writer wins.
    Remove { key: String },
    /// Read-modify-write through the plugin's pure reducer for `name`,
    /// which sees the published namespace plus earlier commands of the
    /// batch.
    Apply {
        key: String,
        name: String,
        input: serde_json::Value,
    },
}

impl StateCommand {
    fn key(&self) -> &str {
        match self {
            Self::Set { key, .. } | Self::Remove { key } | Self::Apply { key, .. } => key,
        }
    }
}

/// Where a batch came from: its dedup identity. A crash redelivery is the
/// same origin; a reported retry is a new attempt and so a new origin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case", deny_unknown_fields)]
pub enum StateCommandOrigin {
    /// A tool body's result.
    ToolAttempt {
        call_id: lash_sansio::ToolCallId,
        attempt: AttemptOrdinal,
    },
    /// The recorded finalization of an authenticated Deferred resolution.
    DeferredFinalization {
        call_id: lash_sansio::ToolCallId,
        attempt: AttemptOrdinal,
    },
    /// An after-tool result check at one occurrence.
    ToolHook { occurrence: Box<HookOccurrence> },
    /// A before-turn, after-turn or checkpoint callback in one segment.
    TurnHook {
        callback: PluginCallbackIdentity,
        segment: SegmentOrdinal,
    },
}

/// The bounds a batch is checked against before any reduction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateCommandLimits {
    pub max_commands: usize,
    pub max_encoded_bytes: usize,
}

/// A bounded, ordered command batch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateCommandBatch {
    /// The admitted plugin revision whose namespace the batch addresses.
    pub plugin: PluginRevision,
    pub origin: StateCommandOrigin,
    pub commands: Vec<StateCommand>,
}

/// Why a batch publishes nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "refusal", rename_all = "snake_case", deny_unknown_fields)]
pub enum StateCommandRefusal {
    #[error("a decision-only callback returned state commands")]
    DecisionOnly,
    #[error("the batch addresses another plugin's namespace")]
    WrongOwner,
    #[error("the batch holds {count} commands, over the limit")]
    TooManyCommands { count: usize },
    #[error("the batch encodes to {bytes} bytes, over the limit")]
    TooLarge { bytes: usize },
    #[error("command {index} has an empty key")]
    InvalidKey { index: usize },
    #[error(transparent)]
    IncompatibleWriter(FormatRefusal),
    #[error("the reducer refused command {index}")]
    Reducer { index: usize, cause: HookCause },
}

impl StateCommandBatch {
    /// Check a batch before reduction: the proposing callback's authority,
    /// its owner, and the limits.
    ///
    /// # Errors
    ///
    /// The first [`StateCommandRefusal`]; nothing of the batch publishes.
    pub fn check(
        &self,
        proposer: &PluginCallbackIdentity,
        limits: StateCommandLimits,
    ) -> Result<(), StateCommandRefusal> {
        if CallbackSlot::of(proposer).map(CallbackSlot::state_authority)
            != Some(StateAuthority::Commands)
        {
            return Err(StateCommandRefusal::DecisionOnly);
        }
        if proposer.owner != self.plugin {
            return Err(StateCommandRefusal::WrongOwner);
        }
        if self.commands.len() > limits.max_commands {
            return Err(StateCommandRefusal::TooManyCommands {
                count: self.commands.len(),
            });
        }
        let bytes = serde_json::to_vec(&self.commands).map_or(usize::MAX, |bytes| bytes.len());
        if bytes > limits.max_encoded_bytes {
            return Err(StateCommandRefusal::TooLarge { bytes });
        }
        if let Some(index) = self
            .commands
            .iter()
            .position(|command| command.key().trim().is_empty())
        {
            return Err(StateCommandRefusal::InvalidKey { index });
        }
        Ok(())
    }
}

/// The ordinal of one durable publication in its owner's sequence, from 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PublicationOrdinal(pub u64);

/// One resolved change to a namespace value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "change", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolvedStateChange {
    Put {
        key: String,
        value: serde_json::Value,
    },
    Delete {
        key: String,
    },
}

/// What a reduction resolved to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "resolution", rename_all = "snake_case", deny_unknown_fields)]
pub enum StateResolutionOutcome {
    Applied { changes: Vec<ResolvedStateChange> },
    Refused { refusal: StateCommandRefusal },
}

/// The recorded resolution of one batch: what replay installs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateResolution {
    pub plugin: PluginRevision,
    pub origin: StateCommandOrigin,
    /// The segment that published it.
    pub segment: SegmentOrdinal,
    pub ordinal: PublicationOrdinal,
    /// The publication this one reduced against; `None` for the first.
    pub predecessor: Option<PublicationOrdinal>,
    pub outcome: StateResolutionOutcome,
}

/// The applied frontier a checkpoint or handover carries with the state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateFrontier {
    /// The last publication applied.
    pub applied: Option<PublicationOrdinal>,
    /// The segment that owns publication.
    pub owner_segment: SegmentOrdinal,
}

/// What the frontier does with a resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrontierStep {
    /// Apply it and advance.
    Apply,
    /// Already applied: a repeated delivery applies nothing.
    AlreadyApplied,
}

/// Why a frontier refuses a resolution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "refusal", rename_all = "snake_case", deny_unknown_fields)]
pub enum FrontierRefusal {
    #[error("segment {found} publishes after segment {owner} took ownership")]
    StalePublisher { owner: u32, found: u32 },
    #[error("publication {found} does not follow the applied frontier")]
    OutOfOrder { found: u64 },
}

impl StateFrontier {
    /// Decide what to do with `resolution`.
    ///
    /// # Errors
    ///
    /// [`FrontierRefusal`] for a stale publisher or a publication that
    /// skips or reorders the recorded sequence.
    pub fn step(&self, resolution: &StateResolution) -> Result<FrontierStep, FrontierRefusal> {
        if resolution.segment < self.owner_segment {
            return Err(FrontierRefusal::StalePublisher {
                owner: self.owner_segment.0,
                found: resolution.segment.0,
            });
        }
        if self
            .applied
            .is_some_and(|applied| resolution.ordinal <= applied)
        {
            return Ok(FrontierStep::AlreadyApplied);
        }
        let expected = PublicationOrdinal(self.applied.map_or(1, |applied| applied.0 + 1));
        if resolution.ordinal != expected || resolution.predecessor != self.applied {
            return Err(FrontierRefusal::OutOfOrder {
                found: resolution.ordinal.0,
            });
        }
        Ok(FrontierStep::Apply)
    }
}
