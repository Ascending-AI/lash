//! Host head writes are session commands (FIG-4202): an append, a plugin
//! command or task, and a durable frame open, applied by the drive's command
//! lane at a turn boundary.
//!
//! The bound turn owns the session head. A write a host makes from outside a
//! turn therefore never commits beside the drive: it is a
//! [`SessionCommand`](crate::SessionCommand) the command drain applies once
//! no root is bound (ADR 0101 §4), against the boundary's resident head, and
//! never against a checkpoint the host read before. Each applies alone, under
//! the command root's sealed fence, and settles in the one commit that makes
//! its head write: the commit completes the command's row and carries its
//! typed [`SessionCommandOutcome`](super::SessionCommandOutcome), which the
//! submitter reads back from the batch's completion on any runtime.
//!
//! - An append lands its nodes after everything the bound turn committed, or
//!   settles [`StaleBranch`](crate::AppendSessionNodesOutcome::StaleBranch)
//!   when its required ancestor left the active path.
//! - A plugin command or task runs its plugin's code only here, after
//!   admission. Its services join the command as in-turn services join a
//!   turn: its graph appends ride the command's commit, with its runtime
//!   events and plugin state, and its model usage is delivered by the engine
//!   per call (ADR 0125). A task's effects are journaled under
//!   the command's own session-operation scope, so a redrive of the unsettled
//!   command replays them. A host's cancel reaches an admitted task through
//!   its cancel signal ([`task_cancel`]), and a cancel the drive finds
//!   requested once the task's code returned settles the command
//!   `Cancelled`.
//! - A frame open opens the frame in the commit that settles it and restarts
//!   the live interpreter from the frame's seed (F5).
//!
//! A command that cannot apply (a failed plugin operation, a refused frame
//! open) settles all the same, with its typed refusal: the lane never waits
//! on a command that cannot apply.

use super::*;
use crate::facade_support::RuntimeSessionStateFacadeOps;
use crate::runtime::turn_boundary::SeedCarries;

mod task_cancel;
use task_cancel::PluginTaskCancelSignal;
pub use task_cancel::{PluginTaskCancelRequest, request_plugin_task_cancel};

/// The named phase a runtime's turn-phase probe sees when a host command
/// starts to apply: the drive read its run, and nothing of it has run.
pub const SESSION_COMMAND_APPLYING_PHASE: &str = "session_command.applying";
/// The named phase a runtime's turn-phase probe sees once a host command
/// applied in resident state, right before the commit that settles it.
pub const SESSION_COMMAND_STAGED_PHASE: &str = "session_command.staged";
/// The named phase a runtime's turn-phase probe sees once a host command's
/// settling commit landed, before its command root goes on.
pub const SESSION_COMMAND_COMMITTED_PHASE: &str = "session_command.committed";

/// A host plugin operation the command lane applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HostPluginOperation {
    Command,
    Task,
}

/// How a host command's settling commit ended.
pub(super) enum CommandCommit {
    /// The commit landed, or met the receipt of its first landing: the
    /// command is settled.
    Landed,
    /// A host withdrew the command since the lane was read: nothing was
    /// applied, and the lane is read again (FIG-3927 §2.7).
    Withdrawn,
    /// The store refused an append whose required ancestor is not on the
    /// active path. Nothing was written.
    AncestorNotActive { required_node_id: crate::NodeId },
    /// The commit exceeds the session's commit budget, which no redrive
    /// changes: nothing of the command committed, and it settled failed with
    /// the budget refusal, so the lane never waits on it.
    OverBudget,
}

/// The frame a command's commit opens: the frame it leaves and the scope of
/// the command's own execution, which gates the ended frame's cleanup
/// (ADR 0113 §3.1).
pub(super) struct CommandFrameSwitch {
    ended: Option<crate::FrameNodeId>,
    committing: crate::ExecutionScope,
}

