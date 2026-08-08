use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
#[cfg(any(test, feature = "testing"))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};

use lash_core::facade_support::ToolChildExecutionTraceHook;
use lash_trace::{
    TraceBranchSelection, TraceContext, TraceEvent, TraceLabelMetadata,
    TraceLashlangChildExecution, TraceLashlangExecutionEvent, TraceLashlangExecutionIdentity,
    TraceLashlangMap, TraceLashlangMapEdge, TraceLashlangMapNode, TraceLashlangStatus, TraceRecord,
    TraceRuntimeScope, TraceRuntimeSubject, TraceSink,
};
use lashlang::{ExecutionHost, ExecutionHostError};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{
    LASHLANG_ENGINE_KIND, LashlangHostError, LashlangProcessEngine, LashlangProcessFailureCode,
    LashlangProcessInput,
    bridge::{
        lashlang_value_to_json, process_event_payload, protocol_tool_reply_to_lashlang_value,
        sleep_duration_ms,
    },
    lashlang_host_environment_satisfies_requirements, prepare_lashlang_process_start,
    resolve_lashlang_module_operation,
};

static SEGMENT_BOUNDARY_DECLINED_TOTAL: AtomicU64 = AtomicU64::new(0);
#[cfg(any(test, feature = "testing"))]
static EXECUTION_BOUND_EXHAUSTION_LOUD: AtomicBool = AtomicBool::new(true);

fn record_segment_boundary_decline(error: &dyn std::fmt::Display, message: &'static str) {
    let declined_total = SEGMENT_BOUNDARY_DECLINED_TOTAL
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    tracing::warn!(error = %error, declined_total, "{message}");
}

fn trace_lifecycle_for_segment(
    is_initial_segment: bool,
    output: &lash_core::ProcessRunOutcome,
) -> (bool, bool) {
    (
        is_initial_segment,
        matches!(output, lash_core::ProcessRunOutcome::Terminal(_)),
    )
}

#[derive(serde::Serialize, serde::Deserialize)]
struct LashlangSegmentState {
    vm: lashlang::VmContinuation,
    sleep_sequence: u64,
    event_sequence: u64,
    signal_send_sequence: u64,
    signal_wait_ordinals: BTreeMap<String, u64>,
}

fn lashlang_program_hash(input: &LashlangProcessInput) -> String {
    let identity = serde_json::to_vec(&(
        "lashlang-bytecode",
        lashlang::BYTECODE_FORMAT_VERSION,
        &input.module_ref,
        &input.process_ref,
        &input.host_requirements_ref,
        &input.process_name,
    ))
    .expect("lashlang program identity should serialize");
    format!("sha256:{:x}", Sha256::digest(identity))
}

fn validate_lashlang_program_hash(
    persisted: Option<&str>,
    current: &str,
) -> Result<(), Box<lash_core::ProcessAwaitOutput>> {
    if let Some(persisted) = persisted
        && persisted != current
    {
        return Err(Box::new(process_lashlang_failure(
            LashlangProcessFailureCode::RestateSegmentProgramHashMismatch,
            format!(
                "lashlang bytecode v{} segment program identity mismatch: persisted {persisted}, current {current}",
                lashlang::BYTECODE_FORMAT_VERSION
            ),
            None,
        )));
    }
    Ok(())
}

