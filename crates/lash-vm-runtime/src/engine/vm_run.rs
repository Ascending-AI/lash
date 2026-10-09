//! The `vm_run` engine step: run the VM from the committed snapshot to its
//! next quiet point (ADR 0132 §8; D-L6a, FIG-5198).
//!
//! The VM resumes from the snapshot its input carries (the program's entry
//! only for a first run, which has none). The operation the snapshot parked
//! on is issued again and answered from the input's injection. The run then
//! goes on until the VM issues its next host operation, which this step
//! never performs: it answers the VM `HandedOver`, the VM parks positioned
//! to issue it again, and the step answers the new snapshot with the
//! operation. The process activation commits both, with the operation's
//! admission, in one `process.advance` transaction.
//!
//! The step is `Repeatable`: a run that never committed is a recomputation
//! of an effect-free stretch of VM from the same snapshot, and reaches the
//! same quiet point. So the broker's quiet points along the way commit
//! nowhere and admit nothing ([`HeldQuietPoints`], [`NoAdmissions`]): the
//! activation commits the one the run ends on.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use lash_core::tool_run::{KnownFailureReason, MaterialOwner, MaterialRole};
use lash_core::{EngineStepRun, Material, ProcessId, SettledOutput};
use lash_sansio::sync::MutexExt;
use lash_vm::{AbilityOp, AbilityOutcome, ExecutionHostError, Value};
use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
use tokio_util::sync::CancellationToken;

use super::injection;
use super::state::{BatchShape, EncodedOutcome, Injection, IssuedLeaf, IssuedOperation};
use super::state::{VmRunInput, VmRunOutput};
use crate::bridge::{ExecutionCancellation, lash_vm_value_to_json};
use crate::process::{
    lash_vm_program_hash, process_lash_vm_execution_result, process_lash_vm_failure,
    process_worker_failure, retired_generation_at, segment_continuation_expectation,
    segment_continuation_owner, validate_lash_vm_process_for_run,
};
use crate::{
    LashVmHostEnvironmentCheck, LashVmHostError, LashVmProcessEngine, LashVmProcessFailureCode,
    LashVmProcessInput, LashVmRecordedSettings,
};

/// Why a `vm_run` reached no quiet point and no end: the run left nothing,
/// and the same run is asked again.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct VmRunFault(pub(super) String);

/// Run the `vm_run` step `run` to its outcome.
pub(crate) async fn run_vm_step(
    engine: &LashVmProcessEngine,
    run: EngineStepRun,
    stop: CancellationToken,
) -> SettledOutput {
    let process = run.process.clone();
    match vm_run(engine, run, stop).await {
        Ok(output) => match serde_json::to_string(&output) {
            Ok(text) => completed(&process, text),
            Err(error) => failed(&process, &VmRunFault(error.to_string())),
        },
        Err(fault) => failed(&process, &fault),
    }
}

fn material(process: &ProcessId, text: String) -> Material {
    Material::journal_local(
        MaterialOwner::Process {
            process_id: process.clone(),
        },
        MaterialRole::AttemptOutput,
        text,
    )
}

pub(super) fn completed(process: &ProcessId, text: String) -> SettledOutput {
    SettledOutput::Completed(material(process, text))
}

pub(super) fn failed(process: &ProcessId, fault: &VmRunFault) -> SettledOutput {
    tracing::warn!(process_id = %process, error = %fault, "vm_run reached no quiet point");
    let output = lash_core::ToolCallOutput::failure(lash_core::ToolFailure::runtime(
        lash_core::ToolFailureClass::Internal,
        "vm_run_fault",
        fault.to_string(),
    ));
    let text = serde_json::to_string(&output).unwrap_or_default();
    SettledOutput::Failed(material(process, text).failure(KnownFailureReason::Reported, None))
}

