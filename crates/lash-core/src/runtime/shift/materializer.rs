//! Runtime-owned composition policy supplied to the recorded admission body.
use super::*;

#[async_trait::async_trait]
pub trait ShiftAdmissionMaterializer: Send + Sync {
    async fn request(
        &self,
        store: &crate::store::SessionStore,
        selection: &crate::store::ShiftAdmissionSelection,
        admitted_generation: &crate::engine::BuildGeneration,
        preparation: &crate::store::ShiftAdmissionPreparation,
        executor: crate::store::RunExecutor,
        scope: &crate::AdmittedScope,
    ) -> Result<crate::store::AdmitRunRequest, crate::RuntimeEffectControllerError>;
}

pub struct ShiftAdmissionTemplate {
    host: crate::RuntimeHostConfig,
    policy: crate::TurnLaneAdmissionPolicy,
    plugins: crate::plugin::PluginHost,
}

impl LashRuntime {
    pub fn shift_admission_template(&self) -> Result<ShiftAdmissionTemplate, RuntimeError> {
        Ok(ShiftAdmissionTemplate {
            host: self.host.core.clone(),
            policy: self
                .host
                .core
                .durability
                .queued_work_batching
                .admission_policy(self.max_context_tokens()?),
            plugins: self.services.plugins.host().clone(),
        })
    }
}

#[async_trait::async_trait]
impl ShiftAdmissionMaterializer for ShiftAdmissionTemplate {
    async fn request(
        &self,
        store: &crate::store::SessionStore,
        selection: &crate::store::ShiftAdmissionSelection,
        admitted_generation: &crate::engine::BuildGeneration,
        preparation: &crate::store::ShiftAdmissionPreparation,
        executor: crate::store::RunExecutor,
        scope: &crate::AdmittedScope,
    ) -> Result<crate::store::AdmitRunRequest, crate::RuntimeEffectControllerError> {
        let head = match &selection.work {
            crate::engine::AdmittedWork::Input { head } => {
                crate::store::AdmittedHead::Input(head.clone())
            }
            crate::engine::AdmittedWork::Queued { head } => {
                crate::store::AdmittedHead::Batch(head.clone())
            }
            _ => {
                return Err(crate::RuntimeEffectControllerError::new(
                    RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                    "non-turn work requested turn composition",
                ));
            }
        };
        let live = preparation.head.clone().ok_or_else(|| {
            crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::SessionHeadRefresh,
                "admission requires a session head",
            )
        })?;
        let base = crate::store::SessionHeadRef {
            generation: 0,
            revision: live.head_revision,
            leaf: live.leaf_node_id,
            checkpoint: live.checkpoint_ref,
        };
        // The idle runtime may have opened before another run committed. The
        // prepared head owns both the composition and its physical-turn index.
        let loaded = crate::store::load_session_window_state(
            store,
            crate::store::WindowSelector::Admitted(base.clone()),
        )
        .await
        .map_err(|error| admission::store_fault("admission turn index", error))?
        .ok_or_else(|| {
            admission::store_fault(
                "admission turn index",
                crate::StoreError::TurnBaseNotRetained {
                    revision: base.revision,
                },
            )
        })?;
        let turn_index = (loaded.state.turn_index as u64)
            .checked_add(1)
            .ok_or_else(|| {
                crate::RuntimeEffectControllerError::new(
                    RuntimeErrorCode::RuntimeStoreCorrupt,
                    "admission exhausted its physical-turn indices",
                )
            })?;
        let effect_host = &self.host.control.effect_host;
        let scoped = effect_host
            .scoped(scope.clone())
            .map_err(crate::RuntimeEffectControllerError::from)?;
        let binding = effect_host
            .turn_control_binding(&scoped)
            .await
            .map_err(crate::RuntimeEffectControllerError::from)?;
        let binding_id = binding.binding_id();
        let plugins = self
            .plugins
            .admit_plugins(store.store().as_ref())
            .await
            .map_err(|error| admission::store_fault("plugin admission", error))?;
        Ok(crate::store::AdmitRunRequest {
            fence: preparation.prospective_fence.clone(),
            unsealed_epoch: Some(preparation.epoch.epoch),
            run: selection.run.clone(),
            head,
            max_inputs: self
                .host
                .durability
                .queued_work_batching
                .max_turn_input_admission(),
            policy: self.policy.clone(),
            base,
            turn_index,
            admitted_generation: admitted_generation.clone(),
            executor,
            plugins,
            turn_cancellation: Some(crate::store::TurnCancellationBinding {
                binding_id: binding_id.to_string(),
                admitted_scope: crate::runtime::effect::executor::admitted_turn_cancel_scope(
                    &crate::TurnAddress::new(store.session_id(), &selection.run),
                    scoped.execution_scope(),
                    binding_id,
                ),
            }),
            trace_scopes: Arc::clone(self.host.tracing.scopes()),
        })
    }
}
