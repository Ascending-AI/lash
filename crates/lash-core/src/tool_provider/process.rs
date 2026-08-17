use std::sync::Arc;

use crate::plugin::PluginError;

/// Owner-bound capabilities for an internal durable process body.
///
/// This is ADR 0051's protocol and process-engine implementor class. Lash
/// constructs it only after resolving an `Internal` activation inside process
/// replay. Leaf tools receive [`crate::AttemptContext`] and orchestration
/// definitions receive [`crate::OrchestrationContext`] instead.
#[derive(Clone)]
pub struct InternalProcessContext<'run> {
    context: super::ToolContext<'run>,
}

impl<'run> InternalProcessContext<'run> {
    pub(crate) fn new(context: super::ToolContext<'run>) -> Self {
        Self { context }
    }

    /// Construct the runtime-only context in an integrator test.
    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    pub fn __for_testing(context: &super::ToolContext<'run>) -> Self {
        Self::new(context.clone())
    }

    /// Read the session that owns this internal process body.
    pub fn session_id(&self) -> &str {
        self.context.session_id()
    }

    /// Read the durable process id assigned to this internal body.
    pub fn process_id(&self) -> Option<&str> {
        self.context.async_process_id()
    }

    /// Access owner-bound process lifecycle operations.
    pub fn processes(&self) -> InternalProcessAdmin<'run> {
        self.context.process_admin()
    }

    /// Access the current process's event stream.
    pub fn process_events(&self) -> super::ToolProcessEventClient {
        self.context.process_events()
    }

    /// Observe cancellation of the owner-bound process body.
    pub fn cancellation_token(&self) -> Option<&tokio_util::sync::CancellationToken> {
        self.context.cancellation_token()
    }

    /// Project this internal process body down to the sealed leaf-attempt
    /// context, for an internal executor whose fallback is a pure
    /// [`crate::ToolProvider::execute`] body.
    ///
    /// This is ADR 0051's protocol and process-engine implementor class. The
    /// projection is controller-free: the pure fallback gets the same
    /// journal-incapable surface a recorded leaf attempt gets.
    ///
    /// The projection carries no completion key and reports the deferral as
    /// undeclared on purpose. An internal owner-bound process tool is invoked
    /// by its process runner, not by the attempt coordinator that reserves
    /// completion keys, so nothing on this route can park: an internal body
    /// asking for a key is a mistake, and it is told which declaration is
    /// missing instead of being handed a key nobody would ever resolve.
    #[doc(hidden)]
    pub fn __attempt_context(&self) -> crate::AttemptContext<'run> {
        let scope_id = self
            .context
            .effect_controller
            .scoped()
            .scope_id()
            .to_string();
        crate::AttemptContext::from_tool_context(
            &self.context,
            scope_id,
            None,
            crate::tool_provider::AttemptCompletionSupport::NotDeclared,
        )
    }
}

/// Inputs handed to an internal owner-bound process tool.
///
/// This is ADR 0051's protocol and process-engine implementor class. Runtime
/// dispatch constructs it only for `ToolActivation::Internal`; model-facing
/// and leaf-attempt calls cannot obtain its process capabilities.
pub struct InternalProcessToolCall<'a> {
    pub name: &'a str,
    pub args: &'a serde_json::Value,
    pub context: &'a InternalProcessContext<'a>,
}

/// Process lifecycle operations available only to an internal durable body.
///
/// This is ADR 0051's protocol and process-engine implementor class. Leaf tool
/// attempts cannot obtain this value.
#[derive(Clone)]
pub struct InternalProcessAdmin<'run> {
    pub(super) session_id: String,
    pub(super) agent_frame_id: crate::AgentFrameId,
    pub(super) processes: Arc<dyn crate::ProcessService>,
    pub(super) effect_controller: crate::runtime::RuntimeEffectControllerHandle<'run>,
    pub(super) parent_invocation: Option<crate::RuntimeInvocation>,
    pub(super) tool_call_id: Option<String>,
    pub(super) execution_env_spec: crate::ProcessExecutionEnvSpec,
}

impl InternalProcessAdmin<'_> {
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
    ///
    /// This is ADR 0051's protocol and process-engine implementor class.
    pub async fn start(
        &self,
        mut request: crate::ProcessStartRequest,
    ) -> Result<crate::ProcessHandleSummary, PluginError> {
        if !request
            .observers
            .iter()
            .any(|observer| observer == &self.session_id)
        {
            request.observers.push(self.session_id.clone());
        }
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
    ///
    /// This is ADR 0051's protocol and process-engine implementor class.
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
    ///
    /// This is ADR 0051's protocol and process-engine implementor class.
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

    /// List process handles visible to this internal process body.
    ///
    /// This is ADR 0051's protocol and process-engine implementor class.
    pub async fn list_handles_filtered(
        &self,
        filter: &crate::ProcessListFilter,
    ) -> Result<Vec<crate::ProcessHandleSummary>, PluginError> {
        Ok(self
            .processes
            .list_visible(&self.session_id, filter.list_mode(), self.process_scope())
            .await?
            .into_iter()
            .filter(|record| filter.matches_record(record))
            .map(crate::ProcessHandleSummary::from_record)
            .collect())
    }

    /// Cancel a process visible to this internal process body.
    ///
    /// This is ADR 0051's protocol and process-engine implementor class.
    pub async fn cancel(
        &self,
        process_id: &str,
    ) -> Result<crate::ProcessCancelSummary, PluginError> {
        self.processes
            .cancel_visible(&self.session_id, process_id, self.process_scope())
            .await
            .map(crate::ProcessCancelSummary::from_record)
    }

    /// Signal a process visible to this internal process body.
    ///
    /// This is ADR 0051's protocol and process-engine implementor class.
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

    pub(crate) async fn signal_with_id(
        &self,
        process_id: &str,
        signal_name: &str,
        signal_id: String,
        payload: serde_json::Value,
    ) -> Result<crate::ProcessEvent, PluginError> {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProcessRegistry;
    use crate::runtime::RuntimeEffectControllerHandle;

    fn admin(processes: Arc<dyn crate::ProcessService>) -> InternalProcessAdmin<'static> {
        InternalProcessAdmin {
            session_id: "session".to_string(),
            agent_frame_id: "frame".to_string(),
            processes,
            effect_controller: RuntimeEffectControllerHandle::shared(Arc::new(
                crate::InlineRuntimeEffectController::default(),
            )),
            parent_invocation: None,
            tool_call_id: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
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
            .add_observer(
                "session",
                "process",
                crate::ProcessObserverBy::host("tool-provider-test"),
            )
            .await
            .expect("observe process");
        assert!(admin.await_process("process").await.is_ok());
    }
}
