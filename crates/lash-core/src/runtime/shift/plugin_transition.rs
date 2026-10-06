//! Recorded transition preparation and fenced publication for a session run.
use super::{run::run_step_invocation, shift_abort};
use crate::ScopedEffectController;
use crate::engine::{Admitted, ShiftAbort};
use crate::runtime::LashRuntime;
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;

pub(super) enum TransitionBasis<'a> {
    Admitted,
    Resume(
        &'a crate::store::SessionHeadRef,
        &'a crate::store::ShiftFence,
    ),
    FollowOn(&'a crate::store::PendingFollowOn),
}

pub(super) fn command_transition_request(
    controller: &ScopedEffectController<'_>,
    admitted: &Admitted,
) -> Result<crate::plugin::PluginTransitionRequest, ShiftAbort> {
    let invocation = run_step_invocation(controller, admitted, "plugin-transition")?;
    Ok(crate::plugin::PluginTransitionRequest {
        id: crate::plugin::PluginTransitionId(invocation.address().clone()),
        owner: crate::RuntimeOwner::Session(admitted.session().clone()),
        base: crate::plugin::PluginTransitionBase::SessionCommand {
            run: admitted.run().clone(),
        },
        target: Default::default(),
    })
}

impl LashRuntime {
    /// Record the complete transition, or resume the native view the run
    /// already published at its admission's recorded advanced head.
    pub(super) async fn record_plugin_transition(
        &self,
        controller: &ScopedEffectController<'_>,
        admitted: &Admitted,
        base: &crate::store::SessionHeadRef,
        target: &crate::store::plugin_writers::PluginAdmission,
        basis: TransitionBasis<'_>,
    ) -> Result<crate::plugin::PluginTransitionRecord, ShiftAbort> {
        let invocation = run_step_invocation(controller, admitted, "plugin-transition")?;
        let request = crate::plugin::PluginTransitionRequest {
            id: crate::plugin::PluginTransitionId(invocation.address().clone()),
            owner: crate::RuntimeOwner::Session(admitted.session().clone()),
            base: crate::plugin::PluginTransitionBase::Session { head: base.clone() },
            target: target.clone(),
        };
        // A turn run states the host's tool-source policy at its transition;
        // a command run applies host commands, a tool restore among them, and
        // tolerates (FIG-5134).
        self.record_transition_request(
            controller,
            admitted,
            request,
            basis,
            self.host.core.control.tool_source_policy,
        )
        .await
    }

    pub(super) async fn record_command_plugin_transition(
        &self,
        controller: &ScopedEffectController<'_>,
        admitted: &Admitted,
    ) -> Result<crate::plugin::PluginTransitionRecord, ShiftAbort> {
        self.record_transition_request(
            controller,
            admitted,
            command_transition_request(controller, admitted)?,
            TransitionBasis::Admitted,
            crate::ToolSourcePolicy::Tolerate,
        )
        .await
    }

