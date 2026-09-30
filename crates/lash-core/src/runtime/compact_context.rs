//! The administrative compaction (FIG-4201): a session command the drive's
//! command lane applies at a turn boundary.
//!
//! The bound turn owns the session head. A store-backed session therefore
//! never compacts beside the drive: `compact_context` is
//! [`SessionCommand::CompactContext`](crate::SessionCommand::CompactContext),
//! which the command drain applies under the command root's sealed fence,
//! once no root is bound (ADR 0101 §4). The compaction's effects are
//! journaled under the command's own scope, named by its batch, so a redrive
//! of the command reads its recorded base and its summary back, and a
//! settled command is never committed again: its replay adopts the head its
//! commit published (FIG-4258). One commit opens the frame,
//! resets the stored execution state and the prompt usage and settles the
//! command with its outcome
//! ([`CompactContextOutcome`](super::CompactContextOutcome)), which the
//! submitter reads back from the batch's completion. The compaction's billed
//! usage is not part of that commit: its summarizer call is a spending
//! effect whose usage run the engine delivers (ADR 0125).
//!
//! A storeless runtime keeps a direct path,
//! [`LashRuntime::compact_storeless_context`]: it has no drive and no durable
//! head, and its `&mut self` already serializes the compaction with every
//! turn it runs.

use super::*;
use crate::facade_support::RuntimeSessionStateFacadeOps;
use crate::runtime::turn_boundary::SeedCarries;

/// The named phase a runtime's turn-phase probe sees once an administrative
/// compaction's commit has landed, before its command root goes on.
pub const COMPACT_CONTEXT_COMMITTED_PHASE: &str = "compact_context.committed";

/// What one administrative compaction did in resident state, before anything
/// of it commits.
enum CompactionRun {
    /// The compactor's seed opened a frame in resident state.
    Opened(CompactionFrameSwitch),
    /// The compactor found nothing to compact.
    NothingToCompact,
    /// The compaction failed before its frame opened: its prompt, its
    /// compactor or its frame open refused.
    Failed(RuntimeError),
}

/// The frame switch an administrative compaction commits (ADR 0113 §3.1):
/// the frame it left, what its seed carries out of that frame, and the
/// compaction's own execution, which gates the ended frame's cleanup.
struct CompactionFrameSwitch {
    ended: Option<crate::FrameNodeId>,
    carries: SeedCarries,
    committing: crate::ExecutionScope,
}

impl LashRuntime {
    /// Whether the runtime's session is store-backed: its administrative
    /// compaction is then a session command its drive applies, and a
    /// storeless runtime compacts through
    /// [`Self::compact_storeless_context`].
    pub fn is_store_backed(&self) -> bool {
        self.services.store.is_some()
    }

    /// Compact a storeless runtime's context directly, under
    /// `scoped_effect_controller`.
    ///
    /// A storeless runtime has no drive and no durable head: its `&mut self`
    /// serializes the compaction with every turn it runs, so the compaction
    /// needs no command lane. It records its base and journals its summary
    /// under the controller, opens its frame in resident state and restarts
    /// the live interpreter from the frame's seed (F5).
    ///
    /// A store-backed runtime is refused with
    /// [`RuntimeErrorCode::ContextCompaction`]: the bound turn owns its head,
    /// so it compacts through
    /// [`SessionCommand::CompactContext`](crate::SessionCommand::CompactContext).
    pub async fn compact_storeless_context(
        &mut self,
        instructions: Option<String>,
        scoped_effect_controller: crate::ScopedEffectController<'_>,
    ) -> Result<super::CompactContextOutcome, RuntimeError> {
        if self.is_store_backed() {
            return Err(RuntimeError::new(
                RuntimeErrorCode::ContextCompaction,
                "a store-backed session compacts through its command lane: submit \
                 `SessionCommand::CompactContext`, which applies at the next turn boundary",
            ));
        }
        match Box::pin(self.run_compaction(instructions, &scoped_effect_controller)).await? {
            CompactionRun::Opened(_) => {
                self.restore_protocol_session_after_frame_open().await?;
                Ok(super::CompactContextOutcome::Opened {
                    frame_node_id: self.opened_compaction_frame()?,
                })
            }
            CompactionRun::NothingToCompact => Ok(super::CompactContextOutcome::NothingToCompact),
            CompactionRun::Failed(error) => Ok(super::CompactContextOutcome::Failed {
                code: error.code,
                message: error.message,
            }),
        }
    }

