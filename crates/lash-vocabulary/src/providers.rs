//! Provider, callback and format identities, independent of registries.
use serde::{Deserialize, Serialize};
use std::fmt;
type ProviderKey = ToolProviderHandle;
/// A plugin's declared, nonzero state and config format version.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct FormatVersion(std::num::NonZeroU32);

impl FormatVersion {
    pub const ONE: Self = Self(std::num::NonZeroU32::MIN);

    pub const fn new(value: u32) -> Option<Self> {
        match std::num::NonZeroU32::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

impl std::fmt::Display for FormatVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl From<std::num::NonZeroU32> for FormatVersion {
    fn from(value: std::num::NonZeroU32) -> Self {
        Self(value)
    }
}

impl From<FormatVersion> for std::num::NonZeroU32 {
    fn from(value: FormatVersion) -> Self {
        value.0
    }
}

/// A provider's local name, checked before registration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ToolProviderKey(&'static str);

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ToolProviderKeyError {
    #[error(
        "invalid tool provider key `{key}`: expected 1 to 64 ASCII letters, digits, `_`, `-` or `.`, beginning with a letter"
    )]
    InvalidGrammar { key: String },
}
impl ToolProviderKey {
    pub fn new(key: &'static str) -> Result<Self, ToolProviderKeyError> {
        if Self::is_valid(key) {
            Ok(Self(key))
        } else {
            Err(ToolProviderKeyError::InvalidGrammar { key: key.into() })
        }
    }
    pub const fn is_valid(key: &str) -> bool {
        let bytes = key.as_bytes();
        if bytes.is_empty() || bytes.len() > 64 || !bytes[0].is_ascii_alphabetic() {
            return false;
        }
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            if !(byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' || byte == b'.') {
                return false;
            }
            index += 1;
        }
        true
    }
    #[doc(hidden)]
    #[expect(clippy::panic, reason = "invalid literals fail compilation")]
    pub const fn __literal(key: &'static str) -> Self {
        if !Self::is_valid(key) {
            panic!("invalid tool provider key literal");
        }
        Self(key)
    }
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}
impl std::fmt::Display for ToolProviderKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// The scoped address of a declared provider. Only the callback codec spells its seam prefix.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ToolProviderHandle {
    registry: crate::claims::RegistryKind,
    owner: std::borrow::Cow<'static, str>,
    key: std::borrow::Cow<'static, str>,
}
impl ToolProviderHandle {
    pub fn new(
        registry: crate::claims::RegistryKind,
        owner: impl Into<String>,
        key: ToolProviderKey,
    ) -> Self {
        Self {
            registry,
            owner: std::borrow::Cow::Owned(owner.into()),
            key: std::borrow::Cow::Borrowed(key.as_str()),
        }
    }
    pub const fn literal(
        registry: crate::claims::RegistryKind,
        owner: &'static str,
        key: ToolProviderKey,
    ) -> Self {
        Self {
            registry,
            owner: std::borrow::Cow::Borrowed(owner),
            key: std::borrow::Cow::Borrowed(key.as_str()),
        }
    }
    pub fn registry(&self) -> crate::claims::RegistryKind {
        self.registry
    }
    pub fn owner(&self) -> &str {
        &self.owner
    }
    pub fn key(&self) -> &str {
        &self.key
    }
    pub fn callback_identity(&self) -> PluginCallbackIdentity {
        PluginCallbackIdentity::tool_provider(self.owner(), self.key())
    }
    /// The callback identity of this provider's reader: what a cell's read
    /// plan owes while it may still read through the provider.
    pub fn reads_identity(&self) -> PluginCallbackIdentity {
        PluginCallbackIdentity::tool_reads(self.owner(), self.key())
    }
}
impl std::fmt::Display for ToolProviderHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}:{}/{}", self.registry, self.owner, self.key)
    }
}
impl<'de> serde::Deserialize<'de> for ToolProviderHandle {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Recorded {
            registry: crate::claims::RegistryKind,
            owner: String,
            key: String,
        }
        let record = Recorded::deserialize(deserializer)?;
        if !ToolProviderKey::is_valid(&record.key) {
            return Err(serde::de::Error::custom(
                ToolProviderKeyError::InvalidGrammar { key: record.key },
            ));
        }
        Ok(Self {
            registry: record.registry,
            owner: std::borrow::Cow::Owned(record.owner),
            key: std::borrow::Cow::Owned(record.key),
        })
    }
}
/// A callback slot within one plugin, selected by its stable key.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct PluginCallbackIdentity {
    pub owner: String,
    pub key: String,
}

impl PluginCallbackIdentity {
    /// The identity of `owner`'s tool provider declared under `key`: the one
    /// identity a call binds to and dispatches through.
    #[must_use]
    pub fn tool_provider(owner: impl Into<String>, key: &str) -> Self {
        Self {
            owner: owner.into(),
            key: format!("{}:{key}", CallbackSlot::ToolProvider.key_prefix()),
        }
    }