    async fn record_transition_request(
        &self,
        controller: &ScopedEffectController<'_>,
        admitted: &Admitted,
        request: crate::plugin::PluginTransitionRequest,
        basis: TransitionBasis<'_>,
        tool_source_policy: crate::ToolSourcePolicy,
    ) -> Result<crate::plugin::PluginTransitionRecord, ShiftAbort> {
        let invocation = run_step_invocation(controller, admitted, "plugin-transition")?;
        let runner = PluginTransitionRunner {
            host: self.services.plugins.host().clone(),
            plugins: std::sync::Arc::clone(&self.services.plugins),
            store: self.shift_store()?,
            resident: self.resident_head(),
            initial: self.state.clone(),
            raw_plugins: self.services.plugins.export_state(),
            commit_budget: self.host.core.durability.commit_budget,
            resume: match basis {
                TransitionBasis::Resume(head, fence) => Some((head.clone(), fence.clone())),
                _ => None,
            },
            follow_on: match basis {
                TransitionBasis::FollowOn(owed) => Some(owed.clone()),
                _ => None,
            },
            tool_source_policy,
        };
        let outcome = controller
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::TransitionPlugins {
                        request: Box::new(request),
                    },
                ),
                lash_core_execution::core_internal::owned_runner_executor(Box::new(runner), None),
            )
            .await
            .map_err(|error| shift_abort(Some(admitted.run()), error.into_runtime_error()))?;
        match outcome {
            crate::RuntimeEffectOutcome::TransitionPlugins { record } => {
                record.candidate().map_err(|error| {
                    shift_abort(
                        Some(admitted.run()),
                        crate::RuntimeEffectControllerError::from(error).into_runtime_error(),
                    )
                })?;
                Ok(*record)
            }
            other => Err(shift_abort(
                Some(admitted.run()),
                crate::RuntimeEffectControllerError::wrong_outcome(
                    crate::RuntimeEffectKind::TransitionPlugins,
                    other.kind(),
                )
                .into_runtime_error(),
            )),
        }
    }
    pub(super) async fn publish_plugin_transition(
        &mut self,
        record: crate::plugin::PluginTransitionRecord,
        fence: &crate::store::ShiftFence,
        resume: Option<&crate::store::SessionHeadRef>,
    ) -> Result<(), crate::RuntimeError> {
        let generation = record.generation.clone();
        let store = self.shift_store().map_err(ShiftAbort::into_error)?;
        let crate::plugin::PluginTransitionBase::Session { head } = &record.request.base else {
            return Err(crate::RuntimeError::new(
                crate::RuntimeErrorCode::StoreCommitFailed,
                "session transition has a process base",
            ));
        };
        // A resident that is the transition's base publishes in place: the
        // committed head is its base with the staged plugin components, so
        // nothing reads the window back (ADR 0112 §14.6, FIG-5137).
        let in_place = self.session.is_some() && self.resident_is_settled();
        let base = if let Some(mut commit) = record.publication.as_deref().cloned() {
            let staged = if in_place && is_head(&self.state, head) {
                let mut state = self.resident_as_head();
                state.pending_follow_on = commit.pending_follow_on.clone().map(Box::new);
                // The journaled commit may have been staged on another base
                // at this head: an earlier attempt's read, or the creating
                // transition's initial frame. Only the same publication is
                // adopted in place; any other is read back.
                match stage_publication(
                    self.services.plugins.host(),
                    &record,
                    &mut state,
                    self.host.core.durability.commit_budget,
                    self.fleet_format(),
                ) {
                    Ok(staged)
                        if same_publication(&staged.commit, &commit, self.fleet_format()) =>
                    {
                        Some(Box::new((state, staged)))
                    }
                    _ => None,
                }
            } else {
                None
            };
            commit.shift_fence = Some(Box::new(fence.clone()));
            let receipt = store
                .commit_runtime_state_verified(commit, self.host.core.tracing.metrics())
                .await
                .map_err(crate::runtime::runtime_error_from_store_commit)?;
            let base = resume.cloned().unwrap_or(crate::store::SessionHeadRef {
                generation: head.generation,
                revision: receipt.head_revision,
                leaf: receipt.committed_leaf_node_id.clone(),
                checkpoint: Some(receipt.checkpoint_ref.clone()),
            });
            if let Some(staged) = staged {
                let (mut state, staged) = *staged;
                state.apply_persisted_commit_result(receipt);
                state.mark_node_ids_persisted(staged.persisted_node_ids);
                return Box::pin(self.adopt_published_in_place(
                    state,
                    &staged.native_view,
                    generation.as_ref(),
                ))
                .await;
            }
            base
        } else if let Some(resume) = resume {
            // The recorded transition step validated the fence and retained
            // native view. Resuming it publishes no new commit.
            if in_place && is_head(&self.state, resume) {
                let native_view = native_view_of(&record, self.fleet_format())
                    .map_err(crate::RuntimeEffectControllerError::into_runtime_error)?;
                return Box::pin(self.adopt_published_in_place(
                    self.resident_as_head(),
                    &native_view,
                    generation.as_ref(),
                ))
                .await;
            }
            resume.clone()
        } else {
            return Err(crate::RuntimeError::new(
                crate::RuntimeErrorCode::StoreCommitFailed,
                "recorded plugin transition has no publication",
            ));
        };
        let loaded = crate::store::load_session_window_state(
            &store,
            crate::store::WindowSelector::Admitted(base.clone()),
        )
        .await
        .map_err(crate::runtime::runtime_error_from_store_commit)?
        .ok_or_else(|| {
            crate::runtime::runtime_error_from_store_commit(
                crate::StoreError::TurnBaseNotRetained {
                    revision: base.revision,
                },
            )
        })?;
        let bytes = loaded.state.plugin_admission_snapshot().ok_or_else(|| {
            crate::RuntimeError::new(
                crate::RuntimeErrorCode::StoreCommitFailed,
                "published transition has no native view",
            )
        })?;
        self.services
            .plugins
            .adopt_native_view(&bytes, self.fleet_format())
            .map_err(|error| {
                crate::RuntimeEffectControllerError::from(error).into_runtime_error()
            })?;
        let mut state = loaded.state;
        // Transition publication changes checkpoint state, not immutable graph
        // nodes. Preserve the warm projection when the validated window is
        // exactly the resident window, so retained readers keep sharing it.
        if state.session_graph.anchor == self.state.session_graph.anchor
            && state.session_graph.leaf_node_id == self.state.session_graph.leaf_node_id
            && state
                .session_graph
                .nodes
                .iter()
                .map(|node| &node.node_id)
                .eq(self
                    .state
                    .session_graph
                    .nodes
                    .iter()
                    .map(|node| &node.node_id))
        {
            state.session_graph = self.state.session_graph.clone();
        }
        state.authority.plugin_config =
            (*self.services.plugins.admitted_plugin_config().config).clone();
        if self.session.is_some() {
            self.adopt_resident_state(state)
                .await
                .map_err(session_error)?;
        } else {
            self.install_resident_state(state)
                .map_err(crate::RuntimeError::from)?;
        }
        Box::pin(self.materialize_published_session())
            .await
            .map_err(session_error)?;
        crate::runtime::turn_loop::generation_fence::admit(self, generation.as_ref())
    }
}

