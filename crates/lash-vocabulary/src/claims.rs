//! Machine and execution claim debt; one token spelling for both registries.
use lash_kernel_value::claims::NodeRequirements;
use {
    crate::providers::PluginCallbackIdentity, crate::providers::ToolProviderHandle,
    lash_kernel_value::providers::FormatVersion, lash_kernel_value::providers::ProcessComponentId,
};

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

/// Machine requirement prefixes and member tags for registry-owned provider
/// and codec obligations (FIG-6993, FIG-7266).
const NATIVE: &str = "native:";

const ARCHIVE: &str = "helper-archive:";
const OUTSIDE: &str = "helper-outside:";

const LIBRARY: &str = "helper-library:";

const KERNEL: &str = "kernel:";

const PROVIDER: &str = "provider:";

const CODEC: &str = "codec:";

/// One claim token: what an actor's `requires` holds and a node's `serves`
/// offers. Only this codec spells them, including the owner token a
/// deployment serves to refuse a retired owner's outstanding callbacks.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObligationToken(String);

fn archive_token(token: &str) -> Option<(lash_kernel_value::HelperLibraryId, u32)> {
    let (archive, generation) = token.strip_prefix(ARCHIVE)?.split_once(':')?;
    let archive = archive.parse().ok()?;
    let parsed: u32 = generation.parse().ok()?;
    (generation == format!("{parsed:010}")).then_some((archive, parsed))
}

