use crate::plugin::PluginError;

use super::ToolProcessEventContext;

impl ToolProcessEventContext {
    /// Append `request` to the journal of the process this call runs inside.
    /// A wake it carries is the target session's queued work, written and
    /// woken in the append's own transaction (ADR 0132 §12).
    pub(crate) async fn append(
        &self,
        request: crate::ProcessEventAppendRequest,
    ) -> Result<crate::ProcessEvent, PluginError> {
        let result = self
            .process_work
            .registry()
            .append_event_with_authority(&self.process_id, request, &self.execution_write_authority)
            .await?;
        Ok(result.event)
    }
}
