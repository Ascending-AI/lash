//! The runtime's operation bodies behind the parent broker.
//!
//! Every operation a run blocks on is admitted with a snapshot of the VM
//! that issued it, through the run's [`SnapshotStore`] (ADR 0132 §8), before
//! its body runs; [`OperationAdmissions`] says what admitting it records.
use lash_vm_broker::*;
use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
use lash_vm_client::{PoolSlots, service::Service};
use lash_vm_protocol::*;
use lashlang::{ExecutionBounds, ExecutionHost};
use std::collections::BTreeMap;
use std::sync::Arc;

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
    /// What a run with no committed snapshot starts from: fresh, or the
    /// session's VM snapshot.
    pub state: StartState,
    /// The execution's latest committed snapshot, which the run resumes
    /// from.
    pub from: Option<Checkpoint>,
    /// Where the run's quiet points commit.
    pub snapshots: &'a dyn SnapshotStore,
    /// What admitting each operation the run blocks on records.
    pub admissions: &'a dyn OperationAdmissions,
    pub boundary: &'a (dyn Fn() -> bool + Send + Sync),
    /// The gate a foreground run's host reads while it performs an operation
    /// the run is parked on: its state is committed, so the host may leave a
    /// wait open beyond this activation ([`lashlang::AbilityOutcome::HandedOver`]).
    pub hand_over: Option<&'a HandOverGate>,
    /// The providers that answer the run's projection reads, on this node
    /// (ADR 0132 §9).
    pub providers: lashlang::ProjectionCatalog,
}

/// What admitting an operation a run blocks on records: decided by the host
/// that performs it, which knows its tool and policy.
pub trait OperationAdmissions: Send + Sync {
    /// The admission of `call`, the command `request` takes: its execution
    /// (none for a wait the host performs again on restore) and its waits.
    ///
    /// # Errors
    ///
    /// Why the operation cannot be admitted; the run stops.
    fn admission(
        &self,
        call: &lash_sansio::ToolCallId,
        request: &OperationRequest,
    ) -> Result<Admission, String>;

    /// The host's own state at a quiet point, committed with the VM's
    /// snapshot.
    ///
    /// # Errors
    ///
    /// Why the state cannot be encoded; the run stops.
    fn host_state(&self) -> Result<Option<EncodedPayload>, String> {
        Ok(None)
    }
}

/// Whether the operation a foreground run's host is performing is one the
/// run parked on, so its state is committed and a wait can be left open
/// beyond this activation.
#[derive(Debug, Default)]
pub struct HandOverGate {
    parked: std::sync::atomic::AtomicBool,
}

impl HandOverGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the operation being performed may be handed over.
    pub fn parked(&self) -> bool {
        self.parked.load(std::sync::atomic::Ordering::SeqCst)
    }
}

struct Effects<'a, H> {
    host: &'a H,
    projections: lash_vm_client::Projections,
    context: &'a AdmittedContext,
    admissions: &'a dyn OperationAdmissions,
    boundary: &'a (dyn Fn() -> bool + Send + Sync),
    hand_over: Option<&'a HandOverGate>,
}