pub async fn run_lashlang_process(
    engine: LashlangProcessEngine,
    mut context: lash_core::ProcessEngineRunContext<'_>,
    payload: serde_json::Value,
) -> Result<lash_core::ProcessRunOutcome, lash_core::ProcessInfraError> {
    let handover = context.take_handover();
    let is_initial_segment = handover.is_none();
    let persisted_program_hash = handover
        .as_ref()
        .and_then(|handover| handover.program_hash.clone());
    let segment_controller = context.scoped_effect_controller();
    let phase_probe = context.turn_phase_probe();
    let input = match LashlangProcessInput::from_payload(payload) {
        Ok(input) => input,
        Err(err) => {
            return Ok(process_lashlang_failure(
                LashlangProcessFailureCode::ProcessPayloadInvalid,
                format!("invalid lashlang process payload: {err}"),
                None,
            )
            .into());
        }
    };
    let artifact = {
        let _phase = context.named_phase("rlm_process.load_artifact");
        match engine
            .artifact_store
            .get_module_artifact(&input.module_ref)
            .await
        {
            Ok(Some(artifact)) => artifact,
            Ok(None) => {
                return Ok(process_lashlang_failure(
                    LashlangProcessFailureCode::ProcessModuleArtifactMissing,
                    format!("missing lashlang module artifact `{}`", input.module_ref),
                    None,
                )
                .into());
            }
            Err(err) => {
                return Err(lash_core::ProcessInfraError::new(
                    lash_core::PluginError::Session(format!(
                        "failed to load lashlang module artifact `{}`: {err}",
                        input.module_ref
                    )),
                ));
            }
        }
    };
    if artifact.host_requirements_ref != input.host_requirements_ref {
        return Ok(process_lashlang_failure(
            LashlangProcessFailureCode::ProcessHostRequirementsMismatch,
            format!(
                "lashlang process `{}` requested surface {}, artifact has {}",
                input.process_name, input.host_requirements_ref, artifact.host_requirements_ref
            ),
            None,
        )
        .into());
    }
    if artifact.process_ref(&input.process_name) != Some(&input.process_ref) {
        return Ok(process_lashlang_failure(
            LashlangProcessFailureCode::ProcessRefMismatch,
            format!(
                "lashlang module `{}` does not export process `{}` as requested ref {:?}",
                input.module_ref, input.process_name, input.process_ref
            ),
            None,
        )
        .into());
    }
    let (tool_catalog, host_environment) = {
        let _phase = context.named_phase("rlm_process.resolve_environment");
        let tool_catalog = match context.resolved_tool_catalog() {
            Ok(tool_catalog) => tool_catalog,
            Err(err) => {
                return Err(lash_core::ProcessInfraError::new(err));
            }
        };
        let surface = engine
            .surface
            .clone()
            .for_process_registry(context.process_registry_available());
        let host_environment = match surface.host_environment(&tool_catalog) {
            Ok(host_environment) => host_environment,
            Err(err) => {
                return Ok(process_lashlang_failure(
                    LashlangProcessFailureCode::ProcessHostEnvironmentInvalid,
                    err.to_string(),
                    None,
                )
                .into());
            }
        };
        if let Err(err) = lashlang_host_environment_satisfies_requirements(
            &artifact.host_requirements,
            &host_environment,
        ) {
            return Ok(process_lashlang_failure(
                LashlangProcessFailureCode::ProcessHostEnvironmentIncompatible,
                format!(
                    "lashlang process `{}` is incompatible with this host surface: {err}",
                    input.process_name
                ),
                None,
            )
            .into());
        }
        (tool_catalog, host_environment)
    };
    let compiled = {
        let _phase = context.named_phase("rlm_process.compile");
        let compiled = match engine.process_cache.lock() {
            Ok(mut cache) => {
                cache.get_or_compile(&artifact, &input.process_ref, &input.host_requirements_ref)
            }
            Err(_) => Err(lashlang::RuntimeError::CompiledProcessCacheUnavailable),
        };
        match compiled {
            Ok(compiled) => compiled,
            Err(err) => {
                return Ok(process_lashlang_failure(
                    LashlangProcessFailureCode::ProcessCompileFailed,
                    format!("failed to compile process `{}`: {err}", input.process_name),
                    None,
                )
                .into());
            }
        }
    };
    let current_program_hash = lashlang_program_hash(&input);
    if let Err(output) =
        validate_lashlang_program_hash(persisted_program_hash.as_deref(), &current_program_hash)
    {
        return Ok((*output).into());
    }
    let segment_state: Option<LashlangSegmentState> = match handover {
        Some(handover) => match serde_json::from_slice(&handover.engine_state) {
            Ok(state) => Some(state),
            Err(err) => {
                return Ok(process_lashlang_failure(
                    LashlangProcessFailureCode::ProcessSegmentHandoverInvalid,
                    format!("invalid lashlang segment handover: {err}"),
                    None,
                )
                .into());
            }
        },
        None => None,
    };
    let process_id = context.registration().id.clone();
    let session_id = context.session_id().to_string();
    let lashlang_execution_trace = LashlangProcessExecutionTrace::new(
        engine.execution_sink.clone(),
        engine.trace_context.clone(),
        session_id,
        process_id.clone(),
        artifact.module_ref.clone(),
        input.process_ref.clone(),
        input.process_name.clone(),
    );
    if is_initial_segment {
        lashlang_execution_trace.emit_started(&artifact);
    }
    let processes = context.processes();
    let cancellation = context.cancellation_token();
    let (ctx, guard, mut state) = {
        let _phase = context.named_phase("rlm_process.build_context");
        let runtime_context = match context.into_runtime_context(tool_catalog) {
            Ok(runtime_context) => runtime_context,
            Err(err) => {
                return Err(lash_core::ProcessInfraError::new(err));
            }
        };
        let (ctx, guard) = runtime_context.into_parts();
        let mut globals = lashlang::Record::with_capacity(input.args.len());
        for (name, value) in input.args {
            globals.insert(name, lashlang::from_json(value));
        }
        let state = lashlang::State::from_snapshot(lashlang::Snapshot { globals });
        (ctx, guard, state)
    };
    let sleep_sequence = segment_state
        .as_ref()
        .map_or(0, |state| state.sleep_sequence);
    let event_sequence = segment_state
        .as_ref()
        .map_or(0, |state| state.event_sequence);
    let signal_send_sequence = segment_state
        .as_ref()
        .map_or(0, |state| state.signal_send_sequence);
    let signal_wait_ordinals = segment_state
        .as_ref()
        .map_or_else(BTreeMap::new, |state| state.signal_wait_ordinals.clone());
    let host = LashlangProcessHost {
        ctx,
        host_environment,
        artifact_store: engine.artifact_store(),
        processes,
        process_id: process_id.clone(),
        lashlang_execution_trace: lashlang_execution_trace.clone(),
        sleep_sequence: AtomicU64::new(sleep_sequence),
        event_sequence: AtomicU64::new(event_sequence),
        signal_send_sequence: AtomicU64::new(signal_send_sequence),
        signal_wait_ordinals: tokio::sync::Mutex::new(signal_wait_ordinals),
    };
    let env = lashlang::ExecutionEnvironment::new(&host)
        .process()
        .with_execution_bounds(engine.execution_bounds);
    let output = {
        let _phase = host.ctx.named_phase("rlm_process.execute");
        execute_lashlang(
            compiled,
            &mut state,
            &env,
            cancellation.clone(),
            segment_controller.controller(),
            &host,
            (segment_state, current_program_hash),
        )
        .await
    };
    drop(env);
    drop(host);
    {
        let _phase =
            lash_core::runtime::RuntimeNamedPhase::begin(phase_probe, "rlm_process.shutdown");
        guard.shutdown().await;
    }
    if trace_lifecycle_for_segment(is_initial_segment, &output).1
        && let lash_core::ProcessRunOutcome::Terminal(output) = &output
    {
        lashlang_execution_trace.emit_finished(output);
    }
    Ok(output)
}

