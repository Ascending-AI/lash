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

/// The hard requirement families' prefixes (FIG-6993).
const NATIVE: &str = "native:";
const KERNEL: &str = "kernel:";
const PROVIDER: &str = "provider:";
const CODEC: &str = "codec:";

/// The hard tokens of four sets of natives, kernels, providers and codecs.
fn hard<'a>(
    natives: &'a std::collections::BTreeSet<crate::FunctionId>,
    kernels: &'a std::collections::BTreeSet<u32>,
    providers: &'a std::collections::BTreeSet<String>,
    codecs: &'a std::collections::BTreeSet<(String, u32)>,
) -> impl Iterator<Item = ObligationToken> + 'a {
    let natives = natives
        .iter()
        .map(|function| ObligationToken(format!("{NATIVE}{function}")));
    let kernels = kernels
        .iter()
        .map(|version| ObligationToken(format!("{KERNEL}{version}")));
    let providers = providers
        .iter()
        .map(|provider| ObligationToken(format!("{PROVIDER}{provider}")));
    let codecs = codecs
        .iter()
        .map(|(provider, version)| ObligationToken(format!("{CODEC}{provider}@{version}")));
    natives.chain(kernels).chain(providers).chain(codecs)
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

    /// The tokens of a hard requirements record (FIG-6993): `native:<id>`
    /// for each native-only function the code reached, `kernel:<n>` for
    /// each kernel version, `provider:<key>` for each linked provider and
    /// `codec:<key>@<n>` for each codec version that reads a link's
    /// payload. Work carrying them runs only on a node whose capabilities
    /// serve each ([`Self::for_capabilities`]).
    pub fn for_requirements(
        requirements: &crate::legacy_capabilities::ExecutionRequirements,
    ) -> impl Iterator<Item = Self> + '_ {
        hard(
            &requirements.natives,
            &requirements.kernels,
            &requirements.providers,
            &requirements.codecs,
        )
    }

    /// The tokens a node offering `capabilities` serves: the same spelling
    /// as [`Self::for_requirements`], so a record is met exactly when each
    /// of its tokens is served.
    pub fn for_capabilities(
        capabilities: &crate::legacy_capabilities::NodeCapabilities,
    ) -> impl Iterator<Item = Self> + '_ {
        hard(
            &capabilities.natives,
            &capabilities.kernels,
            &capabilities.providers,
            &capabilities.codecs,
        )
    }

    /// The hard requirements `tokens` name, read back: every other token is
    /// left out, and so is one that does not read.
    #[must_use]
    pub fn requirements_of<'a>(
        tokens: impl IntoIterator<Item = &'a str>,
    ) -> crate::legacy_capabilities::ExecutionRequirements {
        let mut requirements = crate::legacy_capabilities::ExecutionRequirements::default();
        for token in tokens {
            if let Some(function) = token.strip_prefix(NATIVE) {
                if let Ok(function) = crate::FunctionId::parse(function) {
                    requirements.natives.insert(function);
                }
            } else if let Some(version) = token.strip_prefix(KERNEL) {
                if let Ok(version) = version.parse() {
                    requirements.kernels.insert(version);
                }
            } else if let Some(provider) = token.strip_prefix(PROVIDER) {
                requirements.providers.insert(provider.to_owned());
            } else if let Some((provider, version)) = token
                .strip_prefix(CODEC)
                .and_then(|codec| codec.rsplit_once('@'))
                && let Ok(version) = version.parse()
            {
                requirements.codecs.insert((provider.to_owned(), version));
            }
        }
        requirements
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
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Only pure natives and kernel versions required by selected executable reach.
pub struct NodeRequirements {
    pub natives: BTreeSet<FunctionId>,
    pub kernels: BTreeSet<u32>,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Exact callback identity qualified by the registry that hosts it.
pub struct CallbackRequirement {
    pub registry: RegistryKind,
    pub callback: PluginCallbackIdentity,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Known machine/provider/codec/callback/component debt. Resolution adds target debt atomically before admission.
pub struct ClaimRequirements {
    pub machine: NodeRequirements,
    pub providers: BTreeSet<ProviderName>,
    pub codecs: BTreeSet<(ProviderName, FormatVersion)>,
    pub callbacks: BTreeSet<CallbackRequirement>,
    pub components: BTreeSet<(ProcessComponentId, FormatVersion)>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Neutral machine and registered execution capabilities offered by one worker.
pub struct NodeCapabilities {
    pub machine: NodeRequirements,
    pub providers: BTreeSet<ProviderName>,
    pub codecs: BTreeSet<(ProviderName, FormatVersion)>,
    pub callbacks: BTreeSet<CallbackRequirement>,
    pub components: BTreeSet<(ProcessComponentId, FormatVersion)>,
}
