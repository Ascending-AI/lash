//! Pure operation requirements and composition preflight refusals.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
pub enum RequiredPort {
    ProtocolSession,
    ProtocolDriver,
}
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
pub enum ServiceKind {
    TurnScope,
    CurrentSessionSnapshot,
    NamedSessionSnapshot,
    RecordedToolCatalog,
    CommittedTurnRead,
    SessionCreate,
    SessionGraphAppend,
    ProcessList,
    ProcessStart,
    ProcessRecordedStart,
    ProcessBoundStart,
    ProcessBoundCancel,
    ProcessAwait,
    ProcessCancel,
    ProcessTransfer,
    ProcessReleaseHold,
    ProcessAbandonHold,
    GrantedSessionReads,
    ProcessListForAttempt,
    ProcessStartFromRequest,
    ProcessValidateVisible,
    ProcessRecordedCancel,
}
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    thiserror::Error,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
pub enum PreflightIssue {
    #[error("plugin `{owner}` requires unavailable service {service:?}")]
    MissingService { owner: String, service: ServiceKind },
    #[error("missing required port {port:?}")]
    MissingPort { port: RequiredPort },
    #[error("duplicate port {port:?} supplied by {owners:?}")]
    DuplicatePort {
        port: RequiredPort,
        owners: Vec<String>,
    },
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
    pub(crate) const fn is_valid(key: &str) -> bool {
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
    registry: crate::obligations::RegistryKind,
    owner: std::borrow::Cow<'static, str>,
    key: std::borrow::Cow<'static, str>,
}
impl ToolProviderHandle {
    pub fn new(
        registry: crate::obligations::RegistryKind,
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
        registry: crate::obligations::RegistryKind,
        owner: &'static str,
        key: ToolProviderKey,
    ) -> Self {
        Self {
            registry,
            owner: std::borrow::Cow::Borrowed(owner),
            key: std::borrow::Cow::Borrowed(key.as_str()),
        }
    }
    pub fn registry(&self) -> crate::obligations::RegistryKind {
        self.registry
    }
    pub fn owner(&self) -> &str {
        &self.owner
    }
    pub fn key(&self) -> &str {
        &self.key
    }
    pub fn callback_identity(&self) -> crate::store::plugin_writers::PluginCallbackIdentity {
        crate::store::plugin_writers::PluginCallbackIdentity::tool_provider(
            self.owner(),
            self.key(),
        )
    }
    /// The callback identity of this provider's reader: what a cell's read
    /// plan owes while it may still read through the provider.
    pub fn reads_identity(&self) -> crate::store::plugin_writers::PluginCallbackIdentity {
        crate::store::plugin_writers::PluginCallbackIdentity::tool_reads(self.owner(), self.key())
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
            registry: crate::obligations::RegistryKind,
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
