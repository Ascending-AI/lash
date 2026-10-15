//! A durable deployment's process steps (ADR 0132 §10; D-L7b1).
//!
//! A step names a catalog tool or one of its engine's own bodies:
//!
//! - **Tool steps** resolve against the process's own catalog, which its
//!   runtime builds from only the components its environment names and the
//!   tools that environment admits (FIG-6034). The activation builds that
//!   composition and its step tools before any step is admitted or resumed
//!   ([`ProcessSteps::prepare`]); one this node cannot build parks the
//!   process, and a fault building them retries the pass, so no admitted
//!   step is ever settled for it. The tool's declaration pins the step's
//!   policy and its limit within the tool ceiling, and its body runs through
//!   the round tools a turn's round runs on, owned by the process. Every
//!   step of the activation shares those tools ([`StepRuntime::tools`]), so
//!   each reduces against the process's committed plugin state. A
//!   store-local effect commits with the step's outcome. The step's payload
//!   retains the call's `ToolCallRecord`; incorporation projects its output
//!   for the engine.
//! - **`call_dynamic` calls** are two steps (P6). The engine's step of the
//!   endpoint is the call's resolution: the env's deferred provider resolves
//!   its path, and its committed record's value is the match. The step lash
//!   issues in the commit that consumes it runs the matched tool on the
//!   match's provider and binding, admitted under the tool's own
//!   declaration, as a linked call is.
//! - **Resolved links** run the tool the declaring cell's committed
//!   deferred match resolved, as the env retains it: its definition, on the
//!   link's provider, under its binding, which this node must read before
//!   the call is admitted. Nothing resolves it again.
//! - **Engine steps** run through the [`EngineSteps`](crate::EngineSteps)
//!   the registration of the process's engine declares, under the pinned
//!   `Repeatable` policy. A registration that declares none, or not this
//!   kind, refuses the step before admission.

use std::sync::Arc;

