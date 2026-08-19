mod files;
mod host_bridge;
mod snapshot;
mod state;

pub use snapshot::RLM_SNAPSHOT_VERSION;
pub use state::RlmExecutionState;
#[cfg(feature = "testing")]
pub(crate) use state::capture_scratch_files_for_testing;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
#[cfg(any(test, feature = "testing"))]
use std::sync::atomic::{AtomicBool, Ordering};

use lash_core::{
    ExecRequest, ExecResponse, RuntimeEffectKind, RuntimeExecutionContext, SessionError,
    TraceContext, facade_support::TraceRuntimeScope, facade_support::TraceRuntimeSubject,
    facade_support::TraceSink,
};
use lash_lashlang_runtime::{
    LashlangSurface, TraceLanguageExecutionEvent, TraceLanguageExecutionIdentity,
    TraceLanguageExecutionMap, TraceLanguageExecutionStatus,
};
use lashlang::{ExecutionOutcome, State as FlowState};

use self::host_bridge::{HostBridge, HostBridgeConfig, LashlangExecutionTrace};
use crate::projection::{
    ProjectionResolver, RLM_TURN_INPUT_PLUGIN_ID, RlmProjectedBindings, RlmProjectionExtension,
    flow_to_json_value, json_to_flow_value, projected_bindings, prune_projected_binding_names,
    rehydrate_projected_globals,
};

#[cfg(any(test, feature = "testing"))]
static EXECUTION_BOUND_EXHAUSTION_LOUD: AtomicBool = AtomicBool::new(true);

#[cfg(test)]
fn set_execution_bound_exhaustion_loud(loud: bool) -> bool {
    EXECUTION_BOUND_EXHAUSTION_LOUD.swap(loud, Ordering::SeqCst)
}

