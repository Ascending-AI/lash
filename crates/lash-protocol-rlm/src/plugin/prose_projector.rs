use crate::dialect::SessionDialect;
use lash_core::plugin::AssistantProseProjectorPlugin;
use std::sync::Arc;

pub(super) struct RlmAssistantProseProjector {
    pub(super) dialect: Arc<SessionDialect>,
    /// A relay session shows no model prose: its user output is committed
    /// `send_user_output` calls only (FIG-4441).
    pub(super) relay: bool,
}

impl AssistantProseProjectorPlugin for RlmAssistantProseProjector {
    fn project_assistant_prose(&self, text: &str) -> String {
        if self.relay {
            return String::new();
        }
        crate::protocol::project_visible_assistant_prose_for_dialect(text, self.dialect.cell_tags())
    }
}
