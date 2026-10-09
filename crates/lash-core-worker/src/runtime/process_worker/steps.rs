//! A durable deployment's process steps (ADR 0132 §10; D-L7b1).
//!
//! A step names a catalog tool or one of its engine's own bodies:
//!
//! - **Tool steps** resolve against the process's own catalog, which its
//!   runtime builds from the environment its start captured. The tool's
//!   declaration pins the step's policy and its limit within the tool
//!   ceiling, and its body runs through the round tools a turn's round runs
//!   on, owned by the process. The activation's first tool step builds
//!   those tools ([`StepRuntime::tools`]) and its later steps share them, so
//!   each reduces against the process's committed plugin state. A
//!   store-local effect commits with the step's outcome. The step's payload
//!   retains the call's `ToolCallRecord`; incorporation projects its output
//!   for the engine.
//! - **Engine steps** run through the [`EngineSteps`](crate::EngineSteps)
//!   the registration of the process's engine declares, under the pinned
//!   `Repeatable` policy. A registration that declares none, or not this
//!   kind, refuses the step before admission.

use std::sync::Arc;

use lash_core_execution::runtime::actor::round::MemberPin;
use lash_core_execution::runtime::actor::round::{
    AdmittedExecution, Material, MemberBody, MemberResult, SettledOutput,
};
use lash_core_execution::runtime::actor::waits::Resolution;
use lash_core_execution::runtime::process::steps::{
    ProcessSteps, StepAdmission, StepRefusal, StepRuntime, tool_step_output, tool_step_resolved,
    tool_step_settled,
};
use lash_core_execution::tool_run::CompletionSource;
use lash_core_execution::{
    ActorContext, EngineStepRun, ExecutionLimit, MaxToolCalls, ProcessInput, ProcessRecord,
    StepRequest, ToolCatalog,
};

use super::DurableProcessWorker;

/// The engine a process runs, which its engine steps belong to.
fn engine_kind(process: &ProcessRecord) -> Option<&str> {
    match process.input.as_ref() {
        ProcessInput::Engine { kind, .. } => Some(kind.as_str()),
        ProcessInput::SessionTurn { .. } => None,
    }
}

impl DurableProcessWorker {
    /// The catalog `process`'s steps resolve against: its runtime's, or none
    /// when its start captured no environment.
    async fn step_catalog(
        &self,
        process: &ProcessRecord,
    ) -> Result<Arc<ToolCatalog>, crate::PluginError> {
        if process.env_ref.is_none() {
            return Ok(Arc::new(ToolCatalog::default()));
        }
        self.runtime(process).await?.step_catalog()
    }

    /// The tool step `execution` of `process`, which runs `step` on
    /// `runtime`'s step tools.
    async fn tool_step(
        &self,
        runtime: Arc<StepRuntime>,
        process: ProcessRecord,
        step: StepRequest,
        execution: AdmittedExecution,
        token: tokio_util::sync::CancellationToken,
    ) -> MemberResult {
        let StepRequest::Tool { tool, input, .. } = step else {
            return SettledOutput::Interrupted.into();
        };
        let step = match self.built_step_tools(&runtime, &process).await {
            Ok(step) => step,
            // The tools did not build: the tool never ran, and its
            // admission stands, so the step is answered as one that may
            // have taken effect, never run again under `Once`.
            Err(error) => {
                tracing::warn!(process_id = %process.id, %error, "a process tool step could not reach its tools");
                return SettledOutput::Interrupted.into();
            }
        };
        let name = step
            .catalog
            .tools
            .iter()
            .find(|entry| entry.manifest.id == tool)
            .map_or_else(
                || tool.as_str().to_owned(),
                |entry| entry.manifest.name.clone(),
            );
        let call = lash_core_execution::sansio::PendingToolCall {
            call_id: execution.call().clone(),
            provider_call_id: None,
            tool_name: name,
            args: input,
            replay: None,
        };
        let result = step.tools.body(&call, &execution)(token).await;
        tool_step_output(&process.id, &call, result)
    }

    /// Report that the admitted body of `step` is starting, at the site the
    /// engine issued it from, bound to the call its admission pinned. Only an
    /// admitted execution reaches here: a refused step reports nothing, and
    /// a retried body reports again with the same site and call.
    fn observe_step_body_started(
        &self,
        process: &ProcessRecord,
        step: &StepRequest,
        execution: &AdmittedExecution,
    ) {
        let StepRequest::Tool {
            site: Some(site), ..
        } = step
        else {
            return;
        };
        let tracing = crate::plugin::PluginExecutionTrace::new(
            self.config.runtime_host.tracing.unreplayed(None),
        );
        if !tracing.observes_language() {
            return;
        }
        let started = lash_trace::StepBodyStarted {
            process_id: process.id.clone(),
            at: site.clone(),
            call_id: execution.call().clone(),
            attempt: execution.attempt(),
        };
        let mut context = tracing.trace_runtime().base_context().clone();
        context.graph_node_id = Some(site.site.node_id.to_string());
        tracing.observe_language(&started.event_key(), || {
            (
                context.clone(),
                crate::TraceEvent::StepBodyStarted {
                    step: started.clone(),
                },
            )
        });
    }

