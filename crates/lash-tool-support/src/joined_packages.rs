use crate::ToolPackageProvider;
use lash_core::plugin::packages::{
    PackageContribution, PackageProvider, operation_environment_fragment, operation_publication,
};
use lash_kernel_value::{
    names::Segment,
    package::{PackageAdvertisement, PublicationContext},
};
use std::collections::{BTreeMap, BTreeSet};

/// One package publisher over several explicitly registered tool sources.
/// Each source keeps its runner, manifest, payload and execution policy. Sources
/// must be registered in the publisher's registry. Duplicate operation names
/// are refused with `AdmissionRefusal::PathConflict`; conflicting publication
/// ownership is refused with `AdmissionRefusal::NotOwner`.
pub struct JoinedToolPackageProvider {
    providers: Vec<ToolPackageProvider>,
}
impl JoinedToolPackageProvider {
    /// Join operation advertisements. Register only this package publisher for
    /// the combined roots, and register each source's tool provider separately.
    pub fn new(providers: Vec<ToolPackageProvider>) -> Self {
        Self { providers }
    }
}
#[async_trait::async_trait]
impl PackageProvider for JoinedToolPackageProvider {
    fn environment_tool_sources(
        &self,
        publisher: &lash_core::plugin::ToolProviderHandle,
    ) -> BTreeSet<lash_core::plugin::ToolProviderHandle> {
        self.providers
            .iter()
            .flat_map(|provider| provider.environment_tool_sources(publisher))
            .collect()
    }
    fn shares_tool_source(&self) -> bool {
        true
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
        let owned;
        let tools = match tools {
            Some(tools) => tools,
            None => {
                owned = self
                    .providers
                    .iter()
                    .map(|provider| {
                        let source = lash_core::plugin::ToolProviderHandle::new(
                            key.registry(),
                            provider.owner.clone(),
                            provider.key,
                        );
                        provider
                            .provider
                            .environment_snapshot(
                                &source,
                                &lash_core_store::environment::SourceId(format!(
                                    "provider:{source}"
                                )),
                                lash_core_store::environment::SourceRevision {
                                    incarnation: Default::default(),
                                    counter: 0,
                                },
                                config,
                                Some((domain, scope)),
                            )
                            .map(|snapshot| snapshot.fragment.tools)
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>();
                &owned
            }
        };
        let tools: Vec<_> = tools
            .iter()
            .filter(|entry| {
                self.providers.iter().any(|provider| {
                    entry.provider().owner() == provider.owner
                        && entry.provider().key() == provider.key.as_str()
                        && provider
                            .roots
                            .contains(&entry.definition.manifest.operation.package.as_slice()[0])
                })
            })
            .cloned()
            .collect();
        operation_environment_fragment(&tools, self.roots(config), config, domain, scope, key)
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
            if !provider.roots.contains(&context.root) {
                continue;
            }
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