async fn execute_lashlang(
    compiled: Arc<lashlang::CompiledProgram>,
    state: &mut lashlang::State,
    env: &lashlang::ExecutionEnvironment<'_, LashlangProcessHost<'_>>,
    cancellation: CancellationToken,
    controller: &dyn lash_core::RuntimeEffectController,
    host: &LashlangProcessHost<'_>,
    segment: (Option<LashlangSegmentState>, String),
) -> lash_core::ProcessRunOutcome {
    let (segment_state, program_hash) = segment;
    let mut vm = if let Some(segment_state) = segment_state {
        match lashlang::Vm::resume_from(segment_state.vm, compiled.as_ref(), env) {
            Ok(vm) => vm,
            Err(err) => {
                let exhausted = err.is_execution_bound_exhausted();
                #[cfg(any(test, feature = "testing"))]
                assert!(
                    !EXECUTION_BOUND_EXHAUSTION_LOUD.load(Ordering::SeqCst) || !exhausted,
                    "confidence durable process exhausted a required Lashlang bound: {err}"
                );
                return process_lashlang_failure(
                    if exhausted {
                        LashlangProcessFailureCode::ProcessExecutionBoundExhausted
                    } else {
                        LashlangProcessFailureCode::ProcessSegmentResumeFailed
                    },
                    format!("failed to resume lashlang segment: {err}"),
                    None,
                )
                .into();
            }
        }
    } else {
        lashlang::Vm::from_state(compiled.as_ref(), state, env)
    };
    let mut progress = lash_core::SegmentProgress::default();
    loop {
        let execution = if env.trace_runtime_errors() {
            tokio::select! {
                _ = cancellation.cancelled() => {
                    return process_lashlang_cancelled("lashlang process was cancelled").into();
                }
                result = vm.run_process_traced_until_effect() => {
                    result.map_err(|failure| {
                        let error = failure.error.clone();
                        env.observe_runtime_failure(failure);
                        error
                    })
                }
            }
        } else {
            tokio::select! {
                _ = cancellation.cancelled() => {
                    return process_lashlang_cancelled("lashlang process was cancelled").into();
                }
                result = vm.run_process_until_effect() => result,
            }
        };
        if cancellation.is_cancelled() {
            return process_lashlang_cancelled("lashlang process was cancelled").into();
        }
        match execution {
            Ok(lashlang::VmRunOutcome::Complete(output)) => {
                vm.flush_profile(compiled.as_ref(), env);
                return process_lashlang_execution_result(Ok(output)).into();
            }
            Err(err) => {
                vm.flush_profile(compiled.as_ref(), env);
                return process_lashlang_execution_result(Err(err)).into();
            }
            Ok(lashlang::VmRunOutcome::EffectCompleted) => {
                progress.effects_executed += 1;
                let Some(reason) = controller.wants_segment_boundary(&progress) else {
                    continue;
                };
                match vm.suspend() {
                    Ok(continuation) => {
                        let segment_state = LashlangSegmentState {
                            vm: continuation,
                            sleep_sequence: host.sleep_sequence.load(Ordering::Relaxed),
                            event_sequence: host.event_sequence.load(Ordering::Relaxed),
                            signal_send_sequence: host.signal_send_sequence.load(Ordering::Relaxed),
                            signal_wait_ordinals: host.signal_wait_ordinals.lock().await.clone(),
                        };
                        match serde_json::to_vec(&segment_state) {
                            Ok(engine_state) => {
                                return lash_core::ProcessRunOutcome::SegmentBoundary(
                                    lash_core::SegmentHandover {
                                        reason,
                                        program_hash: Some(program_hash.clone()),
                                        engine_state,
                                    },
                                );
                            }
                            Err(err) => {
                                record_segment_boundary_decline(
                                    &err,
                                    "lashlang segment continuation was not serializable; continuing",
                                );
                            }
                        }
                    }
                    Err(err) => {
                        record_segment_boundary_decline(
                            &err,
                            "lashlang segment boundary declined at non-capturable point",
                        );
                    }
                }
            }
        }
    }
}

struct LashlangProcessHost<'run> {
    ctx: lash_core::RuntimeExecutionContext<'run>,
    host_environment: lashlang::LashlangHostEnvironment,
    artifact_store: Arc<dyn lashlang::LashlangArtifactStore>,
    processes: lash_core::facade_support::ProcessEngineProcessContext,
    process_id: String,
    lashlang_execution_trace: LashlangProcessExecutionTrace,
    sleep_sequence: AtomicU64,
    event_sequence: AtomicU64,
    signal_send_sequence: AtomicU64,
    signal_wait_ordinals: tokio::sync::Mutex<BTreeMap<String, u64>>,
}

type ProcessHostAbilityFuture<'a> =
    Pin<Box<dyn Future<Output = Result<lashlang::AbilityResult, ExecutionHostError>> + Send + 'a>>;