    /// The identity of the reader `owner`'s provider `key` registered: the
    /// callback a cell's read plan owes for that provider's reads.
    pub fn tool_reads(owner: impl Into<String>, key: &str) -> Self {
        Self {
            owner: owner.into(),
            key: format!("{}:{key}", CallbackSlot::ToolReads.key_prefix()),
        }
    }

    /// The declared key of a tool provider identity, or `None` when this
    /// names another callback or a provider by anything but a declared key.
    #[must_use]
    pub fn tool_provider_key(&self) -> Option<&str> {
        self.key
            .strip_prefix(CallbackSlot::ToolProvider.key_prefix())
            .and_then(|rest| rest.strip_prefix(':'))
            .filter(|key| is_declared_tool_provider_key(key))
    }
}

impl std::fmt::Display for PluginCallbackIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}:{}", self.owner, self.key)
    }
}

/// Whether `key` is a declared tool provider key: 1 to 64 bytes of ASCII
/// letters, digits, `_`, `-` and `.`, beginning with a letter. A provider is
/// named by its author, never by its position, so no key is a number.
#[must_use]
pub fn is_declared_tool_provider_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    bytes.len() <= 64
        && bytes.first().is_some_and(u8::is_ascii_alphabetic)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

/// Decodes a recorded tool provider identity, refusing one that names its
/// provider by anything but a declared key (FIG-6073).
pub mod declared_tool_provider {
    use super::PluginCallbackIdentity;

    /// # Errors
    /// The identity does not decode, or names no declared tool provider.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<PluginCallbackIdentity, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let identity = <PluginCallbackIdentity as serde::Deserialize>::deserialize(deserializer)?;
        if identity.tool_provider_key().is_none() {
            return Err(serde::de::Error::custom(format!(
                "recorded tool provider `{}:{}` is not named by a declared provider key",
                identity.owner, identity.key
            )));
        }
        Ok(identity)
    }

    /// [`deserialize`] for an optional identity.
    pub mod option {
        use super::PluginCallbackIdentity;

        /// # Errors
        /// A present identity does not decode, or names no declared tool
        /// provider.
        pub fn deserialize<'de, D>(
            deserializer: D,
        ) -> Result<Option<PluginCallbackIdentity>, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            #[derive(serde::Deserialize)]
            struct Declared(
                #[serde(deserialize_with = "super::deserialize")] PluginCallbackIdentity,
            );
            Ok(
                <Option<Declared> as serde::Deserialize>::deserialize(deserializer)?
                    .map(|Declared(identity)| identity),
            )
        }
    }
}

/// The id a process component registers under. A `:` is refused: the fleet
/// writer record keys a component's config format as `component:<id>`.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
pub struct ProcessComponentId(String);

/// Why a process component id is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProcessComponentIdError {
    #[error("a process component id is empty")]
    Empty,
    #[error("process component id `{id}` contains `:`, which the fleet writer key reserves")]
    Colon { id: String },
}

impl ProcessComponentId {
    /// # Errors
    ///
    /// An empty id, or one containing `:`.
    pub fn new(id: impl Into<String>) -> Result<Self, ProcessComponentIdError> {
        let id = id.into();
        if id.is_empty() {
            return Err(ProcessComponentIdError::Empty);
        }
        if id.contains(':') {
            return Err(ProcessComponentIdError::Colon { id });
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The fleet writer-record key this component's state/config format is
    /// admitted under: `component:<id>`, never a session plugin's key.
    pub fn writer_key(&self) -> String {
        format!("component:{}", self.0)
    }
}

impl TryFrom<String> for ProcessComponentId {
    type Error = ProcessComponentIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ProcessComponentId> for String {
    fn from(id: ProcessComponentId) -> Self {
        id.0
    }
}

impl fmt::Display for ProcessComponentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The host's execution and retry contract for a dispatch grant's
/// discovery, recorded with the grant in the process environment before its
/// first resolution. Resolution and the matched tool have independent
/// bounds. As with ordinary tool bounds,
/// there is no runtime-selected ceiling; enclosing deadlines still clip it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeferredResolutionPolicy {
    pub execution: std::time::Duration,
    pub execution_policy: crate::ExecutionPolicy,
}

impl Default for DeferredResolutionPolicy {
    /// Sixty seconds, with three repeatable attempts and exponential backoff
    /// from 100 ms up to 1,000 ms.
    fn default() -> Self {
        Self {
            execution: std::time::Duration::from_secs(60),
            execution_policy: crate::ExecutionPolicy::repeatable(
                std::num::NonZeroU32::MIN.saturating_add(2),
                100,
                1_000,
            ),
        }
    }
}

/// A host-owned routing payload a resolved deferred match carries to its
/// provider: the provider whose declared formats read it, the format it is
/// written in and the payload. Readability is checked before the binding is
/// installed or a call is prepared, never by the provider's reader first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionBinding {
    pub codec: ProviderKey,
    pub format: FormatVersion,
    pub payload: serde_json::Value,
}

impl ExecutionBinding {
    /// Whether a codec that reads `readable` reads this binding: checked
    /// before the binding is installed or a call prepared (S8), so an
    /// unreadable binding never reaches the provider's reader.
    ///
    /// # Errors
    ///
    /// [`ExecutionBindingUnreadable`] naming the codec, the format and what
    /// it reads.
    pub fn check_readable(
        &self,
        readable: &[FormatVersion],
    ) -> Result<(), ExecutionBindingUnreadable> {
        if readable.contains(&self.format) {
            Ok(())
        } else {
            Err(ExecutionBindingUnreadable {
                codec: self.codec.clone(),
                format: self.format,
                readable: readable.to_vec(),
            })
        }
    }
}

/// A recorded execution binding its codec cannot read on this node.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[error(
    "execution binding of provider `{codec}` is in format {format}, which this node's provider \
     does not read (it reads {readable:?})"
)]
pub struct ExecutionBindingUnreadable {
    pub codec: ProviderKey,
    pub format: FormatVersion,
    pub readable: Vec<FormatVersion>,
}

