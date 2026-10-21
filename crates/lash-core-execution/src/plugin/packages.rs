//! Trusted package advertisement, publication, starts and deferred discovery.
//! Package visibility controls Imports; only explicit tool registration exposes
//! model tools. Resolution outcomes are committed before target execution.
use super::{OwnerConfigView, PluginRegistrar};
use crate::deferred_provider::DeferredResolveContext;
use lash_core_store::artifact_referrer::ReferrerClaim;
use lash_core_store::{
    process_env::ProcessEnv, resolutions::ResolutionSeed, tool_surface::OperationRoute,
};
use lash_execution_contracts::{
    PluginError,
    process::{EngineSettings, linking::*},
};
use lash_vocabulary::package::PublicationRecord;
use lash_vocabulary::{
    ToolDefinition,
    identity::ProcessId,
    names::*,
    package::*,
    providers::{CallbackSlot, PluginCallbackIdentity},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::Arc};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Package data plus route candidates. Package membership never automatically enters the Tool Catalog.
pub struct PackageContribution {
    pub publication: PublicationRecord,
    pub advertisement: PackageAdvertisement,
    pub routes: Vec<AdvertisedRoute>,
}
pub use lash_core_store::tool_surface::{AdvertisedRoute, RecordedPackages};
#[async_trait::async_trait]
/// Trusted package advertisement from recorded owner config; independent of model-tool registration.
pub trait PackageProvider: Send + Sync {
    fn roots(&self) -> BTreeSet<Segment>;
    fn advertise(
        &self,
        config: &OwnerConfigView,
        context: &PublicationContext,
    ) -> Result<Vec<PackageContribution>, PluginError>;
    /// Retain the exact advertisement under the caller's existing cleanup
    /// owner before a round exposes it. Return authenticated provenance intact.
    async fn hold(
        &self,
        claim: &ReferrerClaim,
        advertisement: &PackageAdvertisement,
    ) -> Result<(), EnginePortError>;
}
/// A publication for an operation-only or deferred advertisement. Code
/// providers must retain their graph and supply its original provenance.
pub fn operation_publication(
    context: &PublicationContext,
    advertisement: &PackageAdvertisement,
) -> Result<PublicationRecord, EnginePortError> {
    let content = match advertisement {
        PackageAdvertisement::Enumerated(package) => {
            if package
                .members
                .values()
                .any(|member| matches!(member.kind, MemberKind::Export { .. }))
            {
                return Err(EnginePortError::Unsupported {
                    operation: "code advertisement requires explicit provenance".into(),
                });
            }
            PublishedContent::Package(package.identity())
        }
        PackageAdvertisement::Deferred(package) => PublishedContent::Deferred(package.clone()),
    };
    Ok(PublicationRecord {
        context: context.clone(),
        path: advertisement_path(advertisement).clone(),
        content,
        code_links: BTreeSet::new(),
        imported_publications: Vec::new(),
    })
}