impl LashlangProcessHost<'_> {
    fn resource_payload(
        &self,
        args: &[lashlang::Value],
    ) -> Result<serde_json::Value, ExecutionHostError> {
        let mut payload = if let [lashlang::Value::Record(record)] = args {
            lashlang_value_to_json(&lashlang::Value::Record(Arc::clone(record)))?
        } else {
            serde_json::json!({
                "args": args
                    .iter()
                    .map(lashlang_value_to_json)
                    .collect::<Result<Vec<_>, _>>()?,
            })
        };
        payload
            .as_object_mut()
            .ok_or_else(|| ExecutionHostError::from(LashlangHostError::ModulePayloadNotObject))?;
        Ok(payload)
    }

    fn resource_tool_call_id(
        &self,
        host_operation: &str,
        call_site: &lashlang::LashlangExecutionCallSite,
        batch_index: Option<usize>,
    ) -> String {
        let mut call_id = format!(
            "lashlang:{}:resource:{}:{}:{}",
            self.process_id, host_operation, call_site.site.node_id, call_site.occurrence
        );
        if let Some(batch_index) = batch_index {
            call_id.push_str(&format!(":child:{batch_index}"));
        }
        call_id
    }

    fn prepare_resource_invocation(
        &self,
        operation: String,
        receiver: lashlang::Value,
        args: Vec<lashlang::Value>,
        call_site: Option<lashlang::LashlangExecutionCallSite>,
        batch_index: Option<usize>,
    ) -> Result<(String, lash_core::facade_support::ToolInvocation), ExecutionHostError> {
        let receiver = match &receiver {
            lashlang::Value::Resource(receiver) => receiver,
            _ => {
                return Err(LashlangHostError::ModuleAuthorityRequired { operation }.into());
            }
        };
        let host_operation =
            resolve_lashlang_module_operation(&self.host_environment, receiver, &operation)?;
        let tool_id = lash_core::ToolId::from(host_operation.as_str());
        let manifest = self
            .ctx
            .callable_tool_manifest_by_id(&tool_id)
            .ok_or_else(|| {
                ExecutionHostError::from(LashlangHostError::ResolvedOperationUnavailable {
                    operation: operation.clone(),
                    host_operation: host_operation.clone(),
                })
            })?;
        let payload = self.resource_payload(&args)?;
        let call_site = call_site.ok_or_else(|| {
            ExecutionHostError::from(LashlangHostError::OperationCallSiteMissing {
                operation,
                host_operation: host_operation.clone(),
            })
        })?;
        let call_id = self.resource_tool_call_id(&host_operation, &call_site, batch_index);
        let mut invocation =
            lash_core::facade_support::ToolInvocation::new(call_id, manifest.id.clone(), payload);
        if let Some(hook) = self
            .lashlang_execution_trace
            .tool_child_execution_trace_hook(call_site)
        {
            invocation = invocation.with_child_execution_trace_hook(hook);
        }
        Ok((host_operation, invocation))
    }

    async fn resource_operation(
        &self,
        operation: String,
        receiver: lashlang::Value,
        args: Vec<lashlang::Value>,
        call_site: Option<lashlang::LashlangExecutionCallSite>,
    ) -> Result<lashlang::Value, ExecutionHostError> {
        let (_, invocation) =
            self.prepare_resource_invocation(operation, receiver, args, call_site, None)?;
        let lash_core::facade_support::ToolInvocation {
            id,
            tool_id,
            args,
            execution_grant: _,
            child_execution_trace_hook,
        } = invocation;
        let reply = if let Some(call_site) = child_execution_trace_hook {
            self.ctx
                .call_tool_by_id_with_child_execution_trace_hook(id, tool_id, args, 0, call_site)
                .await
        } else {
            self.ctx.call_tool_by_id(id, tool_id, args, 0).await
        };
        protocol_tool_reply_to_lashlang_value(reply)
    }

    async fn resource_operation_batch(
        &self,
        batch: lashlang::ResourceOperationBatch,
    ) -> lashlang::ResourceOperationBatchResult {
        let mut results = vec![None; batch.operations.len()];
        let mut positions = Vec::new();
        let mut invocations = Vec::new();
        for (index, operation) in batch.operations.into_iter().enumerate() {
            match self.prepare_resource_invocation(
                operation.operation,
                operation.receiver,
                operation.args,
                operation.call_site,
                Some(index),
            ) {
                Ok((_, invocation)) => {
                    positions.push(index);
                    invocations.push(invocation);
                }
                Err(error) => {
                    results[index] = Some(lashlang::ResourceOperationResult::Error(error));
                }
            }
        }

        for (index, reply) in positions
            .into_iter()
            .zip(self.ctx.call_tool_batch(invocations).await)
        {
            results[index] = Some(lashlang::ResourceOperationResult::from_result(
                protocol_tool_reply_to_lashlang_value(reply),
            ));
        }

        lashlang::ResourceOperationBatchResult {
            results: results
                .into_iter()
                .map(|result| result.expect("every batch result slot should be filled"))
                .collect(),
        }
    }

    async fn await_handle(
        &self,
        handle: lashlang::Value,
    ) -> Result<lashlang::Value, ExecutionHostError> {
        let reply = {
            let _phase = self.ctx.named_phase("rlm_process.await_handle");
            self.ctx
                .await_tool_handle(
                    uuid::Uuid::new_v4().to_string(),
                    lashlang_value_to_json(&handle)?,
                )
                .await
        };
        protocol_tool_reply_to_lashlang_value(reply)
    }

    async fn cancel_handle(
        &self,
        handle: lashlang::Value,
    ) -> Result<lashlang::Value, ExecutionHostError> {
        let reply = self
            .ctx
            .cancel_tool_handle(
                uuid::Uuid::new_v4().to_string(),
                lashlang_value_to_json(&handle)?,
            )
            .await;
        protocol_tool_reply_to_lashlang_value(reply)
    }

    async fn start_process(
        &self,
        start: lashlang::ProcessStart,
    ) -> Result<lashlang::Value, ExecutionHostError> {
        let prepared = {
            let _phase = self.ctx.named_phase("rlm_process.prepare_start");
            let parent_start_seed = format!("parent-process:{}", self.process_id);
            prepare_lashlang_process_start(
                Arc::clone(&self.artifact_store),
                &parent_start_seed,
                start,
            )
            .await
            .map_err(|error| LashlangHostError::PrepareProcessStart {
                message: error.to_string(),
            })?
        };
        let reply = {
            let _phase = self.ctx.named_phase("rlm_process.start");
            self.ctx
                .start_child_process(prepared.registration, LASHLANG_ENGINE_KIND, prepared.label)
                .await
        };
        protocol_tool_reply_to_lashlang_value(reply)
    }

    async fn process_event(&self, event: lashlang::ProcessEvent) -> Result<(), ExecutionHostError> {
        let event_type = match event.kind {
            lashlang::ProcessEventKind::Yield => "process.yield",
            lashlang::ProcessEventKind::Wake => "process.wake",
        };
        let ordinal = self.event_sequence.fetch_add(1, Ordering::Relaxed);
        self.ctx
            .append_process_event(
                lash_core::ProcessEventAppendRequest::new(
                    event_type,
                    process_event_payload(&event.value)?,
                )
                .with_replay_key(format!("process:{}:event:{ordinal}", self.process_id)),
            )
            .await
            .map_err(|error| LashlangHostError::AppendProcessEvent {
                message: error.to_string(),
            })?;
        Ok(())
    }

    async fn sleep(&self, sleep: lashlang::Sleep) -> Result<lashlang::Value, ExecutionHostError> {
        let duration_ms = sleep_duration_ms(sleep.kind, &sleep.value)?;
        let sequence = self.sleep_sequence.fetch_add(1, Ordering::Relaxed);
        let scope = format!("process:{}", self.process_id);
        self.ctx
            .sleep_process(&scope, sequence, duration_ms)
            .await
            .map_err(|error| LashlangHostError::SleepProcess {
                message: error.to_string(),
            })?;
        Ok(lashlang::Value::Null)
    }

    async fn wait_signal(&self, name: String) -> Result<lashlang::Value, ExecutionHostError> {
        let event_type =
            lash_core::facade_support::process_signal_event_type(&name).map_err(|error| {
                LashlangHostError::ValidateSignalName {
                    message: error.to_string(),
                }
            })?;
        let event_ordinal = {
            let mut ordinals = self.signal_wait_ordinals.lock().await;
            let ordinal = ordinals.entry(name.clone()).or_insert(0);
            *ordinal += 1;
            *ordinal
        };
        let key = lash_core::facade_support::process_signal_wait_key(
            &self.process_id,
            &name,
            event_ordinal,
        );
        let since_ms =
            self.wait_since_ms(&key)
                .await
                .map_err(|error| LashlangHostError::ReadSignalWait {
                    message: error.to_string(),
                })?;
        let wait = lash_core::WaitState {
            since_ms,
            kind: lash_core::WaitKind::Signal {
                name: name.clone(),
                event_type: event_type.clone(),
                key: key.clone(),
                ordinal: event_ordinal,
            },
        };
        self.processes
            .set_wait(wait.clone())
            .await
            .map_err(|error| LashlangHostError::SetSignalWait {
                message: error.to_string(),
            })?;
        let payload = self
            .ctx
            .await_process_signal_event(&self.process_id, &name, event_ordinal)
            .await
            .map_err(|error| LashlangHostError::AwaitSignal {
                message: error.to_string(),
            })?;
        self.processes
            .clear_wait()
            .await
            .map_err(|error| LashlangHostError::ClearSignalWait {
                message: error.to_string(),
            })?;
        Ok(lashlang::from_json(payload))
    }

    async fn wait_since_ms(&self, key: &str) -> Result<u64, lash_core::PluginError> {
        if let Some(since_ms) = self.processes.record().await?.and_then(|record| {
            let wait = record.wait?;
            match &wait.kind {
                lash_core::WaitKind::Signal { key: wait_key, .. } if wait_key == key => {
                    Some(wait.since_ms)
                }
                _ => None,
            }
        }) {
            return Ok(since_ms);
        }

        for event in self.processes.events_after(0).await?.into_iter().rev() {
            if event.event_type != "process.waiting" {
                continue;
            }
            let Some(wait_value) = event.payload.get("wait") else {
                continue;
            };
            if let Ok(wait) = serde_json::from_value::<lash_core::WaitState>(wait_value.clone())
                && wait.key() == key
            {
                return Ok(wait.since_ms);
            }
        }
        Ok(lash_core::facade_support::current_epoch_ms())
    }

    async fn signal_run(
        &self,
        signal: lashlang::ProcessSignal,
    ) -> Result<lashlang::Value, ExecutionHostError> {
        let target = process_id_from_lashlang_handle(&signal.run)?;
        let payload = lashlang_value_to_json(&signal.payload)?;
        let sequence = self.signal_send_sequence.fetch_add(1, Ordering::Relaxed);
        let signal_id = format!(
            "lashlang:{}:signal.{}:{sequence}",
            self.process_id, signal.name
        );
        self.ctx
            .signal_process_by_id(&target, &signal.name, signal_id, payload)
            .await
            .map_err(|error| LashlangHostError::SignalProcess {
                message: error.to_string(),
            })?;
        Ok(lashlang::Value::Null)
    }

    fn perform_selected_ability<'a>(
        &'a self,
        op: lashlang::AbilityOp,
    ) -> ProcessHostAbilityFuture<'a> {
        match op {
            lashlang::AbilityOp::ResourceOperation(operation) => Box::pin(async move {
                self.resource_operation(
                    operation.operation,
                    operation.receiver,
                    operation.args,
                    operation.call_site,
                )
                .await
                .map(lashlang::AbilityResult::Value)
            }),
            lashlang::AbilityOp::ResourceOperationBatch(batch) => Box::pin(async move {
                Ok(lashlang::AbilityResult::ResourceOperationBatch(
                    self.resource_operation_batch(batch).await,
                ))
            }),
            lashlang::AbilityOp::Await(handle) => Box::pin(async move {
                self.await_handle(handle)
                    .await
                    .map(lashlang::AbilityResult::Value)
            }),
            lashlang::AbilityOp::Cancel(handle) => Box::pin(async move {
                self.cancel_handle(handle)
                    .await
                    .map(lashlang::AbilityResult::Value)
            }),
            lashlang::AbilityOp::StartProcess(start) => Box::pin(async move {
                self.start_process(*start)
                    .await
                    .map(lashlang::AbilityResult::Value)
            }),
            lashlang::AbilityOp::ProcessEvent(event) => Box::pin(async move {
                self.process_event(event).await?;
                Ok(lashlang::AbilityResult::Unit)
            }),
            lashlang::AbilityOp::Sleep(sleep) => {
                Box::pin(async move { self.sleep(sleep).await.map(lashlang::AbilityResult::Value) })
            }
            lashlang::AbilityOp::WaitSignal { name } => Box::pin(async move {
                self.wait_signal(name)
                    .await
                    .map(lashlang::AbilityResult::Value)
            }),
            lashlang::AbilityOp::SignalRun(signal) => Box::pin(async move {
                self.signal_run(signal)
                    .await
                    .map(lashlang::AbilityResult::Value)
            }),
            lashlang::AbilityOp::Print(_) => {
                Box::pin(async { Err(LashlangHostError::PrintUnavailable.into()) })
            }
            lashlang::AbilityOp::Finish(value) | lashlang::AbilityOp::Fail(value) => {
                Box::pin(async move { Ok(lashlang::AbilityResult::Value(value)) })
            }
        }
    }
}