    /// Apply the administrative compaction the command run `completion`
    /// names, under the command root's `drive_fence` (FIG-4201). `false`
    /// when the command was withdrawn since the lane was read: nothing was
    /// applied.
    ///
    /// The compaction runs under the command's own scope, the queue drain its
    /// batch names, rescoped from the command root's controller: a redrive of
    /// the unsettled command replays the base it recorded and the summary it
    /// journaled, and the frame key it derives from that scope names the
    /// same frame. A compaction that opened nothing, or failed, settles the
    /// command all the same, so a lane never waits on a compaction that
    /// cannot apply.
    pub(super) async fn apply_compact_context_command(
        &mut self,
        instructions: Option<String>,
        completion: crate::QueuedWorkCompletion,
        drive_fence: &crate::store::DriveFence,
        root_controller: &crate::ScopedEffectController<'_>,
    ) -> Result<bool, RuntimeError> {
        let [batch_id] = completion.batch_ids.as_slice() else {
            return Err(RuntimeError::new(
                RuntimeErrorCode::SessionCommandRun,
                format!(
                    "an administrative compaction applies alone, but its run names {:?}",
                    completion.batch_ids
                ),
            ));
        };
        let host = Arc::clone(&self.host.core.control.effect_host);
        let controller = super::drive::step_controller(
            root_controller,
            host.as_ref(),
            crate::AdmittedScope::queue_drain(self.state.session_id.clone(), batch_id.as_str()),
        )?;
        let run = Box::pin(self.run_compaction(instructions, &controller)).await?;
        Box::pin(self.commit_compact_context_command(run, completion, drive_fence)).await
    }

    /// Summarize the frame current at the compaction's recorded base and
    /// open the compaction frame in resident state.
    ///
    /// The run records its base first (FIG-4133), so a redrive summarizes
    /// the same history and reads its summary back from its journal. The
    /// frame key derives from the session, the compaction's scope and the
    /// frame it leaves. A failure that is the compaction's own (its prompt,
    /// its compactor, its frame open) is [`CompactionRun::Failed`]; a
    /// failure to read the session or record the base is the run's error.
    async fn run_compaction(
        &mut self,
        instructions: Option<String>,
        controller: &crate::ScopedEffectController<'_>,
    ) -> Result<CompactionRun, RuntimeError> {
        self.reload_invalidated_resident_session_state().await?;
        self.adopt_recorded_compaction_base(controller).await?;
        let services = self.runtime_session_services().map_err(|error| {
            RuntimeError::new(
                RuntimeErrorCode::ResidentSessionReloadFailed,
                error.to_string(),
            )
        })?;
        let ended = self.state.current_frame_node_id.clone();
        let committing = controller.execution_scope().clone();
        let Some(session) = self.session.as_ref() else {
            return Err(RuntimeError::new(
                RuntimeErrorCode::ResidentSessionReloadFailed,
                "runtime session not available",
            ));
        };
        let plugin_session = Arc::clone(session.plugins());
        let state = self.read_view();
        let system_prompt = match Self::compaction_system_prompt(
            session.context_prompt_contributions().to_vec(),
            Arc::clone(&plugin_session),
            Arc::clone(&services),
            self.state.session_id.clone(),
            state.clone(),
            self.protocol_turn_options().clone(),
            self.host.core.prompt.prompt.clone(),
            self.state.effective_policy().prompt.clone(),
        )
        .await
        {
            Ok(system_prompt) => system_prompt,
            Err(error) => {
                return Ok(CompactionRun::Failed(RuntimeError::new(
                    RuntimeErrorCode::ContextCompaction,
                    format!("the compaction's system prompt failed: {error}"),
                )));
            }
        };
        let ctx = crate::CompactionContext {
            session_id: self.state.session_id.clone(),
            plugin_config: self.state.admitted_plugin_config(),
            state,
            instructions,
            system_prompt,
            traces: services.trace_emitter(),
            scoped_effect_controller: controller.clone(),
            direct_completions: services.direct_completion_client(controller.clone(), None),
        };
        let compaction = match plugin_session.compact_context(&ctx).await {
            Ok(Some(compaction)) => compaction,
            Ok(None) => return Ok(CompactionRun::NothingToCompact),
            Err(error) => {
                return Ok(CompactionRun::Failed(RuntimeError::new(
                    RuntimeErrorCode::ContextCompaction,
                    format!("context compaction failed: {error}"),
                )));
            }
        };
        drop(ctx);
        let frame_key = compaction_frame_key(
            &self.state.session_id,
            controller.scope_id(),
            ended.as_deref().unwrap_or_default(),
        );
        let opened = match self
            .stage_agent_frame(
                crate::OpenAgentFrameRequest::new(frame_key, crate::AgentFrameReason::compaction())
                    .with_initial_nodes(compaction.initial_nodes),
                super::frame_open::StagedOpen::Compaction,
            )
            .await
        {
            Ok(opened) => opened,
            Err(error) => return Ok(CompactionRun::Failed(error)),
        };
        if !opened.result.opened {
            return Ok(CompactionRun::NothingToCompact);
        }
        Ok(CompactionRun::Opened(CompactionFrameSwitch {
            ended,
            carries: opened.carries,
            committing,
        }))
    }