impl LashRuntime {
    /// The one batch a command that settles with an outcome applies alone
    /// from: `completion` names exactly its batch.
    fn sole_command_batch(
        completion: &crate::QueuedWorkCompletion,
    ) -> Result<crate::BatchId, RuntimeError> {
        match completion.batch_ids.as_slice() {
            [batch_id] => Ok(batch_id.clone()),
            batch_ids => Err(RuntimeError::new(
                RuntimeErrorCode::SessionCommandRun,
                format!("a host command applies alone, but its run names {batch_ids:?}"),
            )),
        }
    }

    /// Apply a host's append (FIG-4202): its nodes land on the boundary's
    /// resident head, in the commit that settles the command. `false` when
    /// the command was withdrawn since the lane was read.
    pub(super) async fn apply_append_session_nodes_command(
        &mut self,
        request: crate::AppendSessionNodesRequest,
        completion: crate::QueuedWorkCompletion,
        drive_fence: &crate::store::DriveFence,
    ) -> Result<bool, RuntimeError> {
        let batch_id = Self::sole_command_batch(&completion)?;
        self.reload_invalidated_resident_session_state().await?;
        // An append's receipt identity is owned by the append operation key.
        let operation = crate::OperationId::new(
            self.state.session_operation_scope(&batch_id),
            "append-session-nodes",
        );
        let append_stamp = crate::RuntimeTurnCommitStamp::append_session_nodes(
            operation.clone(),
            request.requires_ancestor_node_id.as_deref(),
            &request.nodes,
        )
        .map_err(super::runtime_error_from_store_commit)?;
        let draft_namespace = operation
            .storage_key()
            .map_err(super::runtime_error_from_store_commit)?;
        let requested = request.nodes.len();
        super::state::append_session_nodes_to_state_with_clock(
            &mut self.state,
            &request.nodes,
            &draft_namespace,
            self.host.core.clock.as_ref(),
        );
        if let Some(session) = self.session.as_mut() {
            let protocol_session = Arc::clone(session.plugins().protocol_session());
            let session_id = self.state.session_id.clone();
            let appended = protocol_session
                .append_session_nodes(
                    crate::plugin::ProtocolSessionContext::new(&session_id, session.fleet_format()),
                    &request.nodes,
                )
                .await;
            if let Err(error) = appended {
                // The protocol refused the nodes: the command settles failed
                // over the durable head, with nothing appended.
                self.invalidate_resident_session_state();
                self.reload_invalidated_resident_session_state().await?;
                let committed = Box::pin(self.commit_host_command(
                    &completion,
                    drive_fence,
                    None,
                    None,
                    |_, _| crate::runtime::SessionCommandOutcome::Failed {
                        code: RuntimeErrorCode::SessionCommandRun,
                        message: format!("the protocol refused the appended nodes: {error}"),
                    },
                ))
                .await?;
                return Ok(!matches!(committed, CommandCommit::Withdrawn));
            }
        }
        self.stamp_live_plugin_state()?;
        let committed = Box::pin(self.commit_host_command(
            &completion,
            drive_fence,
            Some(append_stamp),
            None,
            |state, persisted| crate::runtime::SessionCommandOutcome::AppendSessionNodes {
                outcome: crate::AppendSessionNodesOutcome::Appended {
                    node_ids: persisted[persisted.len().saturating_sub(requested)..].to_vec(),
                    leaf_node_id: state.session_graph.leaf_node_id.clone(),
                },
            },
        ))
        .await?;
        match committed {
            CommandCommit::Landed | CommandCommit::OverBudget => Ok(true),
            CommandCommit::Withdrawn => Ok(false),
            // The required ancestor left the active path: the command settles
            // refused, over the durable head, with nothing appended.
            CommandCommit::AncestorNotActive { required_node_id } => {
                self.reload_invalidated_resident_session_state().await?;
                let settled = Box::pin(self.commit_host_command(
                    &completion,
                    drive_fence,
                    None,
                    None,
                    |_, _| crate::runtime::SessionCommandOutcome::AppendSessionNodes {
                        outcome: crate::AppendSessionNodesOutcome::StaleBranch {
                            required_node_id: required_node_id.clone(),
                        },
                    },
                ))
                .await?;
                Ok(!matches!(settled, CommandCommit::Withdrawn))
            }
        }
    }

