//! The context-pressure step of prepare (FIG-4110, ADR 0001, ADR 0105 §6).
//!
//! Core calls each plugin's context-pressure hook once per physical turn,
//! before the Prompt View transforms, with recorded facts: the committed read
//! view, the previous provider-reported prompt usage and the context window.
//! A hook holds no write service; it returns a decision and core writes it.
//!
//! - `Record` nodes join the turn's graph-append draft and commit with the
//!   turn.
//! - `OpenFrame` commits on its own, before the turn's model call, exactly as
//!   an administrative compaction's frame does: the records every hook
//!   decided land in the frame being left, the frame opens with its seed
//!   through the one frame-open primitive (which derives what the seed
//!   carries, and resets execution state and the prompt usage), and the
//!   commit makes all of it durable together, carrying the seed's artifacts
//!   into the new frame. The turn then runs in the new frame on resident
//!   state, with no reload (ADR 0112 §9), and the protocol restores its live
//!   execution state from the seed. A later failure of the turn leaves the
//!   frame in place.
//!
//! The commit is an idempotent fenced store write under an operation named by
//! the turn and the hook (ADR 0105 §9), and the frame key is core's, derived
//! from the session, the frame current at open, the turn and the hook. A hook
//! is named by the plugin that registered it and its own id, so two plugins
//! whose hooks share an id never share a record or frame namespace. A
//! redriven turn replays the head its run was admitted on (ADR 0105 §2), so
//! it decides again over the same base, reads the summarizer completion back
//! from its journal, and meets the frame commit's receipt: it never opens a
//! second frame or bills a second summary.

use super::*;

/// The writes one context-pressure hook's decision names, all derived from
/// the turn and the hook.
struct ContextPressureWrite<'a> {
    session_id: &'a SessionId,
    turn_id: &'a TurnId,
    plugin_id: &'a str,
    hook_id: &'a str,
}

impl ContextPressureWrite<'_> {
    /// The hook's author name within the turn: the registering plugin's id,
    /// length-prefixed so no pair of ids can spell another pair, and the
    /// hook's own id.
    fn author(&self) -> String {
        format!(
            "{}/context-pressure/{}:{}/{}",
            self.turn_id,
            self.plugin_id.len(),
            self.plugin_id,
            self.hook_id
        )
    }

    /// The append a decision's records ride. Named by the turn and the hook,
    /// so a redriven prepare re-records the same append.
    fn records_request(
        &self,
        nodes: Vec<crate::SessionAppendNode>,
    ) -> crate::AppendSessionNodesRequest {
        crate::AppendSessionNodesRequest {
            operation_id: format!("{}/records", self.author()),
            nodes,
            requires_ancestor_node_id: None,
        }
    }

    /// The draft namespace the records' nodes are minted in when they ride
    /// the frame's own commit: the same one a turn draft would mint them in.
    fn records_namespace(&self) -> Result<String, RuntimeError> {
        crate::runtime::state::boundary_operation(
            self.session_id,
            &format!("{}/records", self.author()),
            "append-session-nodes",
        )
        .storage_key()
        .map_err(|error| RuntimeError::new(RuntimeErrorCode::ContextPrepareTurn, error.to_string()))
    }

    /// The frame an `OpenFrame` decision opens (F3): the session, the frame
    /// current at open, the turn and the author. Compaction material is its
    /// own key domain, so it never collides with a `continue_as` key.
    fn frame_key(&self, current_frame_node_id: Option<&str>) -> crate::FrameKey {
        crate::FrameKey::from_compaction_material(
            self.session_id,
            &self.author(),
            current_frame_node_id.unwrap_or_default(),
        )
    }

    /// The operation the frame's own commit is written under.
    fn frame_operation(&self) -> crate::OperationId {
        crate::runtime::state::boundary_operation(self.session_id, &self.author(), "frame-commit")
    }
}

/// What the context-pressure step left for the turn.
pub(super) struct ContextPressureOutcome {
    /// A frame opened and committed; the turn runs in it.
    pub(super) opened_frame: bool,
    pub(super) decisions: Vec<crate::plugin::DecidedContextPressure>,
    /// Records decided without a frame, for the turn's graph-append draft.
    pub(super) turn_records: Vec<crate::AppendSessionNodesRequest>,
}

/// Everything the step reads from the turn being prepared.
pub(super) struct ContextPressureStep<'a, 'run> {
    pub(super) trace_turn_id: &'a TurnId,
    pub(super) previous_prompt_usage: Option<crate::TokenUsage>,
    pub(super) scoped_effect_controller: &'a ScopedEffectController<'run>,
    pub(super) shift_fence: Option<&'a ShiftFence>,
}

