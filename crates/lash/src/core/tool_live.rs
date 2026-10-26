use super::*;
use std::collections::{BTreeMap, BTreeSet};

use lash_core::plugin::packages::{
    PackageContribution, PackageProvider, operation_environment_fragment, operation_publication,
};
use lash_kernel_value::names::Segment;
use {
    lash_kernel_value::package::PackageAdvertisement,
    lash_kernel_value::package::PublicationContext,
};

/// Providers sharing a root publish together; runner identities stay independent.
pub(super) fn tool_plugins(
    providers: Vec<(lash_core::plugin::ToolProviderKey, Arc<dyn ToolProvider>)>,
) -> Result<Vec<Arc<dyn PluginFactory>>> {
    let mut factories: Vec<Arc<dyn PluginFactory>> = Vec::new();
    let mut pending_providers = BTreeMap::new();
    let mut root_members = BTreeMap::<Segment, BTreeSet<_>>::new();
    for (key, provider) in providers {
        let roots: BTreeSet<_> = provider
            .tool_manifests(&lash_core::OwnerConfigView::default())?
            .into_iter()
            .map(|manifest| manifest.operation.package.as_slice()[0].clone())
            .collect();
        for root in &roots {
            root_members.entry(root.clone()).or_default().insert(key);
        }
        let publication = lash_tool_support::ToolPackageProvider::new(
            key.as_str(),
            key,
            roots,
            Arc::clone(&provider),
        );
        pending_providers.insert(key, (provider, publication));
    }
    while let Some(publisher) = pending_providers.keys().next().copied() {
        let mut pending = vec![publisher];
        let mut runners = Vec::new();
        let mut publications = Vec::new();
        while let Some(member) = pending.pop() {
            let Some((provider, publication)) = pending_providers.remove(&member) else {
                continue;
            };
            for root in publication.roots(&lash_core::OwnerConfigView::default()) {
                if let Some(peers) = root_members.remove(&root) {
                    pending.extend(peers);
                }
            }
            runners.push((member, provider));
            publications.push(publication);
        }
        let packages: Arc<dyn PackageProvider> = Arc::new(BuilderPackages {
            providers: publications,
        });
        for (key, provider) in runners {
            let mut spec = PluginSpec::new().with_tool_provider(key, provider);
            if key == publisher {
                spec = spec.with_package_provider(key, Arc::clone(&packages));
            }
            factories.push(Arc::new(StaticPluginFactory::new(
                lash_core::plugin::PluginDeclaration::initial(key.as_str()),
                spec,
            )));
        }
    }
    Ok(factories)
}

struct BuilderPackages {
    providers: Vec<lash_tool_support::ToolPackageProvider>,
}
#[async_trait::async_trait]
impl PackageProvider for BuilderPackages {
    fn environment_tool_sources(
        &self,
        publisher: &lash_core::plugin::ToolProviderHandle,
    ) -> BTreeSet<lash_core::plugin::ToolProviderHandle> {
        self.providers
            .iter()
            .map(|provider| {
                let key = provider.provider_key();
                lash_core::plugin::ToolProviderHandle::new(publisher.registry(), key.as_str(), key)
            })
            .collect()
    }
    fn environment_mode(&self) -> lash_core_store::environment::SourceMode {
        // Package facts are a pure projection of the declared tool observations.
        lash_core_store::environment::SourceMode::Immutable
    }
    fn environment_fragment(
        &self,
        config: &lash_core::OwnerConfigView,
        domain: &lash_kernel_value::package::PublicationDomain,
        scope: &lash_kernel_value::package::PublicationScope,
        key: &lash_core::plugin::ToolProviderHandle,
        tools: Option<&[lash_core::SurfaceEntry]>,
    ) -> std::result::Result<
        lash_core_store::environment::EnvironmentFragment,
        lash_execution_contracts::PluginError,
    > {
        let tools = tools.ok_or_else(|| {
            lash_execution_contracts::PluginError::Registration(
                "builder packages require their declared tool sources".into(),
            )
        })?;
        operation_environment_fragment(tools, self.roots(config), config, domain, scope, key)
    }
    fn roots(&self, config: &lash_core::OwnerConfigView) -> BTreeSet<Segment> {
        self.providers
            .iter()
            .flat_map(|provider| provider.roots(config))
            .collect()
    }
    fn advertise(
        &self,
        config: &lash_core::OwnerConfigView,
        context: &PublicationContext,
    ) -> std::result::Result<Vec<PackageContribution>, lash_execution_contracts::PluginError> {
        use lash_execution_contracts::process::linking::{AdmissionRefusal, EnginePortError};
        use std::collections::btree_map::Entry;
        let mut packages: BTreeMap<_, PackageContribution> = BTreeMap::new();
        for provider in &self.providers {
            for contribution in provider.advertise(config, context)? {
                let PackageAdvertisement::Enumerated(package) = &contribution.advertisement else {
                    unreachable!("tool providers advertise operation packages")
                };
                match packages.entry(package.path.clone()) {
                    Entry::Vacant(entry) => {
                        entry.insert(contribution);
                    }
                    Entry::Occupied(mut entry) => {
                        let combined = entry.get_mut();
                        let PackageAdvertisement::Enumerated(target) = &mut combined.advertisement
                        else {
                            unreachable!("tool providers advertise operation packages")
                        };
                        for (name, member) in &package.members {
                            if target
                                .members
                                .insert(name.clone(), member.clone())
                                .is_some()
                            {
                                return Err(lash_execution_contracts::PluginError::Package(
                                    Box::new(EnginePortError::Refused(
                                        AdmissionRefusal::PathConflict {
                                            path: package.path.clone(),
                                        },
                                    )),
                                ));
                            }
                        }
                        combined.routes.extend(contribution.routes);
                        combined.publication =
                            operation_publication(context, &combined.advertisement)?;
                    }
                }
            }
        }
        Ok(packages.into_values().collect())
    }
    async fn hold(
        &self,
        _: &lash_core_store::artifact_referrer::ReferrerClaim,
        _: &[&PackageAdvertisement],
    ) -> std::result::Result<(), lash_execution_contracts::process::linking::EnginePortError> {
        Ok(())
    }
}

impl LashCore {
    pub fn tool_live(&self) -> &lash_core::ToolLive {
        &self.env.core.tool_live
    }
}
impl LashCoreBuilder {
    pub fn tool_live_policy(mut self, policy: lash_core::ToolLivePolicy) -> Self {
        self.tool_live_policy = policy;
        self
    }
}
