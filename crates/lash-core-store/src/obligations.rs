//! Claim obligations (S7, FIG-6074): the callbacks an actor's outstanding
//! recorded work will call, as the tokens its `requires` carries.
//!
//! One token family names every recorded callback, qualified by the registry
//! that resolves it: a session's plugins or a process's components. A node
//! serves the token of every callback its installed registrations resolve,
//! without invoking any; an actor whose outstanding work names a callback
//! the node does not resolve is claimed only for its cancel. Config-format
//! tokens are a separate family of the same codec.
//!
//! [`ObligationToken`] is the one encoder: a requirement and a served
//! registration are spelled by it and nowhere else.

use crate::process_env::ProcessComponentId;
use crate::store::plugin_writers::PluginCallbackIdentity;
use lash_core_ids::FormatVersion;

/// The registry a recorded callback resolves through.
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
#[serde(rename_all = "snake_case")]
pub enum RegistryKind {
    /// A session's plugins.
    Session,
    /// A process's environment components.
    Process,
}

impl RegistryKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Process => "process",
        }
    }
}

/// One claim token: what an actor's `requires` holds and a node's `serves`
/// offers. The scheduler compares tokens as opaque strings; only this type
/// spells them.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObligationToken(String);

impl ObligationToken {
    /// The token of `callback`, resolved through `kind`'s registry:
    /// `callback:<kind>:<owner>/<key>`.
    #[must_use]
    pub fn for_callback(kind: RegistryKind, callback: &PluginCallbackIdentity) -> Self {
        Self(format!(
            "callback:{}:{}/{}",
            kind.as_str(),
            callback.owner,
            callback.key
        ))
    }

    /// The token of process component `component`'s config at `format`:
    /// `component:<id>@cfg<format>`. A process naming the component at that
    /// format requires it, and a node whose component reads it serves it.
    #[must_use]
    pub fn for_config(component: &ProcessComponentId, format: FormatVersion) -> Self {
        Self(format!("component:{component}@cfg{format}"))
    }

    /// The token as the scheduler stores it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The token as the scheduler stores it.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for ObligationToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<ObligationToken> for String {
    fn from(token: ObligationToken) -> Self {
        token.0
    }
}

impl PartialEq<ObligationToken> for String {
    fn eq(&self, token: &ObligationToken) -> bool {
        *self == token.0
    }
}