/// Generates [`CallbackSlot`] from one table, so a slot cannot exist
/// without its key prefix, its state authority and its tracing scope.
macro_rules! callback_slots {
    ($($(#[$doc:meta])* $variant:ident => $prefix:literal, $authority:ident, $trace:ident;)+) => {
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

            /// Where the slot's callbacks stand in the trace: the span their
            /// tracing capability is bound at, or none.
            #[must_use]
            pub const fn trace_scope(self) -> TraceScopeOf {
                match self {
                    $(Self::$variant => TraceScopeOf::$trace,)+
                }
            }
        }
    };
}

/// Where a slot's callbacks stand in the trace, and so whether they get a
/// tracing capability and under which span. A column of the slot table: a
/// new slot cannot exist without a tracing decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TraceScopeOf {
    /// The `execute_tool` span of one admitted attempt; the body knows its
    /// call address.
    Attempt,
    /// The `execute_tool` span of a call, before or after its attempts.
    Call,
    /// The turn's `invoke_agent` span.
    Turn,
    /// The code cell's span while a cell runs, else the turn's.
    Executing,
    /// The `lash.plugin.invoke` producer of a host call.
    Producer,
    /// Pure, read-only or decision-only: no capability.
    Untraced,
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
    ToolProvider => "tool_provider", DecisionOnly, Attempt;
    /// A tool provider's reader: what answers its `Read` tools' calls in a
    /// code cell, owed while a cell's read plan names the provider.
    ToolReads => "tool_reads", DecisionOnly, Untraced;
    /// Trusted package advertisement, keyed by provider local key.
    PackageProvider => "package_provider", DecisionOnly, Untraced;
    /// Repeatable deferred discovery, keyed by provider local key.
    DeferredPackageProvider => "deferred_package_provider", DecisionOnly, Untraced;
    ToolCatalog => "tool_catalog", DecisionOnly, Untraced;
    BeforeTurn => "before_turn", Commands, Turn;
    /// Argument transforms, chained before provider preparation.
    ToolArgsTransform => "tool_args_transform", DecisionOnly, Call;
    /// Before-checks over the immutable prepared call.
    ToolArgsCheck => "tool_args_check", DecisionOnly, Call;
    /// Result transforms, chained over the original and preceding candidate.
    ToolResultTransform => "tool_result_transform", DecisionOnly, Call;
    /// The after-tool result check, on the Run's sequential path.
    ToolResultCheck => "tool_result_check", Commands, Call;
    AfterTurn => "after_turn", Commands, Turn;
    Checkpoint => "checkpoint", Commands, Turn;
    AssistantStream => "assistant_stream", DecisionOnly, Turn;
    AssistantResponse => "assistant_response", DecisionOnly, Turn;
    AssistantStreamFinished => "assistant_stream_finished", DecisionOnly, Turn;
    PresentationStep => "presentation_step", DecisionOnly, Untraced;
    PresentationPresenter => "presentation_presenter", DecisionOnly, Untraced;
    RuntimeEvent => "runtime_event", DecisionOnly, Untraced;
    Operation => "operation", DecisionOnly, Producer;
    /// Attachment-omission history policies (ADR 0133).
    AttachmentOmission => "attachment_omission", DecisionOnly, Untraced;
    ContextCompactor => "context_compactor", DecisionOnly, Turn;
    ContextPressure => "context_pressure", DecisionOnly, Turn;
    ProtocolSession => "protocol_session", DecisionOnly, Executing;
    ProtocolDriver => "protocol_driver", DecisionOnly, Executing;
    CodeExecutor => "code_executor", DecisionOnly, Executing;
    AssistantProseProjector => "assistant_prose_projector", DecisionOnly, Untraced;
    CellFailureFeedback => "cell_failure_feedback", DecisionOnly, Untraced;
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

/// Logical package provider identity, independent of its hosting registry.
pub use crate::package::ProviderName;
