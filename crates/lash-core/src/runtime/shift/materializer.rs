//! Runtime-owned composition policy supplied to the recorded admission body.
use super::*;

#[async_trait::async_trait]
pub trait ShiftAdmissionMaterializer: Send + Sync {
    async fn request(
        &self,
        store: &crate::store::SessionStore,
        selection: &crate::store::ShiftAdmissionSelection,
        preparation: &crate::store::ShiftAdmissionPreparation,
        executor: crate::store::RunExecutor,
        scope: &crate::AdmittedScope,
    ) -> Result<crate::store::AdmitRunRequest, crate::RuntimeEffectControllerError>;
}

pub struct ShiftAdmissionTemplate {
    host: crate::RuntimeHostConfig,
    policy: crate::TurnLaneAdmissionPolicy,
    plugins: crate::plugin::PluginHost,
    resident: Option<ResidentAdmissionBase>,
}

/// The head the template's runtime is settled at, and that head's
/// physical-turn index (FIG-5137).
struct ResidentAdmissionBase {
    revision: u64,
    leaf: Option<crate::NodeId>,
    checkpoint: Option<crate::store::BlobRef>,
    turn_index: usize,
}

impl ShiftAdmissionTemplate {
    /// Whether the template's runtime is settled at the head `preparation`
    /// prepared, by that head's identity: its revision, leaf and checkpoint.
    pub fn holds_prepared_head(
        &self,
        preparation: &crate::store::ShiftAdmissionPreparation,
    ) -> bool {
        self.resident_at(preparation).is_some()
    }

    fn resident_at(
        &self,
        preparation: &crate::store::ShiftAdmissionPreparation,
    ) -> Option<&ResidentAdmissionBase> {
        let head = preparation.head.as_ref()?;
        self.resident.as_ref().filter(|resident| {
            resident.revision == head.head_revision
                && resident.leaf == head.leaf_node_id
                && resident.checkpoint == head.checkpoint_ref
        })
    }
}

impl LashRuntime {
    pub fn shift_admission_template(&self) -> Result<ShiftAdmissionTemplate, RuntimeError> {
        // An installed run view swaps its run's config into the resident
        // policy. The admission composes under the session's own, as a
        // runtime opened at the head does.
        let session_state = self.state.authority.run_view().is_some().then(|| {
            let mut state = self.state.clone();
            state.take_run_view();
            state
        });
        let state = session_state.as_ref().unwrap_or(&self.state);
        Ok(ShiftAdmissionTemplate {
            resident: self.resident_is_settled().then(|| ResidentAdmissionBase {
                revision: self.state.head_revision,
                leaf: self.state.session_graph.leaf_node_id.clone(),
                checkpoint: self.state.checkpoint_ref.clone(),
                turn_index: self.state.turn_index,
            }),
            host: self.host.core.clone(),
            policy: self
                .host
                .core
                .durability
                .queued_work_batching
                .admission_policy(crate::runtime::turn_loop::max_context_tokens_of(state)?),
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
        // prepared head owns both the composition and its physical-turn index:
        // the runtime's own when it is that head, otherwise the window's.
        let base_turn_index = match self.resident_at(preparation) {
            Some(resident) => resident.turn_index,
            None => {
                crate::store::load_session_window_state(
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
                })?
                .state
                .turn_index
            }
        };
        let turn_index = (base_turn_index as u64).checked_add(1).ok_or_else(|| {
            crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeStoreCorrupt,
                "admission exhausted its physical-turn indices",
            )
        })?;
        let effect_host = &self.host.control.effect_host;
        let scoped = effect_host
            .scoped(scope.clone())
            .map_err(crate::RuntimeEffectControllerError::from)?;
        let binding = scoped
            .turn_control_binding()
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