    /// The activation's step tools, built on its first step that needs them.
    async fn built_step_tools(
        &self,
        runtime: &Arc<StepRuntime>,
        process: &ProcessRecord,
    ) -> Result<
        Arc<lash_core_execution::runtime::process::ProcessStepTools>,
        lash_core_execution::runtime::process::StepToolsError,
    > {
        runtime
            .tools(|| async {
                self.runtime(process)
                    .await?
                    .step_tools(runtime.cx().clone(), process)
                    .await
            })
            .await
    }
}

/// The process steps a [`DurableProcessWorker`] serves, which a node runs
/// its processes' steps through. A host neither implements nor calls them,
/// so they are not the worker's own trait.
struct WorkerSteps(DurableProcessWorker);

/// The process steps `worker` serves: what a node hands its
/// [`ProcessActivation`](lash_core_execution::runtime::actor::process::ProcessActivation).
pub fn process_steps(worker: &DurableProcessWorker) -> Arc<dyn ProcessSteps> {
    Arc::new(WorkerSteps(worker.clone()))
}

#[lash_core::async_trait]
impl ProcessSteps for WorkerSteps {
    fn stop_grace(&self) -> std::time::Duration {
        self.0
            .config
            .runtime_host
            .control
            .execution_budgets
            .stop_grace()
    }

    async fn max_tool_calls(
        &self,
        process: &ProcessRecord,
    ) -> Result<Option<MaxToolCalls>, StepRefusal> {
        let Some(env_ref) = process.env_ref.as_ref() else {
            return Ok(None);
        };
        let environment = lash_core_execution::runtime::load_process_execution_env(
            self.0
                .config
                .runtime_host
                .durability
                .process_env_store
                .as_ref(),
            env_ref,
        )
        .await
        .map_err(|error| StepRefusal::Unavailable {
            step: String::new(),
            reason: error.to_string(),
        })?;
        Ok(Some(environment.policy.max_tool_calls))
    }

