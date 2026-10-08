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
//! Publication is sequenced per namespace: a resolution's ordinal is the
//! position in the namespace's publication sequence, and its predecessor is
//! the publication it was reduced against. Checkpoints carry the applied
//! frontier and receipt digests. Only an identical recorded resolution can
//! be delivered again without applying anything.
//!
//! Only the sequential before-turn, after-turn, checkpoint and after-tool
//! (result-check) callbacks may return commands ([`CallbackSlot`]), beside a
//! tool body's own result; every other callback is decision-only.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::run_event::{AttemptOrdinal, SegmentOrdinal};
use super::tool_hooks::{HookCause, HookOccurrence};
use crate::plugin_state::{
    FormatRefusal, KeyRejection, PLUGIN_STATE_NAMESPACE_LIMIT, PLUGIN_STATE_VALUE_LIMIT,
    validate_state_key,
};
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
    /// Attachment-omission history policies (ADR 0133).
    AttachmentOmission => "attachment_omission", DecisionOnly;
    ContextCompactor => "context_compactor", DecisionOnly;
    ContextPressure => "context_pressure", DecisionOnly;
    ProtocolSession => "protocol_session", DecisionOnly;
    ProtocolDriver => "protocol_driver", DecisionOnly;
    CodeExecutor => "code_executor", DecisionOnly;
    AssistantProseProjector => "assistant_prose_projector", DecisionOnly;
    TranscriptProjector => "transcript_projector", DecisionOnly;
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
    /// The namespace key the command addresses.
    #[must_use]
    pub fn key(&self) -> &str {
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

impl StateCommandLimits {
    /// The bounds every published batch is held to: 64 commands, and one
    /// namespace's worth of encoded input with room for its keys.
    pub const PUBLISHED: Self = Self {
        max_commands: 64,
        max_encoded_bytes: PLUGIN_STATE_NAMESPACE_LIMIT + 16 * 1024,
    };
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
    #[error("command {index} names an invalid key: {reason}")]
    InvalidKey { index: usize, reason: KeyRejection },
    #[error("command {index} writes a {bytes}-byte value, limit {limit}")]
    ValueTooLarge {
        index: usize,
        bytes: usize,
        limit: usize,
    },
    #[error("the namespace would encode to {bytes} bytes, limit {limit}")]
    NamespaceTooLarge { bytes: usize, limit: usize },
    /// The session's plugin state would encode past its total budget
    /// (FIG-5301).
    #[error("the session's plugin state would encode to {bytes} bytes, limit {limit}")]
    SessionTooLarge { bytes: usize, limit: usize },
    #[error("command {index} names reducer `{name}`, which its plugin does not register")]
    UnknownReducer { index: usize, name: String },
    #[error(transparent)]
    IncompatibleWriter(FormatRefusal),
    #[error("the reducer refused command {index}")]
    Reducer { index: usize, cause: HookCause },
}

/// What [`StateCommandBatch::reduce`] calls for each [`StateCommand::Apply`]:
/// the key, the reducer's name, the key's current candidate value and the
/// command's input, to the key's next value or a refusal.
pub type ApplyReducer<'a> = dyn FnMut(
        &str,
        &str,
        Option<&serde_json::Value>,
        &serde_json::Value,
    ) -> Result<Option<serde_json::Value>, ReducerRefusal>
    + 'a;

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
        self.check_bounds(limits)
    }

    /// Check a batch's limits and keys, whoever proposed it.
    ///
    /// # Errors
    ///
    /// The first [`StateCommandRefusal`]; nothing of the batch publishes.
    pub fn check_bounds(&self, limits: StateCommandLimits) -> Result<(), StateCommandRefusal> {
        if self.commands.len() > limits.max_commands {
            return Err(StateCommandRefusal::TooManyCommands {
                count: self.commands.len(),
            });
        }
        let bytes = serde_json::to_vec(&self.commands).map_or(usize::MAX, |bytes| bytes.len());
        if bytes > limits.max_encoded_bytes {
            return Err(StateCommandRefusal::TooLarge { bytes });
        }
        for (index, command) in self.commands.iter().enumerate() {
            validate_state_key(command.key())
                .map_err(|reason| StateCommandRefusal::InvalidKey { index, reason })?;
            if let StateCommand::Set { value, .. } = command {
                check_value(index, value)?;
            }
        }
        Ok(())
    }

    /// Reduce the batch against `published`, the namespace's last published
    /// values: every command sees the changes of the commands before it.
    /// `reducer` resolves an [`StateCommand::Apply`] from its key, its
    /// reducer's name, the key's current candidate value and its input; it
    /// must be pure.
    ///
    /// The outcome is all-or-none: the first refusal rejects every command.
    pub fn reduce(
        &self,
        published: &BTreeMap<String, serde_json::Value>,
        reducer: &mut ApplyReducer<'_>,
    ) -> StateResolutionOutcome {
        match self.resolve(published, reducer) {
            Ok(changes) => StateResolutionOutcome::Applied { changes },
            Err(refusal) => StateResolutionOutcome::Refused { refusal },
        }
    }

    fn resolve(
        &self,
        published: &BTreeMap<String, serde_json::Value>,
        reducer: &mut ApplyReducer<'_>,
    ) -> Result<Vec<ResolvedStateChange>, StateCommandRefusal> {
        let mut candidate = published.clone();
        let mut changes = Vec::with_capacity(self.commands.len());
        for (index, command) in self.commands.iter().enumerate() {
            let change = match command {
                StateCommand::Set { key, value } => ResolvedStateChange::Put {
                    key: key.clone(),
                    value: canonical(value.clone()),
                },
                StateCommand::Remove { key } => ResolvedStateChange::Delete { key: key.clone() },
                StateCommand::Apply { key, name, input } => {
                    match reducer(key, name, candidate.get(key), input) {
                        Ok(Some(value)) => {
                            check_value(index, &value)?;
                            ResolvedStateChange::Put {
                                key: key.clone(),
                                value: canonical(value),
                            }
                        }
                        Ok(None) => ResolvedStateChange::Delete { key: key.clone() },
                        Err(ReducerRefusal::Unknown) => {
                            return Err(StateCommandRefusal::UnknownReducer {
                                index,
                                name: name.clone(),
                            });
                        }
                        Err(ReducerRefusal::Refused(cause)) => {
                            return Err(StateCommandRefusal::Reducer { index, cause });
                        }
                    }
                }
            };
            change.apply_to(&mut candidate);
            changes.push(change);
        }
        let bytes = serde_json::to_vec(&candidate).map_or(usize::MAX, |bytes| bytes.len());
        if bytes > PLUGIN_STATE_NAMESPACE_LIMIT {
            return Err(StateCommandRefusal::NamespaceTooLarge {
                bytes,
                limit: PLUGIN_STATE_NAMESPACE_LIMIT,
            });
        }
        Ok(changes)
    }
}