#[derive(Debug)]
/// Prepared namespace and code. Publication validates ownership, Exports, Brands and full provenance holds.
pub struct PublishPackage {
    pub package: Package,
    pub code: Vec<Fulfilled>,
}
#[async_trait::async_trait]
/// Trusted immutable publication with complete content/provenance retention before references are exposed.
pub trait PackagePublisher: Send + Sync {
    async fn publish(
        &self,
        claim: &ReferrerClaim,
        context: &PublicationContext,
        request: PublishPackage,
    ) -> Result<PublicationRecord, EnginePortError>;
}
#[derive(Debug)]
/// Trusted host-selected start, arguments, complete environment/settings and explicit initial pins.
pub struct StartLinked {
    pub start: LinkedStart,
    pub args: serde_json::Map<String, serde_json::Value>,
    pub env: ProcessEnv,
    pub settings: Option<EngineSettings>,
    pub initial_resolutions: Vec<lash_core_store::resolutions::ResolutionSeed>,
}
#[async_trait::async_trait]
/// Host starts after linked reach, routes, hosting, arguments and initial pins validate.
pub trait LinkedProcessStarts: Send + Sync {
    async fn start(&self, request: StartLinked) -> Result<ProcessId, EnginePortError>;
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// Discovery outcome. Only validated recorded winners execute; a miss/refusal is pinned before guest delivery.
pub enum DeferredAnswer {
    Resolved {
        definition: Box<ToolDefinition>,
        route: Box<OperationRoute>,
    },
    Refused {
        note: String,
    },
    Unavailable {
        note: Option<String>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
/// Retryable infrastructure discovery failure under the grant's resolution policy; no negative resolution is stored.
pub struct ResolverFault {
    pub message: String,
}
#[async_trait::async_trait]
/// Read-only repeatable discovery over pinned grant scope/config, never a live-session fallback.
pub trait DeferredPackageProvider: Send + Sync {
    /// Certify this explicitly registered implementation can host the exact
    /// pinned origin scope/config. Matching a provider name is insufficient.
    fn validate_grant(
        &self,
        grant: &DeferredGrant,
        registry: super::RegistryKind,
    ) -> Result<(), AdmissionRefusal>;

    async fn resolve(
        &self,
        cx: &DeferredResolveContext<'_>,
        grant: &DeferredGrant,
        requested: &QualifiedName,
    ) -> Result<DeferredAnswer, ResolverFault>;
}

impl std::fmt::Display for ResolverFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ResolverFault {}
/// The existing process registrar, specialized for static component contributions.
pub type ProcessComponentRegistrar = PluginRegistrar<(), super::registries::Process>;

impl super::ExecutionComposition {
    pub fn tool_registry(&self) -> &Arc<crate::ToolRegistry> {
        &self.tool_registry
    }

    /// Resolve only an explicitly registered logical resolver. Executors do
    /// not imply discovery, and a session registration is never a process twin.
    pub fn deferred_package_provider(
        &self,
        name: &ProviderName,
    ) -> Option<&Arc<dyn DeferredPackageProvider>> {
        self.deferred_package_providers
            .iter()
            .find(|registered| {
                registered.identity.owner() == name.owner && registered.identity.key() == name.key
            })
            .map(|registered| &registered.hook)
    }

    pub fn deferred_package_callback(&self, name: &ProviderName) -> Option<PluginCallbackIdentity> {
        self.deferred_package_providers
            .iter()
            .find(|registered| {
                registered.identity.owner() == name.owner && registered.identity.key() == name.key
            })
            .map(|registered| PluginCallbackIdentity {
                owner: registered.identity.owner().to_owned(),
                key: format!(
                    "{}:{}",
                    CallbackSlot::DeferredPackageProvider.key_prefix(),
                    registered.identity.key()
                ),
            })
    }

    /// Validate known resolver debt before the first claim. This inspects
    /// declared format support; it never invokes a codec on unsupported bytes.
    pub fn validate_deferred_grant(
        &self,
        grant: &DeferredGrant,
        registry: super::RegistryKind,
    ) -> Result<lash_vocabulary::claims::ClaimRequirements, EnginePortError> {
        use lash_vocabulary::claims::{CallbackRequirement, ClaimRequirements};
        let unavailable = || {
            EnginePortError::Refused(AdmissionRefusal::ProviderNotHostable {
                provider: grant.provider.clone(),
                registry,
            })
        };
        if grant.signature.result != lash_vocabulary::Type::Json
            || grant
                .signature
                .params
                .iter()
                .any(|param| param.ty != lash_vocabulary::Type::Json)
        {
            return Err(EnginePortError::Refused(
                AdmissionRefusal::InvalidDocument {
                    message: "deferred Imports require Json arguments and result".into(),
                },
            ));
        }
        let registration = self
            .deferred_package_providers
            .iter()
            .find(|registered| {
                registered.identity.registry() == registry
                    && registered.identity.owner() == grant.provider.owner
                    && registered.identity.key() == grant.provider.key
            })
            .ok_or_else(unavailable)?;
        registration
            .hook
            .validate_grant(grant, registry)
            .map_err(EnginePortError::Refused)?;
        let callback = PluginCallbackIdentity {
            owner: registration.identity.owner().to_owned(),
            key: format!(
                "{}:{}",
                CallbackSlot::DeferredPackageProvider.key_prefix(),
                registration.identity.key()
            ),
        };
        let mut requirements = ClaimRequirements::default();
        requirements.providers.insert(grant.provider.clone());
        requirements
            .callbacks
            .insert(CallbackRequirement { registry, callback });
        if let Some(component) = &grant.config.component {
            if component.as_str() != grant.provider.owner {
                return Err(unavailable());
            }
            requirements
                .components
                .insert((component.clone(), grant.config.format));
        } else if registry == super::RegistryKind::Process {
            return Err(unavailable());
        }
        if let Some(payload) = &grant.config.payload {
            let key = self
                .tool_registry()
                .keys()
                .find(|key| {
                    key.registry() == registry
                        && key.owner() == payload.codec.owner
                        && key.key() == payload.codec.key
                })
                .ok_or_else(unavailable)?;
            let provider = self.tool_registry().provider(key).ok_or_else(unavailable)?;
            if !provider
                .readable_binding_formats()
                .contains(&payload.format)
            {
                return Err(unavailable());
            }
            requirements
                .codecs
                .insert((payload.codec.clone(), payload.format));
            requirements.callbacks.insert(CallbackRequirement {
                registry,
                callback: key.callback_identity(),
            });
        }
        Ok(requirements)
    }

    /// Check a discovered target before committing its immutable outcome.
    /// Decoder availability is a claim requirement, not a reason to discover again.
    pub fn validate_resolved_route(
        &self,
        grant: &DeferredGrant,
        requested: &QualifiedName,
        definition: &ToolDefinition,
        route: &OperationRoute,
        registry: super::RegistryKind,
    ) -> Result<lash_vocabulary::claims::ClaimRequirements, EnginePortError> {
        use lash_core_store::tool_surface::RouteSource;
        use lash_vocabulary::claims::CallbackRequirement;
        let refuse = || {
            EnginePortError::Refused(AdmissionRefusal::NotVisible {
                name: requested.clone(),
            })
        };
        // A terminal outcome retains its executor. Resolver registration and
        // discovery configuration are needed only while a use is unresolved.
        let mut requirements = lash_vocabulary::claims::ClaimRequirements::default();
        let manifest = &definition.manifest;
        if !requested
            .package
            .as_slice()
            .starts_with(grant.path.as_slice())
            || requested.type_member.is_some()
            || manifest.operation.package != requested.package
            || manifest.operation.name != requested.member
            || route.provider.registry() != registry
            || route.provider.owner() != grant.provider.owner
            || route.tool != manifest.id
            || route.policy != manifest.execution_policy
            || route.policy == lash_vocabulary::ExecutionPolicy::Read
            || route.projection != manifest.argument_projection
            || !matches!(&route.source, RouteSource::Resolved { definition: retained } if **retained == *definition)
            || !self.tool_registry().serves(&route.provider)
        {
            return Err(refuse());
        }
        if let Some(refusal) = manifest.read_refusal() {
            return Err(EnginePortError::Refused(
                AdmissionRefusal::InvalidDocument {
                    message: refusal.to_string(),
                },
            ));
        }
        let name = ProviderName {
            owner: route.provider.owner().to_owned(),
            key: route.provider.key().to_string(),
        };
        requirements.providers.insert(name);
        requirements.callbacks.insert(CallbackRequirement {
            registry,
            callback: route.provider.callback_identity(),
        });
        if let Some(payload) = &route.payload {
            if payload.codec.registry() != registry || !self.tool_registry().serves(&payload.codec)
            {
                return Err(refuse());
            }
            requirements.codecs.insert((
                ProviderName {
                    owner: payload.codec.owner().to_owned(),
                    key: payload.codec.key().to_string(),
                },
                payload.format,
            ));
            requirements.callbacks.insert(CallbackRequirement {
                registry,
                callback: payload.codec.callback_identity(),
            });
        }
        Ok(requirements)
    }

    /// Retain the exact round namespace before its write exposes references.
    /// The supplied claim already has a cleanup owner; this method creates none.
    pub async fn retain_packages(
        &self,
        claim: &ReferrerClaim,
        packages: &RecordedPackages,
        store: &dyn lash_durable::packages::PackageStore,
        recorded: bool,
    ) -> Result<(), EnginePortError> {
        for accepted in &packages.snapshot.advertisements {
            if recorded || matches!(accepted.publication.context.scope, PublicationScope::Host) {
                store
                    .acquire_publication(claim, &accepted.publication.key())
                    .await
                    .map_err(package_store_error)?;
                continue;
            }
            self.hold_package(claim, accepted).await?;
            if let PackageAdvertisement::Enumerated(package) = &accepted.advertisement {
                store
                    .put_package(
                        claim,
                        &lash_core_store::packages::PackageRecord {
                            id: package.identity(),
                            package: package.clone(),
                        },
                    )
                    .await
                    .map_err(package_store_error)?;
            }
            store
                .publish(claim, &accepted.publication)
                .await
                .map_err(package_store_error)?;
        }
        Ok(())
    }

    /// Hold using the same registered publisher that supplied the accepted
    /// advertisement. A recorded publication bypasses live advertisement.
    pub async fn hold_package(
        &self,
        claim: &ReferrerClaim,
        accepted: &AcceptedAdvertisement,
    ) -> Result<(), EnginePortError> {
        let registered = self
            .package_providers
            .iter()
            .find(|registered| {
                accepted.publication.context.publisher.0
                    == format!(
                        "{}/{}",
                        registered.identity.owner(),
                        registered.identity.key()
                    )
            })
            .ok_or_else(|| {
                EnginePortError::Refused(AdmissionRefusal::NotOwner {
                    path: advertisement_path(&accepted.advertisement).clone(),
                })
            })?;
        registered.hook.hold(claim, &accepted.advertisement).await
    }

    /// Accept one coherent advertisement from committed config. No content
    /// installation or resolver invocation happens for operation-only packages.
    pub fn advertise_packages(
        &self,
        config: &OwnerConfigView,
        host: &RecordedPackages,
        domain: &PublicationDomain,
        scope: PublicationScope,
    ) -> Result<RecordedPackages, PluginError> {
        let mut checked = NamespaceSnapshot::default();
        for prior in &host.snapshot.advertisements {
            if prior.publication.context.domain != *domain
                || prior.publication.context.scope != PublicationScope::Host
            {
                return Err(package_refusal(AdmissionRefusal::NotOwner {
                    path: advertisement_path(&prior.advertisement).clone(),
                }));
            }
            validate_advertisement(&checked, &prior.publication.context, &prior.advertisement)
                .map_err(package_refusal)?;
            checked.advertisements.push(prior.clone());
        }
        let mut accepted = host.clone();
        let mut roots = std::collections::BTreeMap::new();
        for registered in &self.package_providers {
            config.validate_provider(&registered.identity)?;
            let publisher = PublisherInstance(format!(
                "{}/{}",
                registered.identity.owner(),
                registered.identity.key()
            ));
            let owned = registered.hook.roots();
            for root in &owned {
                if roots.insert(root.clone(), publisher.clone()).is_some() {
                    return Err(package_refusal(AdmissionRefusal::NotOwner {
                        path: Vec1::new(root.clone()),
                    }));
                }
            }
            for root in owned {
                let context = PublicationContext {
                    domain: domain.clone(),
                    scope: scope.clone(),
                    publisher: publisher.clone(),
                    root,
                };
                for contribution in registered.hook.advertise(config, &context)? {
                    let path = advertisement_path(&contribution.advertisement);
                    let content = match &contribution.advertisement {
                        PackageAdvertisement::Enumerated(package) => {
                            PublishedContent::Package(package.identity())
                        }
                        PackageAdvertisement::Deferred(package) => {
                            PublishedContent::Deferred(package.clone())
                        }
                    };
                    if contribution.publication.context != context
                        || contribution.publication.path != *path
                        || contribution.publication.content != content
                    {
                        return Err(package_refusal(AdmissionRefusal::NotOwner {
                            path: path.clone(),
                        }));
                    }
                    validate_advertisement(
                        &accepted.snapshot,
                        &context,
                        &contribution.advertisement,
                    )
                    .map_err(package_refusal)?;
                    for candidate in &contribution.routes {
                        let manifest = &candidate.definition.manifest;
                        if candidate.operation.package != *path
                            || candidate.operation != manifest.operation
                            || candidate.route.provider.registry() != registered.identity.registry()
                            || candidate.route.provider.owner() != registered.identity.owner()
                            || candidate.route.tool != manifest.id
                            || candidate.route.policy != manifest.execution_policy
                            || candidate.route.projection != manifest.argument_projection
                            || !self.tool_registry().serves(&candidate.route.provider)
                            || candidate.route.policy == lash_vocabulary::ExecutionPolicy::Read
                                && self
                                    .tool_registry()
                                    .reads(&candidate.route.provider)
                                    .is_none()
                        {
                            return Err(package_refusal(AdmissionRefusal::InvalidDocument {
                                message: format!(
                                    "invalid advertised route for {}",
                                    candidate.operation.effect_name()
                                ),
                            }));
                        }
                    }
                    accepted
                        .snapshot
                        .advertisements
                        .push(AcceptedAdvertisement {
                            publication: contribution.publication.clone(),
                            advertisement: contribution.advertisement,
                        });
                    accepted.routes.extend(contribution.routes);
                }
            }
        }
        Ok(accepted)
    }
}

/// Preserve deterministic package and fence causes across the host boundary.
pub fn package_store_error(error: lash_durable::DurableError) -> EnginePortError {
    match error {
        lash_durable::DurableError::Package(refusal) => EnginePortError::PackageRefused(refusal),
        lash_durable::DurableError::OwnershipLost(_) => EnginePortError::Fenced,
        error => EnginePortError::Unavailable {
            message: error.to_string(),
        },
    }
}

fn package_refusal(refusal: AdmissionRefusal) -> PluginError {
    PluginError::Package(Box::new(EnginePortError::Refused(refusal)))
}

fn advertisement_path(advertisement: &PackageAdvertisement) -> &PackagePath {
    match advertisement {
        PackageAdvertisement::Enumerated(package) => &package.path,
        PackageAdvertisement::Deferred(package) => &package.path,
    }
}

/// Compare segments, including member leaves. Textual prefixes are not paths.
pub fn validate_advertisement(
    snapshot: &NamespaceSnapshot,
    context: &PublicationContext,
    advertisement: &PackageAdvertisement,
) -> Result<(), AdmissionRefusal> {
    let path = advertisement_path(advertisement);
    if path.as_slice()[0] != context.root {
        return Err(AdmissionRefusal::NotOwner { path: path.clone() });
    }
    for prior in &snapshot.advertisements {
        let previous = advertisement_path(&prior.advertisement);
        let left = path.as_slice();
        let right = previous.as_slice();
        let conflict = left == right
            || matches!(advertisement, PackageAdvertisement::Deferred(_))
                && right.starts_with(left)
            || matches!(prior.advertisement, PackageAdvertisement::Deferred(_))
                && left.starts_with(right)
            || match &prior.advertisement {
                PackageAdvertisement::Enumerated(package) if left.starts_with(right) => {
                    package.members.contains_key(&left[right.len()])
                }
                _ => false,
            }
            || match advertisement {
                PackageAdvertisement::Enumerated(package) if right.starts_with(left) => {
                    package.members.contains_key(&right[left.len()])
                }
                _ => false,
            };
        if conflict {
            return Err(AdmissionRefusal::PathConflict { path: path.clone() });
        }
        if context.domain != prior.publication.context.domain
            || context.root == prior.publication.context.root
                && (context.publisher != prior.publication.context.publisher
                    || context.scope != prior.publication.context.scope)
        {
            return Err(AdmissionRefusal::NotOwner { path: path.clone() });
        }
    }
    Ok(())
}

/// Trusted deployment data. Published code keeps its original LinkIds here;
/// rebuilding a session never re-fulfils configured package code.
#[derive(Clone, Debug)]
pub struct HostPackageNamespace {
    pub domain: PublicationDomain,
    pub packages: RecordedPackages,
}
impl Default for HostPackageNamespace {
    fn default() -> Self {
        Self {
            domain: PublicationDomain("ephemeral".into()),
            packages: RecordedPackages::default(),
        }
    }
}
impl super::PluginHost {
    /// Configure the durable domain and the host's accepted namespace.
    pub fn with_package_namespace(mut self, namespace: HostPackageNamespace) -> Self {
        self.package_namespace = Arc::new(namespace);
        self
    }
}
impl super::PluginSession {
    pub fn advertised_packages(
        &self,
        config: &super::AdmittedPluginConfig,
    ) -> Result<RecordedPackages, PluginError> {
        let Some(session) = self.owner.session_id() else {
            return Err(package_refusal(AdmissionRefusal::InvalidDocument {
                message: "session advertisement requires a session owner".into(),
            }));
        };
        let mut packages = self.composition().advertise_packages(
            &OwnerConfigView::session(config.clone()),
            &self.host.package_namespace.packages,
            &self.host.package_namespace.domain,
            PublicationScope::Session(session.clone()),
        )?;
        packages.consumer = Some(PublicationContext {
            domain: self.host.package_namespace.domain.clone(),
            scope: PublicationScope::Session(session.clone()),
            publisher: PublisherInstance(format!("session:{session}")),
            root: Segment::new("session").unwrap_or_else(|_| unreachable!("static root")),
        });
        Ok(packages)
    }
}

/// Project explicit tool definitions into operation packages. Callers register
/// this provider themselves; package advertisement never registers model tools.
pub fn operation_packages(
    provider: &lash_vocabulary::providers::ToolProviderHandle,
    definitions: impl IntoIterator<Item = ToolDefinition>,
    payload: impl Fn(&ToolDefinition) -> Option<lash_vocabulary::providers::ExecutionBinding>,
    context: &PublicationContext,
) -> Result<Vec<PackageContribution>, PluginError> {
    use lash_core_store::tool_surface::RouteSource;
    use std::collections::BTreeMap;
    let mut packages: BTreeMap<PackagePath, (BTreeMap<Segment, Member>, Vec<AdvertisedRoute>)> =
        BTreeMap::new();
    for definition in definitions {
        let manifest = &definition.manifest;
        let operation = manifest.operation.clone();
        let route = OperationRoute {
            tool: manifest.id.clone(),
            provider: provider.clone(),
            policy: manifest.execution_policy,
            projection: manifest.argument_projection.clone(),
            payload: payload(&definition),
            source: RouteSource::Advertised,
        };
        let offer = OperationOffer {
            requirement: lash_vocabulary::operations::OperationRequirement {
                signature: lash_vocabulary::json_schema::tool_signature(
                    &definition.contract.input_schema.canonical,
                    &definition.contract.output_schema.canonical,
                ),
                contract: definition.contract_id(),
            },
            intents: manifest
                .declaration()
                .intents
                .iter()
                .map(|intent| Intent(intent.as_str().to_owned()))
                .collect(),
            controls: manifest.declaration().controls.clone(),
            mode: if manifest.execution_policy == lash_vocabulary::ExecutionPolicy::Read {
                OperationMode::Read
            } else {
                OperationMode::Waited
            },
        };
        let (members, routes) = packages.entry(operation.package.clone()).or_default();
        if members
            .insert(
                operation.name.clone(),
                Member {
                    visibility: Visibility::Public,
                    kind: MemberKind::Operation(offer),
                },
            )
            .is_some()
        {
            return Err(package_refusal(AdmissionRefusal::PathConflict {
                path: operation.package,
            }));
        }
        routes.push(AdvertisedRoute {
            operation,
            definition,
            route,
        });
    }
    packages
        .into_iter()
        .filter(|(path, _)| path.as_slice()[0] == context.root)
        .map(|(path, (members, routes))| {
            let advertisement = PackageAdvertisement::Enumerated(Package { path, members });
            Ok(PackageContribution {
                publication: operation_publication(context, &advertisement)
                    .map_err(|error| PluginError::Package(Box::new(error)))?,
                advertisement,
                routes,
            })
        })
        .collect::<Result<Vec<_>, PluginError>>()
}

#[cfg(test)]
mod tests;

impl super::ExecutionComposition {
    /// Select exactly one serving candidate per reached OperationUse. Contract,
    /// provider and tool identity stay attached to the candidate; no name map
    /// can overwrite another contract under the same operation name.
    pub fn execution_environment(
        &self,
        inspection: &LinkInspection,
        packages: &RecordedPackages,
        publications: Vec<PublicationRecord>,
        registry: super::RegistryKind,
        selected: &std::collections::BTreeMap<
            OperationUse,
            (
                lash_vocabulary::providers::ToolProviderHandle,
                lash_vocabulary::ToolId,
            ),
        >,
    ) -> Result<lash_core_store::tool_surface::ExecutionEnvironment, EnginePortError> {
        use lash_core_store::tool_surface::{ExecutionEnvironment, RoutedOperation, UseBinding};
        use lash_vocabulary::claims::{CallbackRequirement, ClaimRequirements};
        let mut requirements = ClaimRequirements {
            machine: inspection.machine.clone(),
            ..Default::default()
        };
        let mut uses = std::collections::BTreeMap::new();
        for (use_, need) in &inspection.operations {
            let binding = match need {
                OperationNeed::Deferred { grant, .. } => {
                    let known = self.validate_deferred_grant(grant, registry)?;
                    requirements.providers.extend(known.providers);
                    requirements.codecs.extend(known.codecs);
                    requirements.callbacks.extend(known.callbacks);
                    requirements.components.extend(known.components);
                    UseBinding::Deferred((**grant).clone())
                }
                OperationNeed::Fixed { name, requirement } => {
                    let mut candidates = packages.routes.iter().filter(|candidate| {
                        candidate.operation.package == name.package
                            && candidate.operation.name == name.member
                            && candidate.definition.contract_id() == requirement.contract
                            && candidate.route.provider.registry() == registry
                            && selected.get(use_).is_none_or(|(provider, tool)| {
                                *provider == candidate.route.provider
                                    && *tool == candidate.route.tool
                            })
                    });
                    let candidate = candidates.next().ok_or({
                        EnginePortError::Refused(AdmissionRefusal::MissingRoute { use_: *use_ })
                    })?;
                    if candidates.next().is_some() {
                        return Err(EnginePortError::Refused(
                            AdmissionRefusal::InvalidDocument {
                                message: format!(
                                    "operation use {use_:?} has ambiguous serving routes; select a provider and tool explicitly"
                                ),
                            },
                        ));
                    }
                    let route = &candidate.route;
                    if !self.tool_registry().serves(&route.provider) {
                        return Err(EnginePortError::Refused(
                            AdmissionRefusal::ProviderNotHostable {
                                provider: ProviderName {
                                    owner: route.provider.owner().into(),
                                    key: route.provider.key().into(),
                                },
                                registry,
                            },
                        ));
                    }
                    let mode = if route.policy == lash_vocabulary::ExecutionPolicy::Read {
                        OperationMode::Read
                    } else {
                        OperationMode::Waited
                    };
                    let offer = packages
                        .snapshot
                        .advertisements
                        .iter()
                        .find_map(|accepted| match &accepted.advertisement {
                            PackageAdvertisement::Enumerated(package)
                                if package.path == name.package =>
                            {
                                match &package.members.get(&name.member)?.kind {
                                    MemberKind::Operation(offer)
                                        if offer.requirement.contract == requirement.contract =>
                                    {
                                        Some(offer.clone())
                                    }
                                    _ => None,
                                }
                            }
                            _ => None,
                        })
                        .unwrap_or_else(|| OperationOffer {
                            requirement: lash_vocabulary::OperationRequirement {
                                signature: lash_vocabulary::json_schema::tool_signature(
                                    &candidate.definition.contract.input_schema.canonical,
                                    &candidate.definition.contract.output_schema.canonical,
                                ),
                                contract: candidate.definition.contract_id(),
                            },
                            mode,
                            intents: candidate
                                .definition
                                .manifest
                                .declaration()
                                .intents
                                .iter()
                                .map(|intent| Intent(intent.as_str().to_owned()))
                                .collect(),
                            controls: candidate.definition.manifest.declaration().controls.clone(),
                        });
                    if offer.mode != mode
                        || !lash_vocabulary::schema::signature_serves(
                            &offer.requirement.signature,
                            &requirement.signature,
                        )
                    {
                        return Err(EnginePortError::Refused(
                            AdmissionRefusal::InvalidDocument {
                                message: format!("route does not serve operation use {use_:?}"),
                            },
                        ));
                    }
                    requirements.providers.insert(ProviderName {
                        owner: route.provider.owner().into(),
                        key: route.provider.key().into(),
                    });
                    let callback = if mode == OperationMode::Read {
                        if self.tool_registry().reads(&route.provider).is_none() {
                            return Err(EnginePortError::Refused(AdmissionRefusal::MissingRoute {
                                use_: *use_,
                            }));
                        }
                        route.provider.reads_identity()
                    } else {
                        route.provider.callback_identity()
                    };
                    requirements
                        .callbacks
                        .insert(CallbackRequirement { registry, callback });
                    if let Some(payload) = &route.payload {
                        if payload.codec.registry() != registry
                            || !self.tool_registry().serves(&payload.codec)
                        {
                            return Err(EnginePortError::Refused(AdmissionRefusal::MissingRoute {
                                use_: *use_,
                            }));
                        }
                        requirements.codecs.insert((
                            ProviderName {
                                owner: payload.codec.owner().into(),
                                key: payload.codec.key().into(),
                            },
                            payload.format,
                        ));
                        requirements.callbacks.insert(CallbackRequirement {
                            registry,
                            callback: payload.codec.callback_identity(),
                        });
                    }
                    UseBinding::Fixed(RoutedOperation {
                        requirement: requirement.clone(),
                        offer,
                        route: route.clone(),
                    })
                }
            };
            uses.insert(*use_, binding);
        }
        Ok(ExecutionEnvironment {
            authored: BTreeSet::new(),
            publications,
            roots: [inspection.link].into(),
            uses,
            registry,
            requirements,
        })
    }
}
