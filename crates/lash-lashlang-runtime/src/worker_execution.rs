//! The runtime's admitted effect bodies behind the parent broker.
use lash_vm_broker::*;
use lash_vm_client::{PoolSlots, service::Service};
use lash_vm_protocol::*;
use lashlang::{ExecutionBounds, ExecutionHost};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

pub struct WorkerRun<'a, H> {
    pub service: &'a Service,
    pub host: &'a H,
    pub identities: CodeCallIdentities,
    pub owner: VmOwner,
    pub frame_epoch: FrameEpoch,
    pub program: ProgramSource,
    pub context: lash_vm_client::RunContext,
    pub projected: lashlang::ProjectedBindings,
    pub bounds: ExecutionBounds,
    pub state: StartState,
    pub boundary: &'a (dyn Fn() -> bool + Send + Sync),
}

struct Effects<'a, H> {
    host: &'a H,
    projections: lash_vm_client::Projections,
    boundary: &'a (dyn Fn() -> bool + Send + Sync),
}
#[async_trait::async_trait]
impl<H: ExecutionHost + Sync> ParentEffects for Effects<'_, H> {
    fn resolve(
        &self,
        _context: &AdmittedContext,
        _grants: &BTreeMap<String, HandleGrant>,
        _frame: FrameEpoch,
        request: &EffectRequest,
    ) -> Result<authority::ResolvedRequest, AuthorityRefusal> {
        use OperationRequestCodec;
        let decoded = OperationRequest::decode(&request.payload)?;
        if decoded.kind() != request.kind {
            return Err(AuthorityRefusal::KindMismatch {
                kind: request.kind,
                payload: decoded.kind(),
            });
        }
        // The admitted runtime host owns the complete resource/grant checks.
        // Its effect body checks before journal admission or tool dispatch.
        Ok(authority::ResolvedRequest::Control {
            kind: request.kind,
            payload: request.payload.clone(),
        })
    }
    async fn retain(
        &self,
        operation: &AdmittedOperation,
    ) -> Result<RequestFingerprint, ParentFault> {
        // The existing run grammar retains each full command inside its
        // effect body, under its parent-issued ordinal, before dispatch.
        Ok(operation.fingerprint)
    }
    async fn perform(&self, operation: &AdmittedOperation) -> Result<Performed, ParentFault> {
        use OperationRequestCodec;
        let payload = operation
            .request
            .as_ref()
            .ok_or_else(|| ParentFault("admitted request is missing".into()))?;
        let request = OperationRequest::decode(payload).map_err(|e| ParentFault(e.to_string()))?;
        let request = self
            .projections
            .import_operation(request)
            .map_err(ParentFault)?;
        let result = self.host.perform(request).await;
        let outcome = if self.host.is_cancelled() {
            EffectOutcome::Cancelled
        } else {
            match result {
                Ok(lashlang::AbilityOutcome::HandedOver) => EffectOutcome::HandedOver,
                Ok(value) => {
                    let value = self
                        .projections
                        .export_outcome(value)
                        .map_err(ParentFault)?;
                    EffectOutcome::Value(EncodedPayload(
                        rmp_serde::to_vec_named(&value).map_err(|e| ParentFault(e.to_string()))?,
                    ))
                }
                Err(error) => EffectOutcome::Failed(EncodedPayload(
                    rmp_serde::to_vec_named(&error).map_err(|e| ParentFault(e.to_string()))?,
                )),
            }
        };
        Ok(Performed::outcome(outcome))
    }
    async fn observe_cancellation(&self, checkpoint: u64) -> Result<bool, ParentFault> {
        self.host.cancel_checkpoint(checkpoint).await;
        Ok(self.host.is_cancelled())
    }
    fn needs_worker(&self, operation: &AdmittedOperation) -> bool {
        matches!(&operation.kind, AdmittedKind::Control { kind, .. } if kind.parkable())
    }
    fn projection(&self, payload: &EncodedPayload) -> Result<EncodedPayload, ParentFault> {
        let request: lash_vm_client::ProjectionRead =
            rmp_serde::from_slice(&payload.0).map_err(|e| ParentFault(e.to_string()))?;
        Ok(EncodedPayload(
            rmp_serde::to_vec_named(&self.projections.read(request).map_err(ParentFault)?)
                .map_err(|e| ParentFault(e.to_string()))?,
        ))
    }
    fn boundary(&self) -> bool {
        (self.boundary)()
    }
    fn observe(&self, payload: &EncodedPayload) -> Result<(), ParentFault> {
        let observations =
            rmp_serde::from_slice::<Vec<lashlang::LashlangExecutionObservation>>(&payload.0)
                .map_err(|e| ParentFault(e.to_string()))?;
        for observation in observations {
            self.host.observe_lashlang_execution(observation);
        }
        Ok(())
    }
    fn park_declined(&self, reason: &str) {
        crate::process::record_segment_boundary_decline(&reason, "worker declined segment capture");
    }
}
struct Capture(Mutex<Option<Checkpoint>>);
#[async_trait::async_trait]
impl CheckpointStore for Capture {
    async fn commit(&self, checkpoint: &Checkpoint) -> Result<(), CheckpointRefusal> {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(checkpoint.clone());
        Ok(())
    }
    async fn latest(&self) -> Result<Option<Checkpoint>, CheckpointRefusal> {
        Ok(self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone())
    }
    async fn open_frame(&self, _frame: FrameEpoch) -> Result<(), CheckpointRefusal> {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        Ok(())
    }
}
impl<H: ExecutionHost + Sync> WorkerRun<'_, H> {
    pub async fn run(self) -> Result<BrokeredEnd, BrokerFailure> {
        if self.service.execution_budget().is_some() {
            return self.run_scoped().await;
        }
        let recovery = self
            .service
            .begin_execution(&self.identities.scope())
            .await
            .map_err(pool_failure)?;
        let service = recovery.service().clone();
        let result = WorkerRun {
            service: &service,
            ..self
        }
        .run_scoped()
        .await;
        recovery.settle().await.map_err(pool_failure)?;
        result
    }
    async fn run_scoped(mut self) -> Result<BrokeredEnd, BrokerFailure> {
        let context = AdmittedContext {
            owner: self.owner,
            owner_epoch: OwnerEpoch(0),
            identities: self.identities,
            bindings: Arc::new(FrozenBindings::default()),
        };
        let (projections, descriptions) = lash_vm_client::Projections::new(
            &self.projected,
            self.service.config().protocol.decode.max_nodes as usize,
        )
        .map_err(|fault| BrokerFailure::Parent {
            fault: ParentFault(fault),
        })?;
        self.context.projection_namespace = projections.namespace().to_owned();
        self.context.projected = descriptions;
        let bytes = rmp_serde::to_vec_named(&self.context).map_err(|e| BrokerFailure::Parent {
            fault: ParentFault(e.to_string()),
        })?;
        let limits = VmLimits {
            instruction_budget: bound(self.bounds.instruction_budget),
            memory_limit_bytes: bound(self.bounds.memory_limit),
            max_frame_depth: self.bounds.max_frame_depth.get(),
        };
        let state = match self.state {
            StartState::Fresh => None,
            StartState::Snapshot(vm) | StartState::Continuation(vm) => Some(Checkpoint {
                vm,
                ledger: LedgerSnapshot::default(),
                frame_epoch: self.frame_epoch,
            }),
        };
        let start = RunStart {
            program: self.program,
            contexts: vec![ContextDescription {
                kind: "vm_run".into(),
                name: "runtime".into(),
                body: EncodedPayload(bytes),
            }],
            limits,
            from: state,
        };
        let pool = self.service.pool().map_err(|e| BrokerFailure::Parent {
            fault: ParentFault(e.to_string()),
        })?;
        let slots = PoolSlots {
            pool,
            owner_epoch: context.owner_epoch,
            frame_epoch: self.frame_epoch,
            recovery: Some(self.service.clone()),
            budget: self.service.execution_budget().cloned().ok_or_else(|| {
                BrokerFailure::Parent {
                    fault: ParentFault("worker execution has no reserved budget".into()),
                }
            })?,
        };
        let effects = Effects {
            host: self.host,
            projections,
            boundary: self.boundary,
        };
        let captures = Capture(Mutex::new(None));
        let broker = Broker {
            context: &context,
            effects: &effects,
            checkpoints: &captures,
            slots: &slots,
            codec: FrameCodec::new(self.service.config().protocol.decode),
            contract: lashlang::vm_contract_reads(),
            bounds: BrokerBounds {
                protocol: self.service.config().protocol,
                ..BrokerBounds::standard()
            },
            frames: FrameFence::new(self.frame_epoch),
        };
        broker
            .run(start, &tokio_util::sync::CancellationToken::new())
            .await
    }
}
fn bound(bound: lashlang::ExecutionBound<std::num::NonZeroU64>) -> Option<u64> {
    match bound {
        lashlang::ExecutionBound::Unbounded => None,
        lashlang::ExecutionBound::Bounded(v) => Some(v.get()),
    }
}

fn pool_failure(error: lash_vm_client::PoolError) -> BrokerFailure {
    match error {
        lash_vm_client::PoolError::Infrastructure(outcome) => BrokerFailure::Unavailable {
            refusal: CheckoutRefusal::Infrastructure(outcome),
        },
        lash_vm_client::PoolError::RetryLimitExceeded => BrokerFailure::Unavailable {
            refusal: CheckoutRefusal::Infrastructure(InfrastructureOutcome::WorkerLimitExceeded {
                limit: WorkerLimit::Deadline,
            }),
        },
        error => BrokerFailure::Parent {
            fault: ParentFault(error.to_string()),
        },
    }
}