/// Why a reducer resolved no value for an [`StateCommand::Apply`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReducerRefusal {
    /// The plugin registers no reducer by the command's name.
    Unknown,
    /// The reducer refused the command, with its typed cause.
    Refused(HookCause),
}

fn check_value(index: usize, value: &serde_json::Value) -> Result<(), StateCommandRefusal> {
    let bytes = serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len());
    if bytes > PLUGIN_STATE_VALUE_LIMIT {
        return Err(StateCommandRefusal::ValueTooLarge {
            index,
            bytes,
            limit: PLUGIN_STATE_VALUE_LIMIT,
        });
    }
    Ok(())
}

fn canonical(mut value: serde_json::Value) -> serde_json::Value {
    value.sort_all_objects();
    value
}

/// The ordinal of one durable publication in its namespace's sequence, from
/// 1, independent of namespace format conversions.
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

impl ResolvedStateChange {
    /// Install the change in `values`.
    pub fn apply_to(&self, values: &mut BTreeMap<String, serde_json::Value>) {
        match self {
            Self::Put { key, value } => {
                values.insert(key.clone(), value.clone());
            }
            Self::Delete { key } => {
                values.remove(key);
            }
        }
    }
}

/// What a reduction resolved to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "resolution", rename_all = "snake_case", deny_unknown_fields)]
pub enum StateResolutionOutcome {
    Applied { changes: Vec<ResolvedStateChange> },
    Refused { refusal: StateCommandRefusal },
}

/// The recorded resolution of one batch: what replay installs. A refusal is
/// a publication too: it advances its namespace's sequence and changes no
/// value, so every recorded resolution has one place in it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateResolution {
    /// The recorded effect address binds the logical Run and callback phase.
    pub publisher: crate::EffectAddress,
    pub plugin: PluginRevision,
    pub origin: StateCommandOrigin,
    /// The segment that published it.
    pub segment: SegmentOrdinal,
    pub ordinal: PublicationOrdinal,
    /// The publication this one reduced against; `None` for the first.
    pub predecessor: Option<PublicationOrdinal>,
    pub outcome: StateResolutionOutcome,
}

