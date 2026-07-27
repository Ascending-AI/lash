use std::sync::Arc;

use crate::plugin::PluginError;

#[derive(Clone)]
pub struct ToolSessionProcessAdmin<'run> {
    pub(super) session_id: String,
    pub(super) agent_frame_id: crate::AgentFrameId,
    pub(super) processes: Arc<dyn crate::ProcessService>,
    pub(super) process_cancel_ability: Arc<dyn crate::ProcessCancelAbility>,
    pub(super) effect_controller: crate::runtime::RuntimeEffectControllerHandle<'run>,
    pub(super) parent_invocation: Option<crate::RuntimeInvocation>,
    pub(super) tool_call_id: Option<String>,
    pub(super) execution_env_spec: crate::ProcessExecutionEnvSpec,
}

impl ToolSessionProcessAdmin<'_> {
    fn process_scope(&self) -> crate::ProcessOpScope<'_> {
        crate::ProcessOpScope::new(self.effect_controller.scoped())
            .with_parent_invocation(self.parent_invocation.clone())
            .with_agent_frame_id(Some(self.agent_frame_id.clone()))
    }

    /// Start a process owned by this session and registered to wake it,
    /// returning its public handle summary. Routes through the same
    /// [`crate::ProcessService::start_from_request`] path the runtime uses for
    /// every request-shaped process start, so the child is provider-re-supplied,
    /// durable, and recoverable through the worker.
    pub async fn start(
        &self,
        mut request: crate::ProcessStartRequest,
    ) -> Result<crate::ProcessHandleSummary, PluginError> {
        if request.env_spec.is_none()
            && matches!(
                &request.input,
                crate::ProcessInput::ToolCall { .. } | crate::ProcessInput::Engine { .. }
            )
        {
            request.env_spec = Some(self.execution_env_spec.clone());
        }
        self.processes
            .start_from_request(&self.session_id, request, self.process_scope())
            .await
    }

    /// Record the terminal outcome of an Externally-Owned process this session
    /// owns (ADR 0019). A `shell.start` detach registers its launch as an
    /// Externally-Owned row and immediately completes it with the launch
    /// identity — lash never claims it as running. Only Externally-Owned rows
    /// accept this out-of-band completion.
    pub async fn complete_external(
        &self,
        process_id: &str,
        await_output: crate::ProcessAwaitOutput,
    ) -> Result<crate::ProcessCompletionOutcome, PluginError> {
        self.processes
            .complete_external(
                &self.session_id,
                process_id,
                await_output,
                self.process_scope(),
            )
            .await
    }

    /// Await a process started from this session to its terminal output.
    pub async fn await_process(
        &self,
        process_id: &str,
    ) -> Result<crate::ProcessAwaitOutput, PluginError> {
        self.processes
            .validate_visible(
                &self.session_id,
                &[process_id.to_string()],
                self.process_scope(),
            )
            .await?;
        self.processes
            .await_process(process_id, self.process_scope())
            .await
    }

    pub async fn list_handles(&self) -> Result<Vec<crate::ProcessHandleSummary>, PluginError> {
        Ok(self
            .processes
            .list_visible(
                &self.session_id,
                crate::ProcessListMode::Live,
                self.process_scope(),
            )
            .await?
            .into_iter()
            .map(crate::ProcessHandleSummary::from)
            .collect())
    }

    pub async fn list_all_handles(&self) -> Result<Vec<crate::ProcessHandleSummary>, PluginError> {
        Ok(self
            .processes
            .list_visible(
                &self.session_id,
                crate::ProcessListMode::All,
                self.process_scope(),
            )
            .await?
            .into_iter()
            .map(crate::ProcessHandleSummary::from)
            .collect())
    }

    pub async fn list_handles_filtered(
        &self,
        filter: &crate::ProcessListFilter,
    ) -> Result<Vec<crate::ProcessHandleSummary>, PluginError> {
        Ok(self
            .processes
            .list_visible(&self.session_id, filter.list_mode(), self.process_scope())
            .await?
            .into_iter()
            .filter(|entry| filter.matches_entry(entry))
            .map(crate::ProcessHandleSummary::from)
            .collect())
    }

    pub async fn validate_handles(&self, handle_ids: &[String]) -> Result<(), PluginError> {
        self.processes
            .validate_visible(&self.session_id, handle_ids, self.process_scope())
            .await
    }

    pub async fn cancel(
        &self,
        process_id: &str,
    ) -> Result<crate::ProcessCancelSummary, PluginError> {
        let request = crate::ProcessCancelRequest::new(
            &self.session_id,
            process_id,
            self.process_scope(),
            crate::ProcessCancelSource::Tool,
        )
        .with_reason("requested by tool");
        self.process_cancel_ability
            .cancel_summary(self.processes.as_ref(), request)
            .await
    }

    pub async fn signal(
        &self,
        process_id: &str,
        signal_name: &str,
        payload: serde_json::Value,
    ) -> Result<crate::ProcessEvent, PluginError> {
        let signal_id = self
            .tool_call_id
            .clone()
            .unwrap_or_else(|| format!("adhoc-{}", uuid::Uuid::new_v4()));
        self.processes
            .signal(
                &self.session_id,
                process_id,
                signal_name.to_string(),
                signal_id,
                payload,
                self.process_scope(),
            )
            .await
    }

    pub async fn cancel_all(&self) -> Result<Vec<crate::ProcessCancelSummary>, PluginError> {
        self.process_cancel_ability
            .cancel_all_visible(
                self.processes.as_ref(),
                crate::ProcessCancelAllRequest::new(
                    &self.session_id,
                    self.process_scope(),
                    crate::ProcessCancelSource::Tool,
                )
                .with_reason("requested by tool"),
            )
            .await
    }

    pub async fn transfer_handles(
        &self,
        to_session_id: &str,
        process_ids: Vec<String>,
    ) -> Result<(), PluginError> {
        self.processes
            .transfer(
                &self.session_id,
                to_session_id,
                process_ids,
                self.process_scope(),
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProcessRegistry;
    use crate::runtime::RuntimeEffectControllerHandle;

    fn admin(processes: Arc<dyn crate::ProcessService>) -> ToolSessionProcessAdmin<'static> {
        ToolSessionProcessAdmin {
            session_id: "session".to_string(),
            agent_frame_id: "frame".to_string(),
            processes,
            process_cancel_ability: Arc::new(crate::DefaultProcessCancelAbility),
            effect_controller: RuntimeEffectControllerHandle::shared(Arc::new(
                crate::InlineRuntimeEffectController::default(),
            )),
            parent_invocation: None,
            tool_call_id: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::default(),
            ),
        }
    }

    #[tokio::test]
    async fn await_process_requires_visibility_then_allows_observed_process() {
        let host = Arc::new(crate::testing::MockSessionManager::default());
        host.process_registry
            .register_process(crate::ProcessRegistration::new(
                "process",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryDisposition::ExternallyOwned,
                crate::ProcessProvenance::host(),
            ))
            .await
            .expect("register process");
        host.process_registry
            .complete_process(
                "process",
                crate::ProcessAwaitOutput::Success {
                    value: serde_json::json!("done"),
                    control: None,
                },
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete process");
        let processes: Arc<dyn crate::ProcessService> = host.clone();
        let admin = admin(processes);

        let hidden = admin
            .await_process("process")
            .await
            .expect_err("unobserved process must be hidden");
        assert_eq!(
            hidden.to_string(),
            "plugin session error: process handle `process` is not live or visible in this session"
        );

        host.process_registry
            .grant_handle(
                &crate::SessionScope::for_agent_frame("session", "frame"),
                "process",
                crate::ProcessHandleDescriptor::new(Some("test"), Some("process")),
            )
            .await
            .expect("grant process handle");
        assert!(admin.await_process("process").await.is_ok());
    }
}
