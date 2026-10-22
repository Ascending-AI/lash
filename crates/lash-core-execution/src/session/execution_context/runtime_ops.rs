use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::super::OpenerState;
use super::{
    RecordedTurnCancel, RuntimeExecutionContext, RuntimeExecutionTracing, RuntimeProcessExecution,
    ToolDispatchContext,
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
        process_env_store: Arc<dyn lash_execution_contracts::process::ProcessEnvStore>,
        attachment_store: Arc<crate::RuntimeAttachmentStore>,
        chronological_projection: Arc<crate::ChronologicalProjection>,
        turn_context: crate::TurnContext,
    ) -> Self;

    /// The tool-execution context this run lends its tool calls.
    ///
    fn dispatch(&self) -> &Arc<ToolDispatchContext<'run>>;

    #[must_use]
    fn with_tracing(self, tracing: Option<RuntimeExecutionTracing>) -> Self;

    /// Run this execution inside `process`: what it reads off the process it
    /// runs in (its id, originator, and for an engine process `env`, its
    /// recorded environment) and what the children it starts inherit come
    /// from the process's recorded facts.
    fn with_process_execution(
        self,
        process: &lash_execution_contracts::process::ProcessRecord,
        env: Option<Arc<crate::ProcessEnv>>,
    ) -> Result<Self, crate::PluginError>;

    /// Starts this execution's recorded turn-cancel fact, unset: `lent` is
    /// the stop the turn lends its tool children, fired when the fact
    /// advances. The turn driver is the only caller.
    #[must_use]
    fn with_recorded_turn_cancel(self, lent: CancellationToken) -> Self;
    /// This context with `handoff` as how its cells hand their ends to the
    /// turn (FIG-5999). The turn driver is the only caller.
    #[must_use]
    fn with_cell_end(self, handoff: super::CellEndHandoff) -> Self;
    /// This context with `pages` as where its session's committed
    /// execution state stands (FIG-6193). The turn driver is the only
    /// caller.
    #[must_use]
    fn with_session_pages(self, pages: Option<crate::plugin::SessionPages>) -> Self;
    /// This context with `held` as the model response its cell's first
    /// save carries (FIG-6000). The turn driver is the only caller.
    #[must_use]
    fn with_held_response(self, held: Option<super::HeldResponse>) -> Self;

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
        process_env_store: Arc<dyn lash_execution_contracts::process::ProcessEnvStore>,
        attachment_store: Arc<crate::RuntimeAttachmentStore>,
        chronological_projection: Arc<crate::ChronologicalProjection>,
        turn_context: crate::TurnContext,
    ) -> Self {
        let facts = dispatch.facts.clone();
        Self {
            tool_live: Arc::new(crate::ToolLive::default()),
            tool_fault_retry: crate::runtime::PollPacing::fault_standard(),
            dispatch,
            tool_material_store: None,
            process_env_store,
            attachment_store,
            chronological_projection: Arc::new(super::ChronologicalRead::Owned(
                chronological_projection,
            )),
            turn_context,
            logical_run: None,
            resolved_run: None,
            // This build's own epoch until the caller binds its store's
            // recorded `F` with `with_fleet_format`, as every context over a
            // durable store must (the session's and the process runner's).
            fleet_format: crate::FleetFormat::current(),
            facts,
            process_execution: None,
            started_process_ids: Arc::default(),
            nested_effect_error: Arc::default(),
            cell_end: super::CellEndHandoff::default(),
            session_pages: None,
            held_response: None,
            incorporation_ledger: Arc::default(),
            tool_requests: Arc::default(),
            tool_call_limit_refusal: Arc::default(),
            linked_execution: None,
            extension_trace: crate::trace::ExtensionTrace::untraced(),
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
        process: &lash_execution_contracts::process::ProcessRecord,
        env: Option<Arc<crate::ProcessEnv>>,
    ) -> Result<Self, crate::PluginError> {
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
            provenance: process.provenance.clone(),
            enclosing: enclosing_process(process, env, self.linked_execution.as_ref())?,
        });
        Ok(self)
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

    fn with_cell_end(mut self, handoff: super::CellEndHandoff) -> Self {
        self.cell_end = handoff;
        self
    }

    fn with_session_pages(mut self, pages: Option<crate::plugin::SessionPages>) -> Self {
        self.session_pages = pages;
        self
    }

    fn with_held_response(mut self, held: Option<super::HeldResponse>) -> Self {
        self.held_response = held;
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

/// `process` with its recorded environment `env`, when it is an engine
/// process: what a child it starts may inherit.
fn enclosing_process(
    process: &lash_execution_contracts::process::ProcessRecord,
    env: Option<Arc<crate::ProcessEnv>>,
    linked: Option<&super::LinkedExecution>,
) -> Result<Option<Arc<crate::EnclosingProcess>>, crate::PluginError> {
    let lash_execution_contracts::process::ProcessInput::Engine {
        kind,
        payload,
        launch,
    } = process.input.as_ref()
    else {
        return Ok(None);
    };
    let invalid = || {
        crate::PluginError::Session(
            "engine process has no matching retained execution context".into(),
        )
    };
    let env = env.ok_or_else(invalid)?;
    let linked = linked.ok_or_else(invalid)?;
    if linked.execution != lash_core_store::resolutions::ExecutionId::process(&process.id)
        || linked.environment.as_ref() != env.execution()
    {
        return Err(invalid());
    }
    Ok(Some(Arc::new(crate::EnclosingProcess {
        process: process.id.clone(),
        execution: lash_core_store::resolutions::ExecutionId::process(&process.id),
        resolutions: linked
            .resolutions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
        env_ref: launch.env.clone(),
        env: Arc::unwrap_or_clone(env),
        engine_kind: kind.clone(),
        payload: payload.clone(),
        settings: launch.settings.clone(),
    })))
}
