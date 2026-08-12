use lash_core::plugin::AssistantProseProjectorPlugin;
use std::sync::Arc;

use crate::dialect::RlmDialect;

pub(super) struct RlmAssistantProseProjector {
    pub(super) dialect: Arc<dyn RlmDialect>,
}

impl AssistantProseProjectorPlugin for RlmAssistantProseProjector {
    fn project_assistant_prose(&self, text: &str) -> String {
        crate::protocol::project_visible_assistant_prose_for_dialect(text, self.dialect.cell_tags())
    }
}