    /// Apply a host's plugin command or task (FIG-4202): the plugin's code
    /// runs here, after admission, with services that join the command's
    /// commit, and its events, state and queued turns settle with it. A task
    /// a host's cancel reached before its code returned settles `Cancelled`
    /// instead (FIG-4391, FIG-4453). `false` when the command was withdrawn since the lane was
    /// read.
    pub(super) async fn apply_plugin_operation_command(
        &mut self,
        operation: HostPluginOperation,
        name: String,
        args: serde_json::Value,
        completion: crate::QueuedWorkCompletion,
        drive_fence: &crate::store::DriveFence,
    ) -> Result<bool, RuntimeError> {
        let batch_id = Self::sole_command_batch(&completion)?;
        self.reload_invalidated_resident_session_state().await?;
        let cancel_signal = match operation {
            HostPluginOperation::Command => None,
            HostPluginOperation::Task => {
                PluginTaskCancelSignal::open(
                    self.effect_host(),
                    &self.state.session_id,
                    batch_id.as_str(),
                )
                .await?
            }
        };
        let cancelled_before_it_ran = match &cancel_signal {
            Some(signal) => signal.cancel_requested().await?,
            None => false,
        };
        let ran = if cancelled_before_it_ran {
            Ok(crate::runtime::PluginOperationCommandOutcome::Cancelled)
        } else {
            Box::pin(self.run_host_plugin_operation(
                operation,
                &name,
                args,
                &batch_id,
                drive_fence,
                cancel_signal.as_ref(),
            ))
            .await?
        };
        let outcome = match ran {
            Ok(crate::runtime::PluginOperationCommandOutcome::Cancelled) => {
                // Nothing of a cancelled task stays resident: the
                // settlement is written over the durable head.
                self.invalidate_resident_session_state();
                self.reload_invalidated_resident_session_state().await?;
                crate::runtime::PluginOperationCommandOutcome::Cancelled
            }
            Ok(outcome) => outcome,
            Err(error) => {
                // Nothing of a failed operation stays resident: the
                // settlement is written over the durable head.
                self.invalidate_resident_session_state();
                self.reload_invalidated_resident_session_state().await?;
                match error {
                    PluginOperationInvokeError::AdmissionRefused(error) => {
                        crate::runtime::PluginOperationCommandOutcome::Refused { error }
                    }
                    error => crate::runtime::PluginOperationCommandOutcome::Failed {
                        message: error.to_string(),
                    },
                }
            }
        };
        let committed =
            Box::pin(
                self.commit_host_command(&completion, drive_fence, None, None, |_, _| {
                    crate::runtime::SessionCommandOutcome::PluginOperation { outcome }
                }),
            )
            .await?;
        Ok(!matches!(committed, CommandCommit::Withdrawn))
    }

