//! Machine and execution claim debt; one token spelling for both registries.
use crate::providers::{FormatVersion, PluginCallbackIdentity, ProcessComponentId};
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

use crate::{FunctionId, package::ProviderName};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
/// Only pure natives and kernel versions required by selected executable reach.
pub struct NodeRequirements {
    pub natives: BTreeSet<FunctionId>,
    pub kernels: BTreeSet<u32>,
}
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
/// Exact callback identity qualified by the registry that hosts it.
pub struct CallbackRequirement {
    pub registry: RegistryKind,
    pub callback: PluginCallbackIdentity,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
/// Known machine/provider/codec/callback/component debt. Resolution adds target debt atomically before admission.
pub struct ClaimRequirements {
    pub machine: NodeRequirements,
    pub providers: BTreeSet<ProviderName>,
    pub codecs: BTreeSet<(ProviderName, FormatVersion)>,
    pub callbacks: BTreeSet<CallbackRequirement>,
    pub components: BTreeSet<(ProcessComponentId, FormatVersion)>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
/// Neutral machine and registered execution capabilities offered by one worker.
pub struct NodeCapabilities {
    pub machine: NodeRequirements,
    pub providers: BTreeSet<ProviderName>,
    pub codecs: BTreeSet<(ProviderName, FormatVersion)>,
    pub callbacks: BTreeSet<CallbackRequirement>,
    pub components: BTreeSet<(ProcessComponentId, FormatVersion)>,
}
