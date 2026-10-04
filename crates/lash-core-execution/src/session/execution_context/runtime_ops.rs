use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::super::{OpenerState, tool_run::ToolRunOwner};
use super::{
    ProcessId, RecordedTurnCancel, RuntimeExecutionContext, RuntimeExecutionProcessEventContext,
    RuntimeExecutionTracing, RuntimeProcessExecution, ToolDispatchContext,
};

impl RuntimeExecutionContext<'_> {
    /// Execution-side only: run one recorded step body that this execution
    /// issues in process (a tool attempt) under a cooperative stop that fires
    /// when the turn's gate pair asks it to stop now (FIG-3672 P9). The body
    /// gets the stop; what it returns is the step's recorded outcome. A watch
    /// that gives up leaves the body running to its end (the engine records
    /// every tool outcome, so the fault must not become one). An execution
    /// with no gate control runs the body under its own token.
    pub(crate) async fn run_turn_step_body<T, F, Fut>(&self, body: F) -> T
    where
        F: FnOnce(Option<CancellationToken>) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let (Some(control), Some(host)) = (
            self.turn_cancel.control.as_ref(),
            self.turn_cancel.host.as_ref(),
        ) else {
            // Each native body owns a child of the process stop: Closing
            // can stop a loser without firing the parent's other bodies.
            let stop = self
                .cancellation_token
                .as_ref()
                .map(CancellationToken::child_token)
                .unwrap_or_default();
            return body(Some(stop)).await;
        };
        control
            .run_recorded_step_body(host, self.is_cancelled(), |stop| body(Some(stop)))
            .await
    }

    /// Called only by the body of a Run record. Its decision captures the
    /// authoritative gate answer, including on a cold owner's first retry;
    /// the context's previously materialized fact cannot discover that stop.
    pub(crate) async fn run_cancel_requested_in_recorded_step(
        &self,
    ) -> Result<bool, crate::RuntimeError> {
        if self.turn_cancel.is_observed() {
            return Ok(true);
        }
        if let (Some(control), Some(host)) = (&self.turn_cancel.control, &self.turn_cancel.host) {
            return control.peek_immediate(host.await_event_resolver()).await;
        }
        // A process has no turn gate; its own cooperative stop is captured
        // by this same recorded decision. A lent turn token is never authority.
        Ok(!self.token_is_lent_stop
            && self
                .cancellation_token
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled))
    }
}

pub trait RuntimeExecutionContextRuntimeOps<'run>: Sized {
    fn new(
        dispatch: Arc<ToolDispatchContext<'run>>,
        process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
        attachment_store: Arc<crate::RuntimeAttachmentStore>,
        chronological_projection: Arc<crate::ChronologicalProjection>,
        turn_context: crate::TurnContext,
        execution_env_spec: crate::ProcessExecutionEnvSpec,
    ) -> Self;

    /// The tool-execution context this run lends its tool calls.
    ///
    fn dispatch(&self) -> &Arc<ToolDispatchContext<'run>>;

    /// The logical Run request channel shared by the turn's phase contexts.
    fn tool_run_owner(&self) -> Option<ToolRunOwner>;

    #[must_use]
    fn with_tool_run_owner(self, owner: &ToolRunOwner) -> Self;

    #[must_use]
    fn with_tracing(self, tracing: Option<RuntimeExecutionTracing>) -> Self;

    #[must_use]
    fn with_process_execution(
        self,
        process_id: ProcessId,
        registration: &crate::ProcessRegistration,
        event_context: impl Into<Option<RuntimeExecutionProcessEventContext>>,
    ) -> Self;

    /// Starts this execution's recorded turn-cancel fact: `honoured` is
    /// whether the turn had already recorded a cancellation when it built
    /// this execution, `control` is the gate pair a code cell's cancel
    /// checkpoints peek, and `lent` is the stop the turn lends its tool
    /// children, fired when the fact advances. The turn driver is the only
    /// caller.
    #[must_use]
    fn with_recorded_turn_cancel(
        self,
        honoured: bool,
        control: Arc<crate::runtime::turn_control::ActiveTurnControl>,
        host: Arc<dyn crate::EffectHost>,
        lent: CancellationToken,
    ) -> Self;

    /// The opener state this context incorporates against and hands groups to.
    #[must_use]
    fn opener_state(&self) -> OpenerState;

    /// Share `state` with this phase context: the owner of the opener (a turn
    /// driver, a process segment) creates it once and passes it to every
    /// context it builds.
    #[must_use]
    fn with_opener_state(self, state: OpenerState) -> Self;
}