    /// Run a host plugin operation's code against the boundary's resident
    /// state, and fold what it did into that state: its graph appends, its
    /// runtime events, its plugin state and the turns it queued.
    ///
    /// A task runs under `cancel_signal`'s watch, and the signal is peeked
    /// the moment its code returns: a host's cancel requested by then answers
    /// `Cancelled`, with nothing of the task folded into resident state. The
    /// decision is durable only with the command's settling commit
    /// (FIG-4453). The outer `Err` is the drive's fault (the peek did not
    /// answer), which settles nothing; the inner one is the operation's
    /// failure.
    async fn run_host_plugin_operation(
        &mut self,
        operation: HostPluginOperation,
        name: &str,
        args: serde_json::Value,
        batch_id: &crate::BatchId,
        drive_fence: &crate::store::DriveFence,
        cancel_signal: Option<&PluginTaskCancelSignal>,
    ) -> Result<
        Result<crate::runtime::PluginOperationCommandOutcome, PluginOperationInvokeError>,
        RuntimeError,
    > {
        let draft = super::turn_commit_draft::TurnGraphAppendDraft::from_resident_state(
            &self.state,
            Arc::clone(&self.host.core.clock),
        );
        // The operation's services join the command as in-turn services join
        // a turn: its appends ride the command's commit.
        let services = match self.runtime_session_services_for_turn(Some(drive_fence), &draft) {
            Ok(services) => services,
            Err(error) => return Ok(Err(error)),
        };
        let session_id = self.state.session_id.clone();
        let Some(session) = self.session.as_ref() else {
            return Ok(Err(PluginOperationInvokeError::Unknown(
                "runtime session not available".to_string(),
            )));
        };
        let plugins = Arc::clone(session.plugins());
        let ran = match operation {
            HostPluginOperation::Command => {
                plugins
                    .run_plugin_command(
                        name,
                        args,
                        Some(session_id.clone()),
                        true,
                        services.state_service(),
                        services.lifecycle_service(),
                        services.graph_service(),
                        services.process_service(),
                    )
                    .await
            }
            HostPluginOperation::Task => {
                // The task's effects are journaled under the command's own
                // scope, so a redrive of the unsettled command replays them.
                let controller =
                    match self
                        .effect_host()
                        .scoped_static(crate::AdmittedScope::session_operation(
                            session_id.clone(),
                            batch_id.as_str(),
                        )) {
                        Ok(Some(controller)) => controller,
                        Ok(None) => {
                            return Ok(Err(PluginOperationInvokeError::Failed(
                                "plugin task execution requires an effect host that can create a \
                             static scope for the command"
                                    .to_string(),
                            )));
                        }
                        Err(error) => {
                            return Ok(Err(PluginOperationInvokeError::Failed(error.to_string())));
                        }
                    };
                let stop = tokio_util::sync::CancellationToken::new();
                let task = plugins.run_plugin_task(
                    name,
                    args,
                    Some(session_id.clone()),
                    true,
                    services.state_service(),
                    services.lifecycle_service(),
                    services.graph_service(),
                    services.process_service(),
                    controller,
                    stop.clone(),
                );
                let ran = match cancel_signal {
                    Some(signal) => run_until_returned(task, signal.watch(&stop)).await,
                    None => task.await,
                };
                // The task's code returned: a cancel requested by now settles
                // nothing of it. The decision is written only by the settling
                // commit, so a redrive before that commit runs the task's
                // code again under the same live signal (FIG-4453).
                if let Some(signal) = cancel_signal
                    && signal.cancel_requested().await?
                {
                    drop(services);
                    return Ok(Ok(crate::runtime::PluginOperationCommandOutcome::Cancelled));
                }
                ran
            }
        };
        drop(services);
        let (plugin_id, outcome) = match ran {
            Ok(ran) => ran,
            Err(error) => return Ok(Err(error)),
        };
        Ok(self
            .fold_host_plugin_operation(
                plugin_id,
                outcome.output,
                outcome.events,
                outcome.directives,
                draft,
                batch_id,
            )
            .await)
    }

