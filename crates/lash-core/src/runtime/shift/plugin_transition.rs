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
        self.record_transition_request(controller, admitted, request, basis)
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
        )
        .await
    }

    async fn record_transition_request(
        &self,
        controller: &ScopedEffectController<'_>,
        admitted: &Admitted,
        request: crate::plugin::PluginTransitionRequest,
        basis: TransitionBasis<'_>,
    ) -> Result<crate::plugin::PluginTransitionRecord, ShiftAbort> {
        let invocation = run_step_invocation(controller, admitted, "plugin-transition")?;
        let runner = PluginTransitionRunner {
            host: self.services.plugins.host().clone(),
            plugins: std::sync::Arc::clone(&self.services.plugins),
            store: self.shift_store()?,
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
        let base = if let Some(mut commit) = record.publication.as_deref().cloned() {
            commit.shift_fence = Some(Box::new(fence.clone()));
            let receipt = store
                .commit_runtime_state_verified(commit, self.host.core.tracing.metrics())
                .await
                .map_err(crate::runtime::runtime_error_from_store_commit)?;
            resume.cloned().unwrap_or(crate::store::SessionHeadRef {
                generation: head.generation,
                revision: receipt.head_revision,
                leaf: receipt.committed_leaf_node_id,
                checkpoint: Some(receipt.checkpoint_ref),
            })
        } else if let Some(resume) = resume {
            // The recorded transition step validated the fence and retained
            // native view. Resuming it publishes no new commit.
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

struct PluginTransitionRunner {
    host: crate::PluginHost,
    plugins: std::sync::Arc<crate::PluginSession>,
    store: crate::store::SessionStore,
    initial: crate::RuntimeSessionState,
    raw_plugins: crate::PluginState,
    commit_budget: crate::CommitBudget,
    resume: Option<(crate::store::SessionHeadRef, crate::store::ShiftFence)>,
    follow_on: Option<crate::store::PendingFollowOn>,
}

impl PluginTransitionRunner {
    async fn load_base(
        &self,
        base: &crate::store::SessionHeadRef,
    ) -> Result<crate::RuntimeSessionState, crate::RuntimeEffectControllerError> {
        crate::store::load_session_window_state(
            &self.store,
            crate::store::WindowSelector::Admitted(base.clone()),
        )
        .await
        .map_err(|error| {
            crate::RuntimeEffectControllerError::from(
                crate::runtime::runtime_error_from_store_commit(error),
            )
            .retryable_uncommitted_derivation()
        })?
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
            let state = self.load_base(resume).await?;
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
                let loaded = crate::store::load_session_window_state(
                    &self.store,
                    crate::store::WindowSelector::Current,
                )
                .await
                .map_err(|error| {
                    crate::RuntimeEffectControllerError::from(
                        crate::runtime::runtime_error_from_store_commit(error),
                    )
                    .retryable_uncommitted_derivation()
                })?
                .ok_or_else(|| {
                    crate::RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::StoreCommitFailed,
                        "command transition has no session head",
                    )
                })?;
                let generation = self.store.read_session_state_version().await?;
                request.base = crate::plugin::PluginTransitionBase::Session {
                    head: crate::store::SessionHeadRef {
                        generation,
                        revision: loaded.state.head_revision,
                        leaf: loaded.state.session_graph.leaf_node_id.clone(),
                        checkpoint: loaded.state.checkpoint_ref.clone(),
                    },
                };
                request.target = self.host.admit_plugins(self.store.store().as_ref()).await?;
                loaded.state
            }
            crate::plugin::PluginTransitionBase::Session { head: base } if base.revision == 0 => {
                let mut initial = self.initial;
                if initial.plugin_state().is_none() {
                    initial.set_plugin_state(Some(self.raw_plugins));
                }
                initial
            }
            crate::plugin::PluginTransitionBase::Session { head: base } => {
                self.load_base(base).await?
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
        if let Ok((native_state, native_config)) = record.candidate() {
            let candidate = self.plugins.materialize_transition_candidate(&record)?;
            record.generation = candidate
                .code_executor()
                .and_then(|executor| executor.executable_generation());
            let view = crate::plugin::PluginNativeView {
                request: record.request.clone(),
                source: record.source.clone(),
                generation: record.generation.clone(),
                state: native_state.clone(),
                config: native_config.clone(),
            };
            state.set_plugin_admission_snapshot(view.encode(self.store.fleet_format())?);
            let writers = record.request.target.writers();
            state.set_plugin_state(Some(self.host.encode_state(&native_state, &writers)?));
            state.authority.plugin_config = self.host.encode_config(&native_config, &writers)?;
            let operation = crate::OperationId::new(
                record.request.id.0.execution_scope.clone(),
                record.request.id.0.replay_key.clone(),
            );
            let (commit, _) = crate::RuntimeCommit::persisted_state_with_operation_and_budget(
                &mut state,
                operation,
                self.commit_budget,
                self.store.fleet_format(),
            )
            .map_err(|error| {
                crate::RuntimeEffectControllerError::from(
                    crate::runtime::runtime_error_from_store_commit(error),
                )
            })?;
            record.publication = Some(Box::new(commit));
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
