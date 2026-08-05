mod files;
mod host_bridge;
mod snapshot;
mod state;

#[cfg(test)]
use snapshot::restore_runtime;
pub use state::RlmExecutionState;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_core::{
    ExecRequest, ExecResponse, RuntimeEffectKind, RuntimeExecutionContext, SessionError,
    TraceContext, facade_support::TraceRuntimeScope, facade_support::TraceRuntimeSubject,
    facade_support::TraceSink,
};
use lash_lashlang_runtime::{
    LashlangSurface, TraceLashlangExecutionEvent, TraceLashlangExecutionIdentity, TraceLashlangMap,
    TraceLashlangStatus,
};
use lashlang::{ExecutionOutcome, State as FlowState};

use self::host_bridge::{HostBridge, HostBridgeConfig, LashlangExecutionTrace};
use crate::projection::{
    ProjectionResolver, RLM_TURN_INPUT_PLUGIN_ID, RlmProjectedBindings, RlmProjectionExtension,
    flow_to_json_value, json_to_flow_value, projected_bindings, prune_projected_binding_names,
    rehydrate_projected_globals,
};

#[derive(Clone, Default)]
pub(crate) struct RlmLashlangExecutionTraceConfig {
    pub(crate) sink: Option<Arc<dyn TraceSink>>,
    pub(crate) trace_context: TraceContext,
}

