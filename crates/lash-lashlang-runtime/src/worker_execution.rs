//! The runtime's admitted effect bodies behind the parent broker.
use lash_vm_broker::*;
use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
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
    BrokerFailure::Unavailable {
        refusal: CheckoutRefusal::Infrastructure(error.into_outcome()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::{
        EffectAddress, RuntimeAttribution, RuntimeEffectCommand, RuntimeEffectEnvelope,
        RuntimeEffectInvocation, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
        ScopedEffectController,
    };
    use lashlang::testing::{ast_builders as b, harness};
    use lashlang::{AbilityOp, AbilityOutcome, ExecutionHostError, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct JournalHost<'a> {
        scoped: ScopedEffectController<'a>,
        issued: AtomicUsize,
        performed: Arc<Mutex<Vec<EffectKind>>>,
    }

    impl ExecutionHost for JournalHost<'_> {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
            let ordinal = self.issued.fetch_add(1, Ordering::SeqCst);
            let kind = op.kind();
            let operation = serde_json::to_string(&op).expect("encode the journaled request");
            let performed = Arc::clone(&self.performed);
            let outcome = self
                .scoped
                .execute_effect(
                    RuntimeEffectEnvelope::new(
                        RuntimeEffectInvocation::new(
                            EffectAddress::new(
                                self.scoped.execution_scope().clone(),
                                format!("operation:{ordinal}"),
                            )
                            .expect("effect address"),
                            RuntimeAttribution::none(),
                            "declined_park_law",
                        ),
                        RuntimeEffectCommand::LanguageRuntimeValue { operation },
                    ),
                    RuntimeEffectLocalExecutor::testing(move |_| async move {
                        performed.lock().expect("performed requests").push(kind);
                        let result = match op {
                            AbilityOp::Await(handle) => Ok(AbilityOutcome::Value(handle)),
                            AbilityOp::Sleep(_) => Ok(AbilityOutcome::Value(Value::Null)),
                            op => harness::EchoHost.perform(op).await,
                        }
                        .expect("fixture operation succeeds");
                        Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
                            value: serde_json::to_value(result)
                                .expect("encode the journaled answer"),
                        })
                    }),
                )
                .await
                .map_err(|error| ExecutionHostError::new(error.to_string()))?;
            let RuntimeEffectOutcome::LanguageRuntimeValue { value } = outcome else {
                panic!("the journal must answer the recorded operation");
            };
            Ok(serde_json::from_value(value).expect("decode the journaled answer"))
        }
    }

    /// Delegates to the production adapter, settling the first 1024 awaits in
    /// place so capture is attempted only at and past the continuation bound.
    struct ObservedEffects<'a, H> {
        inner: Effects<'a, H>,
        grants: BTreeMap<String, HandleGrant>,
        resolutions: Mutex<Vec<EffectRequest>>,
        declines: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl<H: ExecutionHost + Sync> ParentEffects for ObservedEffects<'_, H> {
        fn resolve(
            &self,
            context: &AdmittedContext,
            grants: &BTreeMap<String, HandleGrant>,
            frame: FrameEpoch,
            request: &EffectRequest,
        ) -> Result<authority::ResolvedRequest, AuthorityRefusal> {
            assert_eq!(
                grants, &self.grants,
                "admission and reissue receive the ledger's grants"
            );
            self.resolutions
                .lock()
                .expect("resolved requests")
                .push(request.clone());
            self.inner.resolve(context, grants, frame, request)
        }
        async fn retain(
            &self,
            operation: &AdmittedOperation,
        ) -> Result<RequestFingerprint, ParentFault> {
            self.inner.retain(operation).await
        }
        async fn perform(&self, operation: &AdmittedOperation) -> Result<Performed, ParentFault> {
            self.inner.perform(operation).await
        }
        async fn observe_cancellation(&self, checkpoint: u64) -> Result<bool, ParentFault> {
            self.inner.observe_cancellation(checkpoint).await
        }
        fn needs_worker(&self, operation: &AdmittedOperation) -> bool {
            const FIRST_AWAIT_ORDINAL: u64 = 4;
            const SETTLED_AWAIT_BOUND: u64 = 1024;
            self.inner.needs_worker(operation)
                && !matches!(&operation.kind, AdmittedKind::Control { kind: EffectKind::Await, .. } if operation.ordinal < FIRST_AWAIT_ORDINAL + SETTLED_AWAIT_BOUND)
        }
        fn projection(&self, payload: &EncodedPayload) -> Result<EncodedPayload, ParentFault> {
            self.inner.projection(payload)
        }
        fn boundary(&self) -> bool {
            self.inner.boundary()
        }
        fn observe(&self, payload: &EncodedPayload) -> Result<(), ParentFault> {
            self.inner.observe(payload)
        }
        fn park_declined(&self, reason: &str) {
            self.declines
                .lock()
                .expect("declined parks")
                .push(reason.into());
            self.inner.park_declined(reason);
        }
    }

    /// FIG-4754: a real worker declines the await carrying 1025 settled results,
    /// reissues through the production Effects resolver, and completes all
    /// operation families around it on the SQLite-backed server double.
    #[tokio::test]
    async fn a_real_worker_reissues_a_declined_aggregate_park_through_production_effects() {
        const HANDLES: usize = 1026;
        let double = lash_restate_test::backend(0x4754, lash_restate_test::ServerConfig::default())
            .await
            .expect("SQLite server double");
        let admitted = lash_core::AdmittedScope::turn("declined-park", "turn");
        let handler = double
            .open_handler(admitted.clone())
            .await
            .expect("open handler");
        let host = JournalHost {
            scoped: handler.scoped(),
            issued: AtomicUsize::new(0),
            performed: Arc::new(Mutex::new(Vec::new())),
        };
        let context = AdmittedContext {
            owner: VmOwner::new("declined-park"),
            owner_epoch: OwnerEpoch(0),
            identities: CodeCallIdentities::cell(
                lash_core::EffectOpener::for_scope(&admitted).expect("turn opener"),
                "cell",
            ),
            bindings: Arc::new(FrozenBindings::default()),
        };
        let mut config = Service::default().config().clone();
        // This 1026-handle fixture exceeded the standard ten-second CPU bound.
        config.deadlines.cumulative_cpu = std::time::Duration::from_secs(60);
        let workers = Service::new(config);
        let pool = workers.pool().expect("real worker pool");
        let slots = PoolSlots {
            pool: pool.clone(),
            owner_epoch: context.owner_epoch,
            frame_epoch: FrameEpoch(0),
            budget: lash_vm_client::ExecutionBudget::default(),
            recovery: None,
        };
        let snapshot = match workers
            .request_accounted(lash_vm_client::service::Request::State {
                snapshot: None,
                action: lash_vm_client::service::StateAction::Inspect,
            })
            .await
            .expect("fresh worker snapshot")
        {
            lash_vm_client::service::Response::State(view) => view.snapshot,
            other => panic!("expected a worker snapshot: {other:?}"),
        };
        let grants = BTreeMap::from([(
            "retained-handle".into(),
            HandleGrant {
                ordinal: 0,
                call_id: context.identities.call_id(0),
                frame_epoch: FrameEpoch(0),
            },
        )]);
        let (projections, _) = lash_vm_client::Projections::new(
            &lashlang::ProjectedBindings::default(),
            workers.config().protocol.decode.max_nodes as usize,
        )
        .expect("no projected bindings");
        let effects = ObservedEffects {
            inner: Effects {
                host: &host,
                projections,
                boundary: &|| false,
            },
            grants: grants.clone(),
            resolutions: Mutex::new(Vec::new()),
            declines: Mutex::new(Vec::new()),
        };
        let echo = |value| {
            b::receiver_call(
                b::resource(&["tools"]),
                "echo",
                vec![b::record(vec![("value", b::num(value))])],
            )
        };
        let handles: Vec<_> = (0..HANDLES)
            .map(|index| {
                b::record(vec![
                    ("__handle__", b::string("lash")),
                    (
                        "id",
                        b::string(&format!(
                            "p.{}",
                            lash_core::ProcessId::fixture(&format!("awaited-{index}"))
                        )),
                    ),
                ])
            })
            .collect();
        let program = b::program(vec![
            b::assign("before", b::unwrap(echo(1.0))),
            b::assign(
                "before_batch",
                b::await_expr(b::list(vec![echo(2.0), echo(3.0)])),
            ),
            b::sleep_for(b::num(0.0)),
            b::assign("results", b::await_expr(b::list(handles))),
            b::sleep_for(b::num(0.0)),
            b::assign(
                "after_batch",
                b::await_expr(b::list(vec![echo(4.0), echo(5.0)])),
            ),
            b::assign("after", b::unwrap(echo(6.0))),
            b::finish(b::record(vec![
                ("before", b::var("before")),
                ("before_batch", b::var("before_batch")),
                ("results", b::var("results")),
                ("after_batch", b::var("after_batch")),
                ("after", b::var("after")),
            ])),
        ]);
        let artifact = lashlang::ModuleArtifact::from_program(program).expect("fixture artifact");
        let captures = Capture(Mutex::new(None));
        let broker = Broker {
            context: &context,
            effects: &effects,
            checkpoints: &captures,
            slots: &slots,
            codec: FrameCodec::new(workers.config().protocol.decode),
            contract: lashlang::vm_contract_reads(),
            bounds: BrokerBounds {
                protocol: workers.config().protocol,
                ..BrokerBounds::standard()
            },
            frames: FrameFence::new(FrameEpoch(0)),
        };
        let stop = tokio_util::sync::CancellationToken::new();
        let run = broker.run(
            RunStart {
                program: ProgramSource::Artifact {
                    module_ref: artifact.module_ref().to_string(),
                    entry: ProgramEntry::Main,
                    artifact: artifact.to_store_bytes().expect("artifact bytes"),
                },
                contexts: vec![ContextDescription {
                    kind: "vm_run".into(),
                    name: "runtime".into(),
                    body: EncodedPayload(
                        rmp_serde::to_vec_named(&lash_vm_client::RunContext {
                            environment: harness::test_environment(),
                            ..Default::default()
                        })
                        .expect("run context"),
                    ),
                }],
                limits: workers.config().vm_limits,
                from: Some(Checkpoint {
                    vm: OpaqueVmState::seal(
                        VmStateKind::Snapshot,
                        context.owner.clone(),
                        lashlang::vm_contract_versions(),
                        snapshot,
                    ),
                    ledger: LedgerSnapshot {
                        next_ordinal: 1,
                        grants,
                    },
                    frame_epoch: FrameEpoch(0),
                }),
            },
            &stop,
        );
        let end = tokio::time::timeout(std::time::Duration::from_secs(60), run)
            .await
            .expect("the declined park completes without redrive")
            .expect("the real worker completes");
        let (value, checkpoint) = match end {
            BrokeredEnd::Complete { value, checkpoint } => (value, checkpoint),
            BrokeredEnd::GuestError { error, .. } => {
                let failure: lashlang::RuntimeFailure =
                    rmp_serde::from_slice(&error.0).expect("worker guest failure");
                panic!("expected completion: {failure:?}");
            }
            end => panic!("expected completion: {end:?}"),
        };
        let lashlang::ExecutionOutcome::Finished(value) =
            rmp_serde::from_slice(&value.0).expect("worker completion")
        else {
            panic!("the program reaches finish");
        };
        let result = serde_json::to_value(&value).expect("detached completion");
        assert_eq!(result["before"], 1);
        assert_eq!(result["after"], 6);
        assert_eq!(
            result["before_batch"],
            serde_json::json!([{"ok":true,"value":2},{"ok":true,"value":3}])
        );
        assert_eq!(
            result["after_batch"],
            serde_json::json!([{"ok":true,"value":4},{"ok":true,"value":5}])
        );
        let results = result["results"].as_array().expect("every awaited result");
        assert_eq!(results.len(), HANDLES);
        for (index, result) in results.iter().enumerate() {
            assert_eq!(
                result,
                &serde_json::json!({"ok":true,"value":{"__handle__":"lash",
                "id":format!("p.{}", lash_core::ProcessId::fixture(&format!("awaited-{index}")))}})
            );
        }
        let declines = effects.declines.into_inner().expect("declines");
        assert_eq!(declines.len(), 1, "only the await past the bound declines");
        assert!(
            declines[0].contains("1025 settled handle results"),
            "{declines:?}"
        );
        let resolutions = effects.resolutions.into_inner().expect("resolutions");
        let final_await = AbilityOp::Await(lashlang::from_json(serde_json::json!({
            "__handle__": "lash",
            "id": format!("p.{}", lash_core::ProcessId::fixture("awaited-1025")),
        })))
        .encode();
        let repeated: Vec<_> = resolutions
            .iter()
            .filter(|request| request.kind == EffectKind::Await && request.payload == final_await)
            .collect();
        assert_eq!(
            repeated.len(),
            2,
            "admission and reissue use the production resolver"
        );
        assert!(repeated[1].id > repeated[0].id);
        {
            let performed = host.performed.lock().expect("performed");
            assert_eq!(
                performed
                    .iter()
                    .filter(|kind| **kind == EffectKind::Await)
                    .count(),
                HANDLES
            );
            assert_eq!(
                performed
                    .iter()
                    .filter(|kind| **kind == EffectKind::ResourceOperation)
                    .count(),
                2
            );
            assert_eq!(
                performed
                    .iter()
                    .filter(|kind| **kind == EffectKind::ResourceOperationBatch)
                    .count(),
                2
            );
            assert_eq!(
                performed
                    .iter()
                    .filter(|kind| **kind == EffectKind::Sleep)
                    .count(),
                2
            );
        }
        assert_eq!(
            checkpoint.ledger.next_ordinal as usize,
            HANDLES + 8,
            "reissue takes no new ordinal"
        );
        assert_eq!(checkpoint.ledger.grants, effects.grants);
        let counters = pool.measurements().counters;
        assert_eq!(
            counters.cell_executions, 8,
            "the at-bound await parks; the oversized await keeps its worker"
        );
        assert_eq!(counters.crashes, 0);
        assert_eq!(counters.discards, 0);
        drop(effects.inner);
        drop(host);
        handler.close().await.expect("close handler");
    }
}