impl lashlang::ExecutionHost for LashlangProcessHost<'_> {
    fn perform(
        &self,
        op: lashlang::AbilityOp,
    ) -> impl Future<Output = Result<lashlang::AbilityResult, ExecutionHostError>> + Send {
        self.perform_selected_ability(op)
    }

    fn observe_lashlang_execution(&self, observation: lashlang::LashlangExecutionObservation) {
        self.lashlang_execution_trace.emit_observation(observation);
    }
}

#[derive(Clone)]
struct LashlangProcessExecutionTrace {
    sink: Option<Arc<dyn TraceSink>>,
    base_context: TraceContext,
    session_id: String,
    process_id: String,
    module_ref: lashlang::ModuleRef,
    process_ref: lashlang::ProcessRef,
    process_name: String,
}

impl LashlangProcessExecutionTrace {
    fn new(
        sink: Option<Arc<dyn TraceSink>>,
        base_context: TraceContext,
        session_id: String,
        process_id: String,
        module_ref: lashlang::ModuleRef,
        process_ref: lashlang::ProcessRef,
        process_name: String,
    ) -> Self {
        Self {
            sink,
            base_context,
            session_id,
            process_id,
            module_ref,
            process_ref,
            process_name,
        }
    }

    fn identity(&self) -> TraceLashlangExecutionIdentity {
        TraceLashlangExecutionIdentity {
            scope: TraceRuntimeScope::new(self.session_id.clone()),
            subject: TraceRuntimeSubject::Process {
                process_id: self.process_id.clone(),
            },
            module_ref: self.module_ref.to_string(),
            entry_kind: "process".to_string(),
            entry_ref: Some(lashlang::process_ref_key(&self.process_ref)),
            entry_name: self.process_name.clone(),
        }
    }

    fn event_key(&self, suffix: impl std::fmt::Display) -> String {
        format!("lashlang_execution:{}:{suffix}", self.process_id)
    }

    fn emit_started(&self, artifact: &lashlang::ModuleArtifact) {
        self.emit(TraceLashlangExecutionEvent::ExecutionStarted {
            event_key: self.event_key("started"),
            identity: self.identity(),
            execution_map: trace_lashlang_process_map(
                artifact,
                &self.process_ref,
                &self.process_name,
            ),
        });
    }

    fn emit_finished(&self, output: &lash_core::ProcessAwaitOutput) {
        let (status, error) = match output {
            lash_core::ProcessAwaitOutput::Success { .. } => (TraceLashlangStatus::Completed, None),
            lash_core::ProcessAwaitOutput::Failure { message, .. } => {
                (TraceLashlangStatus::Failed, Some(message.clone()))
            }
            lash_core::ProcessAwaitOutput::Cancelled { message, .. } => {
                (TraceLashlangStatus::Cancelled, Some(message.clone()))
            }
            // `emit_finished` fires after an actual execution, whose outcome is
            // Success/Failure/Cancelled — abandonment is written out-of-band by the
            // sweep, never returned by a run. Map it defensively to Failed.
            lash_core::ProcessAwaitOutput::Abandoned { .. } => (
                TraceLashlangStatus::Failed,
                Some("process abandoned".to_string()),
            ),
            lash_core::ProcessAwaitOutput::NoLongerRetained { terminal_label, .. } => (
                TraceLashlangStatus::Failed,
                Some(format!("process no longer retained ({terminal_label})")),
            ),
        };
        self.emit(TraceLashlangExecutionEvent::ExecutionFinished {
            event_key: self.event_key("finished"),
            identity: self.identity(),
            status,
            error,
        });
    }