#[allow(clippy::too_many_arguments)]
pub async fn execute_code(
    mut state: RlmExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    request: ExecRequest,
    artifact_store: Arc<dyn lashlang::LashlangArtifactStore>,
    lashlang_surface: LashlangSurface,
    deferred_tool_resolver: Option<lash_lashlang_runtime::SharedDeferredToolResolver>,
    session_projected_bindings: RlmProjectedBindings,
    projection_resolver: Arc<dyn ProjectionResolver>,
    lashlang_execution_trace_config: RlmLashlangExecutionTraceConfig,
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
    ))
    .await;
    Ok((state, response))
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
) -> ExecResponse {
    state.dirty = true;
    select_deferred_resolution_link(state, &ctx);
    let mut host_environment = match lashlang_surface.host_environment(ctx.tool_catalog().as_ref())
    {
        Ok(host_environment) => host_environment,
        Err(err) => {
            return ExecResponse {
                observations: Vec::new(),
                observation_truncation: Vec::new(),
                tool_calls: Vec::new(),
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
        if let Ok(program) = lashlang::parse(code) {
            host_environment = lash_lashlang_runtime::resolve_and_fold_deferred(
                &program,
                host_environment,
                deferred_tool_resolver.as_ref(),
                &mut state.deferred_resolutions,
            )
            .await;
        }
    }

    let compile_result = {
        let _phase = ctx.named_phase("rlm_lashlang.compile_link");
        state
            .linked_programs
            .get_or_compile(code, &host_environment)
    };
    let cached_program = match compile_result {
        Ok(program) => program,
        Err(lashlang::LinkedProgramCacheError::Parse(err)) => {
            return ExecResponse {
                observations: Vec::new(),
                observation_truncation: Vec::new(),
                tool_calls: Vec::new(),
                images: Vec::new(),
                printed_images: Vec::new(),
                error: Some(lashlang::format_parse_diagnostic(code, &err)),
                duration_ms: start.elapsed().as_millis() as u64,
                terminal_finish: None,
            };
        }
        Err(lashlang::LinkedProgramCacheError::Link(err)) => {
            return ExecResponse {
                observations: Vec::new(),
                observation_truncation: Vec::new(),
                tool_calls: Vec::new(),
                images: Vec::new(),
                printed_images: Vec::new(),
                error: Some(format_rlm_link_diagnostic(code, &err)),
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
    prune_projected_binding_names(&mut state.rlm, projected_names.iter().map(String::as_str));
    let tool_result_projectors = tool_result_projectors(&ctx);
    let deferred_execution_grants = deferred_execution_grants(&state.deferred_resolutions);
    let lashlang_execution_trace = foreground_lashlang_execution_trace(
        &ctx,
        &linked_module.artifact,
        &lashlang_execution_trace_config,
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
                images: Vec::new(),
                printed_images: collected.printed_images,
                error: Some(format!("process failed in foreground execution: {value}")),
                duration_ms: start.elapsed().as_millis() as u64,
                terminal_finish: None,
            };
        }
        Err(error) => {
            let failure = runtime_failure.unwrap_or(lashlang::RuntimeFailure { error, span: None });
            let collected = host.into_collected();
            return ExecResponse {
                observations: collected.observations,
                observation_truncation: collected.observation_truncation,
                tool_calls: collected.tool_calls,
                images: Vec::new(),
                printed_images: collected.printed_images,
                error: Some(lashlang::format_runtime_diagnostic(
                    code,
                    &failure.error,
                    failure.span,
                )),
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
        images: Vec::new(),
        printed_images: collected.printed_images,
        error: None,
        duration_ms: start.elapsed().as_millis() as u64,
        terminal_finish,
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
        config.trace_context.clone(),
        TraceLashlangExecutionIdentity {
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
    trace.emit(TraceLashlangExecutionEvent::ExecutionStarted {
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
            (TraceLashlangStatus::Completed, None)
        }
        Ok(ExecutionOutcome::Failed(value)) => {
            (TraceLashlangStatus::Failed, Some(value.to_string()))
        }
        Err(error) => (
            TraceLashlangStatus::Failed,
            Some(
                runtime_failure
                    .map(|failure| failure.error.to_string())
                    .unwrap_or_else(|| error.to_string()),
            ),
        ),
    };
    trace.emit(TraceLashlangExecutionEvent::ExecutionFinished {
        event_key: trace.event_key("finished"),
        identity: trace.identity().clone(),
        status,
        error,
    });
}

fn trace_main_map(artifact: &lashlang::ModuleArtifact) -> TraceLashlangMap {
    lash_lashlang_runtime::trace_lashlang_main_map(artifact)
}

fn apply_global_defaults(
    rlm: &mut FlowState,
    patch: &lash_rlm_types::RlmGlobalsPatchPluginBody,
    protected_names: &BTreeSet<String>,
) -> Result<(), String> {
    if patch.set_default.is_empty() {
        return Ok(());
    }
    let mut snapshot = rlm.snapshot();
    for (key, value) in &patch.set_default {
        if is_reserved_global_name(key) || protected_names.contains(key) {
            return Err(format!(
                "`{key}` is a read-only projected host binding; choose a different Lashlang variable name for `set_default`"
            ));
        }
        if snapshot.globals.get(key).is_none() {
            snapshot
                .globals
                .insert(key.clone(), json_to_flow_value(value.clone()));
        }
    }
    *rlm = FlowState::from_snapshot(snapshot);
    Ok(())
}

fn is_reserved_global_name(key: &str) -> bool {
    key == "history"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::{
        ProjectionRef, ProjectionRegistry, flow_record_to_json_value, flow_record_to_tool_args,
        flow_to_json_value, projected_index,
    };
    use lash_lashlang_runtime::ToolDefinitionLashlangExt;
    use lash_rlm_types::PROJECTED_JSON_TAG;
    use lashlang::{
        AbilityOp, AbilityResult, ExecutionEnvironment, ExecutionHost, ExecutionHostError,
        ExecutionOutcome, ProjectedBindings, ProjectedFuture, ProjectedHostDescriptor,
        ProjectedReadRequest, ProjectedReadResponse, ProjectedValue, Record as FlowRecord,
        Value as FlowValue,
    };
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        let (_, response) = execute_code(
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
        .expect("execute code");
        response
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

            let (state, first) = execute_code(
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

            let (state, second) = execute_code(
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
                .lock()
                .expect("resolver batches")
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
        ) -> Result<lash_core::PreparedToolCall, lash_core::ToolResult> {
            Ok(lash_core::PreparedToolCall::identity(
                call.tool_id,
                call.pending,
            ))
        }

        async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
            self.executions.fetch_add(1, Ordering::SeqCst);
            self.observed_bindings
                .lock()
                .expect("observed bindings")
                .push(call.context.tool_execution_binding().clone());
            lash_core::ToolResult::ok(serde_json::json!("deferred ok"))
        }

        async fn execute_granted(
            &self,
            grant: &lash_core::ToolExecutionGrant,
            args: &serde_json::Value,
            context: &lash_core::ToolContext<'_>,
            progress: Option<&lash_core::ProgressSender>,
        ) -> lash_core::ToolResult {
            self.execute_by_id(&grant.manifest.id, args, context, progress)
                .await
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
            let (mut state, first) = execute_code(
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

            let snapshot = state
                .snapshot_execution_state()
                .expect("snapshot")
                .expect("snapshot bytes");
            let mut restored = RlmExecutionState::new().expect("state");
            restored
                .restore_execution_state(&snapshot)
                .expect("restore");

            // Same stable link: both positive and negative outcomes survive the
            // snapshot and win without another authorization decision.
            let (restored, replay) = execute_code(
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
            let (restored, second_link) = execute_code(
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
            let (restored, next_turn) = execute_code(
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
                *batches.lock().expect("resolver batches"),
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

            let (state, response) = execute_code(
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
                *observed_bindings.lock().expect("observed bindings"),
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

            let (state, first) = execute_code(
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

            let (state, second) = execute_code(
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
            self.envelopes
                .lock()
                .expect("trigger effect envelopes")
                .push(envelope.clone());
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
                .lock()
                .expect("trigger effect envelopes")
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
        let (_, response) = execute_code(
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
            let (_, response) = execute_code(
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
                let envelopes = controller
                    .envelopes
                    .lock()
                    .expect("trigger effect envelopes");
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
                    .lock()
                    .expect("trigger effect envelopes")
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

            let first = capture(
                r#"
                process remember(tick: timer.Tick) { finish tick.fired_at }
                morning = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
                evening = timer.Schedule({ expr: "0 18 * * *", tz: "UTC" })
                await triggers.register({ source: morning, target: remember, inputs: { tick: trigger.event } })?
                await triggers.register({ source: evening, target: remember, inputs: { tick: trigger.event } })?
                finish true
                "#,
            )
            .await;
            let second = capture(
                r#"
                process remember(tick: timer.Tick) { finish tick.fired_at }
                morning = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
                evening = timer.Schedule({ expr: "0 18 * * *", tz: "UTC" })
                await triggers.register({ source: evening, target: remember, inputs: { tick: trigger.event } })?
                await triggers.register({ source: morning, target: remember, inputs: { tick: trigger.event } })?
                finish true
                "#,
            )
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

            let (next, first) = execute_code(
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

            let (_next, replacement) = execute_code(
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
        resources.add_module_operation(
            ["files"],
            "Files",
            "read",
            "read_file",
            lashlang::TypeExpr::Any,
            lashlang::TypeExpr::Any,
        );

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

            assert!(error.starts_with(RLM_BARE_TOOL_CALL_DIAGNOSTIC), "{error}");
            assert!(error.contains("hint: use `files.read`"), "{error}");
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
            assert!(state.rlm.snapshot().globals.get("history").is_none());
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
            state.rlm.snapshot().globals.get("diary"),
            Some(&FlowValue::List(
                vec![FlowValue::String("initial".into())].into()
            ))
        );
        assert!(state.rlm.snapshot().globals.get("current_query").is_none());

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
            state.rlm.snapshot().globals.get("diary"),
            Some(&FlowValue::List(
                vec![FlowValue::String("initial".into())].into()
            ))
        );
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
            assert!(state.rlm.snapshot().globals.get("current_query").is_none());

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
        let mut snapshot = state.rlm.snapshot();
        snapshot.globals.insert(
            "m".to_string(),
            FlowValue::Projected(ProjectedValue::custom(
                "search.matches[0].text",
                projected.clone(),
            )),
        );
        state.rlm = FlowState::from_snapshot(snapshot);

        let bytes = state
            .snapshot_execution_state()
            .expect("executor snapshot")
            .expect("snapshot bytes");

        assert_eq!(projected.render_count.load(Ordering::SeqCst), 0);
        assert_eq!(projected.materialize_count.load(Ordering::SeqCst), 0);
        let outer: Value = serde_json::from_slice(&bytes).expect("snapshot json");
        let vars = outer
            .get("vars")
            .and_then(Value::as_str)
            .expect("vars string");
        assert!(!vars.contains("rendered tool text"));
        assert!(!vars.contains("materialized tool text"));

        let restored = restore_runtime(vars).expect("restore runtime");
        assert!(matches!(
            restored.snapshot().globals.get("m"),
            Some(FlowValue::Projected(_))
        ));
    }

    #[test]
    fn bound_variables_prompt_renders_live_globals_after_execution() {
        block_on(async {
            let state = RlmExecutionState::new().expect("state");
            let ctx = lash_core::testing::code_execution_context();
            let (state, response) = execute_code(
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
            let rendered = crate::rlm_support::render_bound_variables(&mut cache, &globals);

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
            let (state, response) = execute_code(
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
            let _ = crate::rlm_support::render_bound_variables(&mut warm, &globals);
            let t2 = std::time::Instant::now();
            let mut s2 = 0usize;
            for _ in 0..n {
                s2 += crate::rlm_support::render_bound_variables(&mut warm, &globals).len();
            }
            let warm_us = t2.elapsed().as_nanos() as f64 / n as f64 / 1000.0;

            let t3 = std::time::Instant::now();
            let mut s3 = 0usize;
            for _ in 0..n {
                let mut cold = crate::rlm_support::BoundVariableRenderCache::default();
                s3 += crate::rlm_support::render_bound_variables(&mut cold, &globals).len();
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
            let (state, response) = execute_code(
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
            let s = crate::rlm_support::render_bound_variables(&mut cache, &globals).to_string();

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
        let mut snapshot = state.rlm.snapshot();
        snapshot.globals.insert(
            "doc".to_string(),
            FlowValue::Projected(ProjectedValue::custom_with_projection_ref(
                "doc",
                Arc::new(SnapshotProjectedToolText::default()),
                serde_json::json!(reference),
            )),
        );
        state.rlm = FlowState::from_snapshot(snapshot);

        let bytes = state
            .snapshot_execution_state()
            .expect("executor snapshot")
            .expect("snapshot bytes");
        let outer: Value = serde_json::from_slice(&bytes).expect("snapshot json");
        let vars = outer
            .get("vars")
            .and_then(Value::as_str)
            .expect("vars string");
        assert!(vars.contains("projection_ref"));
        assert!(vars.contains("\"kind\":\"memory\""));

        let restored = restore_runtime(vars).expect("restore runtime");
        let restored_snapshot = restored.snapshot();
        let Some(FlowValue::Projected(projected)) = restored_snapshot.globals.get("doc") else {
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
}
