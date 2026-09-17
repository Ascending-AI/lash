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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PluginSpec;
    use crate::plugin::StaticPluginFactory;

    fn factory(id: &'static str) -> Arc<dyn PluginFactory> {
        Arc::new(StaticPluginFactory::new(id, PluginSpec::new()))
    }

    fn ids(stack: &PluginStack) -> Vec<&'static str> {
        stack
            .factories()
            .iter()
            .map(|factory| factory.id())
            .collect()
    }

    #[test]
    fn construction_preserves_factory_order_and_identity() {
        let alpha = factory("alpha");
        let beta = factory("beta");

        let stack = PluginStack::from_factories([Arc::clone(&alpha), Arc::clone(&beta)]);

        assert_eq!(ids(&stack), vec!["alpha", "beta"]);
        assert!(Arc::ptr_eq(&stack.factories()[0], &alpha));
        assert!(Arc::ptr_eq(&stack.factories()[1], &beta));

        let factories = stack.into_factories();
        assert_eq!(
            factories
                .iter()
                .map(|factory| factory.id())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
        assert!(Arc::ptr_eq(&factories[0], &alpha));
        assert!(Arc::ptr_eq(&factories[1], &beta));
    }

    #[test]
    fn mutators_apply_by_plugin_id_without_reordering_unrelated_plugins() {
        let alpha = factory("alpha");
        let beta_v1 = factory("beta");
        let beta_v2 = factory("beta");
        let gamma_v1 = factory("gamma");
        let gamma_v2 = factory("gamma");
        let delta = factory("delta");

        let mut stack = PluginStack::new();
        stack
            .push(Arc::clone(&alpha))
            .push(Arc::clone(&beta_v1))
            .extend([Arc::clone(&gamma_v1), Arc::clone(&delta)]);

        assert_eq!(ids(&stack), vec!["alpha", "beta", "gamma", "delta"]);

        stack.remove("beta");
        assert_eq!(ids(&stack), vec!["alpha", "gamma", "delta"]);
        assert!(
            stack
                .factories()
                .iter()
                .all(|factory| factory.id() != "beta")
        );

        stack.remove("missing");
        assert_eq!(ids(&stack), vec!["alpha", "gamma", "delta"]);

        stack.replace(Arc::clone(&gamma_v2));
        assert_eq!(ids(&stack), vec!["alpha", "gamma", "delta"]);
        assert!(Arc::ptr_eq(&stack.factories()[1], &gamma_v2));
        assert!(!Arc::ptr_eq(&stack.factories()[1], &gamma_v1));

        stack.replace(Arc::clone(&beta_v2));
        assert_eq!(ids(&stack), vec!["alpha", "gamma", "delta", "beta"]);
        assert!(Arc::ptr_eq(&stack.factories()[3], &beta_v2));

        stack.retain(|factory| factory.id() != "delta");
        assert_eq!(ids(&stack), vec!["alpha", "gamma", "beta"]);
        assert!(Arc::ptr_eq(&stack.factories()[0], &alpha));
    }

    #[test]
    fn configure_applies_the_closure_to_the_owned_stack() {
        let alpha_v1 = factory("alpha");
        let alpha_v2 = factory("alpha");
        let beta = factory("beta");
        let gamma = factory("gamma");

        let stack = PluginStack::new().configure(|stack| {
            stack
                .push(Arc::clone(&alpha_v1))
                .extend([Arc::clone(&beta), Arc::clone(&gamma)])
                .remove("beta")
                .replace(Arc::clone(&alpha_v2));
        });

        assert_eq!(ids(&stack), vec!["alpha", "gamma"]);
        assert!(Arc::ptr_eq(&stack.factories()[0], &alpha_v2));
        assert!(Arc::ptr_eq(&stack.factories()[1], &gamma));
    }
}
