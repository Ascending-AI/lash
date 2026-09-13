use super::*;

impl ToolContext<'_> {
    pub(crate) async fn child_process_parent_scope(
        &self,
    ) -> Result<crate::ParentScope, PluginError> {
        if let Some(context) = &self.runtime_execution_context {
            return context.child_process_parent_scope().await;
        }
        match self.effect_controller.scoped().execution_scope() {
            crate::ExecutionScope::Turn {
                session_id,
                turn_id,
            } => Ok(crate::ParentScope::Turn {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
            }),
            crate::ExecutionScope::Process { process_id } => {
                let context = self.process_events.as_ref().ok_or_else(|| {
                    PluginError::Session(
                        "process parent scope requires process registry authority".to_string(),
                    )
                })?;
                let parent = context
                    .process_work
                    .registry()
                    .resolve_process_ref(process_id)
                    .await?;
                Ok(crate::ParentScope::Process {
                    process_id: parent.process_id,
                    incarnation: parent.incarnation,
                })
            }
            crate::ExecutionScope::QueueDrain { .. }
            | crate::ExecutionScope::SessionDelete { .. }
            | crate::ExecutionScope::RuntimeOperation { .. } => Ok(crate::ParentScope::Host),
        }
    }
}
