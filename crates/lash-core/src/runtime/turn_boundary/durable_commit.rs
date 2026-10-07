//! The session head commit a durable turn's `turn.commit` applies (ADR 0132
//! §4, L3 FIG-5172): the turn's messages and outcome over the head the turn
//! started from, built here and written by the phase runner inside the
//! session owner's fenced transaction.

use super::execution_state::{ExecutionStateUpdate, capture_execution_state_update};
use super::materialize::materialize_turn_reply;
use super::{TurnBoundary, derive_commit_node_ids, execution_state_capture_error};
use crate::session::Session;
use crate::store::{RuntimeCommit, StoreError};
use crate::{MessageSequence, TurnOutcome};

impl TurnBoundary {
    /// The turn's head commit once its machine is done with `new_messages`
    /// and `outcome`: the session's next revision over the state the turn
    /// started from, with the plugin states and the code executor's state
    /// `session` holds now. It writes nothing; the committed head is adopted
    /// by reloading it.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the commit cannot be assembled.
    pub(in crate::runtime) async fn durable_commit(
        &mut self,
        new_messages: MessageSequence,
        outcome: &TurnOutcome,
        failure_evidence: &[crate::TurnFailureEvidence],
        session: Option<&mut Session>,
    ) -> Result<RuntimeCommit, StoreError> {
        self.record_outcome_frame_switch(outcome)?;
        let cancelled = matches!(
            outcome,
            TurnOutcome::Stopped(crate::TurnStop::Cancelled { .. })
        );
        // A finished or stopped turn ends its code executor's execution, and
        // the state it leaves rides the commit.
        let (plugins, execution_state) = match session {
            Some(session) => {
                if let Some(executor) = session.plugins().code_executor() {
                    executor
                        .settle_code_execution(crate::plugin::CodeExecutionOutcome::Terminated)
                        .await
                        .map_err(execution_state_capture_error)?;
                }
                let update = capture_execution_state_update(session)
                    .await
                    .map_err(execution_state_capture_error)?;
                (Some(std::sync::Arc::clone(session.plugins())), update)
            }
            None => (None, ExecutionStateUpdate::Clean),
        };
        self.finalize_turn_read_state(new_messages, cancelled);
        let fleet_format = self.fleet_format;
        let clock = std::sync::Arc::clone(&self.clock);
        let graph_appends = self.graph_appends.clone();
        let protocol_terminal_output = self.protocol_terminal_output.clone();
        let turn_id = crate::TurnId::parse(self.operation_scope.id())?;
        let terminal_message_id = format!("m_turn_{turn_id}_assistant");
        let operation = self.final_operation();
        let commit_budget = self.commit_budget;
        let state = self.final_state_mut();
        if let Some(plugins) = plugins.as_deref() {
            state
                .capture_plugin_states(plugins, fleet_format)
                .map_err(|error| StoreError::TurnOutcomeMaterializationRefused {
                    error: Box::new(error),
                })?;
        }
        execution_state.apply(state)?;
        materialize_turn_reply(
            state,
            outcome,
            clock.as_ref(),
            &turn_id,
            &terminal_message_id,
            &protocol_terminal_output,
        );
        graph_appends
            .fold_into_final_state(state)
            .map_err(|error| StoreError::TurnOutcomeMaterializationRefused {
                error: Box::new(error),
            })?;
        let mut graph = state.pending_graph_commit();
        derive_commit_node_ids(state, &mut graph, &operation)?;
        let mut commit = RuntimeCommit::persisted_state_with_graph_commit_and_operation_and_budget(
            state,
            graph,
            operation,
            commit_budget,
            fleet_format,
        )?;
        commit.failure_evidence = failure_evidence.to_vec();
        commit.outcome = Some(crate::store::TurnCommitOutcome::from_terminal(outcome));
        Ok(commit)
    }
}
