use super::{LashCore, LashCoreBuilder};
use crate::support::{PluginHost, Result};

/// Escape hatch for host-supplied runtime internals on [`LashCoreBuilder`].
pub struct AdvancedLashCoreBuilder {
    pub(super) builder: LashCoreBuilder,
}

impl AdvancedLashCoreBuilder {
    pub fn plugin_host(mut self, plugin_host: PluginHost) -> Self {
        self.builder.plugin_host = Some(plugin_host);
        self
    }

    pub fn build(self, drive_owner: lash_core::LeaseOwnerIdentity) -> Result<LashCore> {
        self.builder.build(drive_owner)
    }
}