fn outside_token(
    token: &str,
) -> Option<(
    lash_kernel_value::HelperLibraryId,
    BTreeSet<lash_kernel_value::FunctionId>,
)> {
    let (release, functions) = token.strip_prefix(OUTSIDE)?.split_once(':')?;
    let release = release.parse().ok()?;
    let functions = if functions.is_empty() {
        BTreeSet::new()
    } else {
        functions
            .split(',')
            .map(lash_kernel_value::FunctionId::parse)
            .collect::<Result<_, _>>()
            .ok()?
    };
    Some((release, functions))
}

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
            || archive_token(required).is_some_and(|(archive, generation)| {
                serves
                    .iter()
                    .filter_map(|served| archive_token(served))
                    .any(|(held, available)| held == archive && available >= generation)
            })
            || outside_token(required).is_some_and(|(release, _)| {
                let token = format!("{LIBRARY}{release}");
                serves.iter().any(|served| served == &token)
            })
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
            if let Some((archive, generation)) = archive_token(token) {
                requirements
                    .machine
                    .helpers
                    .archives_mut()
                    .entry(archive)
                    .and_modify(|held| *held = (*held).max(generation))
                    .or_insert(generation);
            } else if let Some((release, functions)) = outside_token(token) {
                requirements
                    .machine
                    .helpers
                    .outside_mut()
                    .entry(release)
                    .or_default()
                    .extend(functions);
            } else if let Some(function) = token.strip_prefix(NATIVE) {
                if let Ok(function) = lash_kernel_value::FunctionId::parse(function) {
                    requirements.machine.natives.insert(function);
                }
            } else if let Some(library) = token.strip_prefix(LIBRARY) {
                if let Ok(library) = library.parse::<lash_kernel_value::HelperLibraryId>() {
                    requirements.machine.libraries.insert(library);
                }
            } else if let Some(version) = token.strip_prefix(KERNEL) {
                if let Ok(version) = version.parse() {
                    requirements.machine.kernels.insert(version);
                }
            } else if let Some(callback) = token.strip_prefix("callback:") {
                if let Some((registry, callback)) = callback.split_once(':')
                    && let Some(registry) = match registry {
                        "session" => Some(RegistryKind::Session),
                        "process" => Some(RegistryKind::Process),
                        _ => None,
                    }
                    && let Some((owner, key)) = callback.rsplit_once('/')
                    && !owner.is_empty()
                    && !key.is_empty()
                {
                    if let Some(key) = key.strip_prefix(PROVIDER) {
                        if let Some(provider) = parse_provider(registry, owner, key) {
                            requirements.providers.insert(provider);
                        }
                    } else if let Some((key, version)) = key
                        .strip_prefix(CODEC)
                        .and_then(|codec| codec.rsplit_once('@'))
                    {
                        if let (Some(provider), Ok(version)) =
                            (parse_provider(registry, owner, key), version.parse::<u32>())
                            && let Some(version) = FormatVersion::new(version)
                        {
                            requirements.codecs.insert((provider, version));
                        }
                    } else {
                        requirements.callbacks.insert(CallbackRequirement {
                            registry,
                            callback: PluginCallbackIdentity {
                                owner: owner.into(),
                                key: key.into(),
                            },
                        });
                    }
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

use lash_kernel_value::package::ProviderName;

use serde::{Deserialize, Serialize};

use std::collections::BTreeSet;

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
/// Known machine/provider/codec/callback/component debt. Provider and codec
/// handles retain the registry and registration owner. Resolution adds target
/// debt atomically before admission.
pub struct ClaimRequirements {
    pub machine: NodeRequirements,
    pub providers: BTreeSet<ToolProviderHandle>,
    pub codecs: BTreeSet<(ToolProviderHandle, FormatVersion)>,
    pub callbacks: BTreeSet<CallbackRequirement>,
    pub components: BTreeSet<(ProcessComponentId, FormatVersion)>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
/// Neutral machine and registry-qualified execution capabilities offered by one worker.
pub struct NodeCapabilities {
    pub machine: NodeRequirements,
    pub providers: BTreeSet<ToolProviderHandle>,
    pub codecs: BTreeSet<(ToolProviderHandle, FormatVersion)>,
    pub callbacks: BTreeSet<CallbackRequirement>,
    pub components: BTreeSet<(ProcessComponentId, FormatVersion)>,
}

fn parse_provider(registry: RegistryKind, owner: &str, key: &str) -> Option<ToolProviderHandle> {
    ToolProviderHandle::from_name(
        registry,
        &ProviderName {
            owner: owner.into(),
            key: key.into(),
        },
    )
    .ok()
}

/// A provider or codec debt belongs to the registration's exact registry
/// and owner, as callback debt does. Retirement settles that one owner
/// through the existing owner predicate, without advertising a live provider.
fn provider_obligation(provider: &ToolProviderHandle, key: String) -> ObligationToken {
    ObligationToken::for_callback(
        provider.registry(),
        &PluginCallbackIdentity {
            owner: provider.owner().into(),
            key,
        },
    )
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
                .helpers
                .archives()
                .iter()
                .map(|(archive, generation)| {
                    ObligationToken(format!("{ARCHIVE}{archive}:{generation:010}"))
                }),
        )
        .chain(
            requirements
                .machine
                .helpers
                .outside()
                .iter()
                .map(|(release, functions)| {
                    let functions = functions
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(",");
                    ObligationToken(format!("{OUTSIDE}{release}:{functions}"))
                }),
        )
        .chain(
            requirements
                .machine
                .libraries
                .iter()
                .map(|id| ObligationToken(format!("{LIBRARY}{id}"))),
        )
        .chain(
            requirements
                .machine
                .kernels
                .iter()
                .map(|version| ObligationToken(format!("{KERNEL}{version}"))),
        )
        .chain(
            requirements.providers.iter().map(|provider| {
                provider_obligation(provider, format!("{PROVIDER}{}", provider.key()))
            }),
        )
        .chain(requirements.codecs.iter().map(|(provider, version)| {
            provider_obligation(provider, format!("{CODEC}{}@{version}", provider.key()))
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
                helpers: self
                    .machine
                    .helpers
                    .unmet_by(&node.machine.helpers, &node.machine.libraries),
                libraries: missing(&self.machine.libraries, &node.machine.libraries),
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

impl lash_kernel_value::tokens::TokenCodec for ClaimRequirements {
    fn encode(&self, out: &mut lash_kernel_value::tokens::TokenWriter) {
        self.tokens().collect::<Vec<_>>().encode(out);
    }

    fn decode(
        input: &mut lash_kernel_value::tokens::TokenReader<'_>,
    ) -> Result<Self, lash_kernel_value::tokens::TokenError> {
        let tokens = Vec::<String>::decode(input)?;
        lash_kernel_value::tokens::check_ascending(&tokens)?;
        let requirements = ObligationToken::requirements_of(tokens.iter().map(String::as_str));
        if requirements.tokens().collect::<Vec<_>>() != tokens {
            return Err(lash_kernel_value::tokens::TokenError::Invalid(
                "unrecognized or noncanonical claim requirement".into(),
            ));
        }
        Ok(requirements)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reusing a generation's claim debt shares its helper map; adding reach
    /// to a later generation must preserve the earlier debt.
    #[test]
    fn generation_claim_snapshots_share_helper_debt_until_reach_changes() {
        let archive = lash_kernel_value::HelperLibraryId::of_members([]);
        let debt = |generation| ClaimRequirements {
            machine: NodeRequirements {
                helpers: lash_kernel_value::claims::HelperRequirements::new(
                    [(archive, generation)].into(),
                    Default::default(),
                ),
                ..Default::default()
            },
            ..Default::default()
        };
        let original = debt(1);
        let wire = serde_json::json!({"archives": {archive.to_string(): 1}, "outside": {}});
        assert_eq!(
            serde_json::to_value(&original.machine.helpers).unwrap(),
            wire
        );
        let schema = serde_json::to_value(schemars::schema_for!(
            lash_kernel_value::claims::HelperRequirements
        ))
        .unwrap();
        assert!(jsonschema::is_valid(&schema, &wire));
        let mut reused = original.clone();
        assert!(std::ptr::eq(
            original.machine.helpers.archives().get(&archive).unwrap(),
            reused.machine.helpers.archives().get(&archive).unwrap(),
        ));
        reused.extend(&debt(2));
        assert_eq!(original.machine.helpers.archives()[&archive], 1);
        assert_eq!(reused.machine.helpers.archives()[&archive], 2);
    }

    /// Archive generations are monotone; outside hashes keep their exact release coverage.
    #[test]
    fn helper_claims_compare_generations_and_retain_outside_hashes() {
        let archive = lash_kernel_value::HelperLibraryId::of_members([]);
        let function = lash_kernel_value::FunctionId::from_bytes([81; 32]);
        let release = lash_kernel_value::HelperLibraryId::of_members([function]);
        let requirements = ClaimRequirements {
            machine: NodeRequirements {
                helpers: lash_kernel_value::claims::HelperRequirements::new(
                    [(archive, 2)].into(),
                    [(release, [function].into())].into(),
                ),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut node = NodeCapabilities {
            machine: NodeRequirements {
                helpers: lash_kernel_value::claims::HelperRequirements::new(
                    [(archive, 3)].into(),
                    Default::default(),
                ),
                libraries: [release].into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let required: Vec<_> = requirements.tokens().collect();
        assert_eq!(required.len(), 2);
        assert_eq!(
            ObligationToken::requirements_of(required.iter().map(String::as_str)),
            requirements
        );
        let served: Vec<_> = ObligationToken::for_capabilities(&node)
            .map(String::from)
            .collect();
        assert!(
            required
                .iter()
                .all(|token| ObligationToken::is_served(token, &served))
        );
        assert!(requirements.met_by(&node));
        node.machine.helpers.archives_mut().insert(archive, 1);
        let served: Vec<_> = ObligationToken::for_capabilities(&node)
            .map(String::from)
            .collect();
        assert!(!ObligationToken::is_served(
            &format!("helper-archive:{archive}:0000000002"),
            &served
        ));
        assert_eq!(
            *requirements.unmet_by(&node).machine.helpers.archives(),
            [(archive, 2)].into()
        );
        assert!(!ObligationToken::is_served(
            &format!("helper-archive:{archive}:000000000x"),
            &served
        ));
        node.machine.libraries.clear();
        assert_eq!(
            *requirements.unmet_by(&node).machine.helpers.outside(),
            [(release, [function].into())].into()
        );
    }

    /// RETIRED-PROVIDER-DEBT (FIG-7266): retirement admits only the exact
    /// registry owner's provider and codec debts, so recovery can refuse
    /// them before delivery. Other owners and registries still need a host.
    #[test]
    fn retirement_settles_exact_registry_provider_and_codec_debt() {
        let owner = "retired/nested";
        let requirements = ClaimRequirements {
            providers: [ToolProviderHandle::new(
                RegistryKind::Session,
                owner,
                crate::providers::ToolProviderKey::new("tools").unwrap(),
            )]
            .into(),
            codecs: [(
                ToolProviderHandle::new(
                    RegistryKind::Session,
                    owner,
                    crate::providers::ToolProviderKey::new("tools").unwrap(),
                ),
                FormatVersion::ONE,
            )]
            .into(),
            callbacks: [CallbackRequirement {
                registry: RegistryKind::Session,
                callback: PluginCallbackIdentity::tool_provider(owner, "tools"),
            }]
            .into(),
            ..Default::default()
        };
        let tokens: Vec<_> = requirements.tokens().collect();
        let retired =
            [ObligationToken::for_retired_owner(RegistryKind::Session, owner).into_string()];
        assert!(
            tokens
                .iter()
                .all(|token| ObligationToken::is_served(token, &retired))
        );
        assert_eq!(
            ObligationToken::requirements_of(tokens.iter().map(String::as_str)),
            requirements
        );
        for (registry, other) in [
            (RegistryKind::Process, owner),
            (RegistryKind::Session, "retired"),
            (RegistryKind::Session, "retired/nested-other"),
        ] {
            let other_retirement =
                [ObligationToken::for_retired_owner(registry, other).into_string()];
            assert!(
                tokens
                    .iter()
                    .all(|token| !ObligationToken::is_served(token, &other_retirement))
            );
        }
    }

    /// FIG-7354: naming another image or serving only its native tokens
    /// cannot discharge an exact helper-library claim.
    #[test]
    fn a_claim_requires_its_exact_helper_library() {
        let library = lash_kernel_value::HelperLibraryId::from_bytes([1; 32]);
        let other = lash_kernel_value::HelperLibraryId::from_bytes([2; 32]);
        let token = format!("helper-library:{library}");
        let required = ObligationToken::requirements_of([token.as_str()]);
        let mut node = NodeCapabilities::default();
        assert!(!required.met_by(&node));
        node.machine.libraries.insert(other);
        assert!(!required.met_by(&node));
        node.machine.libraries.insert(library);
        assert!(required.met_by(&node));
        assert_eq!(required.tokens().collect::<Vec<_>>(), vec![token]);
        assert_eq!(
            lash_kernel_value::tokens::decode_body::<ClaimRequirements>(
                &lash_kernel_value::tokens::encode_body(&required)
            )
            .unwrap(),
            required
        );
    }

    /// L-CLAIM-STAGED: adding a target keeps known resolver debt and exact callbacks.
    #[test]
    fn staged_target_debt_keeps_resolver_debt_and_registry_identity() {
        let provider = |key: &'static str| {
            ToolProviderHandle::new(
                RegistryKind::Process,
                "owner",
                crate::providers::ToolProviderKey::new(key).unwrap(),
            )
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