    fn emit_observation(&self, observation: lashlang::LashlangExecutionObservation) {
        if self.sink.is_none() {
            return;
        }
        let identity = self.identity();
        let event = match observation {
            lashlang::LashlangExecutionObservation::NodeStarted { site, occurrence } => {
                TraceLashlangExecutionEvent::NodeStarted {
                    event_key: self
                        .event_key(format!("node:{}:{occurrence}:started", site.node_id)),
                    identity,
                    node_id: site.node_id,
                    node_kind: site.node_kind,
                    label: site.label,
                    occurrence,
                }
            }
            lashlang::LashlangExecutionObservation::NodeCompleted { site, occurrence } => {
                TraceLashlangExecutionEvent::NodeCompleted {
                    event_key: self
                        .event_key(format!("node:{}:{occurrence}:completed", site.node_id)),
                    identity,
                    node_id: site.node_id,
                    node_kind: site.node_kind,
                    label: site.label,
                    occurrence,
                }
            }
            lashlang::LashlangExecutionObservation::NodeFailed {
                site,
                occurrence,
                error,
            } => TraceLashlangExecutionEvent::NodeFailed {
                event_key: self.event_key(format!("node:{}:{occurrence}:failed", site.node_id)),
                identity,
                node_id: site.node_id,
                node_kind: site.node_kind,
                label: site.label,
                occurrence,
                error,
            },
            lashlang::LashlangExecutionObservation::BranchSelected {
                site,
                occurrence,
                edge_id,
                selected,
            } => TraceLashlangExecutionEvent::BranchSelected {
                event_key: self
                    .event_key(format!("branch:{}:{occurrence}:{edge_id}", site.node_id)),
                identity,
                node_id: site.node_id,
                occurrence,
                edge_id,
                selected: match selected {
                    lashlang::ProcessBranchSelection::Then => TraceBranchSelection::Then,
                    lashlang::ProcessBranchSelection::Else => TraceBranchSelection::Else,
                },
            },
            lashlang::LashlangExecutionObservation::ChildStarted {
                site,
                occurrence,
                child,
            } => TraceLashlangExecutionEvent::ChildStarted {
                event_key: self.event_key(format!(
                    "child:{}:{occurrence}:{}",
                    site.node_id, child.process_id
                )),
                identity,
                parent_node_id: site.node_id,
                occurrence,
                child: TraceLashlangChildExecution {
                    scope: TraceRuntimeScope::new(self.session_id.clone()),
                    subject: TraceRuntimeSubject::Process {
                        process_id: child.process_id,
                    },
                    module_ref: Some(child.module_ref.to_string()),
                    entry_ref: Some(lashlang::process_ref_key(&child.process_ref)),
                    entry_name: Some(child.process_name),
                },
            },
        };
        self.emit(event);
    }

    fn tool_child_execution_trace_hook(
        &self,
        call_site: lashlang::LashlangExecutionCallSite,
    ) -> Option<ToolChildExecutionTraceHook> {
        self.sink.as_ref()?;
        let trace = self.clone();
        let parent_node_id = call_site.site.node_id;
        let occurrence = call_site.occurrence;
        Some(ToolChildExecutionTraceHook::new(move |started| {
            let child = TraceLashlangChildExecution {
                scope: TraceRuntimeScope::new(trace.session_id.clone()),
                subject: TraceRuntimeSubject::Process {
                    process_id: started.process_id,
                },
                module_ref: None,
                entry_ref: None,
                entry_name: started.child_entry_name,
            };
            let child_graph_key = child.graph_key();
            trace.emit(TraceLashlangExecutionEvent::ChildStarted {
                event_key: trace.event_key(format!(
                    "child:{parent_node_id}:{occurrence}:{child_graph_key}"
                )),
                identity: trace.identity(),
                parent_node_id: parent_node_id.clone(),
                occurrence,
                child,
            });
        }))
    }

    fn emit(&self, event: TraceLashlangExecutionEvent) {
        let Some(sink) = &self.sink else {
            return;
        };
        let mut context = self.base_context.clone();
        context.session_id = Some(self.session_id.clone());
        let _ = sink.append(&TraceRecord::new(
            context,
            TraceEvent::LashlangExecution { event },
        ));
    }
}

fn trace_lashlang_process_map(
    artifact: &lashlang::ModuleArtifact,
    process_ref: &lashlang::ProcessRef,
    process_name: &str,
) -> TraceLashlangMap {
    let source = lashlang::canonical_program_source_with_requirements(
        &artifact.canonical_ir,
        &artifact.host_requirements,
    );
    let graph = source
        .ok()
        .and_then(|source| lashlang::workflow_graph_from_source(&source).ok());
    let Some(process) = graph.as_ref().and_then(|graph| graph.process(process_name)) else {
        return TraceLashlangMap {
            module_ref: artifact.module_ref.to_string(),
            entry_kind: "process".to_string(),
            entry_ref: Some(lashlang::process_ref_key(process_ref)),
            entry_name: process_name.to_string(),
            nodes: Vec::new(),
            edges: Vec::new(),
        };
    };
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut primary_runtime_ids = BTreeMap::new();
    append_trace_workflow_subgraph(
        artifact,
        &process.body,
        &mut nodes,
        &mut edges,
        &mut primary_runtime_ids,
    );
    TraceLashlangMap {
        module_ref: artifact.module_ref.to_string(),
        entry_kind: "process".to_string(),
        entry_ref: Some(lashlang::process_ref_key(process_ref)),
        entry_name: process_name.to_string(),
        nodes,
        edges,
    }
}

