//! Claim obligations (S7, FIG-6074): the callbacks an actor's outstanding
//! recorded work will call, as the tokens its `requires` carries.
//!
//! One token family names every recorded callback, qualified by the registry
//! that resolves it: a session's plugins or a process's components. A node
//! serves the token of every callback its installed registrations resolve,
//! without invoking any. A deployment may also serve a retired owner's
//! token to claim its outstanding work and refuse it before redelivery.
//! Otherwise, an actor naming an unresolved callback is claimed only for
//! cancellation. Component-format tokens are a separate family of the codec.
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
/// offers. Only this codec spells them, including the owner token a
/// deployment serves to refuse a retired owner's outstanding callbacks.
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

    /// A retired owner may be claimed to refuse its outstanding callbacks.
    /// The trailing slash distinguishes this from an installed callback.
    #[must_use]
    pub fn for_retired_owner(kind: RegistryKind, owner: &str) -> Self {
        Self(format!("callback:{}:{owner}/", kind.as_str()))
    }

    /// Whether the node installs this callback or declares its owner retired.
    #[must_use]
    pub fn is_served(required: &str, serves: &[String]) -> bool {
        serves.iter().any(|served| served == required)
            || required
                .strip_prefix("callback:")
                .and_then(|_| required.rsplit_once('/'))
                .is_some_and(|(owner, _)| {
                    serves.iter().any(|served| served == &format!("{owner}/"))
                })
    }

    /// The token of process component `component`'s state and config at `format`:
    /// `component:<id>@fmt<format>`. A process naming the component at that
    /// format requires it, and a node whose component reads it serves it.
    #[must_use]
    pub fn for_component(component: &ProcessComponentId, format: FormatVersion) -> Self {
        Self(format!("component:{component}@fmt{format}"))
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

/// A registry retirement refuses new admission or a recorded dependency.
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
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum RetirementRefusal {
    #[error("{registry:?} registration `{id}` is retired")]
    Admission { registry: RegistryKind, id: String },
    #[error("{registry:?} registration `{id}` is retired but serves `{effect}`")]
    Serves {
        registry: RegistryKind,
        id: String,
        effect: String,
    },
    #[error("{registry:?} registration `{id}` is retired but holds recorded state")]
    HasState { registry: RegistryKind, id: String },
}

impl RetirementRefusal {
    /// Retain the typed refusal through the runtime boundary.
    #[must_use]
    pub fn into_runtime_error(self) -> crate::RuntimeError {
        crate::RuntimeError::new(crate::RuntimeErrorCode::PluginRetired, self.to_string())
            .with_cause(crate::RuntimeErrorCause::PluginRetired {
                refusal: Box::new(self),
            })
    }
}