    async fn admit(
        &self,
        process: &ProcessRecord,
        step: &StepRequest,
        now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal> {
        match step {
            StepRequest::Engine { kind, .. } => {
                let engine = engine_kind(process).unwrap_or_default();
                let steps = self
                    .0
                    .config
                    .runtime_host
                    .process_engines
                    .engine_steps(engine, kind)?;
                let total = steps.execution(kind);
                Ok(StepAdmission {
                    policy: steps.retry(kind),
                    limit: ExecutionLimit::starting_at(now_ms, total, total),
                    park: None,
                })
            }
            StepRequest::Tool { step, tool, .. } => {
                let catalog = self.0.step_catalog(process).await.map_err(|error| {
                    StepRefusal::Unavailable {
                        step: step.0.clone(),
                        reason: error.to_string(),
                    }
                })?;
                let manifest = catalog
                    .tools
                    .iter()
                    .find(|entry| entry.manifest.id == *tool)
                    .map(|entry| &entry.manifest)
                    .ok_or_else(|| StepRefusal::UnknownTool {
                        step: step.0.clone(),
                        tool: tool.as_str().to_owned(),
                    })?;
                let bounds = manifest.bounds().map_err(|refusal| StepRefusal::Refused {
                    step: step.0.clone(),
                    reason: refusal.to_string(),
                })?;
                // The body's limit and the park's deadline are pinned
                // separately, as a round member's are.
                let pin = MemberPin::admitted(
                    manifest.id.clone(),
                    manifest.execution_policy,
                    bounds,
                    now_ms,
                );
                Ok(StepAdmission {
                    policy: pin.policy,
                    limit: pin.limit,
                    park: pin.park,
                })
            }
        }
    }

    fn body(
        &self,
        runtime: &Arc<StepRuntime>,
        process: &ProcessRecord,
        step: &StepRequest,
        execution: &AdmittedExecution,
    ) -> MemberBody {
        let worker = self.0.clone();
        let runtime = Arc::clone(runtime);
        let process = process.clone();
        let step = step.clone();
        let execution = execution.clone();
        Box::new(move |token| {
            Box::pin(async move {
                worker.observe_step_body_started(&process, &step, &execution);
                match step {
                    StepRequest::Tool { .. } => {
                        worker
                            .tool_step(runtime, process, step, execution, token)
                            .await
                    }
                    StepRequest::Engine { step, kind, input } => {
                        worker
                            .engine_step(runtime.cx().clone(), process, step, kind, input, token)
                            .await
                    }
                }
            })
        })
    }

    fn engine_output(
        &self,
        process: &lash_core_execution::ProcessId,
        step: &StepRequest,
        output: SettledOutput,
    ) -> SettledOutput {
        match step {
            StepRequest::Tool { .. } => tool_step_settled(process, output),
            StepRequest::Engine { .. } => output,
        }
    }

    /// A parked catalog tool step settles with the tool record its
    /// resolution answers, retaining the request pinned when it parked. An engine step never
    /// parks.
    fn resolved(
        &self,
        process: &ProcessRecord,
        step: &StepRequest,
        _execution: &AdmittedExecution,
        parked: &Material<CompletionSource>,
        resolution: Resolution,
    ) -> SettledOutput {
        let StepRequest::Tool { .. } = step else {
            return SettledOutput::Interrupted;
        };
        tool_step_resolved(&process.id, parked, resolution)
    }
}

impl DurableProcessWorker {
    /// The engine step `step` of `process`, run by its engine's
    /// registration.
    async fn engine_step(
        &self,
        cx: ActorContext,
        process: ProcessRecord,
        step: lash_core_execution::StepName,
        kind: lash_core_execution::EngineStepKind,
        input: serde_json::Value,
        token: tokio_util::sync::CancellationToken,
    ) -> MemberResult {
        let Some(engine) = engine_kind(&process) else {
            return engine_step_failure(
                &process,
                "engine_step_without_engine",
                "an engine step belongs to a process with no engine".to_owned(),
                crate::ToolFailureCause::EngineStepWithoutEngine {
                    step: step.0,
                    step_kind: kind.0,
                },
                None,
            );
        };
        let steps = match self
            .config
            .runtime_host
            .process_engines
            .engine_steps(engine, &kind)
        {
            Ok(steps) => steps,
            // Admitted under a registration this deployment no longer
            // holds: the body cannot run here.
            Err(refusal) => {
                tracing::warn!(process_id = %process.id, %refusal, "an admitted engine step has no body here");
                let code = match &refusal {
                    crate::EngineStepRefusal::UnknownEngine { .. } => "engine_step_engine_missing",
                    crate::EngineStepRefusal::NoEngineSteps { .. } => {
                        "engine_step_registration_missing"
                    }
                    crate::EngineStepRefusal::UndeclaredStep { .. } => "engine_step_undeclared",
                };
                return engine_step_failure(
                    &process,
                    code,
                    refusal.to_string(),
                    crate::ToolFailureCause::EngineStepRegistrationUnavailable {
                        engine: engine.to_owned(),
                        step: step.0,
                        step_kind: kind.0,
                    },
                    Some(serde_json::json!({ "registration_refusal": refusal })),
                );
            }
        };
        let tool_catalog = match self.step_catalog(&process).await {
            Ok(catalog) => catalog,
            Err(error) => {
                tracing::warn!(process_id = %process.id, %error, "an engine step could not read its catalog");
                return engine_step_failure(
                    &process,
                    "engine_step_catalog_unreadable",
                    error.to_string(),
                    crate::ToolFailureCause::EngineStepCatalogUnreadable {
                        engine: engine.to_owned(),
                        step: step.0,
                        step_kind: kind.0,
                    },
                    Some(
                        serde_json::json!({ "catalog_error": crate::ToolIntentCommandFailure::from(&error) }),
                    ),
                );
            }
        };
        let backend = cx.backend().clone();
        let run = EngineStepRun {
            process: process.id.clone(),
            engine_config: process.engine_config.clone(),
            tool_catalog,
            now: cx.now(),
            clock: Arc::clone(cx.clock()),
            projection_providers: Some(Arc::clone(backend.projection_providers())),
            kind,
            input,
        };
        steps.run(run, token).await.into()
    }
}

/// Retain the known pre-body failure as process-owned attempt material.
#[expect(clippy::expect_used, reason = "a tool failure is serializable data")]
fn engine_step_failure(
    process: &ProcessRecord,
    code: &str,
    message: String,
    cause: crate::ToolFailureCause,
    source: Option<serde_json::Value>,
) -> MemberResult {
    use lash_core_execution::tool_run::{KnownFailureReason, MaterialOwner, MaterialRole};
    let mut failure = crate::ToolFailure::runtime(crate::ToolFailureClass::Internal, code, message)
        .with_cause(cause);
    failure.raw = source.map(crate::ToolValue::untrusted_json);
    let output = crate::ToolCallOutput::failure(failure);
    SettledOutput::Failed(
        Material::journal_local(
            MaterialOwner::Process {
                process_id: process.id.clone(),
            },
            MaterialRole::AttemptOutput,
            serde_json::to_string(&output).expect("an engine step failure encodes"),
        )
        .failure(KnownFailureReason::Reported, None),
    )
    .into()
}
