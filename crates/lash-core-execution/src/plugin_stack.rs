use std::sync::Arc;

use crate::PluginFactory;

#[derive(Clone, Default)]
pub struct PluginStack {
    factories: Vec<Arc<dyn PluginFactory>>,
}

impl PluginStack {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_factories(factories: impl IntoIterator<Item = Arc<dyn PluginFactory>>) -> Self {
        Self {
            factories: factories.into_iter().collect(),
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
        self
    }

    pub fn replace(&mut self, plugin: Arc<dyn PluginFactory>) -> &mut Self {
        let id = plugin.id();
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
        self
    }

    pub fn configure(mut self, configure: impl FnOnce(&mut PluginStack)) -> Self {
        configure(&mut self);
        self
    }
}