impl LashRuntime {
    /// Runs the context-pressure hooks and writes what they decided.
    pub(super) async fn run_context_pressure(
        &mut self,
        step: ContextPressureStep<'_, '_>,
    ) -> Result<ContextPressureOutcome, RuntimeError> {
        let ContextPressureStep {
            trace_turn_id,
            previous_prompt_usage,
            scoped_effect_controller,
            shift_fence,
        } = step;
        let session = self.session.as_ref().ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::ContextPrepareTurn,
                "runtime session not available",
            )
        })?;
        let plugin_session = Arc::clone(session.plugins());
        if !plugin_session.has_context_pressure_hooks() {
            return Ok(ContextPressureOutcome {
                opened_frame: false,
                decisions: Vec::new(),
                turn_records: Vec::new(),
            });
        }
        // Hooks cannot append; the services only need a draft to read through.
        let reads = TurnGraphAppendDraft::from_resident_state(
            &self.state,
            Arc::clone(&self.host.core.clock),
        );
        let manager = self
            .runtime_session_services_for_turn(shift_fence, &reads)
            .map_err(|err| {
                RuntimeError::new(RuntimeErrorCode::PluginSessionManager, err.to_string())
            })?;
        let read_view = self.read_view();
        // Lazy: rendered only if a hook actually summarizes. The render is
        // one recorded step of this turn, taken before the summarizer call,
        // so a redrive serves the recorded text and renders nothing
        // (FIG-4589). Every hook of the step shares the one render.
        let system_prompt: crate::plugin::CompactionSystemPrompt<'_> = {
            let input = super::super::compaction_prompt::CompactionPromptInput {
                session_id: self.state.session_id.clone(),
                protocol_session: Arc::clone(plugin_session.protocol_session()),
                plugin_config: self.state.admitted_plugin_config(),
                subagent: self.state.authority.subagent.clone(),
            };
            let controller = scoped_effect_controller.clone();
            let turn_id = trace_turn_id.to_string();
            // The step's hooks run one after another, so the first to ask
            // takes the recorded step and the rest read what it answered.
            let rendered: Arc<std::sync::Mutex<Option<Option<Arc<str>>>>> = Arc::default();
            Arc::new(move || {
                let input = input.clone();
                let controller = controller.clone();
                let turn_id = turn_id.clone();
                let rendered = Arc::clone(&rendered);
                Box::pin(async move {
                    if let Some(prompt) =
                        lash_sansio::sync::MutexExt::lock_recover(&*rendered).clone()
                    {
                        return Ok(prompt);
                    }
                    let prompt = super::super::compaction_prompt::recorded_compaction_prompt(
                        &controller,
                        super::super::compaction_prompt::CompactionPromptKey::Turn(&turn_id),
                        input,
                    )
                    .await
                    .map_err(crate::PluginError::RuntimeEffectController)?;
                    *lash_sansio::sync::MutexExt::lock_recover(&*rendered) = Some(prompt.clone());
                    Ok(prompt)
                })
            })
        };
        let ctx = crate::plugin::ContextPressureContext {
            writer_formats: Arc::new(crate::protocol_build::FleetWriterFormats(
                self.fleet_format(),
            )),
            session_id: self.state.session_id.clone(),
            plugin_config: self.state.admitted_plugin_config(),
            state: read_view,
            prompt_usage: previous_prompt_usage,
            max_context_tokens: LashRuntime::max_context_tokens(self).ok(),
            traces: manager.trace_emitter(),
            scoped_effect_controller: scoped_effect_controller.clone(),
            direct_completions: manager.direct_completion_client(
                scoped_effect_controller.clone(),
                Some(turn_phase_id(trace_turn_id, "context-pressure")),
            ),
            system_prompt: Some(system_prompt),
        };
        let decided = plugin_session
            .decide_context_pressure(&ctx, self.turn_phase_probe.clone())
            .await
            .map_err(|err| err.into_turn_failure(RuntimeErrorCode::ContextPrepareTurn))?;
        drop(ctx);
        drop(manager);
        let decisions = decided.clone();

        let session_id = self.state.session_id.clone();
        let mut turn_records = Vec::new();
        let mut frame_records = Vec::new();
        for crate::plugin::DecidedContextPressure {
            plugin_id,
            hook_id,
            decision,
        } in decided
        {
            let write = ContextPressureWrite {
                session_id: &session_id,
                turn_id: trace_turn_id,
                plugin_id: &plugin_id,
                hook_id: &hook_id,
            };
            match decision {
                crate::plugin::ContextPressureDecision::Continue => {}
                crate::plugin::ContextPressureDecision::Record { nodes } => {
                    frame_records.push((write.records_namespace()?, nodes.clone()));
                    turn_records.push(write.records_request(nodes));
                }
                crate::plugin::ContextPressureDecision::OpenFrame {
                    records,
                    task,
                    seed,
                } => {
                    if !records.is_empty() {
                        frame_records.push((write.records_namespace()?, records));
                    }
                    self.commit_context_pressure_frame(
                        &write,
                        frame_records,
                        task,
                        seed,
                        scoped_effect_controller,
                        shift_fence,
                    )
                    .await?;
                    return Ok(ContextPressureOutcome {
                        decisions,
                        opened_frame: true,
                        turn_records: Vec::new(),
                    });
                }
            }
        }
        Ok(ContextPressureOutcome {
            decisions,
            opened_frame: false,
            turn_records,
        })
    }

    /// Opens an `OpenFrame` decision's frame and commits it on its own (F2):
    /// the records in the frame being left, the frame node, its seed, the
    /// execution-state reset and the artifacts the seed carries become
    /// durable together, or none of them do and resident state is rebuilt
    /// from the store.
    async fn commit_context_pressure_frame(
        &mut self,
        write: &ContextPressureWrite<'_>,
        records: Vec<(String, Vec<crate::SessionAppendNode>)>,
        task: String,
        seed: Vec<crate::SessionAppendNode>,
        committing: &ScopedEffectController<'_>,
        shift_fence: Option<&ShiftFence>,
    ) -> Result<(), RuntimeError> {
        let opened = match self.open_context_pressure_frame(write, records, seed).await {
            Ok(opened) => opened,
            Err(error) => {
                self.invalidate_resident_session_state();
                return Err(error);
            }
        };
        let frame_node_id = opened.result.frame_node_id.clone();
        if let Err(error) = self
            .persist_context_pressure_frame(
                write,
                opened,
                committing.execution_scope(),
                shift_fence,
            )
            .await
        {
            // Nothing of the frame is durable: drop it from resident state,
            // which reloads from the store before the next turn.
            self.invalidate_resident_session_state();
            return Err(error);
        }
        // The frame's own commit just succeeded: this call made it, and a
        // redrive finds the head in the new frame and opens nothing.
        let tracing = &self.host.core.tracing;
        if tracing.is_observed() {
            tracing
                .unreplayed(committing.trace_scope().cloned())
                .observe(|| {
                    (
                        lash_trace::TraceContext::default()
                            .for_session(self.state.session_id.clone())
                            .for_turn(write.turn_id.clone()),
                        lash_trace::TraceEvent::Custom {
                            name: "context_pressure.frame_opened".to_string(),
                            payload: serde_json::json!({
                                "hook": write.hook_id,
                                "plugin": write.plugin_id,
                                "task": task,
                                "frame_node_id": frame_node_id,
                            }),
                        },
                    )
                });
        }
        // The open cleared the stored execution state; the live protocol
        // session restarts from the new frame's seed the same way (F5).
        self.restore_protocol_session_after_frame_open().await
    }

    async fn open_context_pressure_frame(
        &mut self,
        write: &ContextPressureWrite<'_>,
        records: Vec<(String, Vec<crate::SessionAppendNode>)>,
        seed: Vec<crate::SessionAppendNode>,
    ) -> Result<crate::runtime::frame_open::OpenedFrame, RuntimeError> {
        let clock = Arc::clone(&self.host.core.clock);
        let ended = self.state.current_frame_node_id.clone();
        for (namespace, nodes) in &records {
            append_session_nodes_to_state_with_clock(
                &mut self.state,
                nodes,
                namespace,
                clock.as_ref(),
            );
        }
        self.open_frame(
            crate::OpenAgentFrameRequest::new(
                write.frame_key(ended.as_ref().map(|frame| frame.as_str())),
                crate::AgentFrameReason::compaction(),
            )
            .with_initial_nodes(seed),
        )
        .await
    }

    async fn persist_context_pressure_frame(
        &mut self,
        write: &ContextPressureWrite<'_>,
        opened: crate::runtime::frame_open::OpenedFrame,
        committing: &crate::ExecutionScope,
        shift_fence: Option<&ShiftFence>,
    ) -> Result<(), RuntimeError> {
        // A storeless runtime keeps the frame resident, as it keeps
        // everything else.
        let Some(store) = self.services.store.clone() else {
            return Ok(());
        };
        let fleet_format = self.fleet_format();
        let (mut commit, persisted_node_ids) =
            crate::store::RuntimeCommit::persisted_state_with_operation_and_budget(
                &mut self.state,
                write.frame_operation(),
                self.host.core.durability.commit_budget,
                fleet_format,
            )
            .map_err(super::runtime_error_from_store_commit)?;
        commit.shift_fence = shift_fence.cloned().map(Box::new);
        commit.frame_transition = super::turn_boundary::committed_frame_transition(
            &self.state,
            opened.ended,
            opened.carries,
            committing,
            &persisted_node_ids,
        )
        .map_err(super::runtime_error_from_store_commit)?;
        super::frame_definition_carry::prepare(
            &self.host.core.process_engines,
            commit.frame_transition.as_ref(),
        )
        .await
        .map_err(super::runtime_error_from_store_commit)?;
        let result = store
            .commit_runtime_state_verified(commit, self.host.core.tracing.metrics())
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        self.state.apply_persisted_commit_result(result);
        self.state.mark_node_ids_persisted(persisted_node_ids);
        Ok(())
    }
}