impl LashRuntime {
    /// Whether the resident session may stand in for the durable head it
    /// names: valid, with every node persisted (FIG-5137). Which head it is
    /// stays the caller's question, answered by the head's identity alone
    /// ([`is_head`]); what stands in is [`Self::resident_as_head`].
    pub(super) fn resident_is_settled(&self) -> bool {
        self.resident_session.is_valid()
            && self
                .state
                .session_graph
                .nodes
                .iter()
                .all(|node| self.state.persisted_node_ids.contains(&node.node_id))
    }

    /// The resident session as a transition's base, its plugin bodies
    /// restored from the live plugins after each matched the content address
    /// its head recorded (ADR 0112 §14.6, FIG-5137). A transition at the
    /// head it names reads this instead of the window: the same bytes a
    /// reload would give, so a replayed run gets the same base either way.
    fn resident_head(&self) -> Option<crate::RuntimeSessionState> {
        if self.session.is_none() || !self.resident_is_settled() {
            return None;
        }
        let mut base = self.resident_as_head();
        base.rehydrate_plugin_bodies(self.services.plugins.as_ref(), self.fleet_format())
            .ok()?
            .then_some(base)
    }

    /// The resident session as the head it is: without the run view the
    /// last run installed, whose config the head never carries, just as
    /// adopting the head would leave it.
    fn resident_as_head(&self) -> crate::RuntimeSessionState {
        let mut state = self.state.clone();
        state.take_run_view();
        state
    }

    /// Publish a transition onto the resident session that was its base: the
    /// live plugins adopt the recorded native view and `state`, the
    /// committed head, becomes resident without a reload. The resident's
    /// protocol session and tool state are already this head's, so nothing
    /// is restored from the store; the tool catalog follows the adopted
    /// plugin config.
    async fn adopt_published_in_place(
        &mut self,
        mut state: crate::RuntimeSessionState,
        native_view: &[u8],
        generation: Option<&crate::ExecutableGeneration>,
    ) -> Result<(), crate::RuntimeError> {
        self.services
            .plugins
            .adopt_native_view(native_view, self.fleet_format())
            .map_err(|error| {
                crate::RuntimeEffectControllerError::from(error).into_runtime_error()
            })?;
        state.authority.plugin_config =
            (*self.services.plugins.admitted_plugin_config().config).clone();
        if let Some(session) = self.session.as_mut() {
            session.invalidate_runtime_caches();
            session
                .refresh_tool_catalog()
                .await
                .map_err(session_error)?;
        }
        self.install_resident_state(state)
            .map_err(crate::RuntimeError::from)?;
        Box::pin(self.materialize_published_session())
            .await
            .map_err(session_error)?;
        crate::runtime::turn_loop::generation_fence::admit(self, generation)
    }
}