#[derive(Clone, Default)]
pub(crate) struct RlmLashlangExecutionTraceConfig {
    pub(crate) sink: Option<Arc<dyn TraceSink>>,
    pub(crate) trace_context: TraceContext,
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
async fn execute_code_unbounded_for_tests(
    state: RlmExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    request: ExecRequest,
    artifact_store: Arc<dyn lashlang::LashlangArtifactStore>,
    lashlang_surface: LashlangSurface,
    deferred_tool_resolver: Option<lash_lashlang_runtime::SharedDeferredToolResolver>,
    session_projected_bindings: RlmProjectedBindings,
    projection_resolver: Arc<dyn ProjectionResolver>,
    lashlang_execution_trace_config: RlmLashlangExecutionTraceConfig,
) -> Result<(RlmExecutionState, ExecResponse), SessionError> {
    execute_code_with_bounds(
        state,
        ctx,
        request,
        artifact_store,
        lashlang_surface,
        deferred_tool_resolver,
        session_projected_bindings,
        projection_resolver,
        lashlang_execution_trace_config,
        lashlang::ExecutionBounds::unbounded(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_code_with_bounds(
    state: RlmExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    request: ExecRequest,
    artifact_store: Arc<dyn lashlang::LashlangArtifactStore>,
    lashlang_surface: LashlangSurface,
    deferred_tool_resolver: Option<lash_lashlang_runtime::SharedDeferredToolResolver>,
    session_projected_bindings: RlmProjectedBindings,
    projection_resolver: Arc<dyn ProjectionResolver>,
    lashlang_execution_trace_config: RlmLashlangExecutionTraceConfig,
    execution_bounds: lashlang::ExecutionBounds,
) -> Result<(RlmExecutionState, ExecResponse), SessionError> {
    execute_code_with_dialect_and_bounds(
        state,
        ctx,
        request,
        artifact_store,
        lashlang_surface,
        deferred_tool_resolver,
        session_projected_bindings,
        projection_resolver,
        lashlang_execution_trace_config,
        execution_bounds,
        SourceDialect::Lashlang,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_typescript_code_with_bounds(
    state: RlmExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    request: ExecRequest,
    artifact_store: Arc<dyn lashlang::LashlangArtifactStore>,
    lashlang_surface: LashlangSurface,
    deferred_tool_resolver: Option<lash_lashlang_runtime::SharedDeferredToolResolver>,
    session_projected_bindings: RlmProjectedBindings,
    projection_resolver: Arc<dyn ProjectionResolver>,
    lashlang_execution_trace_config: RlmLashlangExecutionTraceConfig,
    execution_bounds: lashlang::ExecutionBounds,
) -> Result<(RlmExecutionState, ExecResponse), SessionError> {
    execute_code_with_dialect_and_bounds(
        state,
        ctx,
        request,
        artifact_store,
        lashlang_surface,
        deferred_tool_resolver,
        session_projected_bindings,
        projection_resolver,
        lashlang_execution_trace_config,
        execution_bounds,
        SourceDialect::Typescript,
    )
    .await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceDialect {
    Lashlang,
    Typescript,
}

impl SourceDialect {
    /// The language id an execution trace record carries.
    ///
    /// Every record said `lashlang` regardless, so a TypeScript session's
    /// `lashlang-execution.jsonl` described its own executions as Lashlang —
    /// the same "evidence that disagrees with its own label" defect the
    /// transcript badge had. The substrate under both dialects is the Lashlang
    /// VM, which is why the file name and the graph API keep their names; what
    /// was wrong is the claim about the *source* that ran.
    fn language_id(self) -> &'static str {
        match self {
            Self::Lashlang => crate::dialect::lashlang::LANGUAGE_ID,
            Self::Typescript => crate::dialect::typescript::LANGUAGE_ID,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_code_with_dialect_and_bounds(
    mut state: RlmExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    request: ExecRequest,
    artifact_store: Arc<dyn lashlang::LashlangArtifactStore>,
    lashlang_surface: LashlangSurface,
    deferred_tool_resolver: Option<lash_lashlang_runtime::SharedDeferredToolResolver>,
    session_projected_bindings: RlmProjectedBindings,
    projection_resolver: Arc<dyn ProjectionResolver>,
    lashlang_execution_trace_config: RlmLashlangExecutionTraceConfig,
    execution_bounds: lashlang::ExecutionBounds,
    source_dialect: SourceDialect,
) -> Result<(RlmExecutionState, ExecResponse), SessionError> {
    let start = std::time::Instant::now();
    let clean_code = clean_model_code(&request.code);
    let response = Box::pin(execute_code_inner(
        &mut state,
        ctx,
        &clean_code,
        start,
        artifact_store,
        lashlang_surface,
        deferred_tool_resolver,
        session_projected_bindings,
        projection_resolver,
        lashlang_execution_trace_config,
        execution_bounds,
        source_dialect,
    ))
    .await;
    Ok((state, response))
}

/// Feature-gated fixture that lets the repository's performance harness drive
/// the production RLM execution-state capture without exposing executor
/// internals as public protocol API.
#[cfg(feature = "testing")]
pub struct RlmCheckpointPerfFixture {
    state: Option<RlmExecutionState>,
    binding_count: usize,
    payload_bytes: usize,
}

#[cfg(feature = "testing")]
impl RlmCheckpointPerfFixture {
    pub fn new(binding_count: usize, payload_bytes: usize) -> Result<Self, SessionError> {
        let mut state = RlmExecutionState::for_engine("lashlang")?;
        // The snapshot's globals became a read-only projection when the heap
        // took ownership of them, so seed through the state's own insert.
        for index in 0..binding_count {
            state
                .rlm
                .insert_global(
                    format!("mid_{index}"),
                    json_to_flow_value(serde_json::json!([format!(
                        "binding-{index}-{}",
                        "x".repeat(payload_bytes)
                    )])),
                )
                .map_err(|error| SessionError::Protocol(error.to_string()))?;
        }
        Ok(Self {
            state: Some(state),
            binding_count,
            payload_bytes,
        })
    }

    pub fn capture(&mut self) -> Result<lash_core::plugin::ExecutionStateSnapshot, SessionError> {
        self.state
            .as_mut()
            .expect("RLM checkpoint perf fixture state present")
            .snapshot_execution_state()
    }

    pub fn acknowledge_capture(&mut self) {
        self.state
            .as_mut()
            .expect("RLM checkpoint perf fixture state present")
            .acknowledge_execution_state_capture();
    }

    pub async fn assign_one(&mut self, index: usize, turn: usize) -> Result<(), SessionError> {
        let binding = index % self.binding_count.max(1);
        let state = self
            .state
            .take()
            .expect("RLM checkpoint perf fixture state present");
        let code = format!(
            "mid_{binding} = push(mid_{binding}, \"turn-{turn}-{}\")",
            "y".repeat(self.payload_bytes / 8)
        );
        let (state, response) = execute_code_with_bounds(
            state,
            lash_core::testing::code_execution_context(),
            ExecRequest {
                language: "lashlang".to_string(),
                code,
                accept_finish: false,
            },
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            None,
            RlmProjectedBindings::default(),
            Arc::new(crate::ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
            lashlang::ExecutionBounds::unbounded(),
        )
        .await?;
        self.state = Some(state);
        if let Some(error) = response.error {
            return Err(SessionError::Protocol(format!(
                "RLM checkpoint perf assignment failed: {error}"
            )));
        }
        Ok(())
    }

    pub fn absorb_dirty_assignments(&mut self) {
        self.state
            .as_mut()
            .expect("RLM checkpoint perf fixture state present")
            .absorb_pending_assignments_for_perf();
    }

    pub fn restore(state: &lash_core::plugin::HydratedExecutionState) -> Result<(), SessionError> {
        let mut restored = RlmExecutionState::for_engine("lashlang")?;
        restored
            .restore_execution_state(state)
            .map_err(|error| SessionError::Protocol(error.to_string()))
    }
}

fn clean_model_code(code: &str) -> String {
    code.lines()
        .filter(|line| {
            let trimmed = line.trim();
            trimmed.is_empty()
                || trimmed
                    .trim_matches('-')
                    .chars()
                    .any(|c| !c.is_whitespace())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[allow(clippy::too_many_arguments)]
async fn execute_code_inner(
    state: &mut RlmExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    code: &str,
    start: std::time::Instant,
    artifact_store: Arc<dyn lashlang::LashlangArtifactStore>,
    lashlang_surface: LashlangSurface,
    deferred_tool_resolver: Option<lash_lashlang_runtime::SharedDeferredToolResolver>,
    session_projected_bindings: RlmProjectedBindings,
    projection_resolver: Arc<dyn ProjectionResolver>,
    lashlang_execution_trace_config: RlmLashlangExecutionTraceConfig,
    execution_bounds: lashlang::ExecutionBounds,
    source_dialect: SourceDialect,
) -> ExecResponse {
    state.mark_execution_started();
    select_deferred_resolution_link(state, &ctx);
    let mut host_environment = match lashlang_surface.host_environment(ctx.tool_catalog().as_ref())
    {
        Ok(host_environment) => host_environment,
        Err(err) => {
            return ExecResponse {
                observations: Vec::new(),
                observation_truncation: Vec::new(),
                tool_calls: Vec::new(),
                executed_calls: Vec::new(),
                images: Vec::new(),
                printed_images: Vec::new(),
                error: Some(format!("invalid Lashlang host tool surface: {err}")),
                duration_ms: start.elapsed().as_millis() as u64,
                terminal_finish: None,
            };
        }
    };

    // gather → resolve → link: fold any deferred call-paths the program
    // references into the host environment before compiling. The resolution
    // record lives in the (snapshotted) execution state, so a re-driven or
    // recovered link replays it without re-calling the resolver. The flat Tool
    // Catalog is never mutated — resolution is link-scoped only. A resolver is
    // present only under hosts that configure RLM deferral; most hosts ship
    // none and this is a no-op.
    if deferred_tool_resolver.is_some() || !state.deferred_resolutions.is_empty() {
        let _phase = ctx.named_phase("rlm_lashlang.deferred_resolve");
        let program = match source_dialect {
            SourceDialect::Lashlang => lashlang::parse(code).ok(),
            SourceDialect::Typescript => lash_typescript::parse(code).ok(),
        };
        if let Some(program) = program {
            host_environment = lash_lashlang_runtime::resolve_and_fold_deferred(
                &program,
                host_environment,
                deferred_tool_resolver.as_ref(),
                &mut state.deferred_resolutions,
            )
            .await;
        }
    }

    let mut live_global_names = state
        .rlm
        .globals()
        .iter()
        .map(|(name, _)| name.to_string())
        .collect::<BTreeSet<_>>();
    live_global_names.insert("history".to_string());
    live_global_names.extend(session_projected_bindings.names());
    if let Some(extension) = ctx
        .turn_context()
        .plugin_input::<crate::projection::RlmProjectionExtension>(
            crate::projection::RLM_TURN_INPUT_PLUGIN_ID,
        )
    {
        live_global_names.extend(extension.bindings.names());
    }
    host_environment = host_environment.with_globals(live_global_names);

    // The kind is decided here, while the failure is still a typed diagnostic.
    // "Compilation failed" is not enough to classify it: a misspelled name and a
    // forbidden construct both fail here and need opposite advice.
    let compile_result: Result<_, (crate::feedback::RlmFeedbackKind, String)> = {
        let _phase = ctx.named_phase("rlm_lashlang.compile_link");
        match source_dialect {
            SourceDialect::Lashlang => state
                .linked_programs
                .get_or_compile(code, &host_environment)
                .map_err(|error| match error {
                    lashlang::LinkedProgramCacheError::Parse(error) => (
                        lashlang_parse_feedback_kind(&error),
                        format_rlm_parse_diagnostic(code, &error),
                    ),
                    lashlang::LinkedProgramCacheError::Link(error) => (
                        lashlang_link_feedback_kind(&error),
                        format_rlm_link_diagnostic(code, &error),
                    ),
                }),
            // TypeScript is parsed here rather than by the cache, so the cache
            // is asked first: otherwise every cell would pay a full parse even
            // when its linked program is already cached.
            SourceDialect::Typescript => match state.linked_programs.cached_linked_program(
                code,
                &host_environment,
                lashlang::CompilationDialect::Typescript,
            ) {
                Some(program) => Ok(program),
                // Parsed with the session's live globals, so a cell can read
                // what an earlier cell bound. Lashlang gets this for free by
                // resolving at link; TypeScript resolves names at parse, so the
                // names have to arrive here. `host_environment` already carries
                // them — it is the same set the linker will check against.
                // Rendered against the cell source, not `to_string()`: the
                // diagnostic carries a span and the model needs the line it
                // wrote. Lashlang's parse failures have always arrived this way.
                None => lash_typescript::parse_with_globals(code, &host_environment.globals)
                    .map_err(|error| {
                        (
                            typescript_feedback_kind(&error),
                            lash_typescript::format_diagnostic(code, &error),
                        )
                    })
                    .and_then(|program| {
                        state
                            .linked_programs
                            .get_or_compile_ast(
                                code,
                                program,
                                &host_environment,
                                lashlang::CompilationDialect::Typescript,
                            )
                            .map_err(|error| {
                                (
                                    lashlang_link_feedback_kind(&error),
                                    format_rlm_link_diagnostic(code, &error),
                                )
                            })
                    }),
            },
        }
    };
    let cached_program = match compile_result {
        Ok(program) => program,
        Err((kind, error)) => {
            let error = kind.label(error);
            return ExecResponse {
                observations: Vec::new(),
                observation_truncation: Vec::new(),
                tool_calls: Vec::new(),
                executed_calls: Vec::new(),
                images: Vec::new(),
                printed_images: Vec::new(),
                error: Some(error),
                duration_ms: start.elapsed().as_millis() as u64,
                terminal_finish: None,
            };
        }
    };
    let linked_module = cached_program.linked_module();
    if !linked_module.artifact.exports.processes.is_empty()
        && !state
            .stored_lashlang_modules
            .contains(&linked_module.module_ref)
    {
        let stored = {
            let _phase = ctx.named_phase("rlm_lashlang.store_module_artifact");
            artifact_store
                .put_module_artifact(&linked_module.artifact)
                .await
        };
        if let Err(err) = stored {
            return ExecResponse {
                observations: Vec::new(),
                observation_truncation: Vec::new(),
                tool_calls: Vec::new(),
                executed_calls: Vec::new(),
                images: Vec::new(),
                printed_images: Vec::new(),
                error: Some(format!("failed to store lashlang module artifact: {err}")),
                duration_ms: start.elapsed().as_millis() as u64,
                terminal_finish: None,
            };
        }
        state
            .stored_lashlang_modules
            .insert(linked_module.module_ref.clone());
    }
    let owner_namespace = match ctx.trigger_owner_scope() {
        Ok(owner_scope) => owner_scope.namespace(),
        Err(err) => {
            return ExecResponse {
                observations: Vec::new(),
                observation_truncation: Vec::new(),
                tool_calls: Vec::new(),
                executed_calls: Vec::new(),
                images: Vec::new(),
                printed_images: Vec::new(),
                error: Some(format!("failed to resolve trigger owner namespace: {err}")),
                duration_ms: start.elapsed().as_millis() as u64,
                terminal_finish: None,
            };
        }
    };
    let manifest_replacement = artifact_store
        .replace_current_trigger_manifest(&owner_namespace, &linked_module.artifact)
        .await;
    let manifest_replacement = match manifest_replacement {
        Ok(replacement) => replacement,
        Err(err) => {
            return ExecResponse {
                observations: Vec::new(),
                observation_truncation: Vec::new(),
                tool_calls: Vec::new(),
                executed_calls: Vec::new(),
                images: Vec::new(),
                printed_images: Vec::new(),
                error: Some(format!(
                    "failed to replace current trigger key manifest: {err}"
                )),
                duration_ms: start.elapsed().as_millis() as u64,
                terminal_finish: None,
            };
        }
    };
    let reconcile_warnings = manifest_replacement
        .diff
        .removed
        .iter()
        .map(|subscription_key| {
            format!(
                "RECONCILE WARNING: trigger subscription `{subscription_key}` is absent from \
                 the replacement artifact for owner `{owner_namespace}` and may be orphaned; \
                 inspect it with `triggers.list({{}})` and remove it explicitly with \
                 `triggers.prune({{ subscription_keys: [\"{subscription_key}\"] }})`"
            )
        })
        .collect::<Vec<_>>();
    let compiled = cached_program.compiled_program();

    let rehydrated = {
        let _phase = ctx.named_phase("rlm_lashlang.rehydrate_projected_globals");
        rehydrate_projected_globals(&mut state.rlm, Arc::clone(&projection_resolver)).await
    };
    if let Err(err) = rehydrated {
        return ExecResponse {
            observations: Vec::new(),
            observation_truncation: Vec::new(),
            tool_calls: Vec::new(),
            executed_calls: Vec::new(),
            images: Vec::new(),
            printed_images: Vec::new(),
            error: Some(err),
            duration_ms: start.elapsed().as_millis() as u64,
            terminal_finish: None,
        };
    }

    let projected = {
        let _phase = ctx.named_phase("rlm_lashlang.resolve_projected_bindings");
        match projected_bindings(&ctx, session_projected_bindings, projection_resolver).await {
            Ok(projected) => projected,
            Err(err) => {
                return ExecResponse {
                    observations: Vec::new(),
                    observation_truncation: Vec::new(),
                    tool_calls: Vec::new(),
                    executed_calls: Vec::new(),
                    images: Vec::new(),
                    printed_images: Vec::new(),
                    error: Some(err),
                    duration_ms: start.elapsed().as_millis() as u64,
                    terminal_finish: None,
                };
            }
        }
    };
    let projected_names = projected.names().collect::<Vec<_>>();
    state.mark_globals_removed(projected_names.iter().map(String::as_str));
    prune_projected_binding_names(&mut state.rlm, projected_names.iter().map(String::as_str));
    let tool_result_projectors = tool_result_projectors(&ctx);
    let deferred_execution_grants = deferred_execution_grants(&state.deferred_resolutions);
    let lashlang_execution_trace = foreground_lashlang_execution_trace(
        &ctx,
        &linked_module.artifact,
        &lashlang_execution_trace_config,
        source_dialect.language_id(),
    );
    if let Some(trace) = &lashlang_execution_trace {
        emit_foreground_execution_started(trace, &linked_module.artifact);
    }
    let host = HostBridge::new(HostBridgeConfig {
        ctx: ctx.clone(),
        print_projector: Arc::new(crate::rlm_support::print_history_projector()),
        tool_result_projectors,
        lashlang_execution_trace: lashlang_execution_trace.clone(),
        host_environment,
        deferred_execution_grants,
        artifact_store: Arc::clone(&artifact_store),
        trigger_key_manifest: linked_module.artifact.trigger_key_manifest.clone(),
        initial_observations: reconcile_warnings,
    });
    let env = lashlang::ExecutionEnvironment::new(&host)
        .traced()
        .with_execution_bounds(execution_bounds)
        .with_scratch(std::mem::take(&mut state.scratch))
        .with_projected_bindings(projected);
    let result = {
        let _phase = ctx.named_phase("rlm_lashlang.execute");
        Box::pin(lashlang::execute(compiled, &mut state.rlm, &env)).await
    };
    state.scratch = env.take_recycled_scratch().unwrap_or_default();
    let runtime_failure = env.take_runtime_failure();
    if let Some(trace) = &lashlang_execution_trace {
        emit_foreground_execution_finished(trace, &result, runtime_failure.as_ref());
    }
    drop(env);
    let terminal_finish = match result {
        Ok(ExecutionOutcome::Finished(value)) => Some(flow_to_json_value(&value).await),
        Ok(ExecutionOutcome::Continued) => None,
        Ok(ExecutionOutcome::Failed(value)) => {
            let collected = host.into_collected();
            return ExecResponse {
                observations: collected.observations,
                observation_truncation: collected.observation_truncation,
                tool_calls: collected.tool_calls,
                executed_calls: collected.executed_calls,
                images: Vec::new(),
                printed_images: collected.printed_images,
                error: Some(
                    crate::feedback::RlmFeedbackKind::Error
                        .label(format!("process failed in foreground execution: {value}")),
                ),
                duration_ms: start.elapsed().as_millis() as u64,
                terminal_finish: None,
            };
        }
        Err(error) => {
            #[cfg(any(test, feature = "testing"))]
            assert!(
                !EXECUTION_BOUND_EXHAUSTION_LOUD.load(Ordering::SeqCst)
                    || !error.is_execution_bound_exhausted(),
                "confidence execution exhausted a required Lashlang bound: {error}"
            );
            // An exhausted execution bound is the runtime declining to keep
            // going, and no amount of debugging the program changes that; every
            // other runtime error is the program's own.
            let kind = if error.is_execution_bound_exhausted() {
                crate::feedback::RlmFeedbackKind::Policy
            } else {
                crate::feedback::RlmFeedbackKind::Error
            };
            let failure = runtime_failure.unwrap_or(lashlang::RuntimeFailure { error, span: None });
            let collected = host.into_collected();
            return ExecResponse {
                observations: collected.observations,
                observation_truncation: collected.observation_truncation,
                tool_calls: collected.tool_calls,
                executed_calls: collected.executed_calls,
                images: Vec::new(),
                printed_images: collected.printed_images,
                error: Some(kind.label(lashlang::format_runtime_diagnostic(
                    code,
                    &failure.error,
                    failure.span,
                ))),
                duration_ms: start.elapsed().as_millis() as u64,
                terminal_finish: None,
            };
        }
    };
    let collected = host.into_collected();
    ExecResponse {
        observations: collected.observations,
        observation_truncation: collected.observation_truncation,
        tool_calls: collected.tool_calls,
        executed_calls: collected.executed_calls,
        images: Vec::new(),
        printed_images: collected.printed_images,
        error: None,
        duration_ms: start.elapsed().as_millis() as u64,
        terminal_finish,
    }
}

/// Whether a TypeScript rejection refuses a construct or reports a wrong
/// program.
///
/// Asked of the diagnostic, not of its code. Three codes carry both families —
/// `TS_METHOD_UNSUPPORTED` covers `Promise.then` and `[].map()` alike — so only
/// the site that emitted it knows, and it records the answer at construction.
fn typescript_feedback_kind(
    error: &lash_typescript::Diagnostic,
) -> crate::feedback::RlmFeedbackKind {
    if error.is_dialect_refusal() {
        crate::feedback::RlmFeedbackKind::Policy
    } else {
        crate::feedback::RlmFeedbackKind::Error
    }
}

/// Whether a Lashlang parse failure is a refusal or a wrong program.
///
/// Almost all of them are the program: a lex failure, an unexpected token, a
/// missing `finish` value. The refusals are the retired forms and the rules
/// about where a construct may appear — no rewrite of the same approach is
/// accepted, so the model must be told to write a different one.
fn lashlang_parse_feedback_kind(error: &lashlang::ParseError) -> crate::feedback::RlmFeedbackKind {
    match error {
        lashlang::ParseError::SubmitRemoved { .. }
        | lashlang::ParseError::DeclarativeTriggerRemoved { .. }
        | lashlang::ParseError::SessionProcessAdminOutsideBlock { .. }
        | lashlang::ParseError::ForegroundControlInsideProcess { .. }
        | lashlang::ParseError::NestingTooDeep { .. } => crate::feedback::RlmFeedbackKind::Policy,
        _ => crate::feedback::RlmFeedbackKind::Error,
    }
}

/// Whether a link failure is a refusal or a wrong program.
///
/// An unknown name, an unknown operation, an arity or type mismatch: those are
/// the program. A bare tool call, a disabled feature, an opaque descriptor read,
/// and the placement rules are the host declining, and no amount of debugging
/// changes them.
fn lashlang_link_feedback_kind(error: &lashlang::LinkError) -> crate::feedback::RlmFeedbackKind {
    match error {
        lashlang::LinkError::BareToolCall { .. }
        | lashlang::LinkError::FeatureDisabled { .. }
        | lashlang::LinkError::OpaqueHostDescriptorAccess { .. }
        | lashlang::LinkError::ProcessLifecycleOutsideProcess { .. }
        | lashlang::LinkError::TriggerEventOutsideInputs { .. } => {
            crate::feedback::RlmFeedbackKind::Policy
        }
        _ => crate::feedback::RlmFeedbackKind::Error,
    }
}

fn format_rlm_parse_diagnostic(code: &str, error: &lashlang::ParseError) -> String {
    format!(
        "{}\n\nA standalone `</lashlang>` line terminates the outer cell even inside multiline source text; construct that content without a standalone delimiter line.",
        lashlang::format_parse_diagnostic(code, error)
    )
}

fn select_deferred_resolution_link(
    state: &mut RlmExecutionState,
    ctx: &RuntimeExecutionContext<'_>,
) {
    let Some(invocation) = ctx.parent_invocation() else {
        state.deferred_resolutions.clear_link();
        return;
    };
    let Some(link_key) =
        lash_lashlang_runtime::DeferredResolutionLinkKey::from_exec_code_invocation(invocation)
    else {
        state.deferred_resolutions.clear_link();
        return;
    };

    state.deferred_resolutions.select_link(link_key);
}

fn deferred_execution_grants(
    record: &lash_lashlang_runtime::DeferredResolutionRecord,
) -> BTreeMap<lash_core::ToolId, lash_core::ToolExecutionGrant> {
    record
        .resolutions
        .values()
        .filter_map(|resolution| {
            let lash_lashlang_runtime::Resolution::Resolved(grant) = resolution else {
                return None;
            };
            let mut execution_grant =
                lash_core::ToolExecutionGrant::from_definition(grant.definition.clone())
                    .with_execution_binding(grant.execution_binding.clone());
            if let Some(source_id) = grant.source_id.as_deref() {
                execution_grant = execution_grant.with_source_id(source_id);
            }
            Some((execution_grant.manifest.id.clone(), execution_grant))
        })
        .collect()
}

const RLM_BARE_TOOL_CALL_DIAGNOSTIC: &str =
    "bare tool calls are not allowed; call the module operation instead.";

fn format_rlm_link_diagnostic(code: &str, err: &lashlang::LinkError) -> String {
    let diagnostic = lashlang::format_link_diagnostic(code, err);
    let lashlang::LinkError::BareToolCall { suggestion, .. } = err else {
        return diagnostic;
    };

    let mut rlm_diagnostic = match diagnostic.find('\n') {
        Some(message_end) => {
            format!(
                "{}{}",
                RLM_BARE_TOOL_CALL_DIAGNOSTIC,
                &diagnostic[message_end..]
            )
        }
        None => RLM_BARE_TOOL_CALL_DIAGNOSTIC.to_string(),
    };
    if !suggestion.is_empty() {
        rlm_diagnostic.push_str("\nhint: use `");
        rlm_diagnostic.push_str(suggestion);
        rlm_diagnostic.push('`');
    }
    rlm_diagnostic
}

fn tool_result_projectors(ctx: &RuntimeExecutionContext<'_>) -> Vec<crate::RlmToolResultProjector> {
    ctx.turn_context()
        .plugin_input::<RlmProjectionExtension>(RLM_TURN_INPUT_PLUGIN_ID)
        .map(|extension| extension.tool_result_projectors.clone())
        .unwrap_or_default()
}

fn foreground_lashlang_execution_trace(
    ctx: &RuntimeExecutionContext<'_>,
    artifact: &lashlang::ModuleArtifact,
    config: &RlmLashlangExecutionTraceConfig,
    language: &'static str,
) -> Option<LashlangExecutionTrace> {
    let sink = config.sink.as_ref()?.clone();
    let invocation = ctx.parent_invocation()?;
    if invocation.effect_kind() != Some(RuntimeEffectKind::ExecCode) {
        return None;
    }
    let effect_id = invocation.effect_id()?;
    let kind = invocation.effect_kind()?;
    Some(LashlangExecutionTrace::new(
        sink,
        language,
        config.trace_context.clone(),
        TraceLanguageExecutionIdentity {
            scope: TraceRuntimeScope {
                session_id: invocation.scope.session_id.clone(),
                turn_id: invocation.scope.turn_id.clone(),
                turn_index: invocation.scope.turn_index,
                protocol_iteration: invocation.scope.protocol_iteration,
            },
            subject: TraceRuntimeSubject::Effect {
                effect_id: effect_id.to_string(),
                kind: kind.as_str().to_string(),
            },
            module_ref: artifact.module_ref.to_string(),
            entry_kind: "main".to_string(),
            entry_ref: None,
            entry_name: "main".to_string(),
        },
    ))
}

fn emit_foreground_execution_started(
    trace: &LashlangExecutionTrace,
    artifact: &lashlang::ModuleArtifact,
) {
    trace.emit(TraceLanguageExecutionEvent::ExecutionStarted {
        event_key: trace.event_key("started"),
        identity: trace.identity().clone(),
        execution_map: trace_main_map(artifact),
    });
}

fn emit_foreground_execution_finished(
    trace: &LashlangExecutionTrace,
    result: &Result<ExecutionOutcome, lashlang::RuntimeError>,
    runtime_failure: Option<&lashlang::RuntimeFailure>,
) {
    let (status, error) = match result {
        Ok(ExecutionOutcome::Finished(_)) | Ok(ExecutionOutcome::Continued) => {
            (TraceLanguageExecutionStatus::Completed, None)
        }
        Ok(ExecutionOutcome::Failed(value)) => (
            TraceLanguageExecutionStatus::Failed,
            Some(value.to_string()),
        ),
        Err(error) => (
            TraceLanguageExecutionStatus::Failed,
            Some(
                runtime_failure
                    .map(|failure| failure.error.to_string())
                    .unwrap_or_else(|| error.to_string()),
            ),
        ),
    };
    trace.emit(TraceLanguageExecutionEvent::ExecutionFinished {
        event_key: trace.event_key("finished"),
        identity: trace.identity().clone(),
        status,
        error,
    });
}

fn trace_main_map(artifact: &lashlang::ModuleArtifact) -> TraceLanguageExecutionMap {
    lash_lashlang_runtime::trace_lashlang_main_map(artifact)
}

/// Applies a `set_default` patch as one transaction.
///
/// Every key is checked before any of them is applied, and the accepted
/// operations then go to the state as a single batch. A rejected patch — a
/// protected or reserved name anywhere in it — therefore leaves the state
/// exactly as it was, instead of committing the defaults that happened to come
/// first while the caller's dirty tracking records nothing.
fn apply_global_defaults(
    rlm: &mut FlowState,
    patch: &lash_rlm_types::RlmGlobalsPatchPluginBody,
    protected_names: &BTreeSet<String>,
) -> Result<Vec<String>, String> {
    if patch.set_default.is_empty() {
        return Ok(Vec::new());
    }
    for key in patch.set_default.keys() {
        if is_reserved_global_name(key) || protected_names.contains(key) {
            return Err(format!(
                "`{key}` is a read-only projected host binding; choose a different Lashlang variable name for `set_default`"
            ));
        }
    }
    let outcome = rlm
        .patch_globals(patch.set_default.iter().map(|(key, value)| {
            lashlang::GlobalPatch::SetDefault {
                name: key.clone(),
                value: json_to_flow_value(value.clone()),
            }
        }))
        .map_err(|error| error.to_string())?;
    Ok(outcome.inserted)
}

fn is_reserved_global_name(key: &str) -> bool {
    key == "history"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_diagnostic_warns_about_multiline_cell_delimiters() {
        let code = "payload = \"\"\"";
        let error = lashlang::parse(code).expect_err("unterminated multiline string");
        let diagnostic = format_rlm_parse_diagnostic(code, &error);

        assert!(diagnostic.contains("standalone `</lashlang>` line"));
        assert!(diagnostic.contains("inside multiline source text"));
    }

    /// A typo is not a policy refusal.
    ///
    /// Classifying every compile failure as `[POLICY]` produced the one thing
    /// the split exists to prevent: `unknown name \`task\`` arrived under "the
    /// runtime refused this cell; sending it again unchanged will be refused
    /// again. Rewrite it in the form named above" — with no form named above,
    /// because a misspelled identifier has no accepted alternative form. The
    /// gate is the diagnostic code, not the fact that compilation failed.
    #[test]
    fn a_wrong_program_and_a_forbidden_construct_are_classified_apart() {
        let typo = lash_typescript::parse_with_globals("finish(taks);", &BTreeSet::new())
            .expect_err("an unbound name is rejected");
        assert_eq!(
            typescript_feedback_kind(&typo),
            crate::feedback::RlmFeedbackKind::Error,
            "a misspelled name is the program being wrong: {typo}"
        );

        let forbidden = lash_typescript::parse_with_globals("class A {}", &BTreeSet::new())
            .expect_err("classes are refused");
        assert_eq!(
            typescript_feedback_kind(&forbidden),
            crate::feedback::RlmFeedbackKind::Policy,
            "a construct outside the dialect is a refusal: {forbidden}"
        );

        // And the imperative the Policy branch chooses is only honest when the
        // diagnostic really does name a form.
        assert!(
            !forbidden.suggestions.is_empty(),
            "a Policy classification promises a named form: {forbidden:?}"
        );

        // One code, both families. `TS_METHOD_UNSUPPORTED` is emitted both for
        // the determinism refusals — which the runtime will never run, however
        // the model rewrites them — and for ordinary arity mistakes. Reading
        // the code alone gets one of the two wrong whichever way it is read.
        let nondeterministic = lash_typescript::parse_with_globals(
            "finish('a'.localeCompare('b'));",
            &BTreeSet::new(),
        )
        .expect_err("locale ordering is refused");
        let miscounted =
            lash_typescript::parse_with_globals("finish([1].map());", &BTreeSet::new())
                .expect_err("map needs a callback");
        assert_eq!(
            nondeterministic.code.as_str(),
            miscounted.code.as_str(),
            "the premise of this check is that one code carries both"
        );
        assert_eq!(
            typescript_feedback_kind(&nondeterministic),
            crate::feedback::RlmFeedbackKind::Policy,
            "the runtime will never run this: {nondeterministic}"
        );
        assert_eq!(
            typescript_feedback_kind(&miscounted),
            crate::feedback::RlmFeedbackKind::Error,
            "the method exists and the call is wrong: {miscounted}"
        );
    }

    /// The executor is where a TypeScript rejection becomes the text a model
    /// reads, and for the whole of the dialect's life that conversion was
    /// `error.to_string()` — which drops the span the diagnostic carries. The
    /// model was told a construct was refused and left to find it.
    #[test]
    fn a_typescript_rejection_reaches_the_model_with_its_own_line_number() {
        let code = "const rows = [1, 2, 3];\nconst total = 0;\nclass Accumulator {}\n";
        let error = lash_typescript::parse_with_globals(code, &BTreeSet::new())
            .expect_err("classes are refused");
        let diagnostic = lash_typescript::format_diagnostic(code, &error);

        assert!(
            diagnostic.starts_with("TS_CLASS_UNSUPPORTED: "),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("--> line 3, column 1"), "{diagnostic}");
        assert!(
            diagnostic.contains("\nclass Accumulator {}\n"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("\nhint: "), "{diagnostic}");
    }
    use crate::projection::{
        ProjectionRef, ProjectionRegistry, flow_record_to_json_value, flow_record_to_tool_args,
        flow_to_json_value, projected_index,
    };
    use lash_core::ProcessRegistry;
    use lash_lashlang_runtime::ToolDefinitionLashlangExt;
    use lash_rlm_types::PROJECTED_JSON_TAG;
    use lash_sansio::sync::MutexExt;
    use lashlang::{
        AbilityOp, AbilityResult, ExecutionEnvironment, ExecutionHost, ExecutionHostError,
        ExecutionOutcome, ProjectedBindings, ProjectedFuture, ProjectedHostDescriptor,
        ProjectedReadRequest, ProjectedReadResponse, ProjectedValue, Record as FlowRecord,
        Value as FlowValue,
    };
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static EXECUTION_BOUND_EXHAUSTION_MODE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[derive(Default)]
    struct NoopHost;

    impl ExecutionHost for NoopHost {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
            match op {
                AbilityOp::ResourceOperation(operation) => Err(ExecutionHostError::new(format!(
                    "unknown module operation: {}",
                    operation.operation
                ))),
                AbilityOp::Finish(value) | AbilityOp::Fail(value) => {
                    Ok(AbilityResult::Value(value))
                }
                _ => Err(ExecutionHostError::new("unsupported host ability")),
            }
        }
    }

    async fn execute_with_projected(
        compiled: &lashlang::CompiledProgram,
        state: &mut lashlang::State,
        projected: &ProjectedBindings,
    ) -> Result<ExecutionOutcome, lashlang::RuntimeError> {
        let env = ExecutionEnvironment::new(&NoopHost).with_projected_bindings(projected.clone());
        lashlang::execute(compiled, state, &env).await
    }

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    fn hydrate_snapshot(
        snapshot: lash_core::plugin::ExecutionStateSnapshot,
    ) -> lash_core::plugin::HydratedExecutionState {
        lash_core::plugin::HydratedExecutionState {
            root: snapshot.root.expect("snapshot root"),
            components: snapshot
                .components
                .into_iter()
                .map(|(key, component)| match component {
                    lash_core::plugin::ExecutionStateComponentSnapshot::Changed(body) => {
                        (key, body)
                    }
                    lash_core::plugin::ExecutionStateComponentSnapshot::Unchanged => {
                        panic!("fresh test snapshot unexpectedly reused `{key}`")
                    }
                })
                .collect(),
        }
    }

    #[derive(Default)]
    struct NoopTraceSink;

    impl lash_core::facade_support::TraceSink for NoopTraceSink {
        fn append(
            &self,
            _record: &lash_core::facade_support::TraceRecord,
        ) -> Result<(), lash_core::facade_support::TraceSinkError> {
            Ok(())
        }
    }

    async fn execute_continue_as_with_trace_sink(
        trace_sink: Option<Arc<dyn lash_core::facade_support::TraceSink>>,
    ) -> lash_core::ToolCallRecord {
        let definition = crate::continue_as_tool_definition();
        let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![definition]);
        let invocation = lash_core::testing::exec_code_invocation(
            "test-session",
            "turn-7",
            7,
            2,
            "exec-code-3",
            "exec-code:3",
        );
        let context =
            lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
                Arc::new(crate::control_tools::RlmControlToolsProvider {
                    vocabulary: crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
                }),
                catalog,
                invocation,
            );
        let (_, response) = execute_code_unbounded_for_tests(
            RlmExecutionState::new().expect("state"),
            context,
            ExecRequest {
                language: "lashlang".to_string(),
                code: r#"await control.continue_as({ task: "continue deterministically" })?"#
                    .to_string(),
                accept_finish: true,
            },
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig {
                sink: trace_sink,
                trace_context: TraceContext::default(),
            },
        )
        .await
        .expect("execute continue_as");
        assert_eq!(response.error, None);
        assert_eq!(response.tool_calls.len(), 1);
        response
            .tool_calls
            .into_iter()
            .next()
            .expect("one continue_as call")
    }

    #[test]
    fn resource_call_identity_is_trace_sink_independent() {
        block_on(async {
            let without_trace = execute_continue_as_with_trace_sink(None).await;
            let with_trace =
                execute_continue_as_with_trace_sink(Some(Arc::new(NoopTraceSink))).await;

            assert_eq!(
                without_trace.call_id.as_deref(),
                Some(
                    "lashlang:effect:test-session:turn-7:exec-code-3:resource:tool:continue_as:resource_operation:82541028d85291e9d3275727:1"
                )
            );
            assert_eq!(
                with_trace.call_id.as_deref(),
                Some(
                    "lashlang:effect:test-session:turn-7:exec-code-3:resource:tool:continue_as:resource_operation:82541028d85291e9d3275727:1"
                )
            );

            let without_trace_key = match without_trace.output.control {
                Some(lash_core::ToolControl::SwitchAgentFrame { frame_key, .. }) => frame_key,
                other => panic!("expected frame switch, got {other:?}"),
            };
            let with_trace_key = match with_trace.output.control {
                Some(lash_core::ToolControl::SwitchAgentFrame { frame_key, .. }) => frame_key,
                other => panic!("expected frame switch, got {other:?}"),
            };
            assert_eq!(
                without_trace_key.as_str(),
                "frame-key/v1/dcbfa2438c3591445220f0d38a3dd6394513c8b91caedbd391db04d98b3a3b63"
            );
            assert_eq!(
                with_trace_key.as_str(),
                "frame-key/v1/dcbfa2438c3591445220f0d38a3dd6394513c8b91caedbd391db04d98b3a3b63"
            );
        });
    }

    async fn execute_test_code(state: RlmExecutionState, code: String) -> RlmExecutionState {
        let (state, response) = Box::pin(execute_code_unbounded_for_tests(
            state,
            lash_core::testing::code_execution_context(),
            ExecRequest {
                language: "lashlang".to_string(),
                code,
                accept_finish: true,
            },
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
        ))
        .await
        .expect("execute test Lashlang");
        assert_eq!(response.error, None, "test Lashlang execution failed");
        state
    }

    struct TestProjectedValue(Vec<FlowValue>);

    #[derive(Default)]
    struct SnapshotProjectedToolText {
        materialize_count: AtomicUsize,
        render_count: AtomicUsize,
    }

    impl ProjectedHostDescriptor for SnapshotProjectedToolText {
        fn type_name(&self) -> &str {
            "string"
        }

        fn read_one(
            &self,
            request: ProjectedReadRequest,
        ) -> ProjectedFuture<'_, ProjectedReadResponse> {
            Box::pin(async move {
                match request {
                    ProjectedReadRequest::Render => {
                        self.render_count.fetch_add(1, Ordering::SeqCst);
                        ProjectedReadResponse::Text("rendered tool text".to_string())
                    }
                    ProjectedReadRequest::Materialize => {
                        self.materialize_count.fetch_add(1, Ordering::SeqCst);
                        ProjectedReadResponse::Value(FlowValue::String(
                            "materialized tool text".into(),
                        ))
                    }
                    _ => ProjectedReadResponse::Missing,
                }
            })
        }
    }

    impl ProjectedHostDescriptor for TestProjectedValue {
        fn type_name(&self) -> &str {
            "list"
        }

        fn read_one(
            &self,
            request: ProjectedReadRequest,
        ) -> ProjectedFuture<'_, ProjectedReadResponse> {
            Box::pin(async move {
                let ProjectedReadRequest::Index(index) = request else {
                    return match request {
                        ProjectedReadRequest::Len => ProjectedReadResponse::Len(self.0.len()),
                        ProjectedReadRequest::Materialize => {
                            ProjectedReadResponse::Value(FlowValue::List(self.0.clone().into()))
                        }
                        _ => ProjectedReadResponse::Missing,
                    };
                };
                let Ok(Some(index)) = projected_index(&index, self.0.len()) else {
                    return ProjectedReadResponse::Missing;
                };
                self.0
                    .get(index)
                    .cloned()
                    .map(ProjectedReadResponse::Value)
                    .unwrap_or(ProjectedReadResponse::Missing)
            })
        }
    }

    fn projected_history(values: Vec<FlowValue>) -> ProjectedBindings {
        let mut projected = ProjectedBindings::new();
        projected.insert(
            "history",
            ProjectedValue::custom("history", Arc::new(TestProjectedValue(values))),
        );
        projected
    }

    async fn execute_with_lashlang_abilities(
        code: &str,
        abilities: lashlang::LashlangAbilities,
    ) -> ExecResponse {
        execute_with_lashlang_host_environment(
            code,
            abilities,
            lashlang::LashlangHostCatalog::new(),
        )
        .await
    }

    async fn execute_with_lashlang_host_environment(
        code: &str,
        abilities: lashlang::LashlangAbilities,
        resources: lashlang::LashlangHostCatalog,
    ) -> ExecResponse {
        let state = RlmExecutionState::new().expect("state");
        let ctx = if abilities.triggers {
            lash_core::testing::code_execution_context_with_trigger_store(Arc::new(
                lash_core::facade_support::InMemoryTriggerStore::default(),
            ))
        } else {
            lash_core::testing::code_execution_context()
        };
        let surface = LashlangSurface::new(
            abilities,
            lashlang::LashlangLanguageFeatures::default(),
            resources,
        );
        let (_, response) = execute_code_with_bounds(
            state,
            ctx,
            ExecRequest {
                language: "lashlang".to_string(),
                code: code.to_string(),
                accept_finish: true,
            },
            Arc::new(lashlang::InMemoryLashlangArtifactStore::new()),
            surface,
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
            lashlang::ExecutionBounds::new(
                lashlang::ExecutionBound::instructions(1_000_000),
                lashlang::ExecutionBound::secs(30),
                lashlang::ExecutionBound::Unbounded,
            ),
        )
        .await
        .expect("execute code");
        response
    }

    #[test]
    #[should_panic(expected = "confidence execution exhausted a required Lashlang bound")]
    fn confidence_execution_fails_loudly_on_bound_exhaustion() {
        let _mode = EXECUTION_BOUND_EXHAUSTION_MODE.lock_recover();
        block_on(async {
            let _ = execute_code_with_bounds(
                RlmExecutionState::new().expect("state"),
                lash_core::testing::code_execution_context(),
                ExecRequest {
                    language: "lashlang".to_string(),
                    code: "i = 0\nwhile i < 5000 { i = i + 1 }\nfinish i".to_string(),
                    accept_finish: true,
                },
                lashlang::global_in_memory_lashlang_artifact_store(),
                LashlangSurface::default(),
                None,
                RlmProjectedBindings::default(),
                Arc::new(ProjectionRegistry::new()),
                RlmLashlangExecutionTraceConfig::default(),
                lashlang::ExecutionBounds::new(
                    lashlang::ExecutionBound::instructions(1),
                    lashlang::ExecutionBound::Unbounded,
                    lashlang::ExecutionBound::Unbounded,
                ),
            )
            .await;
        });
    }

    #[test]
    fn exhaustion_response_remains_testable_when_loudness_is_temporarily_disabled() {
        let _mode = EXECUTION_BOUND_EXHAUSTION_MODE.lock_recover();
        block_on(async {
            let previous = set_execution_bound_exhaustion_loud(false);
            let result = execute_code_with_bounds(
                RlmExecutionState::new().expect("state"),
                lash_core::testing::code_execution_context(),
                ExecRequest {
                    language: "lashlang".to_string(),
                    code: "value = 1".to_string(),
                    accept_finish: true,
                },
                lashlang::global_in_memory_lashlang_artifact_store(),
                LashlangSurface::default(),
                None,
                RlmProjectedBindings::default(),
                Arc::new(ProjectionRegistry::new()),
                RlmLashlangExecutionTraceConfig::default(),
                lashlang::ExecutionBounds::new(
                    lashlang::ExecutionBound::instructions(1),
                    lashlang::ExecutionBound::Unbounded,
                    lashlang::ExecutionBound::Unbounded,
                ),
            )
            .await
            .expect("execution response");
            set_execution_bound_exhaustion_loud(previous);
            assert!(
                result
                    .1
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("instruction budget"))
            );
        });
    }

    #[test]
    fn execute_code_reuses_linked_program_cache_for_repeat_source() {
        block_on(async {
            let state = RlmExecutionState::new().expect("state");
            let request = || ExecRequest {
                language: "lashlang".to_string(),
                code: "finish 1".to_string(),
                accept_finish: true,
            };
            let resolver = || Arc::new(ProjectionRegistry::new());
            let surface = || {
                LashlangSurface::new(
                    lashlang::LashlangAbilities::default(),
                    lashlang::LashlangLanguageFeatures::default(),
                    lashlang::LashlangHostCatalog::new(),
                )
            };

            let (state, first) = execute_code_unbounded_for_tests(
                state,
                lash_core::testing::code_execution_context(),
                request(),
                lashlang::global_in_memory_lashlang_artifact_store(),
                surface(),
                None,
                RlmProjectedBindings::default(),
                resolver(),
                RlmLashlangExecutionTraceConfig::default(),
            )
            .await
            .expect("first execution should succeed");
            assert!(first.error.is_none(), "{:?}", first.error);
            assert_eq!(first.terminal_finish, Some(serde_json::json!(1)));
            let first_stats = state.linked_programs.stats();
            assert_eq!(first_stats.hits, 0);
            assert_eq!(first_stats.misses, 1);

            let (state, second) = execute_code_unbounded_for_tests(
                state,
                lash_core::testing::code_execution_context(),
                request(),
                lashlang::global_in_memory_lashlang_artifact_store(),
                surface(),
                None,
                RlmProjectedBindings::default(),
                resolver(),
                RlmLashlangExecutionTraceConfig::default(),
            )
            .await
            .expect("second execution should succeed");
            assert!(second.error.is_none(), "{:?}", second.error);
            assert_eq!(second.terminal_finish, Some(serde_json::json!(1)));
            let second_stats = state.linked_programs.stats();
            assert_eq!(second_stats.hits, 1);
            assert_eq!(second_stats.misses, 1);
            assert_eq!(second_stats.entries, 1);
            assert!(state.stored_lashlang_modules.is_empty());
        });
    }

    struct CountingDeferredResolver {
        calls: Arc<AtomicUsize>,
        batches: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
        installed: Arc<AtomicUsize>,
    }

    fn deferred_fetch_definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:web_fetch",
            "web_fetch",
            "Fetch a URL",
            lash_core::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "string" }),
        )
        .with_lashlang_binding(lash_lashlang_runtime::LashlangToolBinding::new(
            ["web"],
            "fetch",
        ))
    }

    #[async_trait::async_trait]
    impl lash_lashlang_runtime::DeferredToolResolver for CountingDeferredResolver {
        async fn resolve(
            &self,
            paths: &[&str],
        ) -> BTreeMap<String, lash_lashlang_runtime::Resolution> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.batches
                .lock_recover()
                .push(paths.iter().map(|path| (*path).to_string()).collect());
            paths
                .iter()
                .map(|path| {
                    let resolution = if *path == "web.fetch" {
                        lash_lashlang_runtime::Resolution::Resolved(Box::new(
                            lash_lashlang_runtime::ToolGrant::new(deferred_fetch_definition()),
                        ))
                    } else {
                        lash_lashlang_runtime::Resolution::NotAvailable
                    };
                    ((*path).to_string(), resolution)
                })
                .collect()
        }

        fn install_recorded_grant(&self, path: &str, _grant: &lash_lashlang_runtime::ToolGrant) {
            assert_eq!(path, "web.fetch");
            self.installed.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct BindingRecordingDeferredProvider {
        executions: Arc<AtomicUsize>,
        observed_bindings: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    }

    #[async_trait::async_trait]
    impl lash_core::ToolProvider for BindingRecordingDeferredProvider {
        fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
            Vec::new()
        }

        fn resolve_manifest_by_id(
            &self,
            id: &lash_core::ToolId,
        ) -> Option<lash_core::ToolManifest> {
            (id == &lash_core::ToolId::from("tool:web_fetch"))
                .then(|| deferred_fetch_definition().manifest())
        }

        fn resolve_contract(&self, _name: &str) -> Option<Arc<lash_core::ToolContract>> {
            None
        }

        async fn prepare_granted_tool_call(
            &self,
            _grant: &lash_core::ToolExecutionGrant,
            call: lash_core::ToolPrepareCall<'_>,
        ) -> Result<lash_core::PreparedToolCall, lash_core::ToolOutcome> {
            Ok(lash_core::PreparedToolCall::identity(
                call.tool_id,
                call.pending,
            ))
        }

        async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
            self.executions.fetch_add(1, Ordering::SeqCst);
            self.observed_bindings
                .lock_recover()
                .push(call.context.tool_execution_binding().clone());
            lash_core::ToolOutcome::ok(serde_json::json!("deferred ok"))
        }

        async fn execute_granted(
            &self,
            grant: &lash_core::ToolExecutionGrant,
            args: &serde_json::Value,
            context: &lash_core::AttemptContext<'_>,
        ) -> lash_core::ToolOutcome {
            self.execute_by_id(&grant.manifest.id, args, context).await
        }
    }

    struct BindingDeferredResolver {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl lash_lashlang_runtime::DeferredToolResolver for BindingDeferredResolver {
        async fn resolve(
            &self,
            paths: &[&str],
        ) -> BTreeMap<String, lash_lashlang_runtime::Resolution> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            paths
                .iter()
                .map(|path| {
                    let resolution = if *path == "web.fetch" {
                        lash_lashlang_runtime::Resolution::Resolved(Box::new(
                            lash_lashlang_runtime::ToolGrant::new(deferred_fetch_definition())
                                .with_execution_binding(serde_json::json!({
                                    "kind": "test",
                                    "route": "deferred"
                                })),
                        ))
                    } else {
                        lash_lashlang_runtime::Resolution::NotAvailable
                    };
                    ((*path).to_string(), resolution)
                })
                .collect()
        }
    }

    fn deferred_matrix_request() -> ExecRequest {
        ExecRequest {
            language: "lashlang".to_string(),
            code: "await web.fetch({})?\nawait mystery.x({})?".to_string(),
            accept_finish: true,
        }
    }

    #[test]
    fn deferred_resolution_record_is_scoped_to_the_exec_code_link() {
        block_on(async {
            let calls = Arc::new(AtomicUsize::new(0));
            let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
            let installed = Arc::new(AtomicUsize::new(0));
            let resolver: lash_lashlang_runtime::SharedDeferredToolResolver =
                Arc::new(CountingDeferredResolver {
                    calls: Arc::clone(&calls),
                    batches: Arc::clone(&batches),
                    installed: Arc::clone(&installed),
                });

            let first_invocation = lash_core::testing::exec_code_invocation(
                "test-session",
                "turn-1",
                1,
                0,
                "effect-1",
                "replay:effect-1",
            );
            let first_ctx =
                lash_core::testing::code_execution_context_with_invocation(first_invocation);
            assert!(first_ctx.tool_catalog().tools.is_empty());
            let (mut state, first) = execute_code_unbounded_for_tests(
                RlmExecutionState::new().expect("state"),
                first_ctx.clone(),
                deferred_matrix_request(),
                lashlang::global_in_memory_lashlang_artifact_store(),
                LashlangSurface::default(),
                Some(resolver.clone()),
                RlmProjectedBindings::default(),
                Arc::new(ProjectionRegistry::new()),
                RlmLashlangExecutionTraceConfig::default(),
            )
            .await
            .expect("first drive");
            assert!(first.error.is_some(), "mystery.x must remain unresolved");
            assert_eq!(calls.load(Ordering::SeqCst), 1, "one batch per link");
            assert_eq!(installed.load(Ordering::SeqCst), 0);
            assert!(matches!(
                state.deferred_resolutions.get("web.fetch"),
                Some(lash_lashlang_runtime::Resolution::Resolved(_))
            ));
            assert!(matches!(
                state.deferred_resolutions.get("mystery.x"),
                Some(lash_lashlang_runtime::Resolution::NotAvailable)
            ));
            assert!(first_ctx.tool_catalog().tools.is_empty());

            let snapshot = hydrate_snapshot(
                state
                    .snapshot_execution_state()
                    .expect("snapshot components"),
            );
            let mut restored = RlmExecutionState::new().expect("state");
            restored
                .restore_execution_state(&snapshot)
                .expect("restore");

            // Same stable link: both positive and negative outcomes survive the
            // snapshot and win without another authorization decision.
            let (restored, replay) = execute_code_unbounded_for_tests(
                restored,
                first_ctx.clone(),
                deferred_matrix_request(),
                lashlang::global_in_memory_lashlang_artifact_store(),
                LashlangSurface::default(),
                Some(resolver.clone()),
                RlmProjectedBindings::default(),
                Arc::new(ProjectionRegistry::new()),
                RlmLashlangExecutionTraceConfig::default(),
            )
            .await
            .expect("same-link replay");
            assert!(replay.error.is_some());
            assert_eq!(calls.load(Ordering::SeqCst), 1, "same link must replay");
            assert_eq!(installed.load(Ordering::SeqCst), 1);

            // A second code effect in the same logical turn is a different link
            // and must resolve the same paths against current authority.
            let second_ctx = lash_core::testing::code_execution_context_with_invocation(
                lash_core::testing::exec_code_invocation(
                    "test-session",
                    "turn-1",
                    1,
                    0,
                    "effect-2",
                    "replay:effect-2",
                ),
            );
            let (restored, second_link) = execute_code_unbounded_for_tests(
                restored,
                second_ctx.clone(),
                deferred_matrix_request(),
                lashlang::global_in_memory_lashlang_artifact_store(),
                LashlangSurface::default(),
                Some(resolver.clone()),
                RlmProjectedBindings::default(),
                Arc::new(ProjectionRegistry::new()),
                RlmLashlangExecutionTraceConfig::default(),
            )
            .await
            .expect("different link");
            assert!(second_link.error.is_some());
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            assert_eq!(installed.load(Ordering::SeqCst), 1);
            assert!(second_ctx.tool_catalog().tools.is_empty());

            // A new logical turn also selects a fresh record, even when the
            // program references exactly the same paths.
            let next_turn_ctx = lash_core::testing::code_execution_context_with_invocation(
                lash_core::testing::exec_code_invocation(
                    "test-session",
                    "turn-2",
                    2,
                    0,
                    "effect-3",
                    "replay:effect-3",
                ),
            );
            let (restored, next_turn) = execute_code_unbounded_for_tests(
                restored,
                next_turn_ctx.clone(),
                deferred_matrix_request(),
                lashlang::global_in_memory_lashlang_artifact_store(),
                LashlangSurface::default(),
                Some(resolver),
                RlmProjectedBindings::default(),
                Arc::new(ProjectionRegistry::new()),
                RlmLashlangExecutionTraceConfig::default(),
            )
            .await
            .expect("new turn");
            assert!(next_turn.error.is_some());
            assert_eq!(calls.load(Ordering::SeqCst), 3);
            assert_eq!(installed.load(Ordering::SeqCst), 1);
            assert!(next_turn_ctx.tool_catalog().tools.is_empty());
            assert_eq!(restored.deferred_resolutions.resolutions.len(), 2);
            assert_eq!(
                *batches.lock_recover(),
                vec![
                    vec!["mystery.x".to_string(), "web.fetch".to_string()],
                    vec!["mystery.x".to_string(), "web.fetch".to_string()],
                    vec!["mystery.x".to_string(), "web.fetch".to_string()],
                ]
            );
        });
    }

    #[test]
    fn deferred_call_executes_through_grant_without_mutating_catalog() {
        block_on(async {
            let resolver_calls = Arc::new(AtomicUsize::new(0));
            let executions = Arc::new(AtomicUsize::new(0));
            let observed_bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
            let resolver: lash_lashlang_runtime::SharedDeferredToolResolver =
                Arc::new(BindingDeferredResolver {
                    calls: Arc::clone(&resolver_calls),
                });
            let provider: Arc<dyn lash_core::ToolProvider> =
                Arc::new(BindingRecordingDeferredProvider {
                    executions: Arc::clone(&executions),
                    observed_bindings: Arc::clone(&observed_bindings),
                });
            let ctx = lash_core::testing::code_execution_context_with_tool_provider_and_catalog(
                provider,
                lash_core::ToolCatalog::from_tool_definitions(Vec::new()),
            );
            assert!(ctx.tool_catalog().tools.is_empty());

            let (state, response) = execute_code_unbounded_for_tests(
                RlmExecutionState::new().expect("state"),
                ctx.clone(),
                ExecRequest {
                    language: "lashlang".to_string(),
                    code: r#"
                        result = await web.fetch({ url: "https://example.test" })?
                        finish result
                    "#
                    .to_string(),
                    accept_finish: true,
                },
                lashlang::global_in_memory_lashlang_artifact_store(),
                LashlangSurface::default(),
                Some(resolver),
                RlmProjectedBindings::default(),
                Arc::new(ProjectionRegistry::new()),
                RlmLashlangExecutionTraceConfig::default(),
            )
            .await
            .expect("execute code");

            assert!(response.error.is_none(), "{:?}", response.error);
            assert_eq!(
                response.terminal_finish,
                Some(serde_json::json!("deferred ok"))
            );
            assert_eq!(resolver_calls.load(Ordering::SeqCst), 1);
            assert_eq!(executions.load(Ordering::SeqCst), 1);
            assert_eq!(
                response.executed_calls,
                vec![lash_core::ExecutedCallRecord {
                    operation: "web.fetch".to_string(),
                    outcome: lash_core::ExecutedCallOutcome::Ok,
                }],
                "the ledger must retain the source module.operation, not the host tool id"
            );
            assert_eq!(
                *observed_bindings.lock_recover(),
                vec![serde_json::json!({ "kind": "test", "route": "deferred" })]
            );
            assert!(ctx.tool_catalog().tools.is_empty());
            assert!(matches!(
                state.deferred_resolutions.get("web.fetch"),
                Some(lash_lashlang_runtime::Resolution::Resolved(_))
            ));
        });
    }

    #[test]
    fn typescript_deferred_call_executes_through_the_same_grant_path() {
        block_on(async {
            let resolver_calls = Arc::new(AtomicUsize::new(0));
            let executions = Arc::new(AtomicUsize::new(0));
            let observed_bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
            let resolver: lash_lashlang_runtime::SharedDeferredToolResolver =
                Arc::new(BindingDeferredResolver {
                    calls: Arc::clone(&resolver_calls),
                });
            let provider: Arc<dyn lash_core::ToolProvider> =
                Arc::new(BindingRecordingDeferredProvider {
                    executions: Arc::clone(&executions),
                    observed_bindings: Arc::clone(&observed_bindings),
                });
            let ctx = lash_core::testing::code_execution_context_with_tool_provider_and_catalog(
                provider,
                lash_core::ToolCatalog::from_tool_definitions(Vec::new()),
            );

            let (state, response) = execute_typescript_code_with_bounds(
                RlmExecutionState::for_engine("typescript").expect("TypeScript state"),
                ctx.clone(),
                ExecRequest {
                    language: "typescript".to_string(),
                    code: "const result = await web.fetch({ url: 'https://example.test' }); finish(result);".to_string(),
                    accept_finish: true,
                },
                lashlang::global_in_memory_lashlang_artifact_store(),
                LashlangSurface::default(),
                Some(resolver),
                RlmProjectedBindings::default(),
                Arc::new(ProjectionRegistry::new()),
                RlmLashlangExecutionTraceConfig::default(),
                lashlang::ExecutionBounds::unbounded(),
            )
            .await
            .expect("execute TypeScript deferred call");

            assert!(response.error.is_none(), "{:?}", response.error);
            assert_eq!(
                response.terminal_finish,
                Some(serde_json::json!("deferred ok"))
            );
            assert_eq!(resolver_calls.load(Ordering::SeqCst), 1);
            assert_eq!(executions.load(Ordering::SeqCst), 1);
            assert!(ctx.tool_catalog().tools.is_empty());
            assert!(matches!(
                state.deferred_resolutions.get("web.fetch"),
                Some(lash_lashlang_runtime::Resolution::Resolved(_))
            ));
        });
    }

    #[test]
    fn execute_code_stores_process_module_artifact_once() {
        block_on(async {
            let state = RlmExecutionState::new().expect("state");
            let request = || ExecRequest {
                language: "lashlang".to_string(),
                code: "process later() { finish 1 }\nfinish 1".to_string(),
                accept_finish: true,
            };
            let resolver = || Arc::new(ProjectionRegistry::new());
            let context = || lash_core::testing::code_execution_context();
            let surface = || {
                LashlangSurface::new(
                    lashlang::LashlangAbilities::default().with_processes(),
                    lashlang::LashlangLanguageFeatures::default(),
                    lashlang::LashlangHostCatalog::new(),
                )
            };

            let (state, first) = execute_code_unbounded_for_tests(
                state,
                context(),
                request(),
                lashlang::global_in_memory_lashlang_artifact_store(),
                surface(),
                None,
                RlmProjectedBindings::default(),
                resolver(),
                RlmLashlangExecutionTraceConfig::default(),
            )
            .await
            .expect("first process module execution should succeed");
            assert!(first.error.is_none(), "{:?}", first.error);
            assert_eq!(state.stored_lashlang_modules.len(), 1);

            let (state, second) = execute_code_unbounded_for_tests(
                state,
                context(),
                request(),
                lashlang::global_in_memory_lashlang_artifact_store(),
                surface(),
                None,
                RlmProjectedBindings::default(),
                resolver(),
                RlmLashlangExecutionTraceConfig::default(),
            )
            .await
            .expect("second process module execution should succeed");
            assert!(second.error.is_none(), "{:?}", second.error);
            assert_eq!(state.stored_lashlang_modules.len(), 1);
            let stats = state.linked_programs.stats();
            assert_eq!(stats.hits, 1);
            assert_eq!(stats.misses, 1);
        });
    }

    #[test]
    fn typescript_executor_stores_a_typescript_process_artifact() {
        block_on(async {
            let artifact_store = Arc::new(lashlang::InMemoryLashlangArtifactStore::new());
            let (state, response) = execute_typescript_code_with_bounds(
                RlmExecutionState::for_engine("typescript").expect("TypeScript state"),
                lash_core::testing::code_execution_context(),
                ExecRequest {
                    language: "typescript".to_string(),
                    code: r#"
                        const worker = defineProcess({
                          name: "worker", signals: {},
                          run: async (input: unknown) => { return input; }
                        });
                        finish(1);
                    "#
                    .to_string(),
                    accept_finish: true,
                },
                artifact_store.clone(),
                LashlangSurface::new(
                    lashlang::LashlangAbilities::default().with_processes(),
                    lashlang::LashlangLanguageFeatures::default(),
                    lashlang::LashlangHostCatalog::new(),
                ),
                None,
                RlmProjectedBindings::default(),
                Arc::new(ProjectionRegistry::new()),
                RlmLashlangExecutionTraceConfig::default(),
                lashlang::ExecutionBounds::unbounded(),
            )
            .await
            .expect("execute TypeScript process declaration");
            assert!(response.error.is_none(), "{:?}", response.error);
            let module_ref = state
                .stored_lashlang_modules
                .iter()
                .next()
                .expect("stored process module");
            let artifact = lashlang::LashlangArtifactStore::get_module_artifact(
                artifact_store.as_ref(),
                module_ref,
            )
            .await
            .expect("read stored artifact")
            .expect("artifact exists");
            assert_eq!(
                artifact.compilation_dialect,
                lashlang::CompilationDialect::Typescript
            );
        });
    }

    #[derive(Clone)]
    struct TypeScriptSignalProcessService {
        registry: Arc<lash_core::TestLocalProcessRegistry>,
        controller: Arc<dyn lash_core::RuntimeEffectController>,
    }

    struct EmptyTypeScriptSignalToolProvider;

    fn status_inspect_definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:status_inspect",
            "status_inspect",
            "Inspect a process status",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "process_id": { "type": "string" }
                },
                "required": ["process_id"]
            }),
            serde_json::json!({ "type": "string" }),
        )
        .with_lashlang_binding(lash_lashlang_runtime::LashlangToolBinding::new(
            ["status_tool"],
            "inspect",
        ))
    }

    struct TypeScriptProcessInspectionToolProvider {
        inspected_process_id: Arc<std::sync::Mutex<Option<String>>>,
    }

    #[async_trait::async_trait]
    impl lash_core::ToolProvider for TypeScriptProcessInspectionToolProvider {
        fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
            vec![status_inspect_definition().manifest()]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
            (name == "status_inspect" || name == "tool:status_inspect")
                .then(|| Arc::new(status_inspect_definition().contract()))
        }

        async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
            if call.name == "status_inspect" || call.name == "tool:status_inspect" {
                let pid = call
                    .args
                    .get("process_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                *self.inspected_process_id.lock().unwrap() = pid;
                lash_core::ToolOutcome::ok(serde_json::json!("inspected-ok"))
            } else {
                lash_core::ToolOutcome::err(serde_json::json!(format!(
                    "unknown tool `{}`",
                    call.name
                )))
            }
        }
    }

    #[async_trait::async_trait]
    impl lash_core::ToolProvider for EmptyTypeScriptSignalToolProvider {
        fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
            Vec::new()
        }

        fn resolve_contract(&self, _name: &str) -> Option<Arc<lash_core::ToolContract>> {
            None
        }

        async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
            lash_core::ToolOutcome::err(serde_json::json!(format!(
                "signal round-trip test has no tool `{}`",
                call.name
            )))
        }
    }

    #[async_trait::async_trait]
    impl lash_core::ProcessService for TypeScriptSignalProcessService {
        // The recorded-intent routes belong to atomic tool attempts, which this
        // signal fixture never opens. Refuse them rather than pretend, so a test
        // that starts using them fails loudly instead of silently taking a
        // non-atomic path.
        async fn start_from_recorded_intent(
            &self,
            _session_id: &str,
            _request: lash_core::ProcessStartRequest,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessHandleView, lash_core::PluginError> {
            Err(lash_core::PluginError::Session(
                "recorded process starts are unavailable in this test".to_string(),
            ))
        }

        async fn cancel_recorded_intent(
            &self,
            _session_id: &str,
            _process_id: &str,
            _reason: Option<String>,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessRecord, lash_core::PluginError> {
            Err(lash_core::PluginError::Session(
                "recorded process cancellation is unavailable in this test".to_string(),
            ))
        }

        async fn finish_recorded_intent_parent(
            &self,
            _session_id: &str,
            _identity: lash_core::ToolIntentIdentity,
            _process_id: String,
            _policy: lash_core::ProcessParentEndPolicy,
            _reason: String,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ToolIntentParentEndOutcome, lash_core::PluginError> {
            Err(lash_core::PluginError::Session(
                "recorded parent end is unavailable in this test".to_string(),
            ))
        }

        async fn signal_recorded_intent(
            &self,
            _session_id: &str,
            _process_id: &str,
            _signal: String,
            _call_id: String,
            _payload: serde_json::Value,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessEvent, lash_core::PluginError> {
            Err(lash_core::PluginError::Session(
                "recorded process signals are unavailable in this test".to_string(),
            ))
        }

        async fn emit_event_recorded_intent(
            &self,
            _session_id: &str,
            _process_id: &str,
            _event: String,
            _call_id: String,
            _payload: serde_json::Value,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessEvent, lash_core::PluginError> {
            Err(lash_core::PluginError::Session(
                "recorded process events are unavailable in this test".to_string(),
            ))
        }

        async fn start(
            &self,
            _session_id: &str,
            registration: lash_core::ProcessRegistration,
            options: lash_core::ProcessStartOptions,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessRecord, lash_core::PluginError> {
            lash_core::ProcessRegistry::register_process_with_observers(
                self.registry.as_ref(),
                registration,
                &options.initial_observers,
            )
            .await
        }

        async fn await_process(
            &self,
            process_id: &str,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessAwaitOutput, lash_core::PluginError> {
            let registry: Arc<dyn lash_core::ProcessRegistry> = self.registry.clone();
            lash_core::facade_support::ProcessAwaiter::polling(registry)
                .await_terminal(process_id)
                .await
        }

        async fn list_visible(
            &self,
            session_id: &str,
            mode: lash_core::ProcessListMode,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<Vec<lash_core::ProcessRecord>, lash_core::PluginError> {
            match mode {
                lash_core::ProcessListMode::Live => {
                    self.registry.list_live_observed_by(session_id).await
                }
                lash_core::ProcessListMode::All => self.registry.list_observed_by(session_id).await,
            }
        }

        async fn validate_visible(
            &self,
            session_id: &str,
            process_ids: &[String],
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<(), lash_core::PluginError> {
            for process_id in process_ids {
                if !self.registry.is_observer(session_id, process_id).await? {
                    return Err(lash_core::PluginError::Session(format!(
                        "process `{process_id}` is not visible"
                    )));
                }
            }
            Ok(())
        }

        async fn cancel(
            &self,
            _session_id: &str,
            _process_id: &str,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessRecord, lash_core::PluginError> {
            Err(lash_core::PluginError::Session(
                "process cancellation is unused in this test".to_string(),
            ))
        }

        async fn signal(
            &self,
            session_id: &str,
            process_id: &str,
            signal_name: String,
            signal_id: String,
            payload: Value,
            scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessEvent, lash_core::PluginError> {
            // The signal itself goes through the shared effect-backed service,
            // which wires the process effect controller the durable signal
            // route requires. What this fixture adds is the waiter side: the
            // await key the TypeScript program is parked on has to resolve
            // with the delivered payload.
            let event = lash_core::testing::effect_backed_process_service(self.registry.clone())
                .signal(
                    session_id,
                    process_id,
                    signal_name.clone(),
                    signal_id,
                    payload,
                    scope,
                )
                .await?;
            let event = Box::new(event);
            let ordinal = lash_core::ProcessRegistry::count_events_through(
                self.registry.as_ref(),
                process_id,
                event.event_type.as_str(),
                event.sequence,
            )
            .await?;
            let key = self
                .controller
                .await_event_key(
                    &lash_core::ExecutionScope::process(process_id),
                    lash_core::AwaitEventWaitIdentity::process_signal(
                        process_id,
                        &signal_name,
                        ordinal,
                    ),
                )
                .await
                .map_err(|error| lash_core::PluginError::Session(error.to_string()))?;
            // The durable signal route resolves the waiter itself, so this
            // fixture asserts the delivery rather than performing it: a second
            // resolution must report the terminal the program will observe.
            let resolved = self
                .controller
                .resolve_await_event(&key, lash_core::Resolution::Ok(event.payload.clone()))
                .await
                .map_err(|error| lash_core::PluginError::Session(error.to_string()))?;
            assert_eq!(
                resolved,
                lash_core::ResolveOutcome::AlreadyResolved {
                    terminal: lash_core::Resolution::Ok(event.payload.clone()),
                },
                "the durable signal route must have delivered the payload to the waiter"
            );
            Ok(*event)
        }

        async fn transfer(
            &self,
            _from_session_id: &str,
            _to_session_id: &str,
            _process_ids: Vec<String>,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<(), lash_core::PluginError> {
            Err(lash_core::PluginError::Session(
                "process transfer is unused in this test".to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn typescript_signal_round_trip_crosses_protocol_and_process_engine() {
        let artifact_store: Arc<dyn lashlang::LashlangArtifactStore> =
            Arc::new(lashlang::InMemoryLashlangArtifactStore::new());
        let registry = Arc::new(lash_core::TestLocalProcessRegistry::default());
        let process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore> =
            Arc::new(lash_core::facade_support::InMemoryProcessExecutionEnvStore::new());
        let controller: Arc<dyn lash_core::RuntimeEffectController> = Arc::new(
            lash_core::facade_support::InlineRuntimeEffectController::default()
                .allow_process_lifetime_completion_keys(),
        );
        let surface = LashlangSurface::new(
            lashlang::LashlangAbilities::default()
                .with_processes()
                .with_process_signals(),
            lashlang::LashlangLanguageFeatures::default(),
            lashlang::LashlangHostCatalog::new(),
        );
        let session_policy = lash_core::SessionPolicy {
            model: lash_core::ModelSpec::builder("mock-model")
                .context_window_tokens(200_000)
                .build()
                .expect("TypeScript signal test model"),
            ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        };
        let runtime_host = lash_core::facade_support::RuntimeHostConfig::new(
            Arc::new(
                lash_core::facade_support::InlineEffectHost::new(controller.clone())
                    .allow_process_lifetime_completion_keys(),
            ),
            Arc::new(lash_core::facade_support::InMemoryAttachmentStore::new()),
            process_env_store.clone(),
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
        )
        .with_process_engine(Arc::new(lash_lashlang_runtime::LashlangProcessEngine::new(
            artifact_store.clone(),
            surface.clone(),
        )));
        let registry_dyn: Arc<dyn lash_core::ProcessRegistry> = registry.clone();
        let worker = lash_core::facade_support::DurableProcessWorker::new(
            lash_core::facade_support::DurableProcessWorkerConfig::new(
                Arc::new(lash_core::facade_support::PluginHost::new(
                    lash_core::testing::test_code_protocol_factories(),
                )),
                runtime_host,
                Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
                registry_dyn,
                lash_core::testing::runtime_lease_owner(),
            )
            .with_session_policy(session_policy.clone()),
        );
        let processes: Arc<dyn lash_core::ProcessService> =
            Arc::new(TypeScriptSignalProcessService {
                registry: registry.clone(),
                controller: controller.clone(),
            });
        let ctx = lash_core::testing::code_execution_context_with_process_dependencies(
            Arc::new(EmptyTypeScriptSignalToolProvider),
            lash_core::ToolCatalog::from_tool_definitions(Vec::new()),
            None,
            processes,
            controller,
            process_env_store,
            lash_core::ProcessExecutionEnvSpec::new(
                lash_core::PluginOptions::default(),
                session_policy,
            ),
        );
        let (_, response) = execute_typescript_code_with_bounds(
            RlmExecutionState::for_engine("typescript").expect("TypeScript state"),
            ctx,
            ExecRequest {
                language: "typescript".to_string(),
                code: r#"
                    const worker = defineProcess({
                      name: "worker", signals: { ready: null },
                      run: async () => await waitSignal("ready")
                    });
                    const handle = start(worker);
                    wake(handle, "ready", { ok: true });
                    finish("signal-sent");
                "#
                .to_string(),
                accept_finish: true,
            },
            artifact_store,
            surface,
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
            lashlang::ExecutionBounds::unbounded(),
        )
        .await
        .expect("execute TypeScript signal round-trip");
        assert!(response.error.is_none(), "{:?}", response.error);
        assert_eq!(
            response.terminal_finish,
            Some(serde_json::json!("signal-sent"))
        );

        let _ = worker
            .drive_pending_processes()
            .await
            .expect("drive signalled TypeScript process");
        let records = registry
            .list_observed_by("test-session")
            .await
            .expect("list started TypeScript process");
        let [record] = records.as_slice() else {
            panic!("expected exactly one started TypeScript process, got {records:?}");
        };
        let registry_dyn: Arc<dyn lash_core::ProcessRegistry> = registry.clone();
        let terminal = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            lash_core::facade_support::ProcessAwaiter::polling(registry_dyn)
                .await_terminal(&record.id),
        )
        .await
        {
            Ok(output) => output.expect("await TypeScript signal process"),
            Err(_) => panic!(
                "TypeScript signal process reaches terminal state: {:?}",
                registry.get_process(&record.id).await
            ),
        };
        assert_eq!(
            terminal,
            lash_core::ProcessAwaitOutput::Success {
                value: serde_json::json!({ "ok": true }),
                control: None,
            }
        );
    }

    #[tokio::test]
    async fn typescript_cell_reads_process_handle_id_and_invokes_subsequent_operation() {
        let artifact_store: Arc<dyn lashlang::LashlangArtifactStore> =
            Arc::new(lashlang::InMemoryLashlangArtifactStore::new());
        let registry = Arc::new(lash_core::TestLocalProcessRegistry::default());
        let process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore> =
            Arc::new(lash_core::facade_support::InMemoryProcessExecutionEnvStore::new());
        let controller: Arc<dyn lash_core::RuntimeEffectController> = Arc::new(
            lash_core::facade_support::InlineRuntimeEffectController::default()
                .allow_process_lifetime_completion_keys(),
        );
        let inspected = Arc::new(std::sync::Mutex::new(None));
        let tool_provider = Arc::new(TypeScriptProcessInspectionToolProvider {
            inspected_process_id: Arc::clone(&inspected),
        });
        let tool_catalog =
            lash_core::ToolCatalog::from_tool_definitions(vec![status_inspect_definition()]);
        let surface = LashlangSurface::new(
            lashlang::LashlangAbilities::default()
                .with_processes()
                .with_process_signals(),
            lashlang::LashlangLanguageFeatures::default(),
            lash_lashlang_runtime::lashlang_resources_from_tool_catalog(&tool_catalog)
                .expect("surface resources"),
        );
        let session_policy = lash_core::SessionPolicy {
            model: lash_core::ModelSpec::builder("mock-model")
                .context_window_tokens(200_000)
                .build()
                .expect("TypeScript process handle id test model"),
            ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        };
        let runtime_host = lash_core::facade_support::RuntimeHostConfig::new(
            Arc::new(
                lash_core::facade_support::InlineEffectHost::new(controller.clone())
                    .allow_process_lifetime_completion_keys(),
            ),
            Arc::new(lash_core::facade_support::InMemoryAttachmentStore::new()),
            process_env_store.clone(),
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
        )
        .with_process_engine(Arc::new(lash_lashlang_runtime::LashlangProcessEngine::new(
            artifact_store.clone(),
            surface.clone(),
        )));
        let registry_dyn: Arc<dyn lash_core::ProcessRegistry> = registry.clone();
        let _worker = lash_core::facade_support::DurableProcessWorker::new(
            lash_core::facade_support::DurableProcessWorkerConfig::new(
                Arc::new(lash_core::facade_support::PluginHost::new(
                    lash_core::testing::test_code_protocol_factories(),
                )),
                runtime_host,
                Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
                registry_dyn,
                lash_core::testing::runtime_lease_owner(),
            )
            .with_session_policy(session_policy.clone()),
        );
        let processes: Arc<dyn lash_core::ProcessService> =
            Arc::new(TypeScriptSignalProcessService {
                registry: registry.clone(),
                controller: controller.clone(),
            });
        let ctx = lash_core::testing::code_execution_context_with_process_dependencies(
            tool_provider,
            tool_catalog,
            None,
            processes,
            controller,
            process_env_store,
            lash_core::ProcessExecutionEnvSpec::new(
                lash_core::PluginOptions::default(),
                session_policy,
            ),
        );
        let (_, response) = execute_typescript_code_with_bounds(
            RlmExecutionState::for_engine("typescript").expect("TypeScript state"),
            ctx,
            ExecRequest {
                language: "typescript".to_string(),
                code: r#"
                    const worker = defineProcess({
                      name: "worker", signals: {},
                      run: async () => { return "done"; }
                    });
                    const handle = start(worker);
                    const processId = handle.id;
                    const status = await status_tool.inspect({ process_id: processId });
                    finish({ id: processId, status: status });
                "#
                .to_string(),
                accept_finish: true,
            },
            artifact_store,
            surface,
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
            lashlang::ExecutionBounds::unbounded(),
        )
        .await
        .expect("execute TypeScript start handle id cell");

        assert!(response.error.is_none(), "{:?}", response.error);
        let finish = response.terminal_finish.expect("finish result");
        let finish_id = finish
            .get("id")
            .and_then(|v| v.as_str())
            .expect("id string");
        assert!(!finish_id.is_empty(), "id must not be empty");
        assert_eq!(
            finish.get("status"),
            Some(&serde_json::json!("inspected-ok"))
        );

        let recorded_pid = inspected
            .lock()
            .unwrap()
            .clone()
            .expect("inspected process id");
        assert_eq!(finish_id, recorded_pid);
    }

    fn timer_trigger_resources() -> lashlang::LashlangHostCatalog {
        let mut resources = lashlang::LashlangHostCatalog::new();
        lashlang::add_trigger_resource_operations(&mut resources);
        resources
            .add_trigger_source_constructor(
                ["timer", "Schedule"],
                lashlang::TypeExpr::Object(vec![
                    lashlang::TypeField {
                        name: "expr".into(),
                        ty: lashlang::TypeExpr::Str,
                        optional: false,
                    },
                    lashlang::TypeField {
                        name: "tz".into(),
                        ty: lashlang::TypeExpr::Str,
                        optional: true,
                    },
                ]),
                lashlang::NamedDataType::object(
                    "timer.Tick",
                    vec![lashlang::TypeField {
                        name: "fired_at".into(),
                        ty: lashlang::TypeExpr::Str,
                        optional: false,
                    }],
                )
                .expect("valid timer tick type"),
            )
            .expect("valid timer trigger source");
        resources
    }

    #[derive(Clone, Default)]
    struct CapturingTriggerEffectController {
        envelopes: Arc<std::sync::Mutex<Vec<lash_core::RuntimeEffectEnvelope>>>,
    }

    impl lash_core::AwaitEventResolver for CapturingTriggerEffectController {}

    #[async_trait::async_trait]
    impl lash_core::RuntimeEffectController for CapturingTriggerEffectController {
        async fn execute_effect(
            &self,
            envelope: lash_core::RuntimeEffectEnvelope,
            local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
        ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError>
        {
            self.envelopes.lock_recover().push(envelope.clone());
            match envelope.command {
                lash_core::RuntimeEffectCommand::Trigger { command } => {
                    let operation_id = envelope
                        .invocation
                        .effect_id()
                        .expect("captured trigger effect id")
                        .to_string();
                    let result = local_executor
                        .into_trigger()?
                        .execute(&operation_id, *command)
                        .await?;
                    Ok(lash_core::RuntimeEffectOutcome::Trigger {
                        result: Box::new(result),
                    })
                }
                _ => local_executor.execute(envelope).await,
            }
        }
    }

    impl CapturingTriggerEffectController {
        fn trigger_effects(&self) -> Vec<(String, &'static str)> {
            self.envelopes
                .lock_recover()
                .iter()
                .filter_map(|envelope| {
                    let lash_core::RuntimeEffectCommand::Trigger { command } = &envelope.command
                    else {
                        return None;
                    };
                    let operation = match command.as_ref() {
                        lash_core::TriggerCommand::Register { .. } => "register",
                        lash_core::TriggerCommand::List { .. } => "list",
                        lash_core::TriggerCommand::Update { .. } => "update",
                        lash_core::TriggerCommand::Enable { .. } => "enable",
                        lash_core::TriggerCommand::Disable { .. } => "disable",
                        lash_core::TriggerCommand::Delete { .. } => "delete",
                        lash_core::TriggerCommand::Revive { .. } => "revive",
                        lash_core::TriggerCommand::Prune { .. } => "prune",
                    };
                    Some((
                        envelope
                            .invocation
                            .effect_id()
                            .unwrap_or_default()
                            .to_string(),
                        operation,
                    ))
                })
                .collect()
        }
    }

    async fn execute_with_capturing_trigger_effects(
        code: &str,
        controller: CapturingTriggerEffectController,
    ) -> ExecResponse {
        let state = RlmExecutionState::new().expect("state");
        let ctx =
            lash_core::testing::code_execution_context_with_trigger_store_and_effect_controller(
                Arc::new(lash_core::facade_support::InMemoryTriggerStore::default()),
                Arc::new(controller),
            );
        let surface = LashlangSurface::new(
            lashlang::LashlangAbilities::default()
                .with_processes()
                .with_triggers(),
            lashlang::LashlangLanguageFeatures::default(),
            timer_trigger_resources(),
        );
        let (_, response) = execute_code_unbounded_for_tests(
            state,
            ctx,
            ExecRequest {
                language: "lashlang".to_string(),
                code: code.to_string(),
                accept_finish: true,
            },
            Arc::new(lashlang::InMemoryLashlangArtifactStore::new()),
            surface,
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await
        .expect("execute trigger code");
        response
    }

    async fn execute_with_trigger_environment(code: &str) -> ExecResponse {
        execute_with_lashlang_host_environment(
            code,
            lashlang::LashlangAbilities::default()
                .with_processes()
                .with_triggers(),
            timer_trigger_resources(),
        )
        .await
    }

    async fn execute_typescript_with_trigger_environment(code: &str) -> ExecResponse {
        let state = RlmExecutionState::for_engine("typescript").expect("TypeScript state");
        let (_, response) = execute_typescript_code_with_bounds(
            state,
            lash_core::testing::code_execution_context_with_trigger_store(Arc::new(
                lash_core::facade_support::InMemoryTriggerStore::default(),
            )),
            ExecRequest {
                language: "typescript".to_string(),
                code: code.to_string(),
                accept_finish: true,
            },
            Arc::new(lashlang::InMemoryLashlangArtifactStore::new()),
            LashlangSurface::new(
                lashlang::LashlangAbilities::default()
                    .with_processes()
                    .with_triggers(),
                lashlang::LashlangLanguageFeatures::default(),
                timer_trigger_resources(),
            ),
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
            lashlang::ExecutionBounds::unbounded(),
        )
        .await
        .expect("execute TypeScript trigger code");
        response
    }

    #[test]
    fn typescript_register_trigger_executes_end_to_end() {
        block_on(async {
            let response = execute_typescript_with_trigger_environment(
                r#"
                const remember = defineProcess({
                  name: "remember", signals: {},
                  run: async (tick: unknown) => { return true; }
                });
                const source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" });
                const handle = await registerTrigger({
                  source,
                  target: remember,
                  inputs: { tick: trigger.event },
                  name: "remembered"
                });
                finish(handle);
                "#,
            )
            .await;

            assert!(response.error.is_none(), "{:?}", response.error);
            let handle = response.terminal_finish.expect("terminal finish");
            assert_eq!(handle["type"], serde_json::json!("trigger_handle"));
            assert_eq!(
                response.executed_calls,
                vec![lash_core::ExecutedCallRecord {
                    operation: "triggers.register".to_string(),
                    outcome: lash_core::ExecutedCallOutcome::Ok,
                }]
            );
        });
    }

    #[test]
    fn trigger_registry_operations_execute_foreground_code() {
        block_on(async {
            let response = execute_with_trigger_environment(
                r#"
                process remember(tick: timer.Tick) {
                  finish true
                }

                source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
                handle = await triggers.register({
                  source: source,
                  target: remember,
                  inputs: { tick: trigger.event },
                  name: "remembered"
                })?
                registrations = await triggers.list({ target: remember })?

                finish { answer: "foreground ran", handle: handle, registrations: registrations }
                "#,
            )
            .await;

            assert!(response.error.is_none(), "{:?}", response.error);
            assert!(response.observations.is_empty());
            let finish = response.terminal_finish.expect("terminal finish");
            assert_eq!(finish["answer"], serde_json::json!("foreground ran"));
            assert_eq!(
                finish["handle"]["type"],
                serde_json::json!("trigger_handle")
            );
            assert_eq!(
                finish["registrations"][0]["name"],
                serde_json::json!("remembered")
            );
            assert_eq!(
                finish["registrations"][0]["source"]["$lash_host_descriptor_type"],
                serde_json::json!("timer.Schedule")
            );
            assert_eq!(
                finish["registrations"][0]["source"]["$lash_host_descriptor_value"]["expr"],
                serde_json::json!("0 8 * * *")
            );
            assert_eq!(
                finish["registrations"][0]["revision"],
                finish["handle"]["revision"]
            );
            assert_eq!(
                finish["registrations"][0]["incarnation"],
                finish["handle"]["incarnation"]
            );
            assert_eq!(
                response.executed_calls,
                vec![
                    lash_core::ExecutedCallRecord {
                        operation: "triggers.register".to_string(),
                        outcome: lash_core::ExecutedCallOutcome::Ok,
                    },
                    lash_core::ExecutedCallRecord {
                        operation: "triggers.list".to_string(),
                        outcome: lash_core::ExecutedCallOutcome::Ok,
                    },
                ],
                "trigger effects must appear in the executed-call ledger"
            );
        });
    }

    #[test]
    fn keyless_trigger_registration_reaches_effect_and_owner_scoped_store() {
        block_on(async {
            let store = Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
            let controller = CapturingTriggerEffectController::default();
            let ctx =
                lash_core::testing::code_execution_context_with_trigger_store_and_effect_controller(
                    store.clone(),
                    Arc::new(controller.clone()),
                );
            let surface = LashlangSurface::new(
                lashlang::LashlangAbilities::default()
                    .with_processes()
                    .with_triggers(),
                lashlang::LashlangLanguageFeatures::default(),
                timer_trigger_resources(),
            );
            let (_, response) = execute_code_unbounded_for_tests(
                RlmExecutionState::new().expect("state"),
                ctx,
                ExecRequest {
                    language: "lashlang".to_string(),
                    code: r#"
                        process remember(tick: timer.Tick) { finish tick.fired_at }
                        source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
                        handle = await triggers.register({
                          source: source,
                          target: remember,
                          inputs: { tick: trigger.event }
                        })?
                        finish handle
                    "#
                    .to_string(),
                    accept_finish: true,
                },
                lashlang::global_in_memory_lashlang_artifact_store(),
                surface,
                None,
                RlmProjectedBindings::default(),
                Arc::new(ProjectionRegistry::new()),
                RlmLashlangExecutionTraceConfig::default(),
            )
            .await
            .expect("execute keyless trigger registration");
            assert!(response.error.is_none(), "{:?}", response.error);

            let expected_key =
                "derived/v2/b53772ec2996e72a7fc77c087803dc0dd3e127ccb946d6a9ab585c0a09fb7149";
            let (effect_owner_scope, effect_subscription_key) = {
                let envelopes = controller.envelopes.lock_recover();
                let lash_core::RuntimeEffectCommand::Trigger { command } = &envelopes[0].command
                else {
                    panic!("expected trigger effect")
                };
                let lash_core::TriggerCommand::Register {
                    owner_scope, draft, ..
                } = command.as_ref()
                else {
                    panic!("expected register command")
                };
                (owner_scope.clone(), draft.subscription_key.clone())
            };
            assert_eq!(
                effect_owner_scope,
                lash_core::TriggerOwnerScope::session("test-session")
            );
            assert_eq!(effect_subscription_key, expected_key);

            let stored = lash_core::TriggerStore::list_subscriptions(
                store.as_ref(),
                lash_core::TriggerSubscriptionFilter::for_session("test-session"),
            )
            .await
            .expect("list stored keyless registration");
            assert_eq!(stored.len(), 1);
            assert_eq!(
                stored[0].owner_scope,
                lash_core::TriggerOwnerScope::session("test-session")
            );
            assert_eq!(stored[0].subscription_key, expected_key);
        });
    }

    #[test]
    fn reordered_keyless_registration_calls_keep_derived_keys_across_module_regeneration() {
        block_on(async {
            async fn capture(code: &str) -> Vec<(String, String, String)> {
                let controller = CapturingTriggerEffectController::default();
                let response =
                    execute_with_capturing_trigger_effects(code, controller.clone()).await;
                assert!(response.error.is_none(), "{:?}", response.error);
                controller
                    .envelopes
                    .lock_recover()
                    .iter()
                    .filter_map(|envelope| {
                        let lash_core::RuntimeEffectCommand::Trigger { command } =
                            &envelope.command
                        else {
                            return None;
                        };
                        let lash_core::TriggerCommand::Register { draft, .. } = command.as_ref()
                        else {
                            return None;
                        };
                        Some((
                            draft.source_key.clone(),
                            draft.subscription_key.clone(),
                            draft.target_identity.definition.as_ref()?["module_ref"]
                                .as_str()?
                                .to_string(),
                        ))
                    })
                    .collect()
            }

            // Boxed because the awaited future crosses clippy's large-future
            // threshold once the turn config carries its budgets.
            let first = Box::pin(capture(
                r#"
                process remember(tick: timer.Tick) { finish tick.fired_at }
                morning = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
                evening = timer.Schedule({ expr: "0 18 * * *", tz: "UTC" })
                await triggers.register({ source: morning, target: remember, inputs: { tick: trigger.event } })?
                await triggers.register({ source: evening, target: remember, inputs: { tick: trigger.event } })?
                finish true
                "#,
            ))
            .await;
            // Boxed because the awaited future crosses clippy's large-future
            // threshold once the turn config carries its budgets.
            let second = Box::pin(capture(
                r#"
                process remember(tick: timer.Tick) { finish tick.fired_at }
                morning = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
                evening = timer.Schedule({ expr: "0 18 * * *", tz: "UTC" })
                await triggers.register({ source: evening, target: remember, inputs: { tick: trigger.event } })?
                await triggers.register({ source: morning, target: remember, inputs: { tick: trigger.event } })?
                finish true
                "#,
            ))
            .await;

            let first_keys = first
                .iter()
                .map(|(source, key, _)| (source, key))
                .collect::<BTreeMap<_, _>>();
            let second_keys = second
                .iter()
                .map(|(source, key, _)| (source, key))
                .collect::<BTreeMap<_, _>>();
            assert_eq!(first_keys, second_keys);
            assert_ne!(first[0].2, second[0].2, "the artifacts were regenerated");
        });
    }

    #[test]
    fn regenerated_trigger_manifest_warns_and_list_marks_the_orphan() {
        block_on(async {
            let trigger_store =
                Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
            let artifact_store = Arc::new(lashlang::InMemoryLashlangArtifactStore::new());
            let surface = LashlangSurface::new(
                lashlang::LashlangAbilities::default()
                    .with_processes()
                    .with_triggers(),
                lashlang::LashlangLanguageFeatures::default(),
                timer_trigger_resources(),
            );
            let mut state = RlmExecutionState::new().expect("state");

            let (next, first) = execute_code_unbounded_for_tests(
                state,
                lash_core::testing::code_execution_context_with_trigger_store(
                    trigger_store.clone(),
                ),
                ExecRequest {
                    language: "lashlang".to_string(),
                    code: r#"
                        process remember(tick: timer.Tick) { finish tick.fired_at }
                        source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
                        await triggers.register({
                          source: source,
                          target: remember,
                          inputs: { tick: trigger.event },
                          subscription_key: "old-schedule"
                        })?
                        finish true
                    "#
                    .to_string(),
                    accept_finish: true,
                },
                artifact_store.clone(),
                surface.clone(),
                None,
                RlmProjectedBindings::default(),
                Arc::new(ProjectionRegistry::new()),
                RlmLashlangExecutionTraceConfig::default(),
            )
            .await
            .expect("execute original trigger artifact");
            assert!(first.error.is_none(), "{:?}", first.error);
            state = next;

            let (_next, replacement) = execute_code_unbounded_for_tests(
                state,
                lash_core::testing::code_execution_context_with_trigger_store(
                    trigger_store.clone(),
                ),
                ExecRequest {
                    language: "lashlang".to_string(),
                    code: r#"
                        process remember(tick: timer.Tick) { finish tick.fired_at }
                        source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
                        await triggers.register({
                          source: source,
                          target: remember,
                          inputs: { tick: trigger.event },
                          subscription_key: "new-schedule"
                        })?
                        finish await triggers.list({})?
                    "#
                    .to_string(),
                    accept_finish: true,
                },
                artifact_store,
                surface,
                None,
                RlmProjectedBindings::default(),
                Arc::new(ProjectionRegistry::new()),
                RlmLashlangExecutionTraceConfig::default(),
            )
            .await
            .expect("execute replacement trigger artifact");

            assert!(replacement.error.is_none(), "{:?}", replacement.error);
            assert!(
                replacement.observations.iter().any(|warning| {
                    warning.contains("RECONCILE WARNING")
                        && warning.contains("old-schedule")
                        && warning.contains("triggers.prune")
                }),
                "{:?}",
                replacement.observations
            );
            let registrations = replacement
                .terminal_finish
                .expect("replacement returns reconciled list");
            let registrations = registrations.as_array().expect("list result");
            let old = registrations
                .iter()
                .find(|record| record["subscription_key"] == "old-schedule")
                .expect("old subscription remains visible");
            let new = registrations
                .iter()
                .find(|record| record["subscription_key"] == "new-schedule")
                .expect("new subscription is visible");
            assert_eq!(old["manifest_membership"], "orphaned");
            assert_eq!(new["manifest_membership"], "present_in_current_artifact");
            assert!(old["registrant"].is_object());
            assert!(new["registrant"].is_object());
        });
    }

    #[test]
    fn scalar_and_batched_trigger_verbs_emit_typed_effect_envelopes() {
        block_on(async {
            let scalar = CapturingTriggerEffectController::default();
            let response = execute_with_capturing_trigger_effects(
                r#"
                process remember(tick: timer.Tick) { finish true }
                source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
                registered = await triggers.register({
                  source: source, target: remember, inputs: { tick: trigger.event },
                  name: "scalar", subscription_key: "scalar"
                })?
                listed = await triggers.list({ target: remember })?
                updated = await triggers.update({
                  subscription_key: "scalar", expected_revision: registered.revision,
                  source: source, target: remember, inputs: { tick: trigger.event },
                  name: "scalar-updated"
                })?
                disabled = await triggers.disable({
                  subscription_key: "scalar", expected_revision: updated.revision
                })?
                enabled = await triggers.enable({
                  subscription_key: "scalar", expected_revision: disabled.revision
                })?
                deleted = await triggers.delete({
                  subscription_key: "scalar", expected_revision: enabled.revision
                })?
                await triggers.register({
                  source: source, target: remember, inputs: { tick: trigger.event },
                  subscription_key: "prune-me"
                })?
                pruned = await triggers.prune({ subscription_keys: ["prune-me"] })?
                finish len(listed)
                "#,
                scalar.clone(),
            )
            .await;
            assert!(response.error.is_none(), "{:?}", response.error);
            assert_eq!(
                response
                    .executed_calls
                    .iter()
                    .map(|call| (call.operation.as_str(), call.outcome))
                    .collect::<Vec<_>>(),
                [
                    ("triggers.register", lash_core::ExecutedCallOutcome::Ok),
                    ("triggers.list", lash_core::ExecutedCallOutcome::Ok),
                    ("triggers.update", lash_core::ExecutedCallOutcome::Ok),
                    ("triggers.disable", lash_core::ExecutedCallOutcome::Ok),
                    ("triggers.enable", lash_core::ExecutedCallOutcome::Ok),
                    ("triggers.delete", lash_core::ExecutedCallOutcome::Ok),
                    ("triggers.register", lash_core::ExecutedCallOutcome::Ok),
                    ("triggers.prune", lash_core::ExecutedCallOutcome::Ok),
                ],
                "ledger order is source dispatch order"
            );
            let scalar_effects = scalar.trigger_effects();
            assert_eq!(
                scalar_effects
                    .iter()
                    .map(|(_, operation)| *operation)
                    .collect::<Vec<_>>(),
                [
                    "register", "list", "update", "disable", "enable", "delete", "register",
                    "prune"
                ]
            );
            assert!(
                scalar_effects
                    .iter()
                    .all(|(effect_id, _)| !effect_id.contains(":child:"))
            );

            let batched = CapturingTriggerEffectController::default();
            let response = execute_with_capturing_trigger_effects(
                r#"
                process remember(tick: timer.Tick) { finish true }
                source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
                update_seed = await triggers.register({
                  source: source, target: remember, inputs: { tick: trigger.event },
                  subscription_key: "batch-update"
                })?
                enable_seed = await triggers.register({
                  source: source, target: remember, inputs: { tick: trigger.event },
                  subscription_key: "batch-enable"
                })?
                enable_seed = await triggers.disable({
                  subscription_key: "batch-enable", expected_revision: enable_seed.revision
                })?
                disable_seed = await triggers.register({
                  source: source, target: remember, inputs: { tick: trigger.event },
                  subscription_key: "batch-disable"
                })?
                delete_seed = await triggers.register({
                  source: source, target: remember, inputs: { tick: trigger.event },
                  subscription_key: "batch-delete"
                })?
                results = await {
                  registered: triggers.register({
                    source: source, target: remember, inputs: { tick: trigger.event },
                    subscription_key: "batch-register"
                  })?,
                  listed: triggers.list({})?,
                  updated: triggers.update({
                    subscription_key: "batch-update", expected_revision: update_seed.revision,
                    source: source, target: remember, inputs: { tick: trigger.event },
                    name: "batch-updated"
                  })?,
                  enabled: triggers.enable({
                    subscription_key: "batch-enable", expected_revision: enable_seed.revision
                  })?,
                  disabled: triggers.disable({
                    subscription_key: "batch-disable", expected_revision: disable_seed.revision
                  })?,
                  deleted: triggers.delete({
                    subscription_key: "batch-delete", expected_revision: delete_seed.revision
                  })?
                }
                finish len(results.listed)
                "#,
                batched.clone(),
            )
            .await;
            assert!(response.error.is_none(), "{:?}", response.error);
            assert_eq!(
                response
                    .executed_calls
                    .iter()
                    .map(|call| (call.operation.as_str(), call.outcome))
                    .collect::<Vec<_>>(),
                [
                    ("triggers.register", lash_core::ExecutedCallOutcome::Ok),
                    ("triggers.register", lash_core::ExecutedCallOutcome::Ok),
                    ("triggers.disable", lash_core::ExecutedCallOutcome::Ok),
                    ("triggers.register", lash_core::ExecutedCallOutcome::Ok),
                    ("triggers.register", lash_core::ExecutedCallOutcome::Ok),
                    ("triggers.register", lash_core::ExecutedCallOutcome::Ok),
                    ("triggers.list", lash_core::ExecutedCallOutcome::Ok),
                    ("triggers.update", lash_core::ExecutedCallOutcome::Ok),
                    ("triggers.enable", lash_core::ExecutedCallOutcome::Ok),
                    ("triggers.disable", lash_core::ExecutedCallOutcome::Ok),
                    ("triggers.delete", lash_core::ExecutedCallOutcome::Ok),
                ],
                "ledger order is source dispatch order"
            );
            let batch_effects = batched
                .trigger_effects()
                .into_iter()
                .filter(|(effect_id, _)| effect_id.contains(":child:"))
                .map(|(_, operation)| operation)
                .collect::<Vec<_>>();
            assert_eq!(
                batch_effects,
                ["register", "list", "update", "enable", "disable", "delete"]
            );
        });
    }

    #[test]
    fn trigger_disable_is_revision_checked_and_keeps_registry_entry() {
        block_on(async {
            let response = execute_with_trigger_environment(
                r#"
                process remember(tick: timer.Tick) {
                  finish true
                }

                source = timer.Schedule({ expr: "0 8 * * *" })
                handle = await triggers.register({
                  source: source,
                  target: remember,
                  inputs: { tick: trigger.event },
                  name: "remembered",
                  subscription_key: "remembered"
                })?
                disabled = await triggers.disable({
                  subscription_key: "remembered",
                  expected_revision: handle.revision
                })?
                registrations = await triggers.list({ target: remember })?
                finish { disposition: disabled.disposition, enabled: registrations[0].enabled }
                "#,
            )
            .await;

            assert!(response.error.is_none(), "{:?}", response.error);
            assert_eq!(
                response.terminal_finish,
                Some(serde_json::json!({ "disposition": "disabled", "enabled": false }))
            );
        });
    }

    #[test]
    fn trigger_registration_failure_prevents_foreground_execution() {
        block_on(async {
            let response = execute_with_trigger_environment(
                r#"
                process remember(tick: str) {
                  finish tick
                }

                source = timer.Schedule({ expr: "0 8 * * *" })
                await triggers.register({
                  source: source,
                  target: remember,
                  inputs: { tick: trigger.event }
                })?

                finish "should not run"
                "#,
            )
            .await;

            let error = response
                .error
                .as_deref()
                .expect("event mismatch should fail");
            assert!(error.contains("trigger source emits"), "{error}");
            assert!(response.observations.is_empty());
            assert!(response.terminal_finish.is_none());
        });
    }

    #[test]
    fn foreground_sleep_executes_through_runtime_context() {
        block_on(async {
            let response = execute_with_lashlang_abilities(
                r#"
                sleep for "0ms"
                finish "awake"
                "#,
                lashlang::LashlangAbilities::default().with_sleep(),
            )
            .await;

            assert!(response.error.is_none(), "{:?}", response.error);
            assert_eq!(response.terminal_finish, Some(serde_json::json!("awake")));
        });
    }

    #[test]
    fn print_observation_preserves_raw_output_and_records_projection_metadata() {
        block_on(async {
            let large = "x".repeat(60 * 1024);
            let code = format!(
                "print {{ output: {}, status: \"failed\", error: \"boom\", exit_code: 2, stderr: \"short\" }}",
                serde_json::to_string(&large).expect("string literal")
            );
            let response =
                execute_with_lashlang_abilities(&code, lashlang::LashlangAbilities::default())
                    .await;

            assert!(response.error.is_none(), "{:?}", response.error);
            assert_eq!(response.observations.len(), 1);
            assert!(
                response.observations[0].contains(&large),
                "raw observation should preserve full printed value"
            );
            assert_eq!(response.observation_truncation.len(), 1);
            let metadata = &response.observation_truncation[0];
            assert!(metadata.truncated, "{metadata:?}");
            assert_eq!(
                metadata.limit,
                crate::rlm_support::PRINT_HISTORY_PROJECTION_CONFIG.max_bytes
            );
            assert_eq!(
                metadata.max_lines,
                crate::rlm_support::PRINT_HISTORY_PROJECTION_CONFIG.max_lines
            );
        });
    }

    #[test]
    fn executor_reports_rlm_bare_tool_call_diagnostic_at_link_time() {
        let mut resources = lashlang::LashlangHostCatalog::new();
        resources
            .add_module_operation(
                ["files"],
                "Files",
                "read",
                "read_file",
                lashlang::TypeExpr::Any,
                lashlang::TypeExpr::Any,
            )
            .expect("host catalog operation must not conflict");

        block_on(async {
            let response = execute_with_lashlang_host_environment(
                r#"finish read_file({ path: "Cargo.toml" })"#,
                lashlang::LashlangAbilities::default(),
                resources,
            )
            .await;
            let error = response
                .error
                .as_deref()
                .expect("bare tool call should fail at link time");

            let (kind, evidence) = crate::feedback::RlmFeedbackKind::split(error);
            assert_eq!(
                kind,
                crate::feedback::RlmFeedbackKind::Policy,
                "a link refusal is a policy failure, not a runtime one: {error}"
            );
            assert!(
                evidence.starts_with(RLM_BARE_TOOL_CALL_DIAGNOSTIC),
                "{error}"
            );
            assert!(error.contains("hint: use `files.read`"), "{error}");
            assert!(response.tool_calls.is_empty());
            assert!(response.terminal_finish.is_none());
        });
    }

    #[test]
    fn top_level_typo_on_line_40_fails_before_any_effect() {
        block_on(async {
            let mut lines = (1..40)
                .map(|index| format!("print {index}"))
                .collect::<Vec<_>>();
            lines.push("finish misspelled_result".to_string());
            let response = execute_with_lashlang_abilities(
                &lines.join("\n"),
                lashlang::LashlangAbilities::default(),
            )
            .await;

            let error = response.error.expect("link should reject typo");
            assert!(
                error.contains("unknown name `misspelled_result`"),
                "{error}"
            );
            assert!(error.contains("--> line 40, column 8"), "{error}");
            assert!(
                response.observations.is_empty(),
                "no print effect may execute before a link failure"
            );
            assert!(response.tool_calls.is_empty());
            assert!(response.terminal_finish.is_none());
        });
    }

    #[test]
    fn executor_reports_disabled_lashlang_abilities_at_link_time() {
        struct DisabledCase {
            name: &'static str,
            code: &'static str,
            abilities: lashlang::LashlangAbilities,
            resources: fn() -> lashlang::LashlangHostCatalog,
            feature: &'static str,
        }

        let cases = [
            DisabledCase {
                name: "process declaration",
                code: "process worker() { finish null }",
                abilities: lashlang::LashlangAbilities::default(),
                resources: lashlang::LashlangHostCatalog::new,
                feature: "processes",
            },
            DisabledCase {
                name: "process start",
                code: "start worker()",
                abilities: lashlang::LashlangAbilities::default(),
                resources: lashlang::LashlangHostCatalog::new,
                feature: "processes",
            },
            DisabledCase {
                name: "sleep",
                code: r#"sleep for "1s""#,
                abilities: lashlang::LashlangAbilities::default(),
                resources: lashlang::LashlangHostCatalog::new,
                feature: "sleep",
            },
            DisabledCase {
                name: "wait_signal",
                code: "process worker() signals { ready: any } { payload = wait_signal(\"ready\") }",
                abilities: lashlang::LashlangAbilities::default().with_processes(),
                resources: lashlang::LashlangHostCatalog::new,
                feature: "process signals",
            },
            DisabledCase {
                name: "signal_run",
                code: "process worker(target: any) { signal_run(target, \"ready\", null) }",
                abilities: lashlang::LashlangAbilities::default().with_processes(),
                resources: lashlang::LashlangHostCatalog::new,
                feature: "process signals",
            },
            DisabledCase {
                name: "trigger",
                code: r#"
                    process worker(tick: timer.Tick) { finish true }
                    source = timer.Schedule({ expr: "0 8 * * *" })
                    await triggers.register({
                      source: source,
                      target: worker,
                      inputs: { tick: trigger.event }
                    })?
                "#,
                abilities: lashlang::LashlangAbilities::default().with_processes(),
                resources: timer_trigger_resources,
                feature: "triggers",
            },
        ];

        block_on(async {
            for case in cases {
                lashlang::parse(case.code)
                    .unwrap_or_else(|err| panic!("{} should parse: {err}", case.name));
                let response = execute_with_lashlang_host_environment(
                    case.code,
                    case.abilities,
                    (case.resources)(),
                )
                .await;
                let error = response
                    .error
                    .as_deref()
                    .unwrap_or_else(|| panic!("{} should fail at link time", case.name));

                assert!(
                    error.contains(&format!(
                        "lashlang feature `{}` is disabled by this host",
                        case.feature
                    )),
                    "{} error was {error}",
                    case.name
                );
                assert!(
                    response.tool_calls.is_empty(),
                    "{} should not call runtime tools",
                    case.name
                );
                assert!(
                    response.observations.is_empty(),
                    "{} should not emit observations",
                    case.name
                );
                assert!(
                    response.images.is_empty() && response.printed_images.is_empty(),
                    "{} should not emit images",
                    case.name
                );
                assert!(
                    response.terminal_finish.is_none(),
                    "{} should not finish terminally",
                    case.name
                );
            }
        });
    }

    #[test]
    fn projected_history_is_available_without_clobbering_executor_globals() {
        block_on(async {
            let mut state = RlmExecutionState::new().expect("state");
            let mut set_default = serde_json::Map::new();
            set_default.insert("diary".to_string(), serde_json::json!(["kept"]));
            state
                .patch_globals(
                    &lash_rlm_types::RlmGlobalsPatchPluginBody { set_default },
                    &BTreeSet::new(),
                )
                .expect("patch diary");

            let projected = projected_history(vec![FlowValue::String("hello".into())]);
            let compiled =
                lashlang::compile("finish { history_len: len(history), diary_len: len(diary) }")
                    .expect("compile");
            let outcome = execute_with_projected(&compiled, &mut state.rlm, &projected)
                .await
                .expect("execute");
            let ExecutionOutcome::Finished(FlowValue::Record(record)) = outcome else {
                panic!("expected finishted record");
            };
            assert_eq!(record["history_len"], FlowValue::Number(1.0));
            assert_eq!(record["diary_len"], FlowValue::Number(1.0));
            assert!(state.rlm.snapshot().globals().get("history").is_none());
        });
    }

    #[test]
    fn projected_history_defaults_to_empty_list_when_missing() {
        block_on(async {
            let mut state = RlmExecutionState::new().expect("state");

            let projected = projected_history(Vec::new());
            let compiled =
                lashlang::compile("finish { history_len: len(history) }").expect("compile");
            let outcome = execute_with_projected(&compiled, &mut state.rlm, &projected)
                .await
                .expect("execute");
            let ExecutionOutcome::Finished(FlowValue::Record(record)) = outcome else {
                panic!("expected finishted record");
            };
            assert_eq!(record["history_len"], FlowValue::Number(0.0));
        });
    }

    #[test]
    fn set_default_initializes_once_and_does_not_mutate_projected_globals() {
        let mut state = RlmExecutionState::new().expect("state");
        let projected = BTreeSet::from_iter(["current_query".to_string()]);

        state
            .patch_globals(
                &lash_rlm_types::RlmGlobalsPatchPluginBody {
                    set_default: serde_json::Map::from_iter([(
                        "diary".to_string(),
                        serde_json::json!(["initial"]),
                    )]),
                },
                &projected,
            )
            .expect("apply defaults");
        assert_eq!(
            state.rlm.snapshot().globals().get("diary"),
            Some(&FlowValue::List(
                vec![FlowValue::String("initial".into())].into()
            ))
        );
        assert!(
            state
                .rlm
                .snapshot()
                .globals()
                .get("current_query")
                .is_none()
        );

        state
            .patch_globals(
                &lash_rlm_types::RlmGlobalsPatchPluginBody {
                    set_default: serde_json::Map::from_iter([(
                        "diary".to_string(),
                        serde_json::json!(["clobber"]),
                    )]),
                },
                &projected,
            )
            .expect("reapply defaults");
        assert_eq!(
            state.rlm.snapshot().globals().get("diary"),
            Some(&FlowValue::List(
                vec![FlowValue::String("initial".into())].into()
            ))
        );
    }

    #[test]
    fn heap_backed_default_patch_survives_next_cell_and_cold_restore() {
        block_on(async {
            let projected = ProjectedBindings::new();
            let mut state = RlmExecutionState::new().expect("state");
            let setup = lashlang::compile("seed = [{ nested: [1] }]").expect("compile setup");
            execute_with_projected(&setup, &mut state.rlm, &projected)
                .await
                .expect("execute setup");
            state
                .patch_globals(
                    &lash_rlm_types::RlmGlobalsPatchPluginBody {
                        set_default: serde_json::Map::from_iter([(
                            "diary".to_string(),
                            serde_json::json!(["kept"]),
                        )]),
                    },
                    &BTreeSet::new(),
                )
                .expect("patch heap-backed state");

            let finish = lashlang::compile("finish diary").expect("compile finish");
            assert_eq!(
                execute_with_projected(&finish, &mut state.rlm, &projected)
                    .await
                    .expect("next cell sees patch"),
                ExecutionOutcome::Finished(FlowValue::List(
                    vec![FlowValue::String("kept".into())].into()
                ))
            );

            let bytes = state
                .rlm
                .snapshot()
                .to_canonical_bytes()
                .expect("encode patched state");
            let snapshot =
                lashlang::Snapshot::from_canonical_bytes(&bytes).expect("decode patched state");
            let mut restored = lashlang::State::from_snapshot(snapshot);
            assert_eq!(
                execute_with_projected(&finish, &mut restored, &projected)
                    .await
                    .expect("cold-restored cell sees patch"),
                ExecutionOutcome::Finished(FlowValue::List(
                    vec![FlowValue::String("kept".into())].into()
                ))
            );
        });
    }

    #[test]
    fn rejected_global_patch_leaves_byte_identical_state_and_no_dirty_marks() {
        block_on(async {
            let projected = ProjectedBindings::new();
            let mut state = RlmExecutionState::new().expect("state");
            let setup = lashlang::compile("seed = [{ nested: [1] }]").expect("compile setup");
            execute_with_projected(&setup, &mut state.rlm, &projected)
                .await
                .expect("execute setup");
            let before = state
                .rlm
                .snapshot()
                .to_canonical_bytes()
                .expect("encode pre-patch state");
            let dirty_before = state.execution_state_dirty();

            // A deterministically ordered patch whose first key is acceptable
            // and whose second is the reserved binding. Applying keys one at a
            // time committed `a_good` and then failed, leaving a mutation no
            // dirty mark accounted for.
            let error = state
                .patch_globals(
                    &lash_rlm_types::RlmGlobalsPatchPluginBody {
                        set_default: serde_json::Map::from_iter([
                            ("a_good".to_string(), serde_json::json!(["kept"])),
                            ("history".to_string(), serde_json::json!(["nope"])),
                        ]),
                    },
                    &BTreeSet::new(),
                )
                .expect_err("a reserved name must reject the whole patch");
            assert!(error.to_string().contains("history"));

            assert!(
                state.rlm.globals().get("a_good").is_none(),
                "no key from a rejected patch may be committed"
            );
            assert_eq!(
                state
                    .rlm
                    .snapshot()
                    .to_canonical_bytes()
                    .expect("encode post-patch state"),
                before,
                "a rejected patch must leave the state byte-identical"
            );
            assert_eq!(
                state.execution_state_dirty(),
                dirty_before,
                "a rejected patch must not mark the execution state dirty"
            );
        });
    }

    #[test]
    fn rejected_protected_name_patch_leaves_byte_identical_state() {
        block_on(async {
            let projected = ProjectedBindings::new();
            let mut state = RlmExecutionState::new().expect("state");
            let setup = lashlang::compile("seed = [1]").expect("compile setup");
            execute_with_projected(&setup, &mut state.rlm, &projected)
                .await
                .expect("execute setup");
            let before = state
                .rlm
                .snapshot()
                .to_canonical_bytes()
                .expect("encode pre-patch state");

            let protected = BTreeSet::from(["docs".to_string()]);
            state
                .patch_globals(
                    &lash_rlm_types::RlmGlobalsPatchPluginBody {
                        set_default: serde_json::Map::from_iter([
                            ("a_good".to_string(), serde_json::json!(1)),
                            ("docs".to_string(), serde_json::json!(2)),
                        ]),
                    },
                    &protected,
                )
                .expect_err("a protected name must reject the whole patch");

            assert_eq!(
                state
                    .rlm
                    .snapshot()
                    .to_canonical_bytes()
                    .expect("encode post-patch state"),
                before
            );
        });
    }

    #[test]
    fn heap_backed_projection_rehydrate_and_prune_survive_execution_and_restore() {
        block_on(async {
            let projected = ProjectedBindings::new();
            let setup =
                lashlang::compile("history = [{ role: \"user\" }]\nkept = [{ nested: [2] }]")
                    .expect("compile setup");
            let mut state = lashlang::State::new();
            execute_with_projected(&setup, &mut state, &projected)
                .await
                .expect("execute setup");

            let registry = Arc::new(ProjectionRegistry::new());
            let descriptor = Arc::new(SnapshotProjectedToolText::default());
            let reference = registry.register_memory(descriptor.clone());
            state
                .insert_global(
                    "doc",
                    FlowValue::Projected(ProjectedValue::custom_with_projection_ref(
                        "doc",
                        descriptor,
                        serde_json::to_value(&reference).expect("projection ref"),
                    )),
                )
                .expect("insert projected value");
            let unavailable = state
                .snapshot()
                .to_canonical_bytes()
                .expect("encode projected state");
            state = lashlang::State::from_snapshot(
                lashlang::Snapshot::from_canonical_bytes(&unavailable)
                    .expect("restore projected state as unavailable"),
            );
            rehydrate_projected_globals(
                &mut state,
                Arc::clone(&registry) as Arc<dyn ProjectionResolver>,
            )
            .await
            .expect("rehydrate heap-backed projected value");
            crate::projection::prune_reserved_projected_bindings(&mut state);

            let finish = lashlang::compile("finish { doc: doc, kept: kept }")
                .expect("compile post-patch read");
            let expected =
                ExecutionOutcome::Finished(FlowValue::Record(Arc::new(FlowRecord::from_iter([
                    (
                        "doc".to_string(),
                        FlowValue::String("materialized tool text".into()),
                    ),
                    (
                        "kept".to_string(),
                        FlowValue::List(
                            vec![FlowValue::Record(Arc::new(FlowRecord::from_iter([(
                                "nested".to_string(),
                                FlowValue::List(vec![FlowValue::Number(2.0)].into()),
                            )])))]
                            .into(),
                        ),
                    ),
                ]))));
            assert_eq!(
                execute_with_projected(&finish, &mut state, &projected)
                    .await
                    .expect("next cell sees rehydrate and prune"),
                expected
            );
            assert!(state.globals().get("history").is_none());

            let bytes = state
                .snapshot()
                .to_canonical_bytes()
                .expect("encode rehydrated and pruned state");
            let snapshot = lashlang::Snapshot::from_canonical_bytes(&bytes)
                .expect("decode rehydrated and pruned state");
            let mut restored = lashlang::State::from_snapshot(snapshot);
            rehydrate_projected_globals(
                &mut restored,
                Arc::clone(&registry) as Arc<dyn ProjectionResolver>,
            )
            .await
            .expect("rehydrate after cold restore");
            assert_eq!(
                execute_with_projected(&finish, &mut restored, &projected)
                    .await
                    .expect("cold-restored cell sees rehydrate and prune"),
                expected
            );
            assert!(restored.globals().get("history").is_none());
        });
    }

    #[test]
    fn set_default_rejects_projected_host_bindings() {
        let mut state = RlmExecutionState::new().expect("state");
        let projected = BTreeSet::from_iter(["current_query".to_string()]);

        let err = state
            .patch_globals(
                &lash_rlm_types::RlmGlobalsPatchPluginBody {
                    set_default: serde_json::Map::from_iter([(
                        "current_query".to_string(),
                        serde_json::json!("bad"),
                    )]),
                },
                &projected,
            )
            .expect_err("projected default should fail");
        assert!(err.to_string().contains("read-only projected host binding"));

        let err = state
            .patch_globals(
                &lash_rlm_types::RlmGlobalsPatchPluginBody {
                    set_default: serde_json::Map::from_iter([(
                        "history".to_string(),
                        serde_json::json!([]),
                    )]),
                },
                &BTreeSet::new(),
            )
            .expect_err("history default should fail");
        assert!(err.to_string().contains("read-only projected host binding"));
    }

    #[test]
    fn projected_scalar_bindings_are_read_only_and_not_snapshotted() {
        block_on(async {
            let mut state = RlmExecutionState::new().expect("state");
            let mut projected = ProjectedBindings::new();
            projected.insert(
                "current_query",
                ProjectedValue::scalar("current_query", FlowValue::String("host".into())),
            );

            let compiled =
                lashlang::compile("finish { chars: len(current_query), value: current_query }")
                    .expect("compile read");
            let outcome = execute_with_projected(&compiled, &mut state.rlm, &projected)
                .await
                .expect("execute read");
            let ExecutionOutcome::Finished(FlowValue::Record(record)) = outcome else {
                panic!("expected finishted record");
            };
            assert_eq!(record["chars"], FlowValue::Number(4.0));
            assert_eq!(record["value"], FlowValue::String("host".into()));
            assert!(
                state
                    .rlm
                    .snapshot()
                    .globals()
                    .get("current_query")
                    .is_none()
            );

            let compiled = lashlang::compile("current_query = \"local\"").expect("compile write");
            let env = ExecutionEnvironment::new(&NoopHost)
                .traced()
                .with_projected_bindings(projected.clone());
            let error = lashlang::execute(&compiled, &mut state.rlm, &env)
                .await
                .expect_err("projected write should fail");
            let failure = env
                .take_runtime_failure()
                .unwrap_or(lashlang::RuntimeFailure { error, span: None });
            assert!(
                failure
                    .error
                    .to_string()
                    .contains("read-only projected binding")
            );
        });
    }

    #[test]
    fn executor_snapshot_does_not_materialize_projected_tool_result_globals() {
        let projected = Arc::new(SnapshotProjectedToolText::default());
        let mut state = RlmExecutionState::new().expect("state");
        state
            .rlm
            .insert_global(
                "m".to_string(),
                FlowValue::Projected(ProjectedValue::custom(
                    "search.matches[0].text",
                    projected.clone(),
                )),
            )
            .expect("insert projected global");

        let snapshot =
            hydrate_snapshot(state.snapshot_execution_state().expect("executor snapshot"));
        assert_eq!(projected.render_count.load(Ordering::SeqCst), 0);
        assert_eq!(projected.materialize_count.load(Ordering::SeqCst), 0);
        let mut encoded = snapshot.root.clone();
        for body in snapshot.components.values() {
            encoded.extend_from_slice(body);
        }
        let encoded_text = String::from_utf8_lossy(&encoded);
        assert!(!encoded_text.contains("rendered tool text"));
        assert!(!encoded_text.contains("materialized tool text"));

        let mut restored_execution = RlmExecutionState::new().expect("restored state");
        restored_execution
            .restore_execution_state(&snapshot)
            .expect("restore runtime");
        let restored = restored_execution.rlm;
        assert!(matches!(
            restored.snapshot().globals().get("m"),
            Some(FlowValue::Projected(_))
        ));
    }

    #[test]
    fn measured_commit_budget_carries_only_changed_leaf_bodies() {
        block_on(async {
            let mut source = String::new();
            for index in 0..12 {
                let payload = format!("large-{index}-{}", "x".repeat(6 * 1024));
                source.push_str(&format!("large_{index} = [\"{payload}\"]\n"));
            }
            for index in 0..40 {
                source.push_str(&format!("small_{index} = {index}\n"));
            }
            let mut state =
                execute_test_code(RlmExecutionState::new().expect("state"), source).await;
            let initial = state.snapshot_execution_state().expect("initial snapshot");
            assert_eq!(
                initial
                    .components
                    .values()
                    .filter(|component| matches!(
                        component,
                        lash_core::plugin::ExecutionStateComponentSnapshot::Changed(_)
                    ))
                    .count(),
                12
            );
            state.acknowledge_execution_state_capture();

            state = execute_test_code(
                state,
                "large_0 = push(large_0, \"one changed binding\")".to_string(),
            )
            .await;
            let changed = state.snapshot_execution_state().expect("changed snapshot");
            let changed_bodies = changed
                .components
                .values()
                .filter(|component| {
                    matches!(
                        component,
                        lash_core::plugin::ExecutionStateComponentSnapshot::Changed(_)
                    )
                })
                .count();
            let unchanged_refs = changed
                .components
                .values()
                .filter(|component| {
                    matches!(
                        component,
                        lash_core::plugin::ExecutionStateComponentSnapshot::Unchanged
                    )
                })
                .count();
            assert_eq!(
                changed_bodies, 1,
                "only the assigned large binding carries bytes"
            );
            assert_eq!(unchanged_refs, 11, "all other large bindings ride as refs");

            let initial_budget = state::measure_snapshot(&initial);
            let changed_budget = state::measure_snapshot(&changed);
            assert_eq!(initial_budget.checkpoint_bytes, 82_522);
            assert_eq!(changed_budget.checkpoint_bytes, 14_040);
        });
    }

    #[test]
    fn progress_capture_then_later_assignment_survives_final_cold_reopen() {
        block_on(async {
            let initial_payload = format!("before-{}", "x".repeat(8 * 1024));
            let mut state = execute_test_code(
                RlmExecutionState::new().expect("state"),
                format!("large = [\"{initial_payload}\"]"),
            )
            .await;
            let progress_snapshot = state
                .snapshot_execution_state()
                .expect("progress-boundary capture");

            state = execute_test_code(state, "large = push(large, \"after-progress\")".to_string())
                .await;
            let final_snapshot = state
                .snapshot_execution_state()
                .expect("final capture after later assignment");
            assert_ne!(
                final_snapshot.root, progress_snapshot.root,
                "the final capture must supersede the pending progress capture"
            );
            assert_eq!(
                final_snapshot
                    .components
                    .values()
                    .filter(|component| matches!(
                        component,
                        lash_core::plugin::ExecutionStateComponentSnapshot::Changed(_)
                    ))
                    .count(),
                1,
                "the post-progress value leaf must still carry its uncommitted body"
            );

            let hydrated = hydrate_snapshot(final_snapshot);
            let mut reopened = RlmExecutionState::new().expect("cold state");
            reopened
                .restore_execution_state(&hydrated)
                .expect("cold reopen final capture");
            assert_eq!(
                reopened.rlm.snapshot().globals().get("large"),
                state.rlm.snapshot().globals().get("large"),
                "cold reopen must include the assignment made after the progress capture"
            );

            state.abort_execution_state_capture();
            let retry_snapshot = state
                .snapshot_execution_state()
                .expect("retry superseded capture after commit failure");
            let retry_hydrated = hydrate_snapshot(retry_snapshot);
            let mut retry_reopened = RlmExecutionState::new().expect("retry cold state");
            retry_reopened
                .restore_execution_state(&retry_hydrated)
                .expect("cold reopen retry capture");
            assert_eq!(
                retry_reopened.rlm.snapshot().globals().get("large"),
                state.rlm.snapshot().globals().get("large"),
                "aborting a superseded capture must retain the post-progress assignment"
            );
        });
    }

    #[test]
    fn progress_capture_a_to_b_then_final_a_resends_the_evicted_leaf() {
        block_on(async {
            let payload_a = format!("a-{}", "x".repeat(8 * 1024));
            let payload_b = format!("b-{}", "y".repeat(8 * 1024));
            let mut state = execute_test_code(
                RlmExecutionState::new().expect("state"),
                format!("large = [\"{payload_a}\"]"),
            )
            .await;
            let durable_a = state.snapshot_execution_state().expect("durable A capture");
            state.acknowledge_execution_state_capture();
            let mut staged_runtime = lash_core::RuntimeSessionState {
                session_id: "progress-a-b-a-staged".to_string(),
                ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
                    lash_core::TurnBudget::Unbounded,
                ))
            };
            lash_core::testing::stage_execution_state_components(
                &mut staged_runtime,
                durable_a.clone(),
            )
            .expect("stage durable A");
            let mut retry_runtime = lash_core::RuntimeSessionState {
                session_id: "progress-a-b-a-retry".to_string(),
                ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
                    lash_core::TurnBudget::Unbounded,
                ))
            };
            lash_core::testing::stage_execution_state_components(&mut retry_runtime, durable_a)
                .expect("stage retry baseline A");

            state = execute_test_code(state, format!("large = [\"{payload_b}\"]")).await;
            let progress_b = state
                .snapshot_execution_state()
                .expect("progress-boundary B capture");
            lash_core::testing::stage_execution_state_components(
                &mut staged_runtime,
                progress_b.clone(),
            )
            .expect("stage progress B");
            state = execute_test_code(state, format!("large = [\"{payload_a}\"]")).await;
            let final_a = state.snapshot_execution_state().expect("final A capture");
            assert_ne!(final_a.root, progress_b.root);
            assert_eq!(
                final_a
                    .components
                    .values()
                    .filter(|component| matches!(
                        component,
                        lash_core::plugin::ExecutionStateComponentSnapshot::Changed(_)
                    ))
                    .count(),
                1,
                "A was evicted by the staged B root, so final A must resend its body"
            );

            lash_core::testing::stage_execution_state_components(&mut staged_runtime, final_a)
                .expect("stage final A over progress B");
            let final_hydration = staged_runtime
                .execution_state_hydration()
                .expect("hydrate staged final A")
                .expect("final A root");
            let mut reopened = RlmExecutionState::new().expect("cold state");
            reopened
                .restore_execution_state(&final_hydration)
                .expect("cold reopen final A capture");
            assert_eq!(
                reopened.rlm.snapshot().globals().get("large"),
                state.rlm.snapshot().globals().get("large")
            );

            state.abort_execution_state_capture();
            let retry_a = state
                .snapshot_execution_state()
                .expect("retry A after final commit failure");
            lash_core::testing::stage_execution_state_components(&mut retry_runtime, retry_a)
                .expect("stage retry A over durable A");
            let retry_hydration = retry_runtime
                .execution_state_hydration()
                .expect("hydrate retry A")
                .expect("retry A root");
            let mut retry_reopened = RlmExecutionState::new().expect("retry cold state");
            retry_reopened
                .restore_execution_state(&retry_hydration)
                .expect("cold reopen retry A capture");
            assert_eq!(
                retry_reopened.rlm.snapshot().globals().get("large"),
                state.rlm.snapshot().globals().get("large")
            );
        });
    }

    #[test]
    fn measured_commit_growth_tracks_changed_state_not_session_size() {
        block_on(async {
            let mut source = String::new();
            for index in 0..16 {
                let payload = format!("session-{index}-{}", "y".repeat(8 * 1024));
                source.push_str(&format!("large_{index} = [\"{payload}\"]\n"));
            }
            for index in 0..80 {
                source.push_str(&format!("small_{index} = {index}\n"));
            }
            let mut state =
                execute_test_code(RlmExecutionState::new().expect("state"), source).await;
            let full_state_bytes = state
                .rlm
                .snapshot()
                .to_canonical_bytes()
                .expect("pre-arc flat snapshot baseline")
                .len();
            let _initial = state.snapshot_execution_state().expect("initial snapshot");
            state.acknowledge_execution_state_capture();

            let mut measured = Vec::new();
            for turn in 0..40 {
                let binding = turn % 16;
                state = execute_test_code(
                    state,
                    format!("large_{binding} = push(large_{binding}, \"turn-{turn}\")"),
                )
                .await;
                let snapshot = state.snapshot_execution_state().expect("turn snapshot");
                assert_eq!(
                    state.encoded_globals_in_last_snapshot(),
                    1,
                    "turn {turn} must re-encode only its assigned binding"
                );
                assert_eq!(
                    snapshot
                        .components
                        .values()
                        .filter(|component| matches!(
                            component,
                            lash_core::plugin::ExecutionStateComponentSnapshot::Changed(_)
                        ))
                        .count(),
                    1,
                    "turn {turn} must submit one changed leaf body"
                );
                measured.push(state::measure_snapshot(&snapshot).checkpoint_bytes);
                state.acknowledge_execution_state_capture();
            }
            let minimum = *measured.iter().min().expect("measurements");
            let maximum = *measured.iter().max().expect("measurements");
            println!(
                "FIG1195_FLAT_GROWTH full_state_bytes={full_state_bytes} min_commit_bytes={minimum} max_commit_bytes={maximum} turns={}",
                measured.len()
            );
            assert_eq!(full_state_bytes, 136_711);
            assert_eq!(minimum, 21_047);
            assert_eq!(maximum, 21_101);
        });
    }

    /// The failure geometry this arc exists for: a research session whose state
    /// is many mid-size composite bindings rather than a few large ones. Three
    /// live jitindex episodes committed 1.52/1.32/1.24 MB of exactly this shape
    /// against a 1 MiB budget, so per-commit bytes have to track the changed
    /// binding here too — a payoff that only appears above some large-binding
    /// size would not have prevented those failures.
    #[test]
    fn measured_commit_growth_stays_flat_for_many_mid_size_bindings() {
        block_on(async {
            let mut source = String::new();
            for index in 0..300 {
                let payload = format!("note-{index}-{}", "n".repeat(3 * 1024 + 512));
                source.push_str(&format!("mid_{index} = [\"{payload}\"]\n"));
            }
            let mut state =
                execute_test_code(RlmExecutionState::new().expect("state"), source).await;
            let full_state_bytes = state
                .rlm
                .snapshot()
                .to_canonical_bytes()
                .expect("accumulated canonical state")
                .len();
            assert_eq!(full_state_bytes, 1_104_953);
            let _initial = state.snapshot_execution_state().expect("initial snapshot");
            state.acknowledge_execution_state_capture();

            let mut measured = Vec::new();
            for turn in 0..20 {
                let binding = turn % 300;
                state = execute_test_code(
                    state,
                    format!("mid_{binding} = push(mid_{binding}, \"turn-{turn}\")"),
                )
                .await;
                let snapshot = state.snapshot_execution_state().expect("turn snapshot");
                assert_eq!(
                    state.encoded_globals_in_last_snapshot(),
                    1,
                    "turn {turn} must re-encode only its assigned binding"
                );
                assert_eq!(
                    snapshot
                        .components
                        .values()
                        .filter(|component| matches!(
                            component,
                            lash_core::plugin::ExecutionStateComponentSnapshot::Changed(_)
                        ))
                        .count(),
                    1,
                    "turn {turn} must submit one changed leaf body"
                );
                measured.push(state::measure_snapshot(&snapshot).checkpoint_bytes);
                state.acknowledge_execution_state_capture();
            }
            let minimum = *measured.iter().min().expect("measurements");
            let maximum = *measured.iter().max().expect("measurements");
            println!(
                "FIG1195_FLAT_GROWTH_MID_SIZE full_state_bytes={full_state_bytes} min_commit_bytes={minimum} max_commit_bytes={maximum} turns={}",
                measured.len()
            );
            assert_eq!(minimum, 94_294);
            assert_eq!(maximum, 94_296);
        });
    }

    /// The other side of the leaf line: a session of many short bindings must
    /// keep them inline. Each leaf costs a root reference plus a checkpoint
    /// manifest row on every commit, so promoting short values to leaves would
    /// raise the per-commit floor instead of lowering it.
    #[test]
    fn many_short_bindings_stay_inline_and_hold_the_per_commit_floor() {
        block_on(async {
            let mut source = String::new();
            for index in 0..200 {
                let payload = format!("short-{index}-{}", "s".repeat(48));
                source.push_str(&format!("short_{index} = [\"{payload}\"]\n"));
            }
            let mut state =
                execute_test_code(RlmExecutionState::new().expect("state"), source).await;
            let initial = state.snapshot_execution_state().expect("initial snapshot");
            assert_eq!(initial.components.len(), 0);
            state.acknowledge_execution_state_capture();

            state = execute_test_code(
                state,
                "short_0 = push(short_0, \"one changed binding\")".to_string(),
            )
            .await;
            let changed = state.snapshot_execution_state().expect("changed snapshot");
            let commit_bytes = state::measure_snapshot(&changed).checkpoint_bytes;
            println!(
                "FIG1195_SHORT_BINDING_FLOOR commit_bytes={commit_bytes} leaves={}",
                changed.components.len()
            );
            assert!(
                changed.components.is_empty(),
                "a changed short binding must not mint a leaf"
            );
            // The property under test is the assertion above: no leaf is minted,
            // so 200 short bindings cost no root references and no manifest
            // rows. The byte bound is a sanity ceiling on top of that. The
            // measurement is deterministic and has been 33,027 bytes since the
            // pre-heap tree representation — the heap form encodes the same
            // bytes for these bindings — so the ceiling is set well above it
            // rather than one percent above it: a tight assert here fails on any
            // harmless change to the payload strings while telling us nothing
            // the leaf-count assertion does not.
            assert!(
                commit_bytes < 48 * 1024,
                "many short bindings must keep the per-commit floor low: {commit_bytes}"
            );
        });
    }

    #[test]
    fn bound_variables_prompt_renders_live_globals_after_execution() {
        block_on(async {
            let state = RlmExecutionState::new().expect("state");
            let ctx = lash_core::testing::code_execution_context();
            let (state, response) = execute_code_unbounded_for_tests(
                state,
                ctx,
                ExecRequest {
                    language: "lashlang".to_string(),
                    code: "scratch_note = \"after execution\"".to_string(),
                    accept_finish: true,
                },
                lashlang::global_in_memory_lashlang_artifact_store(),
                LashlangSurface::new(
                    lashlang::LashlangAbilities::default(),
                    lashlang::LashlangLanguageFeatures::default(),
                    lashlang::LashlangHostCatalog::new(),
                ),
                None,
                RlmProjectedBindings::default(),
                Arc::new(ProjectionRegistry::new()),
                RlmLashlangExecutionTraceConfig::default(),
            )
            .await
            .expect("execute");
            assert_eq!(response.error, None);

            let globals = state.bound_variable_values(&BTreeSet::new());
            let mut cache = crate::rlm_support::BoundVariableRenderCache::default();
            let rendered = crate::rlm_support::render_bound_variables(
                &mut cache,
                &globals,
                crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
            );

            assert!(
                rendered.contains("- `scratch_note` = after execution"),
                "{}",
                rendered
            );
        });
    }

    #[test]
    #[ignore = "microbenchmark; run with `-- --ignored --nocapture`"]
    fn bench_bound_variables_render_cost() {
        block_on(async {
            let state = RlmExecutionState::new().expect("state");
            let ctx = lash_core::testing::code_execution_context();
            // Realistic mid-game RLM state: a ~25-room map, a 67-entry notes
            // log, and a small inventory.
            let code = "map = {}\n\
                for i in range(25) {\n\
                  map[format(\"room_{}\", i)] = { exits: [\"north\", \"south\", \"east\"], items: [format(\"item_{}\", i), format(\"thing_{}\", i)] }\n\
                }\n\
                notes = []\n\
                for i in range(67) {\n\
                  notes = push(notes, format(\"note {}: a fairly long observation about world state, the current plan, and the next few steps to try\", i))\n\
                }\n\
                inventory = [\"brass lantern\", \"elvish sword\", \"leaflet\"]"
                .to_string();
            let (state, response) = execute_code_unbounded_for_tests(
                state,
                ctx,
                ExecRequest {
                    language: "lashlang".to_string(),
                    code,
                    accept_finish: true,
                },
                lashlang::global_in_memory_lashlang_artifact_store(),
                LashlangSurface::new(
                    lashlang::LashlangAbilities::default(),
                    lashlang::LashlangLanguageFeatures::default(),
                    lashlang::LashlangHostCatalog::new(),
                ),
                None,
                RlmProjectedBindings::default(),
                Arc::new(ProjectionRegistry::new()),
                RlmLashlangExecutionTraceConfig::default(),
            )
            .await
            .expect("execute");
            assert_eq!(response.error, None);

            let exclude = BTreeSet::new();
            let n = 5000u32;

            let t = std::time::Instant::now();
            let mut sink = 0usize;
            for _ in 0..n {
                sink += state.bound_variable_values(&exclude).len();
            }
            let bv_us = t.elapsed().as_nanos() as f64 / n as f64 / 1000.0;

            let globals = state.bound_variable_values(&exclude);

            let mut warm = crate::rlm_support::BoundVariableRenderCache::default();
            let _ = crate::rlm_support::render_bound_variables(
                &mut warm,
                &globals,
                crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
            );
            let t2 = std::time::Instant::now();
            let mut s2 = 0usize;
            for _ in 0..n {
                s2 += crate::rlm_support::render_bound_variables(
                    &mut warm,
                    &globals,
                    crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
                )
                .len();
            }
            let warm_us = t2.elapsed().as_nanos() as f64 / n as f64 / 1000.0;

            let t3 = std::time::Instant::now();
            let mut s3 = 0usize;
            for _ in 0..n {
                let mut cold = crate::rlm_support::BoundVariableRenderCache::default();
                s3 += crate::rlm_support::render_bound_variables(
                    &mut cold,
                    &globals,
                    crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
                )
                .len();
            }
            let cold_us = t3.elapsed().as_nanos() as f64 / n as f64 / 1000.0;

            println!(
                "BENCH vars={} content_chars={}",
                globals.len(),
                s2 / n as usize
            );
            println!("BENCH bound_variable_values : {bv_us:8.3} us/call");
            println!("BENCH render (warm cache)   : {warm_us:8.3} us/call");
            println!("BENCH render (cold cache)   : {cold_us:8.3} us/call");
            println!(
                "BENCH per prompt build (values+render) ~ {:.3} us",
                bv_us + warm_us
            );
            let _ = (sink, s2, s3);
        });
    }

    #[test]
    fn bound_variables_prompt_degrades_large_live_globals() {
        block_on(async {
            let state = RlmExecutionState::new().expect("state");
            let ctx = lash_core::testing::code_execution_context();
            // Same constructs the runtime-perf `rlm_globals` scenario seeds:
            // a large record and a large list that exceed the inline budget.
            let code = "big_map = {}\n\
                for i in range(24) {\n\
                  big_map[format(\"room_{}\", i)] = { exits: [\"north\", \"south\"], items: [format(\"item_{}\", i)] }\n\
                }\n\
                big_notes = []\n\
                for i in range(45) {\n\
                  big_notes = push(big_notes, format(\"note {}: observation\", i))\n\
                }"
            .to_string();
            let (state, response) = execute_code_unbounded_for_tests(
                state,
                ctx,
                ExecRequest {
                    language: "lashlang".to_string(),
                    code,
                    accept_finish: true,
                },
                lashlang::global_in_memory_lashlang_artifact_store(),
                LashlangSurface::new(
                    lashlang::LashlangAbilities::default(),
                    lashlang::LashlangLanguageFeatures::default(),
                    lashlang::LashlangHostCatalog::new(),
                ),
                None,
                RlmProjectedBindings::default(),
                Arc::new(ProjectionRegistry::new()),
                RlmLashlangExecutionTraceConfig::default(),
            )
            .await
            .expect("execute");
            assert_eq!(response.error, None);

            let globals = state.bound_variable_values(&BTreeSet::new());
            let mut cache = crate::rlm_support::BoundVariableRenderCache::default();
            let s = crate::rlm_support::render_bound_variables(
                &mut cache,
                &globals,
                crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
            )
            .to_string();

            // Large record -> type + keys=N + projector preview.
            assert!(s.contains("`big_map`:"), "{s}");
            assert!(s.contains("keys=24"), "{s}");
            assert!(s.contains("≈ {") && s.contains("room_0"), "{s}");
            assert!(s.contains("fields omitted"), "{s}");
            // Large list -> type + len=N + projector preview.
            assert!(s.contains("`big_notes`:"), "{s}");
            assert!(s.contains("len=45"), "{s}");
            assert!(s.contains("≈ [") && s.contains("note 0:"), "{s}");
            assert!(s.contains("items omitted"), "{s}");
        });
    }

    #[test]
    fn flow_to_json_value_emits_projected_marker_for_projected_values() {
        block_on(async {
            let projected = ProjectedValue::scalar("input", FlowValue::String("hello".into()));
            let value = flow_to_json_value(&FlowValue::Projected(projected)).await;
            let obj = value
                .as_object()
                .expect("expected projected wrapper object");
            assert_eq!(obj.len(), 1, "wrapper should have exactly one key");
            assert_eq!(
                obj.get(PROJECTED_JSON_TAG)
                    .and_then(|v| v.as_str())
                    .expect("inner string"),
                "hello"
            );
        });
    }

    #[test]
    fn flow_to_json_value_preserves_projection_ref_without_materializing() {
        block_on(async {
            let host = Arc::new(SnapshotProjectedToolText::default());
            let reference = ProjectionRef::new("memory", serde_json::json!("doc"));
            let projected = ProjectedValue::custom_with_projection_ref(
                "doc",
                host.clone(),
                serde_json::json!(reference),
            );
            let value = flow_to_json_value(&FlowValue::Projected(projected)).await;
            assert_eq!(host.render_count.load(Ordering::SeqCst), 0);
            assert_eq!(host.materialize_count.load(Ordering::SeqCst), 0);
            assert_eq!(
                value,
                serde_json::json!({
                    PROJECTED_JSON_TAG: {
                        lash_rlm_types::PROJECTION_REF_JSON_TAG: {
                            "kind": "memory",
                            "key": "doc",
                        }
                    }
                })
            );
        });
    }

    #[test]
    fn image_json_round_trip_preserves_mime_and_image_type() {
        block_on(async {
            let image = lashlang::ImageValue::new(
                "image-sha256",
                lash_core::MediaType::parse("image/webp").unwrap(),
                "cover",
                73,
                Some(320),
                Some(180),
            );
            let flow = FlowValue::Image(Box::new(image));
            let json = flow_to_json_value(&flow).await;

            assert_eq!(json.get("mime").and_then(Value::as_str), Some("image/webp"));
            assert!(json.get("media_type").is_none());
            assert_eq!(json_to_flow_value(json), flow);
        });
    }

    #[test]
    fn executor_snapshot_round_trips_projection_ref_metadata() {
        let reference = ProjectionRef::new("memory", serde_json::json!("doc"));
        let mut state = RlmExecutionState::new().expect("state");
        state
            .rlm
            .insert_global(
                "doc".to_string(),
                FlowValue::Projected(ProjectedValue::custom_with_projection_ref(
                    "doc",
                    Arc::new(SnapshotProjectedToolText::default()),
                    serde_json::json!(reference),
                )),
            )
            .expect("insert projected global");

        let snapshot =
            hydrate_snapshot(state.snapshot_execution_state().expect("executor snapshot"));

        let mut restored_execution = RlmExecutionState::new().expect("restored state");
        restored_execution
            .restore_execution_state(&snapshot)
            .expect("restore runtime");
        let restored = restored_execution.rlm;
        let restored_snapshot = restored.snapshot();
        let Some(FlowValue::Projected(projected)) = restored_snapshot.globals().get("doc") else {
            panic!("expected restored projected value");
        };
        assert_eq!(
            projected.projection_ref(),
            Some(&serde_json::json!({"kind": "memory", "key": "doc"}))
        );
    }

    #[test]
    fn flow_record_to_json_value_marks_only_projected_entries() {
        block_on(async {
            let projected = ProjectedValue::scalar("input", FlowValue::String("p".into()));
            let mut record = FlowRecord::default();
            record.insert("proj".to_string(), FlowValue::Projected(projected));
            record.insert("glob".to_string(), FlowValue::String("g".into()));

            let value = flow_record_to_json_value(&record).await;
            let obj = value.as_object().expect("record object");
            // proj entry must be wrapped in {"__projected__": ...}
            let proj = obj
                .get("proj")
                .and_then(|v| v.as_object())
                .expect("proj entry is an object");
            assert!(proj.contains_key(PROJECTED_JSON_TAG));
            // glob entry stays a bare string
            assert_eq!(obj.get("glob").and_then(|v| v.as_str()).expect("glob"), "g");
        });
    }

    #[test]
    fn flow_record_to_tool_args_materializes_ordinary_tools() {
        block_on(async {
            let projected = ProjectedValue::scalar("input", FlowValue::String("p".into()));
            let mut record = FlowRecord::default();
            record.insert("query".to_string(), FlowValue::Projected(projected));

            let value = flow_record_to_tool_args(
                &record,
                &lash_core::ToolArgumentProjectionPolicy::MaterializeProjectedValues,
            )
            .await;

            assert_eq!(value, serde_json::json!({ "query": "p" }));
        });
    }

    #[test]
    fn flow_record_to_tool_args_preserves_only_seed_projected_roots() {
        block_on(async {
            let reference = ProjectionRef::new("memory", serde_json::json!("doc"));
            let projected_root = ProjectedValue::custom_with_projection_ref(
                "doc",
                Arc::new(SnapshotProjectedToolText::default()),
                serde_json::json!(reference),
            );
            let mut computed = FlowRecord::default();
            computed.insert(
                "summary".to_string(),
                FlowValue::Projected(ProjectedValue::scalar(
                    "summary",
                    FlowValue::String("materialized summary".into()),
                )),
            );
            let mut seed = FlowRecord::default();
            seed.insert("problem".to_string(), FlowValue::Projected(projected_root));
            seed.insert(
                "computed".to_string(),
                FlowValue::Record(Arc::new(computed)),
            );
            let mut record = FlowRecord::default();
            record.insert(
                "task".to_string(),
                FlowValue::Projected(ProjectedValue::scalar(
                    "task",
                    FlowValue::String("inspect".into()),
                )),
            );
            record.insert("seed".to_string(), FlowValue::Record(Arc::new(seed)));

            let value = flow_record_to_tool_args(
                &record,
                &lash_core::ToolArgumentProjectionPolicy::preserve_projected_refs_in_field("seed"),
            )
            .await;

            assert_eq!(
                value,
                serde_json::json!({
                    "task": "inspect",
                    "seed": {
                        "problem": {
                            "__projected__": {
                                "__projection_ref__": {
                                    "kind": "memory",
                                    "key": "doc"
                                }
                            }
                        },
                        "computed": {
                            "summary": "materialized summary"
                        }
                    }
                })
            );
        });
    }

    #[test]
    fn parser_accepts_bounded_while_with_nested_for() {
        let source = r#"pool_i = 0
final_ids = []
candidate_pools = [{ matches: ["a", "b"] }]
while len(final_ids) < 2 && pool_i < len(candidate_pools) {
  for m in candidate_pools[pool_i].matches {
    final_ids = final_ids + [m]
  }
  pool_i = pool_i + 1
}
finish final_ids"#;

        lashlang::compile(source).expect("while should compile");
    }

    /// Closure-bearing TypeScript cells, mirroring the closure shapes of
    /// `lash-typescript`'s durability corpus (`tests/dialect.rs`): a recursive
    /// function, a nested function, an arrow that captures and is returned, and
    /// an inline arrow whose closure becomes garbage immediately.
    const CLOSURE_BEARING_TYPESCRIPT_CELLS: &[&str] = &[
        "function fact(n: number): number { if (n <= 1) { return 1; } return fact(n - 1) * n; } const f5 = fact(5);",
        "const top = 9; function outerFn(): number { function innerFn(): number { return top; } return innerFn(); } const nested = outerFn();",
        "const base = 10; const outer = () => { const inner = () => base; return inner; }; const held = outer();",
        "const xs = [1].map(x => x + 1);",
    ];

    /// The trivial next cell from the FIG-1562 report.
    const TRIVIAL_NEXT_CELL: &str = "finish(6 * 7);";

    async fn execute_typescript_test_cell(
        state: RlmExecutionState,
        code: &str,
    ) -> (RlmExecutionState, ExecResponse) {
        execute_typescript_code_with_bounds(
            state,
            lash_core::testing::code_execution_context(),
            ExecRequest {
                language: "typescript".to_string(),
                code: code.to_string(),
                accept_finish: true,
            },
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
            lashlang::ExecutionBounds::unbounded(),
        )
        .await
        .expect("execute TypeScript cell")
    }

    /// A closure allocated by one cell must not fail validation of the next
    /// cell's program.
    ///
    /// Each RLM cell compiles its own `CompiledProgram` while the heap survives
    /// the cell boundary, so a closure from cell N is re-validated against cell
    /// N+1's function table. See FIG-1562.
    #[test]
    fn a_closure_from_one_typescript_cell_does_not_poison_the_next_cell() {
        block_on(async {
            for cell in CLOSURE_BEARING_TYPESCRIPT_CELLS {
                let state = RlmExecutionState::for_engine("typescript").expect("TypeScript state");
                let (state, first) = execute_typescript_test_cell(state, cell).await;
                assert!(first.error.is_none(), "cell `{cell}`: {:?}", first.error);

                let (_, second) = execute_typescript_test_cell(state, TRIVIAL_NEXT_CELL).await;
                assert!(
                    second.error.is_none(),
                    "the trivial cell after `{cell}` failed: {:?}",
                    second.error
                );
            }
        });
    }

    /// The same composition across the durability boundary: snapshot a state
    /// holding a real closure, restore it into a fresh engine, then run a
    /// *different* cell against it.
    ///
    /// This one passes today, and the sibling test above is why it is worth
    /// keeping: the closure demonstrably survives the cell boundary in memory,
    /// so the fact that a *restored* state accepts the next cell is a real
    /// property of the RLM persistence path (closure-valued globals do not
    /// reach the snapshot), not an artefact of an empty heap. It guards that
    /// property once the cell boundary itself is fixed. See FIG-1562.
    #[test]
    fn a_restored_typescript_closure_does_not_poison_a_different_cell() {
        block_on(async {
            for cell in CLOSURE_BEARING_TYPESCRIPT_CELLS {
                let state = RlmExecutionState::for_engine("typescript").expect("TypeScript state");
                let (mut state, first) = execute_typescript_test_cell(state, cell).await;
                assert!(first.error.is_none(), "cell `{cell}`: {:?}", first.error);

                let snapshot = hydrate_snapshot(
                    state
                        .snapshot_execution_state()
                        .expect("snapshot components"),
                );
                let mut restored =
                    RlmExecutionState::for_engine("typescript").expect("restored state");
                restored
                    .restore_execution_state(&snapshot)
                    .expect("restore TypeScript execution state");

                let (_, response) = execute_typescript_test_cell(restored, TRIVIAL_NEXT_CELL).await;
                assert!(
                    response.error.is_none(),
                    "the trivial cell after restoring `{cell}` failed: {:?}",
                    response.error
                );
            }
        });
    }
}
