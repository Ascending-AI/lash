use super::*;

/// The builder's tool owner publishes both surfaces through the same runner.
pub(super) fn tool_plugin(
    key: lash_core::plugin::ToolProviderKey,
    provider: Arc<dyn ToolProvider>,
) -> Result<Arc<dyn PluginFactory>> {
    let roots = provider
        .tool_manifests(&lash_core::OwnerConfigView::default())?
        .into_iter()
        .map(|manifest| manifest.operation.package.as_slice()[0].clone())
        .collect();
    let packages = lash_tool_support::ToolPackageProvider::new(
        key.as_str(),
        key,
        roots,
        Arc::clone(&provider),
    );
    Ok(Arc::new(StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial(key.as_str()),
        PluginSpec::new()
            .with_tool_provider(key, provider)
            .with_package_provider(key, Arc::new(packages)),
    )))
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