/// Builds the trace runtime's read-only foreground skeleton from the workflow graph.
pub fn trace_lashlang_main_map(artifact: &lashlang::ModuleArtifact) -> TraceLashlangMap {
    let graph = lashlang::canonical_program_source_with_requirements(
        &artifact.canonical_ir,
        &artifact.host_requirements,
    )
    .ok()
    .and_then(|source| lashlang::workflow_graph_from_source(&source).ok());
    let Some(graph) = graph else {
        return TraceLashlangMap {
            module_ref: artifact.module_ref.to_string(),
            entry_kind: "main".to_string(),
            entry_ref: None,
            entry_name: "main".to_string(),
            nodes: Vec::new(),
            edges: Vec::new(),
        };
    };
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut primary_runtime_ids = BTreeMap::new();
    append_trace_workflow_subgraph(
        artifact,
        &graph.main,
        &mut nodes,
        &mut edges,
        &mut primary_runtime_ids,
    );
    TraceLashlangMap {
        module_ref: artifact.module_ref.to_string(),
        entry_kind: "main".to_string(),
        entry_ref: None,
        entry_name: "main".to_string(),
        nodes,
        edges,
    }
}

fn append_trace_workflow_subgraph(
    artifact: &lashlang::ModuleArtifact,
    graph: &lashlang::WorkflowSubgraph,
    nodes: &mut Vec<TraceLashlangMapNode>,
    edges: &mut Vec<TraceLashlangMapEdge>,
    primary_runtime_ids: &mut BTreeMap<String, String>,
) {
    for node in &graph.nodes {
        let label_metadata =
            (node.name_source == lashlang::WorkflowNodeNameSource::Label).then(|| {
                TraceLabelMetadata {
                    title: node.name.clone(),
                    description: node.description.clone(),
                }
            });
        for site in &node.execution_sites {
            let Some(runtime_site) =
                lashlang::runtime_execution_site_for_workflow_site(artifact, site)
            else {
                continue;
            };
            primary_runtime_ids
                .entry(node.id.to_string())
                .or_insert_with(|| runtime_site.node_id.clone());
            if nodes.iter().any(|node| node.id == runtime_site.node_id) {
                continue;
            }
            nodes.push(TraceLashlangMapNode {
                id: runtime_site.node_id,
                kind: runtime_site.node_kind,
                label: runtime_site.label,
                label_metadata: label_metadata.clone(),
            });
        }
        match &node.kind {
            lashlang::WorkflowNodeKind::Container(lashlang::WorkflowContainer::If {
                then_graph,
                else_graph,
                ..
            }) => {
                if let Some(graph) = then_graph {
                    append_trace_workflow_subgraph(
                        artifact,
                        graph,
                        nodes,
                        edges,
                        primary_runtime_ids,
                    );
                }
                if let Some(graph) = else_graph {
                    append_trace_workflow_subgraph(
                        artifact,
                        graph,
                        nodes,
                        edges,
                        primary_runtime_ids,
                    );
                }
            }
            lashlang::WorkflowNodeKind::Container(lashlang::WorkflowContainer::For {
                body: Some(graph),
                ..
            }) => {
                append_trace_workflow_subgraph(artifact, graph, nodes, edges, primary_runtime_ids);
            }
            lashlang::WorkflowNodeKind::Container(lashlang::WorkflowContainer::While {
                body: Some(graph),
                ..
            }) => {
                append_trace_workflow_subgraph(artifact, graph, nodes, edges, primary_runtime_ids);
            }
            lashlang::WorkflowNodeKind::Container(
                lashlang::WorkflowContainer::ListComprehension {
                    element: Some(graph),
                    ..
                },
            ) => {
                append_trace_workflow_subgraph(artifact, graph, nodes, edges, primary_runtime_ids);
            }
            _ => {}
        }
    }
    for edge in &graph.edges {
        let (Some(from), Some(to)) = (
            primary_runtime_ids.get(edge.from.as_str()),
            primary_runtime_ids.get(edge.to.as_str()),
        ) else {
            continue;
        };
        let label = match &edge.kind {
            lashlang::WorkflowEdgeKind::Sequence => "sequence".to_string(),
            lashlang::WorkflowEdgeKind::DataDependency { variable, version } => {
                format!("{variable}@{version}")
            }
        };
        edges.push(TraceLashlangMapEdge {
            id: edge.id.clone(),
            from: from.clone(),
            to: to.clone(),
            label,
        });
    }
}

fn process_lashlang_execution_result(
    result: Result<lashlang::ExecutionOutcome, lashlang::RuntimeError>,
) -> lash_core::ProcessAwaitOutput {
    match result {
        Ok(lashlang::ExecutionOutcome::Finished(value)) => lash_core::ProcessAwaitOutput::Success {
            value: lashlang_value_to_json(&value)
                .unwrap_or_else(|err| serde_json::json!({ "error": err.to_string() })),
            control: None,
        },
        Ok(lashlang::ExecutionOutcome::Failed(value)) => process_lashlang_failure(
            LashlangProcessFailureCode::ProcessFailed,
            value.to_string(),
            Some(
                lashlang_value_to_json(&value)
                    .unwrap_or_else(|err| serde_json::json!({ "error": err.to_string() })),
            ),
        ),
        Ok(lashlang::ExecutionOutcome::Continued) => lash_core::ProcessAwaitOutput::Success {
            value: serde_json::Value::Null,
            control: None,
        },
        Err(err) => {
            let exhausted = err.is_execution_bound_exhausted();
            #[cfg(any(test, feature = "testing"))]
            assert!(
                !EXECUTION_BOUND_EXHAUSTION_LOUD.load(Ordering::SeqCst) || !exhausted,
                "confidence durable process exhausted a required Lashlang bound: {err}"
            );
            process_lashlang_failure(
                if exhausted {
                    LashlangProcessFailureCode::ProcessExecutionBoundExhausted
                } else {
                    LashlangProcessFailureCode::ProcessRuntimeError
                },
                err.to_string(),
                None,
            )
        }
    }
}

fn process_lashlang_failure(
    code: LashlangProcessFailureCode,
    message: impl Into<String>,
    raw: Option<serde_json::Value>,
) -> lash_core::ProcessAwaitOutput {
    lash_core::ProcessAwaitOutput::Failure {
        class: lash_core::ToolFailureClass::Execution,
        code: code.as_str().to_string(),
        message: message.into(),
        raw,
        control: None,
    }
}

fn process_lashlang_cancelled(message: impl Into<String>) -> lash_core::ProcessAwaitOutput {
    lash_core::ProcessAwaitOutput::Cancelled {
        message: message.into(),
        raw: None,
        control: None,
    }
}

fn process_id_from_lashlang_handle(handle: &lashlang::Value) -> Result<String, ExecutionHostError> {
    let value = lashlang_value_to_json(handle)?;
    let Some(object) = value.as_object() else {
        return Err(LashlangHostError::InvalidProcessHandle.into());
    };
    if object.get("__handle__").and_then(serde_json::Value::as_str) != Some("process") {
        return Err(LashlangHostError::InvalidProcessHandle.into());
    }
    object
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| LashlangHostError::ProcessHandleMissingId.into())
}

