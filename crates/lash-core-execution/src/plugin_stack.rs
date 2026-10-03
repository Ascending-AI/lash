use std::sync::Arc;

use crate::PluginFactory;

#[derive(Clone, Default)]
pub struct PluginStack {
    factories: Vec<Arc<dyn PluginFactory>>,
    protocol_factory: Option<Arc<dyn PluginFactory>>,
}

impl PluginStack {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_factories(factories: impl IntoIterator<Item = Arc<dyn PluginFactory>>) -> Self {
        Self {
            factories: factories.into_iter().collect(),
            protocol_factory: None,
        }
    }

    pub fn protocol_plugin(mut self, protocol: Arc<dyn PluginFactory>) -> Self {
        self.replace(Arc::clone(&protocol));
        self.protocol_factory = Some(protocol);
        self
    }

    pub fn protocol_factory(&self) -> Option<&Arc<dyn PluginFactory>> {
        self.protocol_factory.as_ref()
    }

    pub fn into_host(self) -> crate::PluginHost {
        let host = crate::PluginHost::new(self.factories);
        match self.protocol_factory {
            Some(protocol) => host.with_protocol_plugin(protocol),
            None => host,
        }
    }

    pub fn factories(&self) -> &[Arc<dyn PluginFactory>] {
        &self.factories
    }

    pub fn into_factories(self) -> Vec<Arc<dyn PluginFactory>> {
        self.factories
    }

    pub fn push(&mut self, plugin: Arc<dyn PluginFactory>) -> &mut Self {
        self.factories.push(plugin);
        self
    }

    pub fn extend(
        &mut self,
        plugins: impl IntoIterator<Item = Arc<dyn PluginFactory>>,
    ) -> &mut Self {
        self.factories.extend(plugins);
        self
    }

    pub fn remove(&mut self, id: &str) -> &mut Self {
        self.factories.retain(|plugin| plugin.id() != id);
        if self
            .protocol_factory
            .as_ref()
            .is_some_and(|plugin| plugin.id() == id)
        {
            self.protocol_factory = None;
        }
        self
    }

    pub fn replace(&mut self, plugin: Arc<dyn PluginFactory>) -> &mut Self {
        let id = plugin.id();
        if self
            .protocol_factory
            .as_ref()
            .is_some_and(|existing| existing.id() == id)
        {
            self.protocol_factory = Some(Arc::clone(&plugin));
        }
        if let Some(slot) = self
            .factories
            .iter_mut()
            .find(|existing| existing.id() == id)
        {
            *slot = plugin;
        } else {
            self.factories.push(plugin);
        }
        self
    }

    pub fn retain(&mut self, keep: impl FnMut(&Arc<dyn PluginFactory>) -> bool) -> &mut Self {
        self.factories.retain(keep);
        if self.protocol_factory.as_ref().is_some_and(|selected| {
            !self
                .factories
                .iter()
                .any(|factory| factory.id() == selected.id())
        }) {
            self.protocol_factory = None;
        }
        self
    }

    pub fn configure(mut self, configure: impl FnOnce(&mut PluginStack)) -> Self {
        configure(&mut self);
        self
    }
}
