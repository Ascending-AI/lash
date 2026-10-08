use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::super::OpenerState;
use super::{
    RecordedTurnCancel, RuntimeExecutionContext, RuntimeExecutionProcessEventContext,
    RuntimeExecutionTracing, RuntimeProcessExecution, ToolDispatchContext,
};

impl RuntimeExecutionContext<'_> {
    /// Execution-side only: run one recorded step body that this execution
    /// issues in process (a tool attempt) under a child of the execution's
    /// cooperative stop. The body gets the stop; what it returns is the
    /// step's recorded outcome.
    pub(crate) async fn run_turn_step_body<T, F, Fut>(&self, body: F) -> T
    where
        F: FnOnce(Option<CancellationToken>) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        // Each native body owns a child of the execution's stop: Closing
        // can stop a loser without firing the parent's other bodies.
        let stop = self
            .cancellation_token
            .as_ref()
            .map(CancellationToken::child_token)
            .unwrap_or_default();
        body(Some(stop)).await
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

    #[must_use]
    fn with_tracing(self, tracing: Option<RuntimeExecutionTracing>) -> Self;

    /// Run this execution inside `process`: what it reads off the process it
    /// runs in (its id, originator and captured environment) and what the
    /// children it starts inherit come from the process's recorded facts.
    #[must_use]
    fn with_process_execution(
        self,
        process: &crate::ProcessRecord,
        event_context: impl Into<Option<RuntimeExecutionProcessEventContext>>,
    ) -> Self;

    /// Starts this execution's recorded turn-cancel fact, unset: `lent` is
    /// the stop the turn lends its tool children, fired when the fact
    /// advances. The turn driver is the only caller.
    #[must_use]
    fn with_recorded_turn_cancel(self, lent: CancellationToken) -> Self;

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
            tool_fault_retry: crate::runtime::PollPacing::fault_standard(),
            dispatch,
            tool_material_store: None,
            process_env_store,
            attachment_store,
            chronological_projection,
            turn_context,
            logical_run: None,
            run_capabilities: Arc::default(),
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
            tool_requests: Arc::default(),
            tool_call_limit_refusal: Arc::default(),
            parent_invocation: None,
            turn_phase_probe: None,
            cancellation_token: None,
            token_is_lent_stop: false,
            turn_cancel: RecordedTurnCancel::default(),
            observe_turn_cancel: true,
            wait_handed_over: Arc::default(),
            turn_cancel_scope: None,
            tracing: None,
            live_step: None,
            #[cfg(any(test, feature = "testing"))]
            fixture_standing: None,
            code_block_graph_key: None,
            issuing_language_node_id: None,
            language_calls: Arc::default(),
            process_work: None,
        }
    }
    fn dispatch(&self) -> &Arc<ToolDispatchContext<'run>> {
        &self.dispatch
    }

    fn with_tracing(mut self, tracing: Option<RuntimeExecutionTracing>) -> Self {
        self.tracing = tracing;
        self
    }

    fn with_process_execution(
        mut self,
        process: &crate::ProcessRecord,
        event_context: impl Into<Option<RuntimeExecutionProcessEventContext>>,
    ) -> Self {
        // The lineage the process's body starts children under (FIG-3607 R1),
        // on the dispatch every start made inside this run realizes through.
        let mut dispatch = (*self.dispatch).clone();
        if dispatch.process_lineage.is_none() {
            dispatch.process_lineage = Some(process.lineage());
        }
        dispatch.process_originator = Some(process.provenance.originator.clone());
        self.dispatch = Arc::new(dispatch);
        self.process_execution = Some(RuntimeProcessExecution {
            process_id: process.id.clone(),
            originator: process.provenance.originator.clone(),
            env_ref: process.env_ref.clone(),
            event_context: event_context.into(),
        });
        self
    }
    fn with_recorded_turn_cancel(mut self, lent: CancellationToken) -> Self {
        // A tool this execution runs in process cooperates through the same
        // lent stop its group children get: it fires only when the recorded
        // fact advances, so a tool's cancel is never a live read of the gate.
        if self.cancellation_token.is_none() {
            self.cancellation_token = Some(lent.clone());
        }
        self.turn_cancel = RecordedTurnCancel {
            observed: Arc::default(),
            lent: Some(lent),
        };
        self
    }
    fn opener_state(&self) -> OpenerState {
        OpenerState {
            ledger: Arc::clone(&self.incorporation_ledger),
        }
    }
    fn with_opener_state(mut self, state: OpenerState) -> Self {
        self.incorporation_ledger = state.ledger;
        self
    }
}

impl RuntimeExecutionContext<'_> {
    /// Called only inside X, before state and declarations can escape an inline
    /// body: whether this execution's recorded fact says the turn stopped. A
    /// fired body token is cooperative delivery, not this authority: Closing
    /// fires it too.
    pub(crate) async fn inline_turn_stop_requested(
        &self,
    ) -> Result<bool, crate::RuntimeEffectControllerError> {
        Ok(self.turn_cancel.is_observed())
    }
}