/// Whether `state` is the durable head `head` names: the same revision, leaf
/// and checkpoint.
fn is_head(state: &crate::RuntimeSessionState, head: &crate::store::SessionHeadRef) -> bool {
    state.head_revision == head.revision
        && state.session_graph.leaf_node_id == head.leaf
        && state.checkpoint_ref == head.checkpoint
}

/// A transition's publication staged onto its base.
struct StagedPublication {
    commit: crate::RuntimeCommit,
    persisted_node_ids: Vec<crate::NodeId>,
    native_view: std::sync::Arc<[u8]>,
}

/// The native view `record` publishes, encoded as its head records it.
fn native_view_of(
    record: &crate::plugin::PluginTransitionRecord,
    fleet: crate::FleetFormat,
) -> Result<std::sync::Arc<[u8]>, crate::RuntimeEffectControllerError> {
    let (state, config) = record.candidate()?;
    Ok(crate::plugin::PluginNativeView {
        request: record.request.clone(),
        source: record.source.clone(),
        generation: record.generation.clone(),
        state,
        config,
    }
    .encode(fleet)?)
}

/// Stage `record`'s candidate onto `state`, the transition's base: its native
/// view, and the plugin state and config it converted to in the formats its
/// admission writes, committed under the transition's operation.
fn stage_publication(
    host: &crate::PluginHost,
    record: &crate::plugin::PluginTransitionRecord,
    state: &mut crate::RuntimeSessionState,
    commit_budget: crate::CommitBudget,
    fleet: crate::FleetFormat,
) -> Result<StagedPublication, crate::RuntimeEffectControllerError> {
    let (native_state, native_config) = record.candidate()?;
    let native_view = native_view_of(record, fleet)?;
    state.set_plugin_admission_snapshot(std::sync::Arc::clone(&native_view));
    let writers = record.request.target.writers();
    state.set_plugin_state(Some(host.encode_state(&native_state, &writers)?));
    state.authority.plugin_config = host.encode_config(&native_config, &writers)?;
    let operation = crate::OperationId::new(
        record.request.id.0.execution_scope.clone(),
        record.request.id.0.replay_key.clone(),
    );
    let (commit, persisted_node_ids) =
        crate::RuntimeCommit::persisted_state_with_operation_and_budget(
            state,
            operation,
            commit_budget,
            fleet,
        )
        .map_err(|error| {
            crate::RuntimeEffectControllerError::from(
                crate::runtime::runtime_error_from_store_commit(error),
            )
        })?;
    Ok(StagedPublication {
        commit,
        persisted_node_ids,
        native_view,
    })
}

/// Whether two stagings of one transition publish the same head: the same
/// checkpoint manifest, config, graph and follow-on over the same base.
fn same_publication(
    staged: &crate::RuntimeCommit,
    recorded: &crate::RuntimeCommit,
    fleet: crate::FleetFormat,
) -> bool {
    let digest = |commit: &crate::RuntimeCommit| {
        serde_json::to_value((
            commit.expected_head_revision,
            &commit.graph_base_leaf_node_id,
            &commit.graph,
            &commit.config,
            &commit.execution_config,
            &commit.pending_follow_on,
            commit.checkpoint.manifest(fleet).ok(),
        ))
        .ok()
    };
    digest(staged).is_some_and(|staged| Some(staged) == digest(recorded))
}

