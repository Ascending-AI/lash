use super::*;

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