async fn vm_run(
    engine: &LashVmProcessEngine,
    run: EngineStepRun,
    stop: CancellationToken,
) -> Result<VmRunOutput, VmRunFault> {
    let ended = |outcome: lash_core::ProcessOutcome| {
        Ok(VmRunOutput::Ended {
            outcome: Box::new(outcome),
        })
    };
    let input: VmRunInput = serde_json::from_value(run.input)
        .map_err(|error| VmRunFault(format!("vm_run input: {error}")))?;
    let epoch_ms = u64::try_from(run.now.0).unwrap_or(0);
    let mut process_input = match LashVmProcessInput::from_payload(input.payload.clone()) {
        Ok(process_input) => process_input,
        Err(error) => {
            return ended(process_lash_vm_failure(
                LashVmProcessFailureCode::ProcessPayloadInvalid,
                format!("invalid lash_vm process payload: {error}"),
                None,
            ));
        }
    };
    // Executable identity: a snapshot captured by another program identity
    // is refused before it resumes, naming the identity it recorded.
    let program_hash = lash_vm_program_hash(&process_input);
    if let Some(found) = &input.program_hash
        && *found != program_hash
    {
        tracing::warn!(
            found,
            current = program_hash,
            "a lash_vm snapshot was captured by another program identity; refusing to resume"
        );
        return ended(retired_generation_at(found.clone(), epoch_ms));
    }
    let artifact = match engine
        .workers
        .inspect_artifact(&engine.artifact_store, &process_input.module_ref)
        .await
    {
        Ok(Some(artifact)) => artifact,
        Ok(None) | Err(lash_core::ArtifactStoreError::ArtifactMissing { .. }) => {
            return ended(process_lash_vm_failure(
                LashVmProcessFailureCode::ProcessModuleArtifactMissing,
                format!(
                    "missing lash_vm module artifact `{}`",
                    process_input.module_ref
                ),
                None,
            ));
        }
        Err(lash_core::ArtifactStoreError::UnsupportedGeneration { refusal }) => {
            tracing::warn!(module_ref = %process_input.module_ref, error = %refusal,
                "refusing an unsupported module artifact generation");
            return ended(retired_generation_at(
                process_input.module_ref.to_string(),
                epoch_ms,
            ));
        }
        Err(lash_core::ArtifactStoreError::StoredDataCorrupt { source, .. }) => {
            return ended(lash_core::ProcessAwaitOutput::Abandoned {
                evidence: Box::new(lash_core::AbandonEvidence {
                    writer: lash_core::AbandonWriter::ResumeRefused {
                        reason: lash_core::ProcessResumeRefusal::StoredArtifactCorrupt {
                            artifact_ref: process_input.module_ref.to_string(),
                            source,
                        },
                    },
                    owner: None,
                    epoch_ms,
                }),
                control: None,
            });
        }
        Err(error) => return Err(VmRunFault(error.to_string())),
    };
    process_input.process_name = artifact
        .process_name_for_ref(&process_input.process_ref)
        .unwrap_or("")
        .to_owned();
    let settings: LashVmRecordedSettings = match run
        .engine_config
        .as_ref()
        .map(|config| serde_json::from_value(config.clone()))
    {
        Some(Ok(settings)) => settings,
        Some(Err(error)) => {
            return ended(process_lash_vm_failure(
                LashVmProcessFailureCode::ProcessHostEnvironmentInvalid,
                format!("the process's recorded lash_vm settings do not decode: {error}"),
                None,
            ));
        }
        None => {
            return ended(process_lash_vm_failure(
                LashVmProcessFailureCode::ProcessHostEnvironmentInvalid,
                "the process recorded no lash_vm settings",
                None,
            ));
        }
    };
    let bounds = settings.execution_bounds;
    let host_environment = settings
        .into_surface()
        .host_environment(&run.tool_catalog)
        .map_err(|error| error.to_string());
    if let Err(output) = validate_lash_vm_process_for_run(
        &artifact,
        &process_input,
        LashVmHostEnvironmentCheck::CheckHostEnvironment(
            host_environment.as_ref().map_err(Clone::clone),
        ),
    ) {
        return ended(*output);
    }
    let host_environment = match host_environment {
        Ok(host_environment) => host_environment,
        Err(error) => {
            return ended(process_lash_vm_failure(
                LashVmProcessFailureCode::ProcessHostEnvironmentInvalid,
                error,
                None,
            ));
        }
    };
    let trace = super::trace::ProcessTrace::new(engine, &run.process, &process_input, &artifact);
    if input.vm.is_none()
        && let Some(trace) = &trace
    {
        trace.started(&artifact);
    }
    let owner = segment_continuation_owner(&run.process);
    let start = match input.vm {
        Some(vm) => {
            let reads = lash_vm::vm_contract_reads();
            if let Err(refusal) = vm.check(&segment_continuation_expectation(&owner, &reads)) {
                return ended(process_lash_vm_failure(
                    LashVmProcessFailureCode::ProcessSegmentHandoverInvalid,
                    format!("invalid lash_vm snapshot: {refusal}"),
                    None,
                ));
            }
            lash_vm_protocol::StartState::Continuation(vm)
        }
        None => {
            let mut state = lash_vm_client::RemoteState::pristine(engine.workers.clone());
            state
                .defaults(
                    process_input
                        .args
                        .iter()
                        .map(|(key, value)| (key.clone(), lash_vm::from_json(value.clone())))
                        .collect(),
                    Default::default(),
                )
                .await
                .map_err(|error| VmRunFault(error.into_runtime_error().to_string()))?;
            lash_vm_protocol::StartState::Snapshot(lash_vm_protocol::OpaqueVmState::seal(
                lash_vm_protocol::VmStateKind::Snapshot,
                owner.clone(),
                lash_vm::vm_contract_versions(),
                state.bytes().unwrap_or_default().to_vec(),
            ))
        }
    };
    let host = QuietPointHost {
        trace: trace.clone(),
        environment: host_environment.clone(),
        catalog: Arc::clone(&run.tool_catalog),
        inject: Mutex::new(input.inject),
        issued: Mutex::new(None),
        fault: Mutex::new(None),
        now_ms: run.now.0,
        stop: stop.clone(),
        cancellation: ExecutionCancellation::new(),
    };
    let identities = lash_vm_broker::CodeCallIdentities::process_body(run.process.clone());
    let boundary = || false;
    let quiet_points = HeldQuietPoints::default();
    let observed_host = crate::LanguageTraceHost::new(host, |_: &QuietPointHost, payload| {
        if let Some(trace) = &trace {
            trace.emit(payload);
        }
    });
    let end = crate::WorkerRun {
        service: &engine.workers,
        host: &observed_host,
        identities,
        owner,
        frame_epoch: lash_vm_protocol::FrameEpoch(0),
        program: lash_vm_protocol::ProgramSource::Artifact {
            module_ref: artifact.module_ref().to_string(),
            entry: lash_vm_protocol::ProgramEntry::Process {
                component: process_input.process_ref.component.to_string(),
                position: process_input.process_ref.pos,
            },
            artifact: artifact.bytes().to_vec(),
        },
        context: lash_vm_client::RunContext {
            environment: host_environment,
            mode: lash_vm::ExecutionMode::Process,
            observe_execution: trace.is_some(),
            ..Default::default()
        },
        projected: lash_vm::ProjectedBindings::new(),
        bounds,
        state: start,
        from: None,
        snapshots: &quiet_points,
        admissions: &NoAdmissions,
        boundary: &boundary,
        performing: None,
        providers: lash_vm::ProjectionCatalog::of_backend(run.projection_providers.as_deref()),
    }
    .run()
    .await;
    let host = observed_host.host();
    if let Some(fault) = host.fault.lock_recover().take() {
        return ended(process_lash_vm_failure(
            LashVmProcessFailureCode::ProcessSegmentResumeFailed,
            fault,
            None,
        ));
    }
    let infra = |message: String| VmRunFault(message);
    let output = match end {
        Err(failure) => match process_worker_failure(&failure) {
            Some(terminal) => ended(terminal),
            None => Err(infra(failure.to_string())),
        },
        Ok(lash_vm_broker::BrokeredEnd::Complete { value, .. }) => {
            ended(process_lash_vm_execution_result(
                Ok(rmp_serde::from_slice(&value.0).map_err(|error| infra(error.to_string()))?),
                None,
            ))
        }
        Ok(lash_vm_broker::BrokeredEnd::GuestError { error, .. }) => {
            ended(process_lash_vm_execution_result(
                Err(rmp_serde::from_slice::<lash_vm::RuntimeFailure>(&error.0)
                    .map_err(|error| infra(error.to_string()))?
                    .error),
                None,
            ))
        }
        Ok(lash_vm_broker::BrokeredEnd::Suspended { checkpoint }) => {
            let issued =
                host.issued.lock_recover().take().ok_or_else(|| {
                    infra("the VM parked without issuing an operation".to_owned())
                })?;
            Ok(VmRunOutput::Parked {
                program_hash,
                vm: checkpoint.vm,
                issued,
            })
        }
        Ok(lash_vm_broker::BrokeredEnd::Cancelled) if stop.is_cancelled() => {
            Err(infra("the vm_run step was stopped".to_owned()))
        }
        Ok(lash_vm_broker::BrokeredEnd::Cancelled) => ended(
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::cancelled(
                lash_core::ToolCancellation::runtime("lash_vm process was cancelled"),
            )),
        ),
    };
    if let Ok(VmRunOutput::Ended { outcome }) = &output
        && let Some(trace) = &trace
    {
        trace.finished(outcome);
    }
    output
}