pub fn lashlang_process_event_types() -> Vec<lash_core::ProcessEventType> {
    vec![
        lash_core::ProcessEventType {
            name: "process.yield".to_string(),
            payload_schema: lash_core::LashSchema::any(),
            semantics: lash_core::ProcessEventSemanticsSpec::default(),
        },
        lash_core::ProcessEventType {
            name: "process.wake".to_string(),
            payload_schema: lash_core::LashSchema::any(),
            semantics: lash_core::ProcessEventSemanticsSpec {
                wake: Some(lash_core::ProcessWakeSpec {
                    when: None,
                    input: lash_core::ProcessValueSelector::Pointer("/text".to_string()),
                }),
                ..lash_core::ProcessEventSemanticsSpec::default()
            },
        },
    ]
}

pub fn lashlang_process_signal_event_types(
    process: &lashlang::ProcessDecl,
) -> Vec<lash_core::ProcessEventType> {
    process
        .signals
        .iter()
        .map(|signal| lash_core::ProcessEventType {
            name: lash_core::facade_support::process_signal_event_type(signal.name.as_str())
                .expect("lashlang process signal declarations use parser-validated names"),
            payload_schema: lash_core::LashSchema::new(lashlang_type_expr_schema(&signal.ty)),
            semantics: lash_core::ProcessEventSemanticsSpec::default(),
        })
        .collect()
}

pub fn lashlang_type_expr_schema(ty: &lashlang::TypeExpr) -> serde_json::Value {
    match ty {
        lashlang::TypeExpr::Any
        | lashlang::TypeExpr::Dict
        | lashlang::TypeExpr::Ref(_)
        | lashlang::TypeExpr::Process { .. }
        | lashlang::TypeExpr::TriggerHandle(_) => serde_json::json!({}),
        lashlang::TypeExpr::Str => serde_json::json!({ "type": "string" }),
        lashlang::TypeExpr::Int => serde_json::json!({ "type": "integer" }),
        lashlang::TypeExpr::Float => serde_json::json!({ "type": "number" }),
        lashlang::TypeExpr::Bool => serde_json::json!({ "type": "boolean" }),
        lashlang::TypeExpr::Null => serde_json::json!({ "type": "null" }),
        lashlang::TypeExpr::Enum(values) => serde_json::json!({
            "enum": values.iter().map(|value| value.as_str()).collect::<Vec<_>>()
        }),
        lashlang::TypeExpr::List(item) => serde_json::json!({
            "type": "array",
            "items": lashlang_type_expr_schema(item),
        }),
        lashlang::TypeExpr::Object(fields) => {
            let mut properties = serde_json::Map::new();
            let mut required = Vec::new();
            for field in fields {
                properties.insert(field.name.to_string(), lashlang_type_expr_schema(&field.ty));
                if !field.optional {
                    required.push(serde_json::Value::String(field.name.to_string()));
                }
            }
            let mut schema = serde_json::Map::new();
            schema.insert(
                "type".to_string(),
                serde_json::Value::String("object".to_string()),
            );
            schema.insert(
                "properties".to_string(),
                serde_json::Value::Object(properties),
            );
            if !required.is_empty() {
                schema.insert("required".to_string(), serde_json::Value::Array(required));
            }
            schema.insert(
                "additionalProperties".to_string(),
                serde_json::Value::Bool(true),
            );
            serde_json::Value::Object(schema)
        }
        lashlang::TypeExpr::Union(variants) => serde_json::json!({
            "anyOf": variants.iter().map(lashlang_type_expr_schema).collect::<Vec<_>>()
        }),
    }
}

#[cfg(test)]
mod segment_trace_tests {
    use super::{
        EXECUTION_BOUND_EXHAUSTION_LOUD, SEGMENT_BOUNDARY_DECLINED_TOTAL,
        process_lashlang_execution_result, record_segment_boundary_decline,
        trace_lifecycle_for_segment, validate_lashlang_program_hash,
    };
    use std::sync::atomic::Ordering;

    #[test]
    fn multi_segment_trace_emits_one_started_and_one_finished() {
        let boundary = || {
            lash_core::ProcessRunOutcome::SegmentBoundary(lash_core::SegmentHandover {
                reason: lash_core::BoundaryReason::JournalBudget,
                program_hash: Some("program-v1".to_string()),
                engine_state: Vec::new(),
            })
        };
        let terminal = lash_core::ProcessRunOutcome::from(lash_core::ProcessAwaitOutput::Success {
            value: serde_json::Value::Null,
            control: None,
        });
        let lifecycle = [
            trace_lifecycle_for_segment(true, &boundary()),
            trace_lifecycle_for_segment(false, &boundary()),
            trace_lifecycle_for_segment(false, &terminal),
        ];
        assert_eq!(lifecycle.iter().filter(|(started, _)| *started).count(), 1);
        assert_eq!(
            lifecycle.iter().filter(|(_, finished)| *finished).count(),
            1
        );
    }

    #[test]
    fn resume_rejects_changed_bytecode_program_hash_with_typed_failure() {
        let output = validate_lashlang_program_hash(Some("sha256:old"), "sha256:current")
            .expect_err("changed bytecode identity must fail closed");
        assert!(matches!(
            *output,
            lash_core::ProcessAwaitOutput::Failure { code, .. }
                if code == "restate_segment_program_hash_mismatch"
        ));
    }

    #[test]
    fn declined_boundary_is_warned_and_counted() {
        let before = SEGMENT_BOUNDARY_DECLINED_TOTAL.load(Ordering::Relaxed);
        record_segment_boundary_decline(&"projected state", "test boundary decline");
        assert_eq!(
            SEGMENT_BOUNDARY_DECLINED_TOTAL.load(Ordering::Relaxed),
            before + 1
        );
    }

    #[test]
    fn durable_exhaustion_has_a_typed_process_failure_surface() {
        let previous = EXECUTION_BOUND_EXHAUSTION_LOUD.swap(false, Ordering::SeqCst);
        let output = process_lashlang_execution_result(Err(
            lashlang::RuntimeError::InstructionBudgetExceeded { limit: 1 },
        ));
        EXECUTION_BOUND_EXHAUSTION_LOUD.store(previous, Ordering::SeqCst);
        assert!(matches!(
            output,
            lash_core::ProcessAwaitOutput::Failure { code, .. }
                if code == "process_execution_bound_exhausted"
        ));
    }
}