#[doc(hidden)]
impl<'run> RuntimeExecutionContextRuntimeOps<'run> for RuntimeExecutionContext<'run> {
    fn new(
        dispatch: Arc<ToolDispatchContext<'run>>,
        process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
        attachment_store: Arc<crate::RuntimeAttachmentStore>,
        chronological_projection: Arc<crate::ChronologicalProjection>,
        turn_context: crate::TurnContext,
        execution_env_spec: crate::ProcessExecutionEnvSpec,
    ) -> Self {
        Self {
            dispatch,
            tool_material_store: None,
            tool_run: None,
            process_env_store,
            attachment_store,
            chronological_projection,
            turn_context,
            logical_run: None,
            live_tool_catalog: None,
            // This build's own epoch until the caller binds its store's
            // recorded `F` with `with_fleet_format`, as every context over a
            // durable store must (the session's and the process runner's).
            fleet_format: crate::FleetFormat::current(),
            execution_env_spec,
            process_execution: None,
            started_process_ids: Arc::default(),
            nested_effect_error: Arc::default(),
            incorporation_ledger: Arc::default(),
            opener_groups: Arc::default(),
            tool_requests: Arc::default(),
            cell_tool_calls: Arc::default(),
            tool_call_limit_refusal: Arc::default(),
            parent_invocation: None,
            turn_phase_probe: None,
            cancellation_token: None,
            token_is_lent_stop: false,
            turn_cancel: RecordedTurnCancel::default(),
            observe_turn_cancel: true,
            transferable_waits: false,
            turn_hands_over: false,
            wait_handed_over: Arc::default(),
            turn_cancel_scope: None,
            tracing: None,
            live_step: None,
            #[cfg(any(test, feature = "testing"))]
            fixture_standing: None,
            code_block_graph_key: None,
            issuing_language_node_id: None,
            process_work: None,
        }
    }
    fn dispatch(&self) -> &Arc<ToolDispatchContext<'run>> {
        &self.dispatch
    }

    fn tool_run_owner(&self) -> Option<ToolRunOwner> {
        RuntimeExecutionContext::tool_run_owner(self)
    }

    fn with_tool_run_owner(self, owner: &ToolRunOwner) -> Self {
        RuntimeExecutionContext::with_tool_run_owner(self, owner)
    }

    fn with_tracing(mut self, tracing: Option<RuntimeExecutionTracing>) -> Self {
        self.tracing = tracing;
        self
    }

    fn with_process_execution(
        mut self,
        process_id: ProcessId,
        registration: &crate::ProcessRegistration,
        event_context: impl Into<Option<RuntimeExecutionProcessEventContext>>,
    ) -> Self {
        // The lineage the process's body starts children under (FIG-3607 R1),
        // on the dispatch every start made inside this run realizes through.
        let mut dispatch = (*self.dispatch).clone();
        if dispatch.process_lineage.is_none() {
            dispatch.process_lineage = Some(registration.lineage(&process_id));
        }
        dispatch.process_originator = Some(registration.provenance.originator.clone());
        self.dispatch = Arc::new(dispatch);
        self.process_execution = Some(RuntimeProcessExecution {
            process_id,
            originator: registration.provenance.originator.clone(),
            env_ref: registration.env_ref.clone(),
            wake_session_id: registration.wake_session_id.clone(),
            event_context: event_context.into(),
        });
        self
    }
    fn with_recorded_turn_cancel(
        mut self,
        honoured: bool,
        control: Arc<crate::runtime::turn_control::ActiveTurnControl>,
        host: Arc<dyn crate::EffectHost>,
        lent: CancellationToken,
    ) -> Self {
        // A tool this execution runs in process cooperates through the same
        // lent stop its group children get: it fires only when the recorded
        // fact advances, so a tool's cancel is never a live read of the gate.
        if self.cancellation_token.is_none() {
            self.cancellation_token = Some(lent.clone());
        }
        let turn_cancel = RecordedTurnCancel {
            observed: Arc::default(),
            control: Some(control),
            host: Some(host),
            lent: Some(lent),
        };
        if honoured {
            turn_cancel.note();
        }
        self.turn_cancel = turn_cancel;
        self
    }
    fn opener_state(&self) -> OpenerState {
        OpenerState {
            ledger: Arc::clone(&self.incorporation_ledger),
            run_state: Arc::clone(&self.opener_groups),
        }
    }
    fn with_opener_state(mut self, state: OpenerState) -> Self {
        self.incorporation_ledger = state.ledger;
        self.opener_groups = state.run_state;
        self
    }
}

impl RuntimeExecutionContext<'_> {
    /// Called only inside X, before state and declarations can escape an inline
    /// body: whether the turn's gate accepted an immediate stop. A fired body
    /// token is cooperative delivery, not this authority: Closing fires it too.
    pub(crate) async fn inline_turn_stop_requested(
        &self,
    ) -> Result<bool, crate::RuntimeEffectControllerError> {
        let (Some(control), Some(host)) = (
            self.turn_cancel.control.as_ref(),
            self.turn_cancel.host.as_ref(),
        ) else {
            return Ok(false);
        };
        control
            .inline_stop_requested(host.await_event_resolver())
            .await
            .map_err(Into::into)
    }
}