    /// Commit what `run` did as the command's one commit (F2): the frame, its
    /// seed, the execution-state and prompt-usage reset and the command's
    /// settlement with its outcome, under the command root's fence.
    ///
    /// A replay of a command this root already settled commits nothing
    /// (FIG-4258): it adopts the durable head. The replay must not present
    /// the fence again. The drive that applied
    /// the command goes on to the input queued behind it, whose seal
    /// supersedes the command root's fence, and Restate replays the whole
    /// drive from its journal: the store checks a commit's fence before its
    /// receipt, so the settled commit would be refused as superseded.
    ///
    /// A frame whose recorded base the head has moved from, which only a
    /// writer outside the lane can do, can never commit: the command settles
    /// [`CompactContextOutcome::Failed`](super::CompactContextOutcome::Failed)
    /// with [`RuntimeErrorCode::StoreCommitSuperseded`] over the live head,
    /// and the lane goes on. A settlement without a frame is written over the
    /// live head whatever moved it. A newer admission that sealed since the
    /// command root's seal refuses the commit as superseded, with nothing of
    /// it durable: that admission's drive applies the command.
    async fn commit_compact_context_command(
        &mut self,
        run: CompactionRun,
        completion: crate::QueuedWorkCompletion,
        drive_fence: &crate::store::DriveFence,
    ) -> Result<bool, RuntimeError> {
        let store = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
            .ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorCode::StoreCommitFailed,
                    "a commanded compaction commits through the session's store",
                )
            })?;
        let batch_id = completion.batch_ids.first().cloned().ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::SessionCommandRun,
                "a commanded compaction settles its command row",
            )
        })?;
        let (mut switch, mut failure) = match run {
            CompactionRun::Opened(switch) => (Some(switch), None),
            CompactionRun::NothingToCompact => (None, None),
            CompactionRun::Failed(error) => (None, Some(error)),
        };
        if failure.is_some() {
            // Nothing of a failed compaction stays resident: the settlement
            // is written over the durable head.
            self.invalidate_resident_session_state();
            self.reload_invalidated_resident_session_state().await?;
        }
        let operation =
            crate::OperationId::new(self.state.queue_drain_scope(&batch_id), "session-command");
        if self
            .session_command_run_settled(&store, &completion)
            .await?
        {
            self.invalidate_resident_session_state();
            self.reload_invalidated_resident_session_state().await?;
            drop(RuntimeNamedPhase::begin(
                self.turn_phase_probe.clone(),
                COMPACT_CONTEXT_COMMITTED_PHASE,
            ));
            return Ok(true);
        }
        loop {
            let fleet_format = self.fleet_format();
            let (mut commit, persisted_node_ids) =
                crate::store::RuntimeCommit::persisted_state_with_operation_and_budget(
                    &mut self.state,
                    operation.clone(),
                    self.host.core.durability.commit_budget,
                    fleet_format,
                )
                .map_err(super::runtime_error_from_store_commit)?;
            let outcome = match (&switch, &failure) {
                (Some(switch), _) => {
                    commit.frame_transition = super::turn_boundary::committed_frame_transition(
                        &self.state,
                        switch.ended.clone(),
                        switch.carries.clone(),
                        &switch.committing,
                        &persisted_node_ids,
                    )
                    .map_err(super::runtime_error_from_store_commit)?;
                    super::CompactContextOutcome::Opened {
                        frame_node_id: self.opened_compaction_frame()?,
                    }
                }
                (None, None) => super::CompactContextOutcome::NothingToCompact,
                (None, Some(error)) => super::CompactContextOutcome::Failed {
                    code: error.code.clone(),
                    message: error.message.clone(),
                },
            };
            commit.drive_fence = Some(Box::new(drive_fence.clone()));
            commit.applied_commands = Some(completion.clone());
            for batch_id in &completion.batch_ids {
                commit.command_outcomes.insert(
                    batch_id.clone(),
                    super::SessionCommandOutcome::CompactContext {
                        outcome: outcome.clone(),
                    },
                );
            }
            let error = match store.commit_runtime_state_verified(commit).await {
                Ok(result) => {
                    self.state.apply_persisted_commit_result(result);
                    self.state.mark_node_ids_persisted(persisted_node_ids);
                    if switch.is_some() {
                        // Every accepted open restarts the live interpreter
                        // from the new frame's seed, on the drive's own
                        // resident runtime (F5).
                        self.restore_protocol_session_after_frame_open().await?;
                    }
                    drop(RuntimeNamedPhase::begin(
                        self.turn_phase_probe.clone(),
                        COMPACT_CONTEXT_COMMITTED_PHASE,
                    ));
                    return Ok(true);
                }
                Err(error) => error,
            };
            // Nothing of the commit is durable: resident state gives way to
            // the durable head before anything else reads it.
            self.invalidate_resident_session_state();
            match error {
                crate::StoreError::HeadRevisionConflict { .. } => {
                    self.reload_invalidated_resident_session_state().await?;
                    if switch.take().is_some() {
                        failure = Some(RuntimeError::new(
                            RuntimeErrorCode::StoreCommitSuperseded,
                            format!(
                                "the compaction's frame can never commit: the session head \
                                 moved from the base it summarized ({error})"
                            ),
                        ));
                    }
                }
                error => {
                    return match error {
                        // A host withdrew the command since the lane was
                        // read: nothing applied, and the lane is read again
                        // (FIG-3927 §2.7).
                        crate::StoreError::SessionCommandWithdrawn { .. } => Ok(false),
                        // A later admission sealed after the command root's:
                        // that admission's drive applies the command.
                        error @ crate::StoreError::StaleDriveFence { .. } => {
                            Err(RuntimeError::new(
                                RuntimeErrorCode::StoreCommitSuperseded,
                                error.to_string(),
                            ))
                        }
                        error => Err(super::runtime_error_from_store_commit(error)),
                    };
                }
            }
        }
    }

    /// The compaction frame an open just made current.
    fn opened_compaction_frame(&self) -> Result<crate::FrameNodeId, RuntimeError> {
        self.state.current_frame_node_id.clone().ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::ContextCompaction,
                "an opened compaction frame is the session's current frame",
            )
        })
    }
}

/// The frame an administrative compaction opens: the session, the
/// compaction's scope and the frame it leaves.
fn compaction_frame_key(
    session_id: &crate::SessionId,
    boundary_id: &str,
    previous_frame_node_id: &str,
) -> crate::FrameKey {
    crate::FrameKey::from_compaction_material(session_id, boundary_id, previous_frame_node_id)
}

#[cfg(test)]
mod tests {
    use super::compaction_frame_key;
    use crate::SessionId;

    #[test]
    fn compaction_frame_identity_is_replay_stable() {
        let first = compaction_frame_key(&SessionId::from("session"), "turn", "frame-before");
        let replay = compaction_frame_key(&SessionId::from("session"), "turn", "frame-before");
        let next = compaction_frame_key(&SessionId::from("session"), "turn", "frame-after");

        assert_eq!(first, replay);
        assert_ne!(first, next);
    }
}
