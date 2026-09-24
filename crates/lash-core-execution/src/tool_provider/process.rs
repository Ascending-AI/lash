use crate::ProcessId;
use crate::SessionId;
use std::sync::Arc;

use crate::plugin::PluginError;
use crate::{
    PreparedToolCall, ToolActivation, ToolContract, ToolDefinition, ToolId, ToolManifest,
    ToolOutcome, ToolOutcomeDone, ToolPrepareCall,
};

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

    #[cfg(any(test, feature = "testing"))]
    pub fn __for_testing(context: &super::ToolContext<'run>) -> Self {
        Self::new(context.clone())
    }

    pub fn session_id(&self) -> &str {
        self.context.session_id()
    }

    /// Read the durable process id assigned to this internal body.
    pub fn process_id(&self) -> Option<&str> {
        self.context.enclosing_process()
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
}

/// The immutable manifest couples stable ID and provider-facing name. Runtime
/// dispatch constructs this view only for explicit internal registrations.
pub struct InternalProcessToolCall<'a> {
    manifest: &'a ToolManifest,
    pub args: &'a serde_json::Value,
    pub context: &'a InternalProcessContext<'a>,
}

impl<'a> InternalProcessToolCall<'a> {
    /// Only the runtime dispatcher builds these; the manifest is the coupling between stable
    /// ID and provider-facing name.
    pub fn new(
        manifest: &'a ToolManifest,
        args: &'a serde_json::Value,
        context: &'a InternalProcessContext<'a>,
    ) -> Self {
        Self {
            manifest,
            args,
            context,
        }
    }

    /// The stable tool ID carried by the pinned manifest.
    pub fn tool_id(&self) -> &'a ToolId {
        &self.manifest.id
    }

    /// The provider-facing tool name carried by the pinned manifest.
    pub fn name(&self) -> &'a str {
        &self.manifest.name
    }
}

/// Implementation of one explicit internal owner-bound process tool.
///
/// This is ADR 0051's protocol and process-engine implementor class. Internal
/// tools are a distinct execution class from leaf [`crate::ToolProvider`]s:
/// they execute inside process replay with an
/// [`InternalProcessContext`], run without a recorded `ToolAttempt` frame, and
/// return a completed [`ToolOutcomeDone`] — internal bodies cannot defer or
/// declare leaf intents.
#[async_trait::async_trait]
pub trait InternalProcessToolImplementation: Send + Sync + 'static {
    /// Seal the pending call's arguments for replay. The default is the
    /// identity prepare, which preserves the call as issued.
    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        Ok(PreparedToolCall::identity(call.tool_id, call.pending))
    }

    async fn execute(&self, call: InternalProcessToolCall<'_>) -> ToolOutcomeDone;
}

/// A registered internal process tool: its tool definition plus the explicit
/// internal implementation that executes it.
///
/// Construction normalizes the definition to
/// [`ToolActivation::Internal`], so the activation derives from the
/// registration lane rather than from flags on the definition.
#[derive(Clone)]
pub struct InternalProcessToolDef {
    definition: ToolDefinition,
    implementation: Arc<dyn InternalProcessToolImplementation>,
}

impl InternalProcessToolDef {
    /// Pair a tool definition with its internal implementation for protocol
    /// and process-engine implementors.
    pub fn new(
        definition: ToolDefinition,
        implementation: Arc<dyn InternalProcessToolImplementation>,
    ) -> Self {
        Self {
            definition: definition.with_activation(ToolActivation::Internal),
            implementation,
        }
    }

    pub(crate) fn manifest(&self) -> ToolManifest {
        self.definition.manifest.clone()
    }

    pub(crate) fn contract(&self) -> Arc<ToolContract> {
        Arc::new(self.definition.contract.clone())
    }

    pub(crate) async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        self.implementation.prepare_tool_call(call).await
    }

    pub(crate) async fn execute(&self, call: InternalProcessToolCall<'_>) -> ToolOutcomeDone {
        self.implementation.execute(call).await
    }
}