/// Where a `vm_run`'s quiet points go: nowhere durable. Each is held only
/// to hand its checkpoint back to the broker, and none admits anything: the
/// VM's state commits in the engine's, with the admissions of the steps the
/// engine asks for, and a run that never committed runs again from it.
#[derive(Default)]
struct HeldQuietPoints {
    revision: AtomicU64,
}

#[async_trait::async_trait]
impl lash_vm_broker::SnapshotStore for HeldQuietPoints {
    async fn commit_quiet_point(
        &self,
        point: lash_vm_broker::QuietPoint,
    ) -> Result<lash_vm_broker::Committed, lash_vm_broker::QuietPointRefusal> {
        if !point.members.is_empty() || !point.waits.is_empty() || !point.with.is_empty() {
            return Err(lash_vm_broker::QuietPointRefusal(
                "a vm_run's quiet point admits nothing".to_owned(),
            ));
        }
        Ok(lash_vm_broker::Committed {
            rev: lash_vm_broker::SnapshotRev(self.revision.fetch_add(1, Ordering::Relaxed) + 1),
            checkpoint: point.checkpoint,
            waits: Vec::new(),
        })
    }

    async fn recover(
        &self,
        _pending: &lash_vm_broker::PendingOperation,
    ) -> Result<Vec<lash_vm_broker::WaitRef>, lash_vm_broker::QuietPointRefusal> {
        Err(lash_vm_broker::QuietPointRefusal(
            "a vm_run resumes from its engine state, never from a broker checkpoint".to_owned(),
        ))
    }

