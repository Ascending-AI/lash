//! A2 gives each code package one owner. These Standard-only laws deliberately
//! collide model names, so they select model tools independently of packages.
use super::*;

pub(super) struct NativeSelected {
    owner: &'static str,
    surface: Arc<Surface>,
}
impl NativeSelected {
    pub(super) fn new(owner: &'static str, surface: Arc<Surface>) -> Self {
        Self { owner, surface }
    }
}
impl lash::plugins::PluginMetadata for NativeSelected {
    fn plugin_declaration(&self) -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial(self.owner)
    }
}
impl lash::plugins::PluginFactory for NativeSelected {
    fn id(&self) -> &'static str {
        self.owner
    }
    fn recipe(self: Arc<Self>) -> Result<lash::plugins::PluginRecipe, lash::plugins::PluginError> {
        let mut reg = lash::plugins::PluginRegistrar::<()>::new(self.owner);
        let provider = Arc::clone(&self);
        reg.tools()
            .provider(lash::tool_provider_key!("tools"), move |_, _| {
                Ok(provider.clone())
            })?;
        let mut config = lash::plugins::ConfigRegistrar::new(self.owner);
        lash::tools::SelectedToolsPlugin::new(self.owner, self.surface.clone())
            .declare_configuration(&mut config)?;
        Ok(lash::plugins::PluginRecipe::from_registrar(self.owner, reg).with_configuration(config))
    }
}
#[async_trait::async_trait]
impl lash_core::ToolProvider for NativeSelected {
    fn tool_manifests(
        &self,
        config: &lash_core::OwnerConfigView,
    ) -> Result<Vec<lash::tools::ToolManifest>, lash::plugins::PluginError> {
        let selection = lash::tools::ToolSelection::of(config, self.owner)
            .map_err(|error| lash::plugins::PluginError::Registration(error.to_string()))?;
        Ok(
            lash_core::ToolProvider::tool_manifests(self.surface.as_ref(), config)?
                .into_iter()
                .filter(|manifest| selection.admits(&manifest.id))
                .collect(),
        )
    }
    fn resolve_contract(
        &self,
        name: &str,
        config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash::tools::ToolContract>> {
        self.tool_manifests(config)
            .ok()?
            .iter()
            .any(|manifest| manifest.tool() == name)
            .then(|| lash_core::ToolProvider::resolve_contract(self.surface.as_ref(), name, config))
            .flatten()
    }
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolProvider::execute(self.surface.as_ref(), call).await
    }
}