use lash_core_execution::runtime::actor::round::MemberPin;
use lash_core_execution::runtime::actor::round::{
    AdmittedExecution, Material, MemberBody, MemberOutcome, SettledOutput,
};
use lash_core_execution::runtime::actor::waits::Resolution;
use lash_core_execution::runtime::process::steps::{
    ProcessSteps, StepAdmission, StepRefusal, StepRuntime, dynamic_resolution, tool_step_output,
    tool_step_refused, tool_step_resolved, tool_step_settled,
};
use lash_core_execution::tool_run::CompletionSource;
use lash_core_execution::{
    EngineStepRun, ExecutionLimit, MaxToolCalls, ProcessInput, ProcessRecord, StepRequest,
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
    /// The tool step `execution` of `process`, which runs `step` on
    /// `runtime`'s step tools.
    async fn tool_step(
        &self,
        runtime: Arc<StepRuntime>,
        process: ProcessRecord,
        step: StepRequest,
        execution: AdmittedExecution,
        token: tokio_util::sync::CancellationToken,
    ) -> MemberOutcome {
        let resolves = step.resolves();
        let StepRequest::Tool {
            effect,
            input,
            matched,
            ..
        } = step
        else {
            return SettledOutput::Interrupted.into();
        };
        if resolves {
            return self
                .resolve_dynamic(&runtime, &process, &execution, input)
                .await;
        }
        // `prepare` built the activation's tools before any step was
        // admitted or resumed, classifying every failure to build them: a
        // retryable fault retried the pass and a refusal parked the process,
        // so no step settles for one. An activation that ran a step without
        // them broke that order; its admission stands, so the step is
        // answered as one that may have taken effect, never run again under
        // `Once`.
        let Some(step) = runtime.built_tools() else {
            tracing::warn!(process_id = %process.id, "a process tool step ran before its activation built its tools");
            return SettledOutput::Interrupted.into();
        };
        // The target of a `call_dynamic` call runs on its recorded match,
        // and a resolved link's call on the resolution its env retains: the
        // matched tool, on the provider and binding the record names.
        let resolution = match &matched {
            Some(matched) => matched.resolved(),
            None => step.effects.get(&effect).and_then(|link| {
                link.resolution().map(|resolved| {
                    (
                        &link.provider,
                        &resolved.definition,
                        resolved.binding.as_ref(),
                    )
                })
            }),
        };
        if let Some((provider, definition, binding)) = resolution {
            let call = lash_core_execution::sansio::PendingToolCall {
                call_id: execution.call().clone(),
                provider_call_id: None,
                tool_name: definition.manifest.name.clone(),
                args: input,
                replay: None,
            };
            let grant =
                crate::ToolExecutionGrant::from_definition(provider.clone(), definition.clone())
                    .with_execution_binding(binding.cloned());
            let result = step.tools.granted_body(&call, &execution, grant)(token).await;
            return tool_step_output(&process.id, &call, result);
        }
        if matched.is_some() {
            return SettledOutput::Interrupted.into();
        }
        let tool = execution.draft().tool();
        let name = step
            .catalog
            .tools
            .iter()
            .find(|entry| entry.manifest.id == *tool)
            .map_or_else(|| effect.to_string(), |entry| entry.manifest.name.clone());
        let call = lash_core_execution::sansio::PendingToolCall {
            call_id: execution.call().clone(),
            provider_call_id: None,
            tool_name: name,
            args: input,
            replay: None,
        };
        let result = step.tools.body(&call, &execution, None)(token).await;
        tool_step_output(&process.id, &call, result)
    }

    /// The resolution `execution` of a `call_dynamic` call of `process`
    /// with `input` (P6): the env's deferred provider resolves its call
    /// path once, and the step's committed outcome is the match it
    /// decided, which the commit that consumes it never resolves again. A
    /// resolution that does not commit runs again: it is read-only.
    async fn resolve_dynamic(
        &self,
        runtime: &Arc<StepRuntime>,
        process: &ProcessRecord,
        execution: &AdmittedExecution,
        input: serde_json::Value,
    ) -> MemberOutcome {
        let call = lash_core_execution::sansio::PendingToolCall {
            call_id: execution.call().clone(),
            provider_call_id: None,
            tool_name: lash_core::call_dynamic_endpoint().manifest.name,
            args: input.clone(),
            replay: None,
        };
        let failed = |code: &str, message: String, cause| {
            let output = step_failure(process, code, message, Some(cause), None).output;
            tool_step_refused(&process.id, &call, output).into()
        };
        let path = match serde_json::from_value::<lash_core::DynamicCall>(input) {
            Ok(dynamic) => dynamic.path,
            Err(error) => {
                return failed(
                    "dynamic_call_invalid",
                    format!("a call_dynamic call takes a call path and its arguments: {error}"),
                    crate::ToolFailureCause::DynamicCallInvalid {
                        reason: error.to_string(),
                    },
                );
            }
        };
        let Some(tools) = runtime.built_tools() else {
            return SettledOutput::Interrupted.into();
        };
        let resolved = tools.context.resolve_dynamic(&path).await;
        match resolved {
            Ok(matched) => dynamic_resolution(&process.id, &call, &matched).into(),
            // The deferred provider left this node since the step was
            // admitted: nothing was resolved, as a missing linked tool.
            Err(error) => failed(
                "linked_tool_missing",
                format!("the call path `{path}` could not be resolved here: {error}"),
                crate::ToolFailureCause::LinkedToolMissing {
                    effect: lash_core::CALL_DYNAMIC_EFFECT.to_owned(),
                    tool: lash_core::call_dynamic_endpoint().manifest.id,
                },
            ),
        }
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
        // A resolution is not the effect's body: its target's is.
        let StepRequest::Tool {
            site: Some(site), ..
        } = step
        else {
            return;
        };
        if step.resolves() {
            return;
        }
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
        let mut context = lash_trace::TraceContext::default().for_process(process.id.clone());
        context.graph_node_id = Some(site.site.to_string());
        tracing.observe_language(&started.event_key(), || {
            (
                context.clone(),
                crate::TraceEvent::StepBodyStarted {
                    step: started.clone(),
                },
            )
        });
    }

    /// The activation's step tools, built once, by its `prepare`.
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
                self.engine_runtime(process)
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

    fn succession(
        &self,
        process: &ProcessRecord,
    ) -> Option<Arc<dyn lash_core_execution::EngineSuccession>> {
        self.0
            .config
            .runtime_host
            .process_engines
            .succession(engine_kind(process)?)
    }

    /// An engine process's composition and the activation's step tools
    /// build here, once per activation, or it parks: a missing component,
    /// an unreadable config or a refused build is the typed park reason,
    /// and a store or attempt fault retries the pass. No step body builds
    /// them, so none settles for a build that failed.
    async fn prepare(
        &self,
        runtime: &Arc<StepRuntime>,
        process: &ProcessRecord,
    ) -> Result<(), StepRefusal> {
        use lash_core_execution::runtime::process::StepToolsError;
        if engine_kind(process).is_none() {
            return Ok(());
        }
        let unavailable = |reason: String| StepRefusal::Unavailable {
            step: String::new(),
            reason,
        };
        match self.0.built_step_tools(runtime, process).await {
            Ok(_) => Ok(()),
            Err(StepToolsError::Build(crate::PluginError::ProcessEnvRefused { refusal })) => {
                Err(StepRefusal::EnvRefused { refusal })
            }
            Err(StepToolsError::Build(error))
                if error.class() != crate::PluginErrorClass::Terminal =>
            {
                Err(unavailable(error.to_string()))
            }
            Err(StepToolsError::State(error)) if !error.is_terminal() => {
                Err(unavailable(error.to_string()))
            }
            // A contribution of a named component refused, or the process's
            // namespaces refuse a committed resolution: the same refusal on
            // every rebuild here.
            Err(error) => Err(StepRefusal::CompositionUnavailable {
                reason: error.to_string(),
            }),
        }
    }

    async fn convert_components(
        &self,
        process: &ProcessRecord,
    ) -> Result<
        Option<lash_core_execution::runtime::actor::process::component_formats::ComponentFormatCut>,
        crate::PluginError,
    > {
        use lash_core_execution::runtime::actor::process::component_formats::{
            ComponentFormatCut, recorded_component_cut, recorded_component_state,
        };
        use lash_core_execution::runtime::actor::process::registry_error;
        let backend = self.0.config.runtime_host.backend();
        let Some(env_ref) = process.env_ref() else {
            return Ok(None);
        };
        let cut = recorded_component_cut(backend.durable().as_ref(), process)
            .await
            .map_err(registry_error)?;
        let (env, state) = match cut {
            Some(cut) => (cut.env, Some(cut.state)),
            None => (
                lash_core_execution::runtime::load_process_env(
                    backend.process_env_store().as_ref(),
                    env_ref,
                )
                .await?,
                None,
            ),
        };
        let state = recorded_component_state(backend.durable().as_ref(), process, &env, state)
            .await
            .map_err(registry_error)?;
        let (decoded_env, decoded_state) = self
            .0
            .config
            .runtime_host
            .process_engines
            .components()
            .decode_components(
                &env,
                &state,
                self.0.config.runtime_host.data_limits.plugin_state,
            )?;
        if decoded_env == env && decoded_state == state {
            return Ok(None);
        }
        Ok(Some(ComponentFormatCut {
            env_ref: env_ref.clone(),
            env: decoded_env,
            state: decoded_state,
        }))
    }

    /// What an engine process carries while it holds `steps` (§6.2): its
    /// environment's components at their state/config formats. A
    /// session-turn process keeps none.
    async fn requirements(
        &self,
        runtime: &Arc<StepRuntime>,
        process: &ProcessRecord,
        _steps: &[StepRequest],
    ) -> Result<
        Option<std::collections::BTreeSet<lash_core::ObligationToken>>,
        lash_core::durable_port::DurableError,
    > {
        if process.env_ref().is_none() {
            return Ok(None);
        }
        let unavailable = |error: crate::PluginError| {
            lash_core::durable_port::DurableError::Store(lash_core::durable_port::StoreFailure {
                kind: lash_core::durable_port::StoreFailureKind::Unavailable,
                message: error.to_string(),
            })
        };
        let context = match runtime.built_tools() {
            Some(tools) => Arc::clone(&tools.context),
            // A successor's candidate env is judged before its commit;
            // its catalog and tools build on the next claim, as before.
            None => self
                .0
                .engine_runtime(process)
                .await
                .map_err(unavailable)?
                .step_context(),
        };
        context.requirements().map(Some).map_err(unavailable)
    }

    /// The `max_tool_calls` the process's environment states (FIG-4546).
    async fn max_tool_calls(
        &self,
        process: &ProcessRecord,
    ) -> Result<Option<MaxToolCalls>, StepRefusal> {
        let Some(env_ref) = process.env_ref() else {
            return Ok(None);
        };
        let environment = lash_core_execution::runtime::load_process_env(
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
        Ok(Some(environment.max_tool_calls()))
    }

    async fn admit(
        &self,
        runtime: &Arc<StepRuntime>,
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
                Ok(StepAdmission::new(
                    kind.admitted_tool(engine),
                    steps.retry(kind),
                    ExecutionLimit::starting_at(now_ms, total, total),
                    None,
                ))
            }
            StepRequest::Tool {
                step,
                effect,
                matched,
                ..
            } => {
                let unavailable = |error: crate::PluginError| StepRefusal::Unavailable {
                    step: step.0.clone(),
                    reason: error.to_string(),
                };
                // The target of a `call_dynamic` call is admitted as the
                // tool its recorded match names, on the match's provider,
                // once this node reads the match's binding (S8).
                let tools = self
                    .0
                    .built_step_tools(runtime, process)
                    .await
                    .map_err(|error| unavailable(crate::PluginError::Session(error.to_string())))?;
                let link = match matched {
                    Some(matched) => {
                        let (link, binding) =
                            tools.context.matched_link(matched).map_err(unavailable)?;
                        if let Err(reason) = binding {
                            return Err(StepRefusal::CompositionUnavailable { reason });
                        }
                        link
                    }
                    None => tools.context.step_link(effect).map_err(unavailable)?,
                };
                match link {
                    crate::plugin::StepLink::Unlinked => Err(StepRefusal::UnknownTool {
                        step: step.0.clone(),
                        tool: effect.to_string(),
                    }),
                    // A resolved link's binding this node cannot read parks
                    // the process before the call is admitted (S8), as a
                    // match's does.
                    crate::plugin::StepLink::Unreadable { reason, .. } => {
                        Err(StepRefusal::CompositionUnavailable { reason })
                    }
                    // The body's limit and the park's deadline are pinned
                    // separately, as a round member's are.
                    crate::plugin::StepLink::Served { manifest, binding } => {
                        let pin = MemberPin::admitted(
                            manifest.id.clone(),
                            manifest.execution_policy,
                            manifest.bounds(),
                            now_ms,
                        );
                        let mut admission =
                            StepAdmission::new(pin.tool, pin.policy, pin.limit, pin.park);
                        admission.binding = Some(*binding);
                        Ok(admission)
                    }
                    // The linked tool is missing or changed here (D3): the
                    // step settles with a typed tool failure, which its
                    // engine receives as a catchable error, and no body
                    // starts, on this attempt or a retry.
                    crate::plugin::StepLink::Drifted { tool, kind } => {
                        let control = self
                            .0
                            .config
                            .runtime_host
                            .control
                            .execution_budgets
                            .tool_check_phase();
                        let effect = effect.to_string();
                        let (code, cause) = match kind {
                            crate::ToolSurfaceDriftKind::Missing => (
                                "linked_tool_missing",
                                crate::ToolFailureCause::LinkedToolMissing {
                                    effect: effect.clone(),
                                    tool: tool.clone(),
                                },
                            ),
                            crate::ToolSurfaceDriftKind::Changed => (
                                "linked_tool_changed",
                                crate::ToolFailureCause::LinkedToolChanged {
                                    effect: effect.clone(),
                                    tool: tool.clone(),
                                },
                            ),
                        };
                        let message = format!(
                            "the effect `{effect}` is linked to tool `{tool}`, which is {} \
                             what this node's provider serves since the process's env \
                             retained it; nothing ran",
                            kind.describe()
                        );
                        let mut admission = StepAdmission::new(
                            tool,
                            crate::ExecutionPolicy::Once,
                            ExecutionLimit::starting_at(now_ms, control, control),
                            None,
                        );
                        admission.settled =
                            Some(step_failure(process, code, message, Some(cause), None).output);
                        Ok(admission)
                    }
                }
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
                            .engine_step(&runtime, process, step, kind, input, token)
                            .await
                    }
                }
            })
        })
    }

    /// A step its admission settled (D3) retains the ToolCallRecord a body
    /// would, so its engine reads the failure as the call's output.
    fn admitted_output(
        &self,
        process: &lash_core_execution::ProcessId,
        step: &StepRequest,
        call: &lash_core_execution::ToolCallId,
        output: SettledOutput,
    ) -> SettledOutput {
        let StepRequest::Tool { effect, input, .. } = step else {
            return output;
        };
        let call = lash_core_execution::sansio::PendingToolCall {
            call_id: call.clone(),
            provider_call_id: None,
            tool_name: effect.to_string(),
            args: input.clone(),
            replay: None,
        };
        tool_step_refused(process, &call, output)
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
        runtime: &StepRuntime,
        process: ProcessRecord,
        step: lash_core_execution::StepName,
        kind: lash_core_execution::EngineStepKind,
        input: serde_json::Value,
        token: tokio_util::sync::CancellationToken,
    ) -> MemberOutcome {
        let Some(engine) = engine_kind(&process) else {
            return step_failure(
                &process,
                "engine_step_without_engine",
                "an engine step belongs to a process with no engine".to_owned(),
                Some(crate::ToolFailureCause::EngineStepWithoutEngine {
                    step: step.0,
                    step_kind: kind.0,
                }),
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
                return step_failure(
                    &process,
                    code,
                    refusal.to_string(),
                    Some(crate::ToolFailureCause::EngineStepRegistrationUnavailable {
                        engine: engine.to_owned(),
                        step: step.0,
                        step_kind: kind.0,
                    }),
                    Some(serde_json::json!({ "registration_refusal": refusal })),
                );
            }
        };
        // The catalog is the one `prepare` built with the activation's step
        // tools: the body reads no store for it (see `tool_step`).
        let Some(tools) = runtime.built_tools() else {
            tracing::warn!(process_id = %process.id, "an engine step ran before its activation built its tools");
            return SettledOutput::Interrupted.into();
        };
        let effects = Arc::clone(&tools.effects);
        let cx = runtime.cx();
        let backend = cx.backend().clone();
        let run = EngineStepRun {
            process: process.id.clone(),
            engine_settings: process.engine_settings().cloned(),
            effects,
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
fn step_failure(
    process: &ProcessRecord,
    code: &str,
    message: String,
    cause: Option<crate::ToolFailureCause>,
    source: Option<serde_json::Value>,
) -> MemberOutcome {
    use lash_core_execution::tool_run::{KnownFailureReason, MaterialOwner, MaterialRole};
    let mut failure = crate::ToolFailure::runtime(crate::ToolFailureClass::Internal, code, message);
    if let Some(cause) = cause {
        failure = failure.with_cause(cause);
    }
    failure.raw = source.map(crate::ToolValue::untrusted_json);
    let output = crate::ToolCallOutput::failure(failure);
    SettledOutput::Failed(
        Material::journal_local(
            MaterialOwner::Process {
                process_id: process.id.clone(),
            },
            MaterialRole::AttemptOutput,
            serde_json::to_string(&lash_core::Durable(&output))
                .expect("an engine step failure encodes"),
        )
        .failure(KnownFailureReason::Reported, None),
    )
    .into()
}