/// Process lifecycle operations available only to an internal durable body.
///
/// This is ADR 0051's protocol and process-engine implementor class. Leaf tool
/// attempts cannot obtain this value.
#[derive(Clone)]
pub struct InternalProcessAdmin<'run> {
    pub(super) session_id: SessionId,
    pub(super) agent_frame_id: crate::FrameNodeId,
    pub(super) processes: Arc<dyn crate::ProcessService>,
    pub(super) effect_controller: crate::runtime::RuntimeEffectControllerHandle<'run>,
    pub(super) parent_invocation: Option<crate::RuntimeInvocation>,
    pub(super) tool_call_id: Option<String>,
    pub(super) execution_env_spec: crate::ProcessExecutionEnvSpec,
    /// The realized-start sink a group child's driver drains into its
    /// settlement's possession. `None` for every other caller — a start an
    /// orchestrating group child makes must reach its settlement, and this
    /// buffer is the only channel that crosses the no-attempt-frame boundary.
    pub(super) orchestrating_starts: Option<crate::tool_dispatch::OrchestratingStartsBuffer>,
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
    ) -> Result<crate::ProcessHandleView, PluginError> {
        if !request.observers.contains(&self.session_id) {
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
        let view = self
            .processes
            .start_from_request(&self.session_id, request, self.process_scope())
            .await?;
        // Captured at the journal boundary the start committed under: a group
        // child's settlement possession must name every process the body
        // realized, and an orchestrating body has no attempt frame whose
        // intent outcomes would carry it.
        if let Some(starts) = &self.orchestrating_starts {
            starts.enqueue(view.process_id.clone());
        }
        Ok(view)
    }

    /// Record the terminal outcome of an Externally-Owned process this session
    /// owns (ADR 0019). A host that launches work outside lash registers it as
    /// an Externally-Owned row and completes it with the launch identity —
    /// lash never claims it as running. Only Externally-Owned rows accept this
    /// out-of-band completion.
    ///
    /// This is ADR 0051's protocol and process-engine implementor class.
    pub async fn complete_external(
        &self,
        process_id: &ProcessId,
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

    /// Arm the caller-departure audit for an Externally-Owned row this body
    /// just registered but has not yet resolved (FIG-1383).
    ///
    /// A detached launch registers its durable audit row *before* the host side effect, so the
    /// row can outlive the caller: cancellation between the two drops this body's future while
    /// the blocking launch continues.
    /// Holding the returned value across that window closes it — dropping it still armed
    /// durably marks the row
    /// [`ProcessStatus::CallerDeparted`](crate::ProcessStatus::CallerDeparted) instead of
    /// leaving it forever indistinguishable from a launch still in flight.
    ///
    /// This is ADR 0051's protocol and process-engine implementor class.
    pub fn external_launch_audit(&self, process_id: &ProcessId) -> ExternalLaunchAudit {
        ExternalLaunchAudit {
            processes: Arc::clone(&self.processes),
            session_id: self.session_id.clone(),
            process_id: ProcessId::from(process_id.to_string()),
            armed: true,
        }
    }

    /// Await a process started from this session to its terminal output.
    ///
    /// This is ADR 0051's protocol and process-engine implementor class.
    pub async fn await_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<crate::ProcessAwaitOutput, PluginError> {
        self.processes
            .validate_visible(
                &self.session_id,
                &[ProcessId::from(process_id.to_string())],
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
    ) -> Result<Vec<crate::ProcessHandleView>, PluginError> {
        Ok(self
            .processes
            .list_visible(&self.session_id, filter.list_mode(), self.process_scope())
            .await?
            .into_iter()
            .filter(|record| filter.matches_record(record))
            .map(crate::ProcessHandleView::from_record)
            .collect())
    }

    /// Cancel a process visible to this internal process body.
    ///
    /// This is ADR 0051's protocol and process-engine implementor class.
    pub async fn cancel(
        &self,
        process_id: &ProcessId,
    ) -> Result<crate::ProcessCancelReceipt, PluginError> {
        self.processes
            .validate_visible(
                &self.session_id,
                &[ProcessId::from(process_id.to_string())],
                self.process_scope(),
            )
            .await?;
        self.processes
            .cancel(&self.session_id, process_id, self.process_scope())
            .await
            .and_then(crate::ProcessCancelReceipt::from_record)
    }

    /// Signal a process visible to this internal process body.
    ///
    /// This is ADR 0051's protocol and process-engine implementor class.
    pub async fn signal(
        &self,
        process_id: &ProcessId,
        signal_name: &str,
        payload: serde_json::Value,
    ) -> Result<crate::ProcessEvent, PluginError> {
        let signal_id = self
            .tool_call_id
            .clone()
            .unwrap_or_else(|| format!("adhoc-{}", uuid::Uuid::new_v4()));
        self.processes
            .validate_visible(
                &self.session_id,
                &[ProcessId::from(process_id.to_string())],
                self.process_scope(),
            )
            .await?;
        self.processes
            .signal_possessed(
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
        process_id: &ProcessId,
        signal_name: &str,
        signal_id: String,
        payload: serde_json::Value,
    ) -> Result<crate::ProcessEvent, PluginError> {
        self.processes
            .validate_visible(
                &self.session_id,
                &[ProcessId::from(process_id.to_string())],
                self.process_scope(),
            )
            .await?;
        self.processes
            .signal_possessed(
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

/// Armed caller-departure audit for one unresolved Externally-Owned row.
///
/// The value is the window itself: it exists from the moment the durable audit
/// row commits until an outcome is recorded. Dropping it while armed — which
/// is what cancelling the caller does — reports the departure, because at that
/// point nobody is left who could ever write the row's outcome and lash may
/// not invent one.
///
/// The report is issued from `Drop`, so it is spawned rather than awaited: the
/// scope that would have carried it is being torn down. That is also why
/// [`ProcessService::report_caller_departure`](crate::ProcessService::report_caller_departure)
/// is controller-free and idempotent.
///
/// This is ADR 0051's protocol and process-engine implementor class.
pub struct ExternalLaunchAudit {
    processes: Arc<dyn crate::ProcessService>,
    session_id: SessionId,
    process_id: ProcessId,
    armed: bool,
}

impl ExternalLaunchAudit {
    /// Disarm the audit because the row's outcome has been recorded.
    pub fn resolved(mut self) {
        self.armed = false;
    }
}

impl Drop for ExternalLaunchAudit {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                process_id = %self.process_id,
                "caller-departure audit dropped outside a runtime; the row stays unresolved",
            );
            return;
        };
        let processes = Arc::clone(&self.processes);
        // Cloned rather than `mem::take`: an identity newtype has no empty
        // default to leave behind, and this runs from `Drop`, so the originals
        // are discarded immediately after.
        let session_id = self.session_id.clone();
        let process_id = self.process_id.clone();
        handle.spawn(async move {
            if let Err(error) = processes
                .report_caller_departure(&session_id, &process_id)
                .await
            {
                tracing::warn!(
                    process_id = %process_id,
                    error = %error,
                    "failed to record the caller departure of an unresolved external launch",
                );
            }
        });
    }
}

#[cfg(feature = "testing")]
impl<'run> InternalProcessAdmin<'run> {
    /// An admin surface for `session_id` over `processes`, as the internal
    /// tool context hands one to a process tool, with no parent invocation
    /// and no orchestrating-start sink.
    pub(crate) fn for_testing(
        session_id: SessionId,
        agent_frame_id: crate::FrameNodeId,
        processes: Arc<dyn crate::ProcessService>,
        effect_controller: crate::runtime::RuntimeEffectControllerHandle<'run>,
        execution_env_spec: crate::ProcessExecutionEnvSpec,
    ) -> Self {
        Self {
            session_id,
            agent_frame_id,
            processes,
            effect_controller,
            parent_invocation: None,
            tool_call_id: None,
            execution_env_spec,
            orchestrating_starts: None,
        }
    }
}