    /// Fold what a host plugin operation's code did into the boundary's
    /// resident state: its graph appends, its runtime events, its plugin
    /// state and the turns it queued.
    async fn fold_host_plugin_operation(
        &mut self,
        plugin_id: String,
        output: serde_json::Value,
        events: Vec<crate::PluginRuntimeEvent>,
        directives: Vec<crate::PluginRuntimeDirective>,
        draft: super::turn_commit_draft::TurnGraphAppendDraft,
        batch_id: &crate::BatchId,
    ) -> Result<crate::runtime::PluginOperationCommandOutcome, PluginOperationInvokeError> {
        draft
            .fold_into_final_state(&mut self.state)
            .map_err(|error| PluginOperationInvokeError::Failed(error.to_string()))?;
        if !events.is_empty() {
            let nodes = events
                .iter()
                .map(|event| {
                    crate::plugin_runtime_protocol_event(&plugin_id, event.clone())
                        .map(crate::SessionAppendNode::protocol_event)
                        .map_err(|err| {
                            PluginOperationInvokeError::Failed(format!(
                                "failed to encode plugin runtime event: {err}"
                            ))
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let events_operation = crate::OperationId::new(
                self.state.session_operation_scope(batch_id),
                "append-plugin-runtime-events",
            );
            let draft_namespace = events_operation.storage_key().map_err(|err| {
                PluginOperationInvokeError::Failed(format!(
                    "failed to encode plugin runtime event identity: {err}"
                ))
            })?;
            super::state::append_session_nodes_to_state_with_clock(
                &mut self.state,
                &nodes,
                &draft_namespace,
                self.host.core.clock.as_ref(),
            );
        }
        self.stamp_live_plugin_state()
            .map_err(|error| PluginOperationInvokeError::Failed(error.to_string()))?;
        // A queued turn lands before the settlement names it. A turn with no
        // source key takes one from the command, so a redrive of the
        // unsettled command enqueues the same turn once.
        let mut inputs = Vec::new();
        for (index, directive) in directives.into_iter().enumerate() {
            match directive {
                crate::PluginRuntimeDirective::QueueTurn { input, source_key } => {
                    let source_key = source_key
                        .unwrap_or_else(|| format!("input:command:{batch_id}:queue-turn:{index}"));
                    inputs.push((input, Some(source_key)));
                }
            }
        }
        let pending_turn_inputs = if inputs.is_empty() {
            Vec::new()
        } else {
            let store = self
                .session
                .as_ref()
                .and_then(|session| session.history_store())
                .ok_or_else(|| {
                    PluginOperationInvokeError::Failed(
                        "plugin input requires a session store".into(),
                    )
                })?;
            super::durable_queue::enqueue_turn_inputs_to_store(
                self.state.session_id.clone(),
                store,
                &self.ingress_relay(),
                inputs,
                crate::TurnInputIngress::NextTurn,
                crate::RunSpec::default(),
                false,
            )
            .await
            .map_err(|error| PluginOperationInvokeError::AdmissionRefused(Box::new(error)))?
            .0
        };
        Ok(crate::runtime::PluginOperationCommandOutcome::Completed {
            plugin_id,
            output,
            events,
            pending_turn_inputs,
        })
    }

    /// Apply a host's durable frame open (FIG-4202): the frame opens in the
    /// commit that settles the command, and the live interpreter restarts
    /// from its seed. A refused open settles refused, with nothing opened.
    /// `false` when the command was withdrawn since the lane was read.
    pub(super) async fn apply_open_agent_frame_command(
        &mut self,
        request: crate::OpenAgentFrameRequest,
        completion: crate::QueuedWorkCompletion,
        drive_fence: &crate::store::DriveFence,
    ) -> Result<bool, RuntimeError> {
        let batch_id = Self::sole_command_batch(&completion)?;
        self.reload_invalidated_resident_session_state().await?;
        let committing = self.state.session_operation_scope(&batch_id);
        let staged = self
            .stage_agent_frame(request, super::frame_open::StagedOpen::Caller)
            .await;
        let (outcome, switch) = match staged {
            Ok(opened) => {
                let switch = opened.result.opened.then(|| CommandFrameSwitch {
                    ended: opened.ended.clone(),
                    committing: committing.clone(),
                });
                (
                    crate::runtime::OpenAgentFrameCommandOutcome::Opened {
                        outcome: opened.result,
                    },
                    switch,
                )
            }
            Err(error) => {
                self.invalidate_resident_session_state();
                self.reload_invalidated_resident_session_state().await?;
                (
                    crate::runtime::OpenAgentFrameCommandOutcome::Refused {
                        code: error.code,
                        message: error.message,
                    },
                    None,
                )
            }
        };
        let opens = switch.is_some();
        let committed = Box::pin(self.commit_host_command(
            &completion,
            drive_fence,
            None,
            switch,
            |state, persisted| {
                // The frame's committed node id replaces the draft id the
                // resident open answered with.
                let outcome = match outcome {
                    crate::runtime::OpenAgentFrameCommandOutcome::Opened { mut outcome }
                        if opens =>
                    {
                        if let Some(current) = state.current_frame_node_id.as_ref() {
                            outcome.frame_node_id = current.to_string();
                        }
                        let seeds = outcome.initial_node_ids.len();
                        outcome.initial_node_ids =
                            persisted[persisted.len().saturating_sub(seeds)..].to_vec();
                        crate::runtime::OpenAgentFrameCommandOutcome::Opened { outcome }
                    }
                    outcome => outcome,
                };
                crate::runtime::SessionCommandOutcome::OpenAgentFrame { outcome }
            },
        ))
        .await?;
        if matches!(committed, CommandCommit::Landed) && opens {
            // Every accepted open restarts the live interpreter from the new
            // frame's seed, on the drive's own resident runtime (F5).
            self.restore_protocol_session_after_frame_open().await?;
        }
        Ok(!matches!(committed, CommandCommit::Withdrawn))
    }

    /// The operation a host command's commit is identified by: the command's
    /// own session operation, named by its batch.
    fn command_operation(&self, batch_id: &crate::BatchId) -> crate::OperationId {
        crate::OperationId::new(
            self.state.session_operation_scope(batch_id),
            "session-command",
        )
    }

    /// Commit the resident state as the command's one commit (F2): whatever
    /// the command put in resident state and the command's settlement with the outcome `outcome` derives from
    /// the committed state and its persisted node ids, under the command
    /// root's fence.
    ///
    /// Nothing of a commit that did not land stays resident: resident state
    /// gives way to the durable head. A newer admission that sealed since
    /// the command root's seal refuses the commit as superseded, with nothing
    /// of it durable: that admission's drive applies the command.
    ///
    /// A commit over the session's commit budget settles the command failed
    /// with the budget refusal instead, over the durable head; the budget
    /// does not charge that refusal receipt (FIG-4471), so the settlement
    /// fits wherever the head's bare commit does. A head whose
    /// bare settlement exceeds the budget is one a host lowered the budget
    /// below (ADR 0058, FIG-4393): the settlement's typed refusal ends the
    /// command root, the drive stops at it, and the command stays open until
    /// the host raises the budget again.
    pub(super) async fn commit_host_command(
        &mut self,
        completion: &crate::QueuedWorkCompletion,
        drive_fence: &crate::store::DriveFence,
        append_stamp: Option<crate::RuntimeTurnCommitStamp>,
        switch: Option<CommandFrameSwitch>,
        outcome: impl FnOnce(
            &crate::RuntimeSessionState,
            &[crate::NodeId],
        ) -> crate::runtime::SessionCommandOutcome,
    ) -> Result<CommandCommit, RuntimeError> {
        let over_budget = match Box::pin(self.commit_host_command_once(
            completion,
            drive_fence,
            append_stamp,
            switch,
            outcome,
        ))
        .await?
        {
            Ok(committed) => return Ok(committed),
            Err(over_budget) => over_budget,
        };
        self.reload_invalidated_resident_session_state().await?;
        let settled =
            Box::pin(
                self.commit_host_command_once(completion, drive_fence, None, None, |_, _| {
                    crate::runtime::SessionCommandOutcome::Failed {
                        code: over_budget.code.clone(),
                        message: over_budget.message.clone(),
                    }
                }),
            )
            .await?;
        match settled {
            Ok(CommandCommit::Landed) => Ok(CommandCommit::OverBudget),
            Ok(committed) => Ok(committed),
            // Even the bare settlement exceeds the budget: the live head
            // itself is over it (ADR 0058), and its refusal names the size
            // the host must raise the budget to.
            Err(bare_over_budget) => Err(bare_over_budget),
        }
    }

    /// One attempt at the command's commit: `Err` carries a commit-budget
    /// refusal, after which nothing of the commit is resident.
    async fn commit_host_command_once(
        &mut self,
        completion: &crate::QueuedWorkCompletion,
        drive_fence: &crate::store::DriveFence,
        append_stamp: Option<crate::RuntimeTurnCommitStamp>,
        switch: Option<CommandFrameSwitch>,
        outcome: impl FnOnce(
            &crate::RuntimeSessionState,
            &[crate::NodeId],
        ) -> crate::runtime::SessionCommandOutcome,
    ) -> Result<Result<CommandCommit, RuntimeError>, RuntimeError> {
        let store = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
            .ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorCode::StoreCommitFailed,
                    "a host command commits through the session's store",
                )
            })?;
        let batch_id = Self::sole_command_batch(completion)?;
        // An append commits under the append's own operation, which owns its
        // receipt identity; every other command under the command's.
        let operation = append_stamp.as_ref().map_or_else(
            || self.command_operation(&batch_id),
            |stamp| stamp.operation.clone(),
        );
        if let Some(session) = self.session.as_ref() {
            self.state.capture_plugin_states(session.plugins())?;
        }
        let fleet_format = self.fleet_format();
        let (mut commit, persisted_node_ids) =
            crate::store::RuntimeCommit::persisted_state_with_operation_and_budget(
                &mut self.state,
                operation,
                self.host.core.durability.commit_budget,
                fleet_format,
            )
            .map_err(super::runtime_error_from_store_commit)?;
        if let Some(stamp) = append_stamp {
            commit.turn_commit = stamp;
        }
        if let Some(switch) = switch {
            commit.frame_transition = super::turn_boundary::committed_frame_transition(
                &self.state,
                switch.ended,
                SeedCarries::none(),
                &switch.committing,
                &persisted_node_ids,
            )
            .map_err(super::runtime_error_from_store_commit)?;
        }
        commit.drive_fence = Some(Box::new(drive_fence.clone()));
        commit.applied_commands = Some(completion.clone());
        let outcome = outcome(&self.state, &persisted_node_ids);
        for batch_id in &completion.batch_ids {
            commit
                .command_outcomes
                .insert(batch_id.clone(), outcome.clone());
        }
        drop(RuntimeNamedPhase::begin(
            self.turn_phase_probe.clone(),
            SESSION_COMMAND_STAGED_PHASE,
        ));
        match store.commit_runtime_state_verified(commit).await {
            Ok(result) => {
                let receipt_replayed = result.receipt_replayed;
                self.state.apply_persisted_commit_result(result);
                self.state.mark_node_ids_persisted(persisted_node_ids);
                if receipt_replayed {
                    // The durable head is the replayed commit's.
                    self.invalidate_resident_session_state();
                    self.reload_invalidated_resident_session_state().await?;
                }
                drop(RuntimeNamedPhase::begin(
                    self.turn_phase_probe.clone(),
                    SESSION_COMMAND_COMMITTED_PHASE,
                ));
                Ok(Ok(CommandCommit::Landed))
            }
            Err(error) => {
                self.invalidate_resident_session_state();
                match error {
                    crate::StoreError::SessionCommandWithdrawn { .. } => {
                        Ok(Ok(CommandCommit::Withdrawn))
                    }
                    error @ (crate::StoreError::CommitByteBudgetExceeded { .. }
                    | crate::StoreError::CommitNodeBudgetExceeded { .. }) => {
                        Ok(Err(super::runtime_error_from_store_commit(error)))
                    }
                    crate::StoreError::AppendAncestorNotActive { required_node_id } => {
                        Ok(Ok(CommandCommit::AncestorNotActive { required_node_id }))
                    }
                    // A later admission that sealed after the command root's
                    // is a superseded commit: that admission's drive applies
                    // the command.
                    error => Err(super::runtime_error_from_store_commit(error)),
                }
            }
        }
    }
}

/// Await `task` to its own end while `watch` runs beside it: the watch may
/// fire the task's cancellation token, and never ends the task itself.
async fn run_until_returned<T>(
    task: impl std::future::Future<Output = T>,
    watch: impl std::future::Future<Output = ()>,
) -> T {
    let task = std::pin::pin!(task);
    let watch = std::pin::pin!(watch);
    match futures_util::future::select(task, watch).await {
        futures_util::future::Either::Left((returned, _watch)) => returned,
        futures_util::future::Either::Right(((), task)) => task.await,
    }
}