    async fn latest(
        &self,
    ) -> Result<
        Option<(lash_vm_broker::SnapshotRev, lash_vm_broker::Checkpoint)>,
        lash_vm_broker::QuietPointRefusal,
    > {
        Ok(None)
    }

    async fn open_frame(
        &self,
        _frame: lash_vm_protocol::FrameEpoch,
    ) -> Result<(), lash_vm_broker::QuietPointRefusal> {
        Ok(())
    }
}

/// A `vm_run` admits none of the operations its VM issues: the activation
/// admits the steps the engine asks for, and a wait is the engine's action.
struct NoAdmissions;

#[async_trait::async_trait]
impl crate::OperationAdmissions for NoAdmissions {
    async fn admission(
        &self,
        _ordinal: u64,
        _request: &lash_vm_broker::OperationRequest,
    ) -> Result<lash_vm_broker::Admission, String> {
        Ok(lash_vm_broker::Admission::default())
    }
}

/// The host a `vm_run` runs the VM under: it answers the reissued operation
/// from the injection, answers what needs no step in place, and parks the
/// VM on the first operation that needs one.
struct QuietPointHost {
    trace: Option<super::trace::ProcessTrace>,
    environment: lash_vm::LashVmHostEnvironment,
    catalog: Arc<lash_core::ToolCatalog>,
    inject: Mutex<Option<Injection>>,
    issued: Mutex<Option<IssuedOperation>>,
    fault: Mutex<Option<String>>,
    now_ms: i64,
    stop: CancellationToken,
    cancellation: ExecutionCancellation,
}

/// How the host answers an operation it does not park on.
enum Issue {
    /// Answered in place.
    Answered(Result<AbilityOutcome, ExecutionHostError>),
    /// Park the VM on it.
    Park(IssuedOperation),
}

