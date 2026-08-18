use super::{LashCore, LashCoreBuilder};
use crate::support::*;
use lash_core::facade_support;

/// Escape hatch for host-supplied runtime internals on [`LashCoreBuilder`].
pub struct AdvancedLashCoreBuilder {
    pub(super) builder: LashCoreBuilder,
}

impl AdvancedLashCoreBuilder {
    pub fn runtime_host_config(mut self, core: facade_support::RuntimeHostConfig) -> Self {
        self.builder.runtime_host_config = Some(core);
        self
    }

    pub fn plugin_host(mut self, plugin_host: PluginHost) -> Self {
        self.builder.plugin_host = Some(plugin_host);
        self
    }

    pub fn build(self, session_execution_owner: lash_core::LeaseOwnerIdentity) -> Result<LashCore> {
        self.builder.build(session_execution_owner)
    }
}
