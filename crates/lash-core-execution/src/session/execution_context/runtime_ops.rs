use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::super::OpenerState;
use super::{
    ProcessId, RecordedTurnCancel, RuntimeExecutionContext, RuntimeExecutionProcessEventContext,
    RuntimeExecutionTracing, RuntimeProcessExecution, ToolDispatchContext,
};

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
    /// Exposed so the turn path can publish it to the live-opener registry
    /// (ADR 0099 §3): a tool child of a group this turn opens borrows the live
    /// half of exactly this context.
    fn dispatch(&self) -> &Arc<ToolDispatchContext<'run>>;

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

    /// Adds sources this context was built from that have no recorded form:
    /// what the embedder's open supplied and the turn's context overlay. A
    /// group tool child this context opens records them (FIG-3712).
    #[must_use]
    fn with_unrecorded_session_sources(
        self,
        sources: crate::runtime::effect::UnrecordedSessionSources,
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
            tool_children: None,
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
            unrecorded_sources: crate::runtime::effect::UnrecordedSessionSources::default(),
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
            #[cfg(any(test, feature = "testing"))]
            live_opener_guard: None,
            #[cfg(any(test, feature = "testing"))]
            tool_child_host: None,
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
    fn with_unrecorded_session_sources(
        mut self,
        sources: crate::runtime::effect::UnrecordedSessionSources,
    ) -> Self {
        self.unrecorded_sources = self.unrecorded_sources.union(sources);
        self
    }
    fn opener_state(&self) -> OpenerState {
        OpenerState {
            ledger: Arc::clone(&self.incorporation_ledger),
            groups: Arc::clone(&self.opener_groups),
        }
    }
    fn with_opener_state(mut self, state: OpenerState) -> Self {
        self.incorporation_ledger = state.ledger;
        self.opener_groups = state.groups;
        self
    }
}