struct PluginTransitionRunner {
    host: crate::PluginHost,
    plugins: std::sync::Arc<crate::PluginSession>,
    store: crate::store::SessionStore,
    /// The resident session, when it may stand in for the head it names.
    resident: Option<crate::RuntimeSessionState>,
    initial: crate::RuntimeSessionState,
    raw_plugins: crate::PluginState,
    commit_budget: crate::CommitBudget,
    resume: Option<(crate::store::SessionHeadRef, crate::store::ShiftFence)>,
    follow_on: Option<crate::store::PendingFollowOn>,
    tool_source_policy: crate::ToolSourcePolicy,
}

impl PluginTransitionRunner {
    /// The session at `base`: the resident session when it is that head,
    /// otherwise the window read at it. The choice is the head's identity,
    /// never what was read before.
    async fn base_at(
        &self,
        base: &crate::store::SessionHeadRef,
    ) -> Result<crate::RuntimeSessionState, crate::RuntimeEffectControllerError> {
        if let Some(resident) = self.resident.as_ref().filter(|state| is_head(state, base)) {
            return Ok(resident.clone());
        }
        crate::store::load_session_window_state(
            &self.store,
            crate::store::WindowSelector::Admitted(base.clone()),
        )
        .await
        .map_err(derivation_fault)?
        .ok_or_else(|| {
            crate::runtime::runtime_error_from_store_commit(
                crate::StoreError::TurnBaseNotRetained {
                    revision: base.revision,
                },
            )
            .into()
        })
        .map(|loaded| loaded.state)
    }

    /// The resident session, when it is the session's current head. Only a
    /// resident that may stand in reads the head, and the window is read
    /// only when the answer is no.
    async fn resident_at_current_head(
        &self,
    ) -> Result<Option<crate::RuntimeSessionState>, crate::RuntimeEffectControllerError> {
        let Some(resident) = self.resident.as_ref() else {
            return Ok(None);
        };
        let head = self
            .store
            .load_session_head_meta()
            .await
            .map_err(derivation_fault)?;
        Ok(head
            .filter(|head| {
                resident.head_revision == head.head_revision
                    && resident.session_graph.leaf_node_id == head.leaf_node_id
                    && resident.checkpoint_ref == head.checkpoint_ref
            })
            .map(|_| resident.clone()))
    }
}

