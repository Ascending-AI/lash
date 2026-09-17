use std::sync::Arc;

use super::{PluginError, PluginFactory, PluginRegistrar, PluginSessionContext, SessionPlugin};

pub(crate) const TRIGGER_RESOURCE_PLUGIN_ID: &str = "lash.triggers";

pub(crate) struct TriggerResourcePluginFactory;

impl PluginFactory for TriggerResourcePluginFactory {
    fn id(&self) -> &'static str {
        TRIGGER_RESOURCE_PLUGIN_ID
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(TriggerResourcePlugin))
    }
}

struct TriggerResourcePlugin;

impl SessionPlugin for TriggerResourcePlugin {
    fn id(&self) -> &'static str {
        TRIGGER_RESOURCE_PLUGIN_ID
    }

    fn register(&self, _reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        Ok(())
    }
}