impl<H> Effects<'_, H> {
    fn request(&self, operation: &AdmittedOperation) -> Result<OperationRequest, ParentFault> {
        let payload = operation
            .request
            .as_ref()
            .ok_or_else(|| ParentFault("admitted request is missing".into()))?;
        OperationRequest::decode(payload).map_err(|e| ParentFault(e.to_string()))
    }
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
        let decoded = OperationRequest::decode(&request.payload)?;
        if decoded.kind() != request.kind {
            return Err(AuthorityRefusal::KindMismatch {
                kind: request.kind,
                payload: decoded.kind(),
            });
        }
        // The admitted runtime host owns the complete resource/grant checks.
        // Its effect body checks before admission or tool dispatch.
        Ok(authority::ResolvedRequest::Control {
            kind: request.kind,
            payload: request.payload.clone(),
        })
    }
    fn admission(&self, operation: &AdmittedOperation) -> Result<Admission, ParentFault> {
        self.admissions
            .admission(
                &operation.command_id(self.context),
                &self.request(operation)?,
            )
            .map_err(ParentFault)
    }
    fn host_state(&self) -> Result<Option<EncodedPayload>, ParentFault> {
        self.admissions.host_state().map_err(ParentFault)
    }
    async fn perform(
        &self,
        operation: &AdmittedOperation,
        _waits: &[(WaitRef, Option<PinnedKey>)],
    ) -> Result<Performed, ParentFault> {
        let request = self
            .projections
            .materialize_operation(self.request(operation)?)
            .await
            .map_err(ParentFault)?;
        // An operation the run blocks on was parked first: its state is
        // committed, so the host may leave a wait open beyond the activation.
        let parked =
            matches!(&operation.kind, AdmittedKind::Control { kind, .. } if kind.parkable());
        if let Some(gate) = self.hand_over.filter(|_| parked) {
            gate.parked.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let result = self.host.perform(request).await;
        if let Some(gate) = self.hand_over {
            gate.parked
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
        let outcome = if self.host.is_cancelled() {
            EffectOutcome::Cancelled
        } else {
            match result {
                Ok(lashlang::AbilityOutcome::HandedOver) => EffectOutcome::HandedOver,
                Ok(value) => EffectOutcome::Value(EncodedPayload(
                    rmp_serde::to_vec_named(&value).map_err(|e| ParentFault(e.to_string()))?,
                )),
                Err(error) => EffectOutcome::Failed(EncodedPayload(
                    rmp_serde::to_vec_named(&error).map_err(|e| ParentFault(e.to_string()))?,
                )),
            }
        };
        Ok(Performed::outcome(outcome))
    }
    fn interrupted(&self, operation: &AdmittedOperation) -> EffectOutcome {
        let call = operation.command_id(self.context);
        let failure = lash_sansio::ToolFailure::tool(
            lash_sansio::ToolFailureClass::Unavailable,
            "lash_operation_interrupted",
            "the operation started and never answered; it is not run again",
        );
        let error = lashlang::ExecutionHostError::from_tool_failure(&failure, call.to_string());
        match rmp_serde::to_vec_named(&error) {
            Ok(bytes) => EffectOutcome::Failed(EncodedPayload(bytes)),
            Err(_) => EffectOutcome::Cancelled,
        }
    }
    async fn observe_cancellation(&self, checkpoint: u64) -> Result<bool, ParentFault> {
        self.host.cancel_checkpoint(checkpoint).await;
        Ok(self.host.is_cancelled())
    }
    async fn projection(&self, payload: &EncodedPayload) -> Result<EncodedPayload, ParentFault> {
        let read: lash_vm_client::ProjectionRead =
            rmp_serde::from_slice(&payload.0).map_err(|e| ParentFault(e.to_string()))?;
        Ok(EncodedPayload(
            rmp_serde::to_vec_named(&self.projections.read(read).await)
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
        self.context.projected = lash_vm_client::Projections::describe(&self.projected);
        let bytes = rmp_serde::to_vec_named(&self.context).map_err(|e| BrokerFailure::Parent {
            fault: ParentFault(e.to_string()),
        })?;
        let limits = VmLimits {
            instruction_budget: bound(self.bounds.instruction_budget),
            memory_limit_bytes: bound(self.bounds.memory_limit),
            max_frame_depth: self.bounds.max_frame_depth.get(),
        };
        let start = RunStart {
            program: self.program,
            contexts: vec![ContextDescription {
                kind: "vm_run".into(),
                name: "runtime".into(),
                body: EncodedPayload(bytes),
            }],
            limits,
            from: self.from,
            fresh: self.state,
        };
        let pool = self.service.pool_accounted().await.map_err(pool_failure)?;
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
            projections: lash_vm_client::Projections::new(self.providers),
            context: &context,
            admissions: self.admissions,
            boundary: self.boundary,
            hand_over: self.hand_over,
        };
        let broker = Broker {
            context: &context,
            effects: &effects,
            checkpoints: self.snapshots,
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
    BrokerFailure::Unavailable {
        refusal: CheckoutRefusal::Infrastructure(error.into_outcome()),
    }
}