/// A store fault while deriving a transition's base: nothing committed, so
/// the derivation is retried.
fn derivation_fault(error: crate::StoreError) -> crate::RuntimeEffectControllerError {
    crate::RuntimeEffectControllerError::from(crate::runtime::runtime_error_from_store_commit(
        error,
    ))
    .retryable_uncommitted_derivation()
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for PluginTransitionRunner {
    async fn execute(
        self: Box<Self>,
        envelope: crate::RuntimeEffectEnvelope,
        _effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let crate::RuntimeEffectCommand::TransitionPlugins { mut request } = envelope.command
        else {
            return Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "plugin transition requires its recorded request",
            ));
        };
        if let Some((resume, fence)) = &self.resume {
            self.store
                .admit_session_state(fence)
                .await
                .map_err(crate::runtime::runtime_error_from_store_commit)?;
            let state = self.base_at(resume).await?;
            let bytes = state.plugin_admission_snapshot().ok_or_else(|| {
                crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::EffectReplayDivergence,
                    "the run's advanced head has no published plugin transition",
                )
            })?;
            let view = crate::plugin::PluginNativeView::decode(&bytes, self.store.fleet_format())?;
            let same_base = matches!(
                (&view.request.base, &request.base),
                (
                    crate::plugin::PluginTransitionBase::Session { head: published },
                    crate::plugin::PluginTransitionBase::Session { head: admitted },
                ) if published == admitted
            );
            if view.request.id != request.id
                || view.request.owner != request.owner
                || view.request.target != request.target
                || !same_base
            {
                return Err(crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::EffectReplayDivergence,
                    "the run's advanced head belongs to another plugin transition",
                ));
            }
            self.host.validate_state_formats(&view.state)?;
            self.host.validate_config_formats(&view.config)?;
            return Ok(crate::RuntimeEffectOutcome::TransitionPlugins {
                record: Box::new(crate::plugin::PluginTransitionRecord {
                    request: view.request,
                    source: view.source,
                    namespaces: view
                        .state
                        .plugins
                        .into_iter()
                        .map(|(id, state)| (id, Ok(state)))
                        .collect(),
                    config: Ok(view.config),
                    generation: view.generation,
                    publication: None,
                }),
            });
        }
        let mut state = match &request.base {
            crate::plugin::PluginTransitionBase::SessionCommand { .. } => {
                let state = match self.resident_at_current_head().await? {
                    Some(resident) => resident,
                    None => {
                        crate::store::load_session_window_state(
                            &self.store,
                            crate::store::WindowSelector::Current,
                        )
                        .await
                        .map_err(derivation_fault)?
                        .ok_or_else(|| {
                            crate::RuntimeEffectControllerError::new(
                                crate::RuntimeErrorCode::StoreCommitFailed,
                                "command transition has no session head",
                            )
                        })?
                        .state
                    }
                };
                let generation = self.store.read_session_state_version().await?;
                request.base = crate::plugin::PluginTransitionBase::Session {
                    head: crate::store::SessionHeadRef {
                        generation,
                        revision: state.head_revision,
                        leaf: state.session_graph.leaf_node_id.clone(),
                        checkpoint: state.checkpoint_ref.clone(),
                    },
                };
                request.target = self.host.admit_plugins(self.store.store().as_ref()).await?;
                state
            }
            crate::plugin::PluginTransitionBase::Session { head: base } if base.revision == 0 => {
                let mut initial = self.initial;
                if initial.plugin_state().is_none() {
                    initial.set_plugin_state(Some(self.raw_plugins));
                }
                initial
            }
            crate::plugin::PluginTransitionBase::Session { head: base } => {
                self.base_at(base).await?
            }
            crate::plugin::PluginTransitionBase::Process { .. } => {
                return Err(crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                    "session publication requires a retained session head",
                ));
            }
        };
        if let Some(owed) = self.follow_on {
            state.pending_follow_on = Some(Box::new(owed));
        }
        let native = state
            .plugin_admission_snapshot()
            .map(|bytes| crate::plugin::PluginNativeView::decode(&bytes, self.store.fleet_format()))
            .transpose()?;
        let native = native.filter(|view| {
            self.host
                .validate_state_formats(&view.state)
                .and_then(|()| self.host.validate_config_formats(&view.config))
                .is_ok()
        });
        let plugins = native
            .as_ref()
            .map(|view| view.state.clone())
            .unwrap_or_else(|| state.plugin_state().cloned().unwrap_or_default());
        let config = native
            .as_ref()
            .map(|view| &view.config)
            .unwrap_or(&state.authority.plugin_config);
        let mut record = self.host.transition_plugins(*request, &plugins, config);
        if record.candidate().is_ok() {
            let candidate = self.plugins.materialize_transition_candidate(&record)?;
            // `Require` refuses the run here, before the transition publishes
            // anything: the refusal is the step's recorded answer, and the
            // run ends with it as its typed terminal (FIG-5134).
            crate::runtime::tool_restore::require_tool_sources(
                self.tool_source_policy,
                candidate.tool_registry().as_ref(),
                state.tool_state_snapshot(),
                &state.session_id,
            )?;
            record.generation = candidate
                .code_executor()
                .and_then(|executor| executor.executable_generation());
            let staged = stage_publication(
                &self.host,
                &record,
                &mut state,
                self.commit_budget,
                self.store.fleet_format(),
            )?;
            record.publication = Some(Box::new(staged.commit));
        }
        Ok(crate::RuntimeEffectOutcome::TransitionPlugins {
            record: Box::new(record),
        })
    }
}

fn session_error(error: crate::SessionError) -> crate::RuntimeError {
    match error {
        crate::SessionError::Plugin(error) => {
            crate::RuntimeEffectControllerError::from(error).into_runtime_error()
        }
        crate::SessionError::Store { source, .. } => {
            crate::runtime::runtime_error_from_store_commit(source)
        }
        error => crate::RuntimeError::new(
            crate::RuntimeErrorCode::SessionHeadRefresh,
            error.to_string(),
        ),
    }
}
