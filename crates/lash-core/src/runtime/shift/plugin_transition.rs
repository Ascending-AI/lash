//! Recorded transition preparation and fenced publication for a session run.
use super::{run::run_step_invocation, shift_abort};
use crate::ScopedEffectController;
use crate::engine::{Admitted, ShiftAbort};
use crate::runtime::LashRuntime;
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;

impl LashRuntime {
    /// Adopt the head a run's admission admitted it on and pin its recorded
    /// turn index for the prepare phase (FIG-3682).
    ///
    /// The admission's head verdict decides which head the resident session
    /// is rebuilt from: a `Ready` verdict rebuilds it from the admission's
    /// base, whatever the live head is now; an `Advanced` one from the head
    /// the run's own commits published (FIG-4201); an `Overtaken` verdict
    /// ends the run typed `StoreCommitSuperseded`, and a `Diverged` one
    /// parks it.
    ///
    /// A base the store no longer retains parks the run too.
    pub(super) async fn record_plugin_transition(
        &self,
        controller: &ScopedEffectController<'_>,
        admitted: &Admitted,
        base: &crate::store::SessionHeadRef,
        target: &crate::store::plugin_writers::PluginAdmission,
    ) -> Result<crate::plugin::PluginTransitionRecord, ShiftAbort> {
        let invocation = run_step_invocation(controller, admitted, "plugin-transition")?;
        let request = crate::plugin::PluginTransitionRequest {
            id: crate::plugin::PluginTransitionId(invocation.address().clone()),
            owner: crate::RuntimeOwner::Session(admitted.session().clone()),
            base: crate::plugin::PluginTransitionBase::Session { head: base.clone() },
            target: target.clone(),
        };
        self.record_transition_request(controller, admitted, request)
            .await
    }

    pub(super) async fn record_command_plugin_transition(
        &self,
        controller: &ScopedEffectController<'_>,
        admitted: &Admitted,
    ) -> Result<crate::plugin::PluginTransitionRecord, ShiftAbort> {
        let invocation = run_step_invocation(controller, admitted, "plugin-transition")?;
        self.record_transition_request(
            controller,
            admitted,
            crate::plugin::PluginTransitionRequest {
                id: crate::plugin::PluginTransitionId(invocation.address().clone()),
                owner: crate::RuntimeOwner::Session(admitted.session().clone()),
                base: crate::plugin::PluginTransitionBase::SessionCommand {
                    run: admitted.run().clone(),
                },
                target: Default::default(),
            },
        )
        .await
    }

    async fn record_transition_request(
        &self,
        controller: &ScopedEffectController<'_>,
        admitted: &Admitted,
        request: crate::plugin::PluginTransitionRequest,
    ) -> Result<crate::plugin::PluginTransitionRecord, ShiftAbort> {
        let invocation = run_step_invocation(controller, admitted, "plugin-transition")?;
        let runner = PluginTransitionRunner {
            host: self.services.plugins.host().clone(),
            store: self.shift_store()?,
            initial: self.state.clone(),
            raw_plugins: self.services.plugins.export_state(),
            commit_budget: self.host.core.durability.commit_budget,
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
        let store = self.shift_store().map_err(ShiftAbort::into_error)?;
        let mut commit = record.publication.as_deref().cloned().ok_or_else(|| {
            crate::RuntimeError::new(
                crate::RuntimeErrorCode::StoreCommitFailed,
                "recorded plugin transition has no publication",
            )
        })?;
        commit.shift_fence = Some(Box::new(fence.clone()));
        let receipt = store
            .commit_runtime_state_verified(commit, self.host.core.tracing.metrics())
            .await
            .map_err(crate::runtime::runtime_error_from_store_commit)?;
        let crate::plugin::PluginTransitionBase::Session { head } = &record.request.base else {
            return Err(crate::RuntimeError::new(
                crate::RuntimeErrorCode::StoreCommitFailed,
                "session transition has a process base",
            ));
        };
        let published = crate::store::SessionHeadRef {
            generation: head.generation,
            revision: receipt.head_revision,
            leaf: receipt.committed_leaf_node_id.clone(),
            checkpoint: Some(receipt.checkpoint_ref.clone()),
        };
        let base = resume.unwrap_or(&published);
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
            .adopt_native_view(&bytes)
            .map_err(|error| {
                crate::RuntimeEffectControllerError::from(error).into_runtime_error()
            })?;
        let mut state = loaded.state;
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
            .map_err(session_error)
    }
}

struct PluginTransitionRunner {
    host: crate::PluginHost,
    store: crate::store::SessionStore,
    initial: crate::RuntimeSessionState,
    raw_plugins: crate::PluginState,
    commit_budget: crate::CommitBudget,
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
                })?
                .state
            }
            crate::plugin::PluginTransitionBase::Process { .. } => {
                return Err(crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                    "session publication requires a retained session head",
                ));
            }
        };
        let native = state
            .plugin_admission_snapshot()
            .map(|bytes| crate::plugin::PluginNativeView::decode(&bytes))
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
            let view = crate::plugin::PluginNativeView {
                request: record.request.clone(),
                source: record.source.clone(),
                state: native_state.clone(),
                config: native_config.clone(),
            };
            state.set_plugin_admission_snapshot(view.encode()?);
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

#[cfg(test)]
mod tests;