/// The applied frontier a checkpoint or handover carries with a namespace:
/// the publications applied so far, and the receipts of those the owner's
/// current run applied.
///
/// A run settles the frontier it begins from ([`Self::settle`]): every
/// publication its base head holds is settled, and only the receipts the run
/// applies itself are kept. A delivered resolution comes from the run's own
/// records (its effects' journaled outcomes, its members' committed outcome
/// records, its after-turn staged state) or from the after-turn state of the
/// run whose commit wrote its base head. The first are reduced within the
/// run, after its base, so their receipts are in the window; the second are
/// what that commit wrote, so they are settled and cannot differ from it. A
/// delivery at or below the settled ordinal therefore applies nothing, and
/// one in the window applies nothing only if its whole receipt matches.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateFrontier {
    /// The segment that owns publication.
    pub owner_segment: SegmentOrdinal,
    /// The last publication settled; `None` when none is.
    settled: Option<PublicationOrdinal>,
    /// Digests of the resolutions applied after `settled`, in order: origin
    /// and resolved content.
    recent: Vec<crate::BlobRef>,
}

impl StateFrontier {
    /// The last publication applied.
    #[must_use]
    pub fn applied(&self) -> Option<PublicationOrdinal> {
        let applied = self
            .settled
            .map_or(0, |settled| settled.0)
            .saturating_add(self.recent.len() as u64);
        (applied > 0).then_some(PublicationOrdinal(applied))
    }

    /// The publication the next resolution reduced now takes.
    #[must_use]
    pub fn next(&self) -> PublicationOrdinal {
        PublicationOrdinal(
            self.applied()
                .map_or(1, |applied| applied.0.saturating_add(1)),
        )
    }

    /// The receipts of the publications applied since the frontier settled.
    #[must_use]
    pub fn recent(&self) -> &[crate::BlobRef] {
        &self.recent
    }

    /// Advance after the resolved changes have been accepted and installed.
    pub fn record(&mut self, resolution: &StateResolution) {
        debug_assert_eq!(resolution.ordinal, self.next());
        self.recent.push(resolution.receipt());
    }

    /// Settle every publication applied so far: a run begins from here, and
    /// keeps only the receipts it applies itself.
    pub fn settle(&mut self) {
        self.settled = self.applied();
        self.recent.clear();
    }
}

impl StateResolution {
    /// Identity evidence for this exact recorded resolution, with no payload copy.
    #[must_use]
    #[expect(
        clippy::expect_used,
        reason = "a state resolution contains only infallibly serializable data"
    )]
    pub fn receipt(&self) -> crate::BlobRef {
        crate::BlobRef::for_content(
            &rmp_serde::to_vec_named(self).expect("state resolution encodes"),
        )
    }
}

/// What the frontier does with a resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrontierStep {
    /// Apply it and advance.
    Apply,
    /// Already applied: a repeated delivery applies nothing.
    AlreadyApplied,
}

/// A frontier's refusal of a recorded resolution of one plugin's namespace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NamespaceFrontierRefusal {
    pub plugin: String,
    pub refusal: FrontierRefusal,
}

/// Why a frontier refuses a resolution.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error, schemars::JsonSchema,
)]
#[serde(tag = "refusal", rename_all = "snake_case", deny_unknown_fields)]
pub enum FrontierRefusal {
    #[error("segment {found} publishes after segment {owner} took ownership")]
    StalePublisher { owner: u32, found: u32 },
    #[error("publication {found} does not follow the applied frontier")]
    OutOfOrder { found: u64 },
    #[error("publication {found} differs from its applied or pending receipt")]
    ReceiptMismatch { found: u64 },
}

impl StateFrontier {
    /// Decide what to do with `resolution`. A delivery of a settled
    /// publication applies nothing; one of a publication applied since
    /// applies nothing only if its entire receipt matches.
    ///
    /// # Errors
    ///
    /// [`FrontierRefusal`] for a stale publisher, a publication that skips
    /// or reorders the recorded sequence, or one whose receipt differs from
    /// the one applied at its ordinal.
    pub fn step(&self, resolution: &StateResolution) -> Result<FrontierStep, FrontierRefusal> {
        if self
            .applied()
            .is_some_and(|applied| resolution.ordinal <= applied)
        {
            let settled = self.settled.map_or(0, |settled| settled.0);
            if resolution.ordinal.0 <= settled {
                return Ok(FrontierStep::AlreadyApplied);
            }
            let receipt = usize::try_from(resolution.ordinal.0 - settled - 1)
                .ok()
                .and_then(|index| self.recent.get(index));
            return if receipt == Some(&resolution.receipt()) {
                Ok(FrontierStep::AlreadyApplied)
            } else {
                Err(FrontierRefusal::ReceiptMismatch {
                    found: resolution.ordinal.0,
                })
            };
        }
        if resolution.segment < self.owner_segment {
            return Err(FrontierRefusal::StalePublisher {
                owner: self.owner_segment.0,
                found: resolution.segment.0,
            });
        }
        if resolution.ordinal != self.next() || resolution.predecessor != self.applied() {
            return Err(FrontierRefusal::OutOfOrder {
                found: resolution.ordinal.0,
            });
        }
        Ok(FrontierStep::Apply)
    }
}