impl QuietPointHost {
    fn perform_now(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => {
                return Ok(AbilityOutcome::Value(value));
            }
            AbilityOp::Print(_) => return Err(LashVmHostError::PrintUnavailable.into()),
            _ => {}
        }
        if let Some(inject) = self.inject.lock_recover().take() {
            if let Some(trace) = &self.trace {
                trace.bind_calls(&op, &inject);
                trace.waiting(&op, self.now_ms, true);
            }
            return match injection::answer(&op, inject, &self.cancellation) {
                Ok(result) => result,
                Err(fault) => Err(self.fault(fault.to_string())),
            };
        }
        if self.issued.lock_recover().is_some() {
            return Err(self.fault("the VM issued an operation after it parked".to_owned()));
        }
        let observed = self.trace.as_ref().map(|_| op.clone());
        match self.issue(op)? {
            Issue::Answered(result) => result,
            Issue::Park(issued) => {
                if let (Some(trace), Some(op)) = (&self.trace, &observed) {
                    trace.waiting(op, self.now_ms, false);
                }
                *self.issued.lock_recover() = Some(issued);
                Ok(AbilityOutcome::HandedOver)
            }
        }
    }

    /// Stop the run: the snapshot and the state disagree.
    fn fault(&self, message: String) -> ExecutionHostError {
        self.fault.lock_recover().get_or_insert(message.clone());
        self.cancellation.cancel();
        ExecutionHostError::new(message)
    }

    fn issue(&self, op: AbilityOp) -> Result<Issue, ExecutionHostError> {
        Ok(match op {
            AbilityOp::ResourceOperation(operation) => match self.leaf(*operation)? {
                IssuedLeaf::Settled { outcome, .. } => Issue::Answered(
                    match outcome
                        .decode()
                        .map_err(|error| ExecutionHostError::new(error.to_string()))?
                    {
                        lash_vm::ResourceOperationOutcome::Value(value) => {
                            Ok(AbilityOutcome::Value(value))
                        }
                        lash_vm::ResourceOperationOutcome::Error(error) => Err(error),
                    },
                ),
                leaf => Issue::Park(IssuedOperation::Leaves {
                    batch: None,
                    leaves: vec![leaf],
                }),
            },
            AbilityOp::ResourceOperationBatch(batch) => {
                let shape = BatchShape {
                    consumer: batch.consumer,
                    settled_value_after: batch.settled_value_after,
                };
                let leaves = batch
                    .leaves
                    .into_iter()
                    .map(|leaf| match leaf {
                        lash_vm::ResourceOperationBatchLeaf::Operation(operation) => {
                            self.leaf(operation)
                        }
                        lash_vm::ResourceOperationBatchLeaf::Timer(sleep) => {
                            match crate::timer_duration_ms(&sleep) {
                                Ok(duration_ms) => Ok(IssuedLeaf::Timer {
                                    until_ms: self.after(duration_ms),
                                }),
                                Err(error) => settled_leaf(Err(error)),
                            }
                        }
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Issue::Park(IssuedOperation::Leaves {
                    batch: Some(shape),
                    leaves,
                })
            }
            AbilityOp::Await(handle) => Issue::Park(IssuedOperation::AwaitProcess {
                process: awaited_process(&handle)?,
            }),
            AbilityOp::Sleep(sleep) => {
                let until_ms = match crate::process_sleep(sleep.kind, &sleep.value)? {
                    lash_core::SleepSpec::For { duration_ms } => self.after(duration_ms),
                    lash_core::SleepSpec::Until { deadline_ms } => {
                        i64::try_from(deadline_ms).unwrap_or(i64::MAX)
                    }
                };
                let site = sleep
                    .call_site
                    .as_ref()
                    .map(|call_site| lash_core::StepEffectSite {
                        node_id: call_site.site.node_id.clone(),
                        occurrence: call_site.occurrence,
                    });
                Issue::Park(IssuedOperation::Sleep { until_ms, site })
            }
            AbilityOp::Finish(_) | AbilityOp::Fail(_) | AbilityOp::Print(_) => {
                return Err(ExecutionHostError::new("answered before issue"));
            }
        })
    }

    fn after(&self, duration_ms: u64) -> i64 {
        self.now_ms
            .saturating_add(i64::try_from(duration_ms).unwrap_or(i64::MAX))
    }

    fn leaf(
        &self,
        operation: lash_vm::ResourceOperation,
    ) -> Result<IssuedLeaf, ExecutionHostError> {
        if let Some(checked) = crate::language_runtime_operation(
            &operation.receiver,
            &operation.operation,
            &operation.args,
        ) {
            return settled_leaf(checked.map(|runtime| self.runtime_value(runtime)));
        }
        match self.tool_leaf(operation) {
            Ok(leaf) => Ok(leaf),
            Err(error) => settled_leaf(Err(error)),
        }
    }

    fn tool_leaf(
        &self,
        operation: lash_vm::ResourceOperation,
    ) -> Result<IssuedLeaf, ExecutionHostError> {
        let lash_vm::ResourceOperation {
            receiver,
            operation,
            args,
            call_site,
        } = operation;
        let site = call_site
            .as_ref()
            .map(|call_site| lash_core::StepEffectSite {
                node_id: call_site.site.node_id.clone(),
                occurrence: call_site.occurrence,
            });
        let Value::Resource(receiver) = &receiver else {
            return Err(LashVmHostError::ModuleAuthorityRequired { operation }.into());
        };
        let host_operation =
            crate::resolve_lash_vm_module_operation(&self.environment, receiver, &operation)?;
        let tool = lash_core::ToolId::from(host_operation.as_str());
        if !self
            .catalog
            .tools
            .iter()
            .any(|entry| entry.manifest.id == tool)
        {
            return Err(LashVmHostError::ResolvedOperationUnavailable {
                operation,
                host_operation,
            }
            .into());
        }
        Ok(IssuedLeaf::Tool {
            tool,
            input: resource_payload(&args)?,
            site,
            language_execution: self.language_execution(call_site.as_ref()),
        })
    }

    fn language_execution(
        &self,
        call_site: Option<&lash_vm::LashVmExecutionCallSite>,
    ) -> Option<Box<lash_trace::TraceLanguageExecution>> {
        let trace = self.trace.as_ref()?;
        Some(Box::new(trace.resource_started(call_site?)))
    }

    /// A language runtime value, sampled in place: it reaches nothing outside
    /// the VM, and it commits with the snapshot the run ends in, so a run
    /// that never committed sampled a value nobody saw.
    fn runtime_value(&self, operation: &str) -> Value {
        if operation == lash_vm::LANGUAGE_RUNTIME_NOW_OPERATION {
            return Value::Number(self.now_ms as f64);
        }
        use std::hash::BuildHasher as _;
        let bits = std::collections::hash_map::RandomState::new().hash_one(self.now_ms)
            & ((1_u64 << 53) - 1);
        Value::Number(bits as f64 / (1_u64 << 53) as f64)
    }
}

fn settled_leaf(
    result: Result<Value, ExecutionHostError>,
) -> Result<IssuedLeaf, ExecutionHostError> {
    let fulfilled = result.is_ok();
    let outcome = EncodedOutcome::encode(&lash_vm::ResourceOperationOutcome::from_result(result))
        .map_err(|error| ExecutionHostError::new(error.to_string()))?;
    Ok(IssuedLeaf::Settled { fulfilled, outcome })
}

/// The process an awaited handle names.
fn awaited_process(handle: &Value) -> Result<ProcessId, ExecutionHostError> {
    let not_a_process = || ExecutionHostError::new("await expects a process handle");
    let Value::Record(record) = handle else {
        return Err(not_a_process());
    };
    let field = |name: &str| match record.get(name) {
        Some(Value::String(text)) => Some(text.to_string()),
        _ => None,
    };
    let (Some(kind), Some(id)) = (field(lash_sansio::handle::HANDLE_FIELD), field("id")) else {
        return Err(not_a_process());
    };
    match lash_sansio::handle::parse_handle(&kind, &id).and_then(|handle| handle.target()) {
        Some(lash_sansio::handle::HandleTarget::Process { process_id }) => Ok(process_id),
        _ => Err(not_a_process()),
    }
}

/// A resource operation's input: its one record argument, or its
/// arguments under `args`.
fn resource_payload(args: &[Value]) -> Result<serde_json::Value, ExecutionHostError> {
    let payload = if let [Value::Record(record)] = args {
        lash_vm_value_to_json(&Value::Record(Arc::clone(record)))?
    } else {
        serde_json::json!({
            "args": args
                .iter()
                .map(lash_vm_value_to_json)
                .collect::<Result<Vec<_>, _>>()?,
        })
    };
    if !payload.is_object() {
        return Err(LashVmHostError::ModulePayloadNotObject.into());
    }
    Ok(payload)
}

impl lash_vm::ExecutionHost for QuietPointHost {
    fn perform(
        &self,
        op: AbilityOp,
    ) -> impl std::future::Future<Output = Result<AbilityOutcome, ExecutionHostError>> + Send {
        let result = self.perform_now(op);
        async move { result }
    }

    fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled() || self.stop.is_cancelled()
    }
}
