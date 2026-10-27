use super::*;
use std::collections::{BTreeMap, BTreeSet};

use lash_core::plugin::packages::PackageProvider;
use lash_kernel_value::names::Segment;

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
        let packages: Arc<dyn PackageProvider> = Arc::new(
            lash_tool_support::JoinedToolPackageProvider::new(publications),
        );
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
