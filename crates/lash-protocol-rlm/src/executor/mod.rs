mod host_bridge;
mod snapshot;
mod state;

pub use snapshot::RLM_SNAPSHOT_VERSION;
pub use state::RlmExecutionState;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
#[cfg(any(test, feature = "testing"))]
use std::sync::atomic::{AtomicBool, Ordering};

use lash_core::{
    ExecRequest, ExecResponse, RuntimeExecutionContext, TraceContext,
    facade_support::TraceRuntimeScope, facade_support::TraceRuntimeSubject,
    facade_support::TraceSink,
};
// Cell execution itself is infallible, so the only fallible surface left in
// this module is the feature-gated performance fixture.
#[cfg(feature = "testing")]
use lash_core::SessionError;
use lash_lashlang_runtime::{
    LashlangSurface, TraceLanguageExecution, TraceLanguageExecutionIdentity,
    TraceLanguageExecutionMap, TraceLanguageExecutionPayload, TraceLanguageExecutionStatus,
};
use lashlang::{ExecutionOutcome, State as FlowState};

use self::host_bridge::{
    CollectedExecutionOutput, HostBridge, HostBridgeConfig, LashlangExecutionTrace,
};
pub(crate) use crate::dialect::{RlmSourceContext, SourceDialect};
use crate::projection::{
    ProjectionResolver, RlmProjectedBindings, flow_to_json_value, json_to_flow_value,
    projected_bindings, prune_projected_binding_names, rehydrate_projected_globals,
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
    state: &mut RlmExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    request: ExecRequest,
    artifact_store: Arc<dyn lashlang::LashlangArtifactStore>,
    lashlang_surface: LashlangSurface,
    deferred_tool_resolver: Option<lash_lashlang_runtime::SharedDeferredToolResolver>,
    session_projected_bindings: RlmProjectedBindings,
    projection_resolver: Arc<dyn ProjectionResolver>,
    lashlang_execution_trace_config: RlmLashlangExecutionTraceConfig,
) -> ExecResponse {
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
#[allow(
    dead_code,
    reason = "the bounded entrypoint is consumed by test and testing-feature harnesses"
)]
pub(crate) async fn execute_code_with_bounds(
    state: &mut RlmExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    request: ExecRequest,
    artifact_store: Arc<dyn lashlang::LashlangArtifactStore>,
    lashlang_surface: LashlangSurface,
    deferred_tool_resolver: Option<lash_lashlang_runtime::SharedDeferredToolResolver>,
    session_projected_bindings: RlmProjectedBindings,
    projection_resolver: Arc<dyn ProjectionResolver>,
    lashlang_execution_trace_config: RlmLashlangExecutionTraceConfig,
    execution_bounds: lashlang::ExecutionBounds,
) -> ExecResponse {
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
        RlmSourceContext::cell(SourceDialect::Lashlang),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_code_with_dialect_and_bounds(
    state: &mut RlmExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    request: ExecRequest,
    artifact_store: Arc<dyn lashlang::LashlangArtifactStore>,
    lashlang_surface: LashlangSurface,
    deferred_tool_resolver: Option<lash_lashlang_runtime::SharedDeferredToolResolver>,
    session_projected_bindings: RlmProjectedBindings,
    projection_resolver: Arc<dyn ProjectionResolver>,
    lashlang_execution_trace_config: RlmLashlangExecutionTraceConfig,
    execution_bounds: lashlang::ExecutionBounds,
    source: RlmSourceContext,
) -> ExecResponse {
    let start = std::time::Instant::now();
    let clean_code = clean_model_code(&request.code);
    Box::pin(execute_code_inner(
        state,
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
        source,
    ))
    .await
}

/// Feature-gated fixture that lets the repository's performance harness drive
/// the production RLM execution-state capture without exposing executor
/// internals as public protocol API.
#[cfg(feature = "testing")]
pub struct RlmCheckpointPerfFixture {
    state: RlmExecutionState,
    binding_count: usize,
    payload_bytes: usize,
}

#[cfg(feature = "testing")]
impl RlmCheckpointPerfFixture {
    pub fn new(binding_count: usize, payload_bytes: usize) -> Result<Self, SessionError> {
        let mut state = RlmExecutionState::for_engine("lashlang");
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
            state,
            binding_count,
            payload_bytes,
        })
    }

    pub fn capture(&mut self) -> Result<lash_core::plugin::ExecutionStateSnapshot, SessionError> {
        self.state.snapshot_execution_state()
    }

    pub fn acknowledge_capture(&mut self) {
        self.state.acknowledge_execution_state_capture();
    }

    pub async fn assign_one(&mut self, index: usize, turn: usize) -> Result<(), SessionError> {
        let binding = index % self.binding_count.max(1);
        let code = format!(
            "mid_{binding} = push(mid_{binding}, \"turn-{turn}-{}\")",
            "y".repeat(self.payload_bytes / 8)
        );
        let response = execute_code_with_bounds(
            &mut self.state,
            lash_core::testing::code_execution_context(),
            ExecRequest {
                language: "lashlang".to_string(),
                code,
            },
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            None,
            RlmProjectedBindings::default(),
            Arc::new(crate::ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
            lashlang::ExecutionBounds::unbounded(),
        )
        .await;
        if let Some(error) = response.error {
            return Err(SessionError::Protocol(format!(
                "RLM checkpoint perf assignment failed: {}",
                error.message,
            )));
        }
        Ok(())
    }

    pub fn absorb_dirty_assignments(&mut self) {
        self.state.absorb_pending_assignments_for_perf();
    }

    pub fn restore(state: &lash_core::plugin::HydratedExecutionState) -> Result<(), SessionError> {
        let mut restored = RlmExecutionState::for_engine("lashlang");
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
    source: RlmSourceContext,
) -> ExecResponse {
    state.mark_execution_started();
    let execution_checkpoint = state.execution_checkpoint();
    state.begin_code_execution(execution_checkpoint);
    select_deferred_resolution_link(state, &ctx);
    let parsed_program = match source.dialect {
        SourceDialect::Lashlang => lashlang::parse(code).ok(),
        SourceDialect::Typescript => lash_typescript::parse(code).ok(),
    };

    // gather → journal → mask → fold: every parsed resource-bearing cell first
    // consults the deferred journal, even if no live resolver and no checkpoint
    // projection are available. This is what closes the postcommit / before-
    // projection crash window. Journal outcomes then mask exact paths while the
    // ambient Tool Catalog is built, before its collision validation can
    // preempt recorded authority. Unrelated catalog errors remain ordinary host
    // failures.
    let mut host_environment = if let Some(program) = parsed_program
        .as_ref()
        .filter(|_| state.deferred_resolutions.link_key.is_some())
    {
        let _phase = ctx.named_phase("rlm_lashlang.deferred_resolve");
        match lash_lashlang_runtime::resolve_and_build_deferred_environment(
            program,
            &lashlang_surface,
            ctx.tool_catalog().as_ref(),
            deferred_tool_resolver.as_ref(),
            &mut state.deferred_resolutions,
            &ctx,
        )
        .await
        {
            Ok(environment) => environment,
            Err(error) => {
                ctx.record_nested_runtime_effect_error(error.runtime_effect_error());
                return exec_setup_failure_or_stop(
                    state,
                    &ctx,
                    lash_core::CellFailureKind::Host,
                    error.to_string(),
                    start,
                    Vec::new(),
                );
            }
        }
    } else {
        match lashlang_surface.host_environment(ctx.tool_catalog().as_ref()) {
            Ok(environment) => environment,
            Err(error) => {
                emit_step_trace(
                    &ctx,
                    &lashlang_execution_trace_config,
                    Err(&format!("invalid Lashlang host tool surface: {error}")),
                );
                return exec_setup_failure_or_stop(
                    state,
                    &ctx,
                    lash_core::CellFailureKind::Host,
                    format!("invalid Lashlang host tool surface: {error}"),
                    start,
                    Vec::new(),
                );
            }
        }
    };

    let mut live_global_names = state
        .rlm
        .globals()
        .iter()
        .map(|(name, _)| name.to_string())
        .collect::<BTreeSet<_>>();
    live_global_names.insert("history".to_string());
    live_global_names.extend(session_projected_bindings.names());
    host_environment = host_environment.with_globals(live_global_names);
    host_environment =
        host_environment.with_process_handles(process_handle_names(state.rlm.globals()));

    // The kind is decided here, while the failure is still a typed diagnostic.
    // "Compilation failed" is not enough to classify it: a misspelled name and a
    // forbidden construct both fail here and need opposite advice.
    let compile_result: Result<_, (lash_core::CellFailureKind, String)> = {
        let _phase = ctx.named_phase("rlm_lashlang.compile_link");
        match source.dialect {
            SourceDialect::Lashlang => state
                .linked_programs
                .get_or_compile(code, &host_environment)
                .map_err(|error| match error {
                    lashlang::LinkedProgramCacheError::Parse(error) => (
                        lashlang_parse_feedback_kind(&error),
                        format_rlm_parse_diagnostic(code, &error, source.channel),
                    ),
                    lashlang::LinkedProgramCacheError::Link(error) => (
                        lashlang_link_feedback_kind(&error),
                        format_rlm_link_diagnostic(code, &error),
                    ),
                    // Future compiler failures still produce diagnostic feedback without a guessed repair.
                    _ => (lash_core::CellFailureKind::Host, error.to_string()),
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
                None => lash_typescript::parse_with_globals_and_process_handles(
                    code,
                    &host_environment.globals,
                    &host_environment.process_handles,
                )
                .map_err(|error| {
                    let error = refine_typescript_method_diagnostic(code, &host_environment, error);
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
    emit_step_trace(
        &ctx,
        &lashlang_execution_trace_config,
        compile_result
            .as_ref()
            .map(|_| ())
            .map_err(|(_, diagnostic)| diagnostic.as_str()),
    );
    let cached_program = match compile_result {
        Ok(program) => program,
        Err((kind, error)) => {
            return exec_setup_failure_or_stop(state, &ctx, kind, error, start, Vec::new());
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
            return exec_setup_failure_or_stop(
                state,
                &ctx,
                lash_core::CellFailureKind::Host,
                format!("failed to store lashlang module artifact: {err}"),
                start,
                Vec::new(),
            );
        }
        state
            .stored_lashlang_modules
            .insert(linked_module.module_ref.clone());
    }
    let compiled = cached_program.compiled_program();

    let rehydrated = {
        let _phase = ctx.named_phase("rlm_lashlang.rehydrate_projected_globals");
        rehydrate_projected_globals(&mut state.rlm, Arc::clone(&projection_resolver)).await
    };
    let degraded_bindings = match rehydrated {
        Ok(rehydrated) => rehydrated.degraded_bindings,
        Err(err) => {
            return exec_setup_failure_or_stop(
                state,
                &ctx,
                lash_core::CellFailureKind::Host,
                err,
                start,
                Vec::new(),
            );
        }
    };

    let projected = {
        let _phase = ctx.named_phase("rlm_lashlang.resolve_projected_bindings");
        match projected_bindings(&ctx, session_projected_bindings, projection_resolver).await {
            Ok(projected) => projected,
            Err(err) => {
                return exec_setup_failure_or_stop(
                    state,
                    &ctx,
                    lash_core::CellFailureKind::Host,
                    err,
                    start,
                    degraded_bindings,
                );
            }
        }
    };
    let projected_names = projected.names().collect::<Vec<_>>();
    state.mark_globals_removed(projected_names.iter().map(String::as_str));
    prune_projected_binding_names(&mut state.rlm, projected_names.iter().map(String::as_str));
    let deferred_execution_grants = deferred_execution_grants(&state.deferred_resolutions);
    let lashlang_execution_trace = foreground_lashlang_execution_trace(
        &ctx,
        &linked_module.artifact,
        &lashlang_execution_trace_config,
        source.dialect.language_id(),
    );
    if let Some(trace) = &lashlang_execution_trace {
        emit_foreground_execution_started(trace, &linked_module.artifact);
    }
    let print_projector = Arc::new(crate::rlm_support::print_history_projector());
    let host = HostBridge::new(HostBridgeConfig {
        ctx: ctx.clone(),
        print_projector,
        lashlang_execution_trace: lashlang_execution_trace.clone(),
        host_environment,
        deferred_execution_grants,
        artifact_store: Arc::clone(&artifact_store),
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
        Ok(ExecutionOutcome::Failed(value)) if host.cancellation_observed() => {
            state.cancel_code_execution();
            return exec_response_from(
                host.into_collected(),
                Some(lash_core::CellFailure::new(
                    lash_core::CellFailureKind::Host,
                    format!("foreground execution stopped while returning failure: {value}"),
                )),
                None,
                start,
                degraded_bindings.clone(),
            );
        }
        Ok(ExecutionOutcome::Failed(value)) => {
            return exec_response_from(
                host.into_collected(),
                Some(lash_core::CellFailure::new(
                    lash_core::CellFailureKind::Program,
                    format!("process failed in foreground execution: {value}"),
                )),
                None,
                start,
                degraded_bindings.clone(),
            );
        }
        Err(error) => {
            #[cfg(any(test, feature = "testing"))]
            assert!(
                !EXECUTION_BOUND_EXHAUSTION_LOUD.load(Ordering::SeqCst)
                    || !error.is_execution_bound_exhausted(),
                "confidence execution exhausted a required Lashlang bound: {error}"
            );
            let kind = lashlang_runtime_feedback_kind(&error, host.cancellation_observed());
            let failure = runtime_failure.unwrap_or(lashlang::RuntimeFailure { error, span: None });
            if host.cancellation_observed()
                || matches!(&failure.error, lashlang::RuntimeError::HostCancelled)
            {
                state.cancel_code_execution();
            }
            return exec_response_from(
                host.into_collected(),
                Some(lash_core::CellFailure::new(
                    kind,
                    lashlang::format_runtime_diagnostic(code, &failure.error, failure.span),
                )),
                None,
                start,
                degraded_bindings.clone(),
            );
        }
    };
    exec_response_from(
        host.into_collected(),
        None,
        terminal_finish,
        start,
        degraded_bindings,
    )
}

fn process_handle_names(globals: &lashlang::Record) -> BTreeSet<String> {
    globals
        .iter()
        .filter_map(|(name, value)| {
            value
                .as_record()
                .is_some_and(lashlang::is_process_handle)
                .then_some(name.to_string())
        })
        .collect()
}

/// Classifies a typed Lashlang runtime outcome before it enters the response.
fn lashlang_runtime_feedback_kind(
    error: &lashlang::RuntimeError,
    host_cancelled: bool,
) -> lash_core::CellFailureKind {
    match error {
        _ if host_cancelled => lash_core::CellFailureKind::Host,
        lashlang::RuntimeError::HostCancelled => lash_core::CellFailureKind::Host,
        error if error.is_execution_bound_exhausted() => lash_core::CellFailureKind::Policy,
        _ => lash_core::CellFailureKind::Program,
    }
}

fn exec_setup_failure_with_degraded(
    error: lash_core::CellFailure,
    start: std::time::Instant,
    degraded_bindings: Vec<lash_core::DegradedBinding>,
) -> ExecResponse {
    ExecResponse {
        observations: Vec::new(),
        calls: Vec::new(),
        printed_images: Vec::new(),
        error: Some(error),
        duration_ms: start.elapsed().as_millis() as u64,
        degraded_bindings,
        terminal_finish: None,
    }
}

fn exec_setup_failure_or_stop(
    state: &mut RlmExecutionState,
    ctx: &RuntimeExecutionContext<'_>,
    kind: lash_core::CellFailureKind,
    error: impl Into<String>,
    start: std::time::Instant,
    degraded_bindings: Vec<lash_core::DegradedBinding>,
) -> ExecResponse {
    if ctx.is_cancelled() {
        state.cancel_code_execution();
        return exec_setup_failure_with_degraded(
            lash_core::CellFailure::new(
                lash_core::CellFailureKind::Host,
                "foreground execution stopped during setup",
            ),
            start,
            degraded_bindings,
        );
    }
    exec_setup_failure_with_degraded(
        lash_core::CellFailure::new(kind, error),
        start,
        degraded_bindings,
    )
}

fn exec_response_from(
    collected: CollectedExecutionOutput,
    error: Option<lash_core::CellFailure>,
    terminal_finish: Option<serde_json::Value>,
    start: std::time::Instant,
    degraded_bindings: Vec<lash_core::DegradedBinding>,
) -> ExecResponse {
    ExecResponse {
        observations: collected.observations,
        calls: collected.calls,
        printed_images: collected.printed_images,
        error,
        duration_ms: start.elapsed().as_millis() as u64,
        degraded_bindings,
        terminal_finish,
    }
}

/// Whether a TypeScript rejection refuses a construct or reports a wrong
/// program.
///
/// Asked of the diagnostic, not of its code. Three codes carry both families —
/// `TS_METHOD_UNSUPPORTED` covers `Promise.then` and `[].map()` alike — so only
/// the site that emitted it knows, and it records the answer at construction.
fn typescript_feedback_kind(error: &lash_typescript::Diagnostic) -> lash_core::CellFailureKind {
    if error.is_dialect_refusal() {
        lash_core::CellFailureKind::Policy
    } else {
        lash_core::CellFailureKind::Program
    }
}

/// Re-lowers method failures with the host catalog that the cache-oriented
/// parse entry point does not accept. Valid cells still take the single-parse
/// path; only a method diagnostic pays this retry to distinguish a real module
/// shadow from an ordinary local receiver.
fn refine_typescript_method_diagnostic(
    source: &str,
    host: &lashlang::LashlangHostEnvironment,
    error: lash_typescript::Diagnostic,
) -> lash_typescript::Diagnostic {
    if error.code != lash_typescript::DiagnosticCode::MethodUnsupported {
        return error;
    }
    match lash_typescript::link(source, host) {
        Err(contextual) if contextual.code == error.code => contextual,
        _ => error,
    }
}

/// Whether a Lashlang parse failure is a refusal or a wrong program.
///
/// Almost all of them are the program: a lex failure, an unexpected token, a
/// missing `finish` value. The refusals are the retired forms and the rules
/// about where a construct may appear — no rewrite of the same approach is
/// accepted, so the model must be told to write a different one.
fn lashlang_parse_feedback_kind(error: &lashlang::ParseError) -> lash_core::CellFailureKind {
    match error {
        lashlang::ParseError::SubmitRemoved { .. }
        | lashlang::ParseError::DeclarativeTriggerRemoved { .. }
        | lashlang::ParseError::SessionProcessAdminOutsideBlock { .. }
        | lashlang::ParseError::ForegroundControlInsideProcess { .. }
        | lashlang::ParseError::NestingTooDeep { .. } => lash_core::CellFailureKind::Policy,
        _ => lash_core::CellFailureKind::Program,
    }
}

/// Whether a link failure is a refusal or a wrong program.
///
/// An unknown name, an unknown operation, an arity or type mismatch: those are
/// the program. A bare tool call, a disabled feature, an opaque descriptor read,
/// and the placement rules are the host declining, and no amount of debugging
/// changes them.
fn lashlang_link_feedback_kind(error: &lashlang::LinkError) -> lash_core::CellFailureKind {
    match error {
        lashlang::LinkError::BareToolCall { .. }
        | lashlang::LinkError::FeatureDisabled { .. }
        | lashlang::LinkError::OpaqueHostDescriptorAccess { .. }
        | lashlang::LinkError::ProcessLifecycleOutsideProcess { .. }
        | lashlang::LinkError::TriggerEventOutsideInputs { .. } => {
            lash_core::CellFailureKind::Policy
        }
        _ => lash_core::CellFailureKind::Program,
    }
}

/// Render a parse failure for the model, with the cell-delimiter warning only
/// where a cell delimiter exists.
///
/// The warning explains a truncation the model cannot see: a `</lashlang>` line
/// inside a multiline string closes the cell early, so the executor receives a
/// program that stops mid-literal. Native `execute_code` calls (ADR 0083) carry
/// the program as a tool argument, where no delimiter can truncate anything —
/// there the sentence names syntax the model never wrote and sends it looking
/// for a cause that does not exist.
///
/// Gated on the channel alone, not on the source containing `</lashlang>`:
/// by the time the executor sees the code, cell extraction has already consumed
/// the delimiter that truncated it, so an implicated delimiter is exactly the
/// case where the source cannot mention one.
fn format_rlm_parse_diagnostic(
    code: &str,
    error: &lashlang::ParseError,
    channel: crate::plugin::RlmChannel,
) -> String {
    let diagnostic = lashlang::format_parse_diagnostic(code, error);
    match channel {
        crate::plugin::RlmChannel::Cell => format!(
            "{diagnostic}\n\nA standalone `</lashlang>` line terminates the outer cell even inside multiline source text; construct that content without a standalone delimiter line."
        ),
        crate::plugin::RlmChannel::NativeTool => diagnostic,
    }
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
            Some((execution_grant.manifest().id.clone(), execution_grant))
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

fn emit_step_trace(
    ctx: &RuntimeExecutionContext<'_>,
    config: &RlmLashlangExecutionTraceConfig,
    result: Result<(), &str>,
) {
    let Some(sink) = &config.sink else { return };
    let Some(invocation) = ctx.parent_invocation() else {
        return;
    };
    let context = lash_core::facade_support::trace_context_for_runtime_invocation(
        config.trace_context.clone(),
        invocation,
    );
    let Some(step_index) = context.protocol_iteration else {
        return;
    };
    let outcome = match result {
        Ok(()) => lash_trace::TraceRlmStepOutcome::Ok,
        Err(diagnostic) => lash_trace::TraceRlmStepOutcome::Failure {
            diagnostic: lash_sansio::session_model::truncate_raw_error(diagnostic),
        },
    };
    let _ = sink.append(&lash_trace::TraceRecord::new(
        context,
        lash_trace::TraceEvent::RlmStep {
            step_index,
            outcome,
        },
    ));
}

fn foreground_lashlang_execution_trace(
    ctx: &RuntimeExecutionContext<'_>,
    artifact: &lashlang::ModuleArtifact,
    config: &RlmLashlangExecutionTraceConfig,
    language: &'static str,
) -> Option<LashlangExecutionTrace> {
    let sink = config.sink.as_ref()?.clone();
    let invocation = ctx.parent_invocation()?;
    let effect_id = invocation.effect_id()?;
    let address = invocation.effect_address()?.clone();
    Some(LashlangExecutionTrace::new(
        sink,
        language,
        config.trace_context.clone(),
        TraceLanguageExecutionIdentity {
            scope: TraceRuntimeScope {
                session_id: invocation.attribution.session_id.clone(),
                turn_id: invocation.attribution.turn_id.clone(),
                turn_index: invocation.attribution.turn_index,
                protocol_iteration: invocation.attribution.protocol_iteration,
            },
            subject: TraceRuntimeSubject::Effect {
                address,
                effect_id: effect_id.to_string(),
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
    trace.emit(TraceLanguageExecution {
        event_key: trace.event_key("started"),
        identity: trace.identity().clone(),
        payload: TraceLanguageExecutionPayload::ExecutionStarted {
            execution_map: trace_main_map(artifact),
        },
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
    trace.emit(TraceLanguageExecution {
        event_key: trace.event_key("finished"),
        identity: trace.identity().clone(),
        payload: TraceLanguageExecutionPayload::ExecutionFinished { status, error },
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
mod tests;

#[cfg(test)]
mod parked_tests;

#[cfg(test)]
pub(crate) use parked_tests::{
    ParkedCellEvidence, execute_parked_cell_for_tests, parked_cell_context_for_tests,
};
