//! The session head commit a durable turn's `turn.commit` applies (ADR 0132
//! §4, L3 FIG-5172): the turn's messages and outcome over the head the turn
//! started from, with what its after-turn callbacks decided (FIG-5283),
//! built here and written by the phase runner inside the session owner's
//! fenced transaction.

use super::execution_state::{ExecutionStateUpdate, capture_execution_state_update};
use super::materialize::materialize_turn_reply;
use super::{TurnBoundary, derive_commit_node_ids, execution_state_capture_error};
use crate::plugin::{AfterTurnDecisions, PluginSession, StagedPluginState};
use crate::session::Session;
use crate::store::{RuntimeCommit, StoreError};
use crate::{MessageSequence, TurnOutcome};
use lash_core_store::session_state::SessionPluginStateSource;

/// A session's plugins as the head commit records them: with the after-turn
/// callbacks' staged resolutions applied over their published state, which
/// publish only once the commit is acknowledged.
struct CommittedPlugins<'a> {
    plugins: &'a PluginSession,
    staged: &'a StagedPluginState,
}

impl SessionPluginStateSource for CommittedPlugins<'_> {
    fn capture_plugin_admission(
        &self,
        config: &crate::PluginConfig,
        fleet: crate::store::FleetFormat,
    ) -> Result<Option<std::sync::Arc<[u8]>>, crate::RuntimeError> {
        self.plugins.capture_plugin_admission(config, fleet)
    }

    fn tool_state_generation(&self) -> u64 {
        self.plugins.tool_state_generation()
    }

    fn export_tool_state(&self) -> crate::ToolState {
        self.plugins.export_tool_state()
    }

    fn export_plugin_state(&self) -> Result<crate::PluginState, crate::RuntimeError> {
        self.plugins.export_plugin_state()
    }

    fn capture_plugin_state(&self) -> Result<crate::PluginState, crate::RuntimeError> {
        self.plugins
            .committed_state_with(self.staged)
            .map_err(|error| crate::RuntimeEffectControllerError::from(error).into_runtime_error())
    }

    fn committed_plugin_config(
        &self,
        config: &crate::PluginConfig,
    ) -> Result<Option<crate::PluginConfig>, crate::RuntimeError> {
        SessionPluginStateSource::committed_plugin_config(self.plugins, config)
    }
}

impl TurnBoundary {
    /// The turn's head commit once its machine is done with `new_messages`
    /// and `outcome`: the session's next revision over the state the turn
    /// started from, with the plugin states and the code executor's state
    /// `session` holds now, and `after_turn`'s records and staged state. It
    /// writes nothing; the committed head is adopted by reloading it.
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
        after_turn: Option<&AfterTurnDecisions>,
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
            let captured = match after_turn {
                Some(after_turn) => state.capture_plugin_states(
                    &CommittedPlugins {
                        plugins,
                        staged: &after_turn.state,
                    },
                    fleet_format,
                ),
                None => state.capture_plugin_states(plugins, fleet_format),
            };
            captured.map_err(|error| StoreError::TurnOutcomeMaterializationRefused {
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
        // The after-turn callbacks' records, in callback order, after
        // everything else the turn appends.
        let mut record_ordinal = 0usize;
        for decision in after_turn.map_or(&[][..], |after_turn| &after_turn.decisions) {
            for record in &decision.records {
                state.session_graph.append_node_drafts_at(
                    &format!(
                        "{turn_id}:after_turn:{}:plugin:{record_ordinal}",
                        decision.plugin_id
                    ),
                    [crate::session_graph::SessionNodeDraft::plugin(
                        record.plugin_type.clone(),
                        record.body.clone(),
                    )],
                    clock.node_timestamp(),
                );
                record_ordinal += 1;
            }
        }
        let attachments = committed_attachment_ids(state);
        let mut graph = state.pending_graph_commit();
        derive_commit_node_ids(state, &mut graph, &operation)?;
        let mut commit = RuntimeCommit::persisted_state_with_graph_commit_and_operation_and_budget(
            state,
            graph,
            operation,
            commit_budget,
            fleet_format,
        )?
        .with_committed_attachments(attachments);
        commit.failure_evidence = failure_evidence.to_vec();
        commit.outcome = Some(crate::store::TurnCommitOutcome::from_terminal(outcome));
        Ok(commit)
    }
}

/// Every stored attachment the committed transcript names, a tool result's
/// blocks and its retained outputs included: the commit holds each on the
/// session, so what the turn wrote outlives its upload's staging referrer
/// (ADR 0124 §4).
fn committed_attachment_ids(state: &crate::RuntimeSessionState) -> Vec<crate::AttachmentId> {
    let mut ids = std::collections::BTreeSet::new();
    for message in &state.read_model().messages {
        for part in message.parts.iter() {
            ids.extend(part.attachments().map(|reference| reference.id.clone()));
            ids.extend(
                part.retained_outputs()
                    .map(|retained| retained.reference.id.clone()),
            );
        }
    }
    ids.into_iter().collect()
}
