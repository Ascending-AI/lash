//! Claim obligations (S7, FIG-6074): the callbacks an actor's outstanding
//! recorded work will call, as the tokens its `requires` carries.
//!
//! One token family names every recorded callback, qualified by the registry
//! that resolves it: a session's plugins or a process's components. A node
//! serves the token of every callback its installed registrations resolve,
//! without invoking any. A deployment may also serve a retired owner's
//! token to claim its outstanding work and refuse it before redelivery.
//! Otherwise, an actor naming an unresolved callback is claimed only for
//! cancellation. Component-format tokens are a separate family of the codec,
//! and so are the hard requirements an execution's record names: the
//! natives, kernels, providers and codec versions its code and links need,
//! which a node serves from the capabilities it advertises (FIG-6993).
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

/// The hard requirement families' prefixes (FIG-6993).
const NATIVE: &str = "native:";
const KERNEL: &str = "kernel:";
const PROVIDER: &str = "provider:";
const CODEC: &str = "codec:";

/// The hard tokens of four sets of natives, kernels, providers and codecs.
fn hard<'a>(
    natives: &'a std::collections::BTreeSet<lash_kernel_doc::FunctionId>,
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
        requirements: &lash_kernel_doc::ExecutionRequirements,
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
        capabilities: &lash_kernel_doc::NodeCapabilities,
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
    ) -> lash_kernel_doc::ExecutionRequirements {
        let mut requirements = lash_kernel_doc::ExecutionRequirements::default();
        for token in tokens {
            if let Some(function) = token.strip_prefix(NATIVE) {
                if let Ok(function) = lash_kernel_doc::FunctionId::parse(function) {
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
