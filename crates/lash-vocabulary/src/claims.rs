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

    /// Canonical claim tokens, including exact registry-qualified callbacks.
    pub fn for_requirements(requirements: &ClaimRequirements) -> impl Iterator<Item = Self> {
        claim_tokens(requirements).into_iter()
    }

    /// Canonical tokens served by one node. There is no fleet surface gate.
    pub fn for_capabilities(capabilities: &NodeCapabilities) -> impl Iterator<Item = Self> {
        claim_tokens(&ClaimRequirements {
            machine: capabilities.machine.clone(),
            providers: capabilities.providers.clone(),
            codecs: capabilities.codecs.clone(),
            callbacks: capabilities.callbacks.clone(),
            components: capabilities.components.clone(),
        })
        .into_iter()
    }

    /// Read all recognized requirement families, ignoring unrelated actor tokens.
    #[must_use]
    pub fn requirements_of<'a>(tokens: impl IntoIterator<Item = &'a str>) -> ClaimRequirements {
        let mut requirements = ClaimRequirements::default();
        for token in tokens {
            if let Some(function) = token.strip_prefix(NATIVE) {
                if let Ok(function) = crate::FunctionId::parse(function) {
                    requirements.machine.natives.insert(function);
                }
            } else if let Some(version) = token.strip_prefix(KERNEL) {
                if let Ok(version) = version.parse() {
                    requirements.machine.kernels.insert(version);
                }
            } else if let Some(provider) = token.strip_prefix(PROVIDER).and_then(parse_provider) {
                requirements.providers.insert(provider);
            } else if let Some((provider, version)) =
                token.strip_prefix(CODEC).and_then(|s| s.rsplit_once('@'))
            {
                if let (Some(provider), Ok(version)) =
                    (parse_provider(provider), version.parse::<u32>())
                    && let Some(version) = FormatVersion::new(version)
                {
                    requirements.codecs.insert((provider, version));
                }
            } else if let Some(callback) = token.strip_prefix("callback:") {
                if let Some((registry, callback)) = callback.split_once(':')
                    && let Some(registry) = match registry {
                        "session" => Some(RegistryKind::Session),
                        "process" => Some(RegistryKind::Process),
                        _ => None,
                    }
                    && let Some((owner, key)) = callback.split_once('/')
                    && !owner.is_empty()
                    && !key.is_empty()
                {
                    requirements.callbacks.insert(CallbackRequirement {
                        registry,
                        callback: PluginCallbackIdentity {
                            owner: owner.into(),
                            key: key.into(),
                        },
                    });
                }
            } else if let Some((component, version)) = token
                .strip_prefix("component:")
                .and_then(|s| s.rsplit_once("@fmt"))
                && let (Ok(component), Ok(version)) =
                    (ProcessComponentId::new(component), version.parse::<u32>())
                && let Some(version) = FormatVersion::new(version)
            {
                requirements.components.insert((component, version));
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

fn parse_provider(value: &str) -> Option<ProviderName> {
    let (owner, key) = value.split_once('/')?;
    (!owner.is_empty() && crate::providers::is_declared_tool_provider_key(key)).then(|| {
        ProviderName {
            owner: owner.into(),
            key: key.into(),
        }
    })
}

fn claim_tokens(requirements: &ClaimRequirements) -> BTreeSet<ObligationToken> {
    requirements
        .machine
        .natives
        .iter()
        .map(|id| ObligationToken(format!("{NATIVE}{id}")))
        .chain(
            requirements
                .machine
                .kernels
                .iter()
                .map(|version| ObligationToken(format!("{KERNEL}{version}"))),
        )
        .chain(requirements.providers.iter().map(|provider| {
            ObligationToken(format!("{PROVIDER}{}/{}", provider.owner, provider.key))
        }))
        .chain(requirements.codecs.iter().map(|(provider, version)| {
            ObligationToken(format!(
                "{CODEC}{}/{}@{version}",
                provider.owner, provider.key
            ))
        }))
        .chain(
            requirements.callbacks.iter().map(|required| {
                ObligationToken::for_callback(required.registry, &required.callback)
            }),
        )
        .chain(
            requirements
                .components
                .iter()
                .map(|(component, format)| ObligationToken::for_component(component, *format)),
        )
        .collect()
}

impl NodeRequirements {
    /// Union of selected linked reach. Extra unused Exports add no debt.
    pub fn extend(&mut self, other: &Self) {
        self.natives.extend(other.natives.iter().copied());
        self.kernels.extend(other.kernels.iter().copied());
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.natives.is_empty() && self.kernels.is_empty()
    }
}

impl ClaimRequirements {
    /// Add debt without retiring another recorded use's requirements.
    pub fn extend(&mut self, other: &Self) {
        self.machine.extend(&other.machine);
        self.providers.extend(other.providers.iter().cloned());
        self.codecs.extend(other.codecs.iter().cloned());
        self.callbacks.extend(other.callbacks.iter().cloned());
        self.components.extend(other.components.iter().cloned());
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.machine.is_empty()
            && self.providers.is_empty()
            && self.codecs.is_empty()
            && self.callbacks.is_empty()
            && self.components.is_empty()
    }

    /// Work parks while any known requirement is absent on the claiming node.
    #[must_use]
    pub fn met_by(&self, node: &NodeCapabilities) -> bool {
        self.unmet_by(node).is_empty()
    }

    #[must_use]
    pub fn unmet_by(&self, node: &NodeCapabilities) -> Self {
        fn missing<T: Ord + Clone>(required: &BTreeSet<T>, offered: &BTreeSet<T>) -> BTreeSet<T> {
            required.difference(offered).cloned().collect()
        }
        Self {
            machine: NodeRequirements {
                natives: missing(&self.machine.natives, &node.machine.natives),
                kernels: missing(&self.machine.kernels, &node.machine.kernels),
            },
            providers: missing(&self.providers, &node.providers),
            codecs: missing(&self.codecs, &node.codecs),
            callbacks: missing(&self.callbacks, &node.callbacks),
            components: missing(&self.components, &node.components),
        }
    }
}

impl NodeCapabilities {
    pub fn extend(&mut self, other: &Self) {
        self.machine.extend(&other.machine);
        self.providers.extend(other.providers.iter().cloned());
        self.codecs.extend(other.codecs.iter().cloned());
        self.callbacks.extend(other.callbacks.iter().cloned());
        self.components.extend(other.components.iter().cloned());
    }
}

impl std::fmt::Display for ClaimRequirements {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tokens: Vec<_> = self.tokens().collect();
        if tokens.is_empty() {
            f.write_str("nothing")
        } else {
            f.write_str(&tokens.join(", "))
        }
    }
}

impl ClaimRequirements {
    pub fn tokens(&self) -> impl Iterator<Item = String> {
        ObligationToken::for_requirements(self).map(ObligationToken::into_string)
    }
}

impl crate::tokens::TokenCodec for NodeRequirements {
    fn encode(&self, out: &mut crate::tokens::TokenWriter) {
        out.seq(2);
        self.natives.iter().copied().collect::<Vec<_>>().encode(out);
        self.kernels.iter().copied().collect::<Vec<_>>().encode(out);
    }

    fn decode(
        input: &mut crate::tokens::TokenReader<'_>,
    ) -> Result<Self, crate::tokens::TokenError> {
        input.expect_struct(2)?;
        let natives = Vec::<FunctionId>::decode(input)?;
        crate::tokens::check_ascending(&natives)?;
        let kernels = Vec::<u32>::decode(input)?;
        crate::tokens::check_ascending(&kernels)?;
        Ok(Self {
            natives: natives.into_iter().collect(),
            kernels: kernels.into_iter().collect(),
        })
    }
}

impl crate::tokens::TokenCodec for ClaimRequirements {
    fn encode(&self, out: &mut crate::tokens::TokenWriter) {
        self.tokens().collect::<Vec<_>>().encode(out);
    }

    fn decode(
        input: &mut crate::tokens::TokenReader<'_>,
    ) -> Result<Self, crate::tokens::TokenError> {
        let tokens = Vec::<String>::decode(input)?;
        crate::tokens::check_ascending(&tokens)?;
        let requirements = ObligationToken::requirements_of(tokens.iter().map(String::as_str));
        if requirements.tokens().collect::<Vec<_>>() != tokens {
            return Err(crate::tokens::TokenError::Invalid(
                "unrecognized or noncanonical claim requirement".into(),
            ));
        }
        Ok(requirements)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L-CLAIM-STAGED: adding a target keeps known resolver debt and exact callbacks.
    #[test]
    fn staged_target_debt_keeps_resolver_debt_and_registry_identity() {
        let provider = |key: &str| ProviderName {
            owner: "owner".into(),
            key: key.into(),
        };
        let mut base = ClaimRequirements::default();
        base.providers.insert(provider("resolver"));
        base.callbacks.insert(CallbackRequirement {
            registry: RegistryKind::Process,
            callback: PluginCallbackIdentity {
                owner: "owner".into(),
                key: "resolver:lookup".into(),
            },
        });
        base.components.insert((
            ProcessComponentId::new("owner").unwrap(),
            FormatVersion::ONE,
        ));
        let mut target = ClaimRequirements::default();
        target.providers.insert(provider("target"));
        target
            .codecs
            .insert((provider("codec"), FormatVersion::ONE));
        target.callbacks.insert(CallbackRequirement {
            registry: RegistryKind::Session,
            callback: PluginCallbackIdentity::tool_reads("owner", "target"),
        });
        let resolver_tokens: BTreeSet<_> = base.tokens().collect();
        base.extend(&target);
        let tokens: Vec<_> = base.tokens().collect();
        assert!(resolver_tokens.is_subset(&base.tokens().collect()));
        assert_eq!(
            ObligationToken::requirements_of(tokens.iter().map(String::as_str)),
            base
        );
        assert!(!base.met_by(&NodeCapabilities::default()));
        let node = NodeCapabilities {
            machine: base.machine.clone(),
            providers: base.providers.clone(),
            codecs: base.codecs.clone(),
            callbacks: base.callbacks.clone(),
            components: base.components.clone(),
        };
        assert!(base.met_by(&node));
        assert_eq!(
            tokens,
            ObligationToken::for_capabilities(&node)
                .map(ObligationToken::into_string)
                .collect::<Vec<_>>()
        );
        assert!(ObligationToken::is_served(
            "callback:process:owner/resolver:lookup",
            &["callback:process:owner/".into()]
        ));
        assert!(!ObligationToken::is_served(
            "callback:process:owner/resolver:lookup",
            &["callback:session:owner/".into()]
        ));
    }
}
