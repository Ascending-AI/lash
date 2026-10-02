mod globals;
use globals::{apply_global_defaults, process_handle_names};
use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
mod definition_holds;
use definition_holds::{
    hold_continuation_definitions, hold_global_definitions, publish_cell_module,
};
mod cell_outputs;
use cell_outputs::record_cell_outputs;
#[cfg(test)]
use cell_outputs::retain_oversized_value;
mod cell_run;
mod cell_segment;
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
    ExecRequest, ExecResponse, RuntimeExecutionContext, facade_support::TraceRuntimeScope,
    facade_support::TraceRuntimeSubject,
};
// Cell execution itself is infallible, so the only fallible surface left in
// this module is the feature-gated performance fixture.
#[cfg(feature = "testing")]
use lash_core::SessionError;
use lash_lashlang_runtime::{
    LashlangSurface, TraceLanguageExecution, TraceLanguageExecutionGeneration,
    TraceLanguageExecutionIdentity, TraceLanguageExecutionMap, TraceLanguageExecutionPayload,
    TraceLanguageExecutionStatus,
};
use lashlang::ExecutionOutcome;

use self::host_bridge::{
    CollectedExecutionOutput, HostBridge, HostBridgeConfig, LashlangExecutionTrace,
};
use crate::projection::{
    RlmProjectedBindings, flow_to_json_value, json_to_flow_value, projected_bindings,
};

#[cfg(any(test, feature = "testing"))]
static EXECUTION_BOUND_EXHAUSTION_LOUD: AtomicBool = AtomicBool::new(true);

#[cfg(test)]
fn set_execution_bound_exhaustion_loud(loud: bool) -> bool {
    EXECUTION_BOUND_EXHAUSTION_LOUD.swap(loud, Ordering::SeqCst)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_code_with_channel_and_bounds_with_trigger_resolver(
    dialect: &dyn crate::dialect::Dialect,
    state: &mut RlmExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    request: ExecRequest,
    artifact_store: lashlang::LashlangArtifacts,
    lashlang_surface: LashlangSurface,
    deferred_tool_resolver: Option<lash_lashlang_runtime::SharedDeferredToolResolver>,
    deferred_trigger_resolver: Option<lash_lashlang_runtime::SharedDeferredTriggerResolver>,
    session_projected_bindings: RlmProjectedBindings,
    execution_bounds: lashlang::ExecutionBounds,
    channel: crate::plugin::RlmChannel,
    code_renderer: crate::render::CodeRendererSlot,
) -> ExecResponse {
    let clean_code = clean_model_code(&request.code);
    // A cell a segment boundary stopped inside is resumed by the next
    // execution of the same cell (FIG-4739): the Run's successor segment
    // restored the turn at the effect that ran it, and issues it again.
    let resumed = match state
        .suspended_cell()
        .map(|bytes| cell_segment::CellSegmentState::decode_for(bytes, &clean_code))
    {
        Some(Ok(resumed)) => resumed,
        Some(Err(error)) => {
            let mut response = exec_setup_failure(lash_core::CellFailure::new(
                lash_core::CellFailureKind::Host,
                format!("the suspended cell cannot be resumed: {error}"),
            ));
            fail_cell_on_nested_error(
                &ctx,
                &mut response,
                lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::ExecutionStateCaptureFailed,
                    format!("the suspended cell cannot be resumed: {error}"),
                ),
            );
            return response;
        }
        None => None,
    };
    // The cell's replay run (FIG-3586): every command it issues is keyed by
    // its issue ordinal under the cell's own replay key, and the cell seals
    // its run as its last nested effect once it has an answer.
    let cell = Arc::new(match &resumed {
        Some(resumed) => cell_run::CellRun::open_at(&ctx, resumed.ordinals.clone()),
        None => cell_run::CellRun::open(&ctx),
    });
    // Boxed: the seal runs under the same context after the cell, and an
    // unboxed context held across the cell would size every caller's future.
    let seal_ctx = Box::new(ctx.clone());
    let prints = Arc::new(std::sync::Mutex::new(
        resumed
            .as_ref()
            .map(|resumed| resumed.prints.iter().map(|print| print.0.clone()).collect())
            .unwrap_or_default(),
    ));
    let mut response = Box::pin(execute_code_inner(
        dialect,
        state,
        ctx,
        Arc::clone(&cell),
        &clean_code,
        artifact_store,
        lashlang_surface,
        deferred_tool_resolver,
        deferred_trigger_resolver,
        session_projected_bindings,
        execution_bounds,
        channel,
        Arc::clone(&prints),
        resumed.map(Box::new),
    ))
    .await;
    // A cell stopped at a segment boundary has no answer yet: it records no
    // outputs and seals nothing. The segment that ends it does both.
    if response.suspended {
        return response;
    }
    // Every module a global of this frame references is held by the frame
    // (I-frame, ADR 0113 §3.1): a value the cell bound or a tool returned
    // outlives the execution that published its module.
    if !seal_ctx.is_cancelled() {
        let _phase = seal_ctx.named_phase("rlm_lashlang.hold_frame_definitions");
        if let Err(err) = hold_global_definitions(state, &seal_ctx).await
            && response.error.is_none()
        {
            response.error = Some(lash_core::CellFailure::new(
                lash_core::CellFailureKind::Host,
                format!("failed to retain definition artifacts for this frame: {err}"),
            ));
        }
    }
    if let Ok(cell) = cell.as_ref()
        && !seal_ctx.is_cancelled()
        && !seal_ctx.has_nested_effect_error()
    {
        let values = prints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if !values.is_empty() || response.terminal_finish.is_some() {
            record_cell_outputs(&seal_ctx, cell, &code_renderer, values, &mut response).await;
        }
    }
    if let Ok(cell) = cell.as_ref()
        && !seal_ctx.is_cancelled()
    {
        Box::pin(cell.seal(
            &seal_ctx,
            &lash_lashlang_runtime::ExecutionCancellation::new(),
        ))
        .await;
    }
    response
}

/// Stops the cell on a nested effect's failure: the error is recorded for the
/// turn, and the cell answers it as a host failure.
fn fail_cell_on_nested_error(
    ctx: &RuntimeExecutionContext<'_>,
    response: &mut ExecResponse,
    error: lash_core::RuntimeEffectControllerError,
) {
    let message = error.to_string();
    ctx.record_nested_runtime_effect_error(error);
    response.error = Some(lash_core::CellFailure::new(
        lash_core::CellFailureKind::Host,
        message,
    ));
}

/// Feature-gated fixture that lets the repository's performance harness shift
/// the production RLM execution-state capture without exposing executor
/// internals as public protocol API.
#[cfg(feature = "testing")]
pub struct RlmCheckpointPerfFixture {
    dialect: Arc<dyn crate::dialect::Dialect>,
    state: RlmExecutionState,
    artifact_store: lashlang::LashlangArtifacts,
    binding_count: usize,
    payload_bytes: usize,
}

#[cfg(feature = "testing")]
impl RlmCheckpointPerfFixture {
    /// A fixture whose cells keep their Lashlang artifacts in `backend`.
    pub async fn new(
        dialect: Arc<dyn crate::dialect::Dialect>,
        backend: &lash_core::Backend,
        binding_count: usize,
        payload_bytes: usize,
    ) -> Result<Self, SessionError> {
        let mut state = RlmExecutionState::for_engine_with_workers(
            dialect.language_id(),
            dialect
                .worker_service()
                .with_recovery_store(backend.worker_recovery()),
        );
        // The snapshot's globals became a read-only projection when the heap
        // took ownership of them, so seed through the state's own insert.
        for index in 0..binding_count {
            state
                .vm
                .state_mut()
                .insert_global(
                    format!("mid_{index}"),
                    json_to_flow_value(serde_json::json!([format!(
                        "binding-{index}-{}",
                        "x".repeat(payload_bytes)
                    )])),
                )
                .await
                .map_err(|error| {
                    SessionError::Plugin(lash_core::PluginError::Runtime(
                        error.into_runtime_error(),
                    ))
                })?;
        }
        Ok(Self {
            dialect,
            state,
            artifact_store: lashlang::LashlangArtifacts::of_backend(backend),
            binding_count,
            payload_bytes,
        })
    }

    pub async fn capture(
        &mut self,
    ) -> Result<lash_core::plugin::ExecutionStateCapture, SessionError> {
        self.state
            .snapshot_execution_state(lash_core::FleetFormat::current())
            .await
    }

    pub fn acknowledge_capture(&mut self) {
        self.state.acknowledge_execution_state_capture();
    }

    /// Run the edit under the backend's scoped controller and this cell's
    /// invocation, including the production projected-bindings journal.
    pub async fn assign_one(
        &mut self,
        index: usize,
        turn: usize,
        ctx: RuntimeExecutionContext<'_>,
    ) -> Result<(), SessionError> {
        let binding = index % self.binding_count.max(1);
        // A seeded global is an ambient `const` to a TypeScript cell, so the
        // per-turn edit is a re-declaration carrying an equivalent payload
        // rather than an append. What the fixture measures is unchanged: one
        // binding is dirtied per turn, at the same order of bytes.
        let code = format!(
            "let mid_{binding} = [\"binding-{binding}-{}\", \"turn-{turn}-{}\"];",
            "x".repeat(self.payload_bytes),
            "y".repeat(self.payload_bytes / 8)
        );
        let response = execute_code_with_channel_and_bounds_with_trigger_resolver(
            self.dialect.as_ref(),
            &mut self.state,
            ctx,
            ExecRequest { code },
            self.artifact_store.clone(),
            LashlangSurface::default(),
            None,
            None,
            RlmProjectedBindings::default(),
            lashlang::ExecutionBounds::unbounded(),
            crate::plugin::RlmChannel::Cell,
            crate::render::CodeRendererSlot::default(),
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

    pub async fn restore(
        dialect: &dyn crate::dialect::Dialect,
        state: &lash_core::plugin::HydratedExecutionState,
    ) -> Result<(), SessionError> {
        let mut restored = RlmExecutionState::for_engine_with_workers(
            dialect.language_id(),
            dialect.worker_service(),
        );
        restored
            .restore_execution_state(state, lash_core::FleetFormat::current())
            .await
            .map_err(SessionError::from)
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
    dialect: &dyn crate::dialect::Dialect,
    state: &mut RlmExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    cell: Arc<Result<cell_run::CellRun, cell_run::LashlangCellOpener>>,
    code: &str,
    artifact_store: lashlang::LashlangArtifacts,
    lashlang_surface: LashlangSurface,
    deferred_tool_resolver: Option<lash_lashlang_runtime::SharedDeferredToolResolver>,
    deferred_trigger_resolver: Option<lash_lashlang_runtime::SharedDeferredTriggerResolver>,
    session_projected_bindings: RlmProjectedBindings,
    execution_bounds: lashlang::ExecutionBounds,
    channel: crate::plugin::RlmChannel,
    prints: Arc<std::sync::Mutex<Vec<lashlang::Value>>>,
    resumed: Option<Box<cell_segment::CellSegmentState>>,
) -> ExecResponse {
    let identities = match cell.as_ref() {
        Ok(cell) => cell.identities().code().clone(),
        Err(_) => match lash_core::EffectOpener::for_scope(&ctx.admitted_scope()) {
            Ok(opener) => lash_vm_broker::CodeCallIdentities::cell(opener, "pure-cell"),
            Err(error) => {
                return exec_setup_failure_or_stop(
                    state,
                    &ctx,
                    lash_core::CellFailureKind::Host,
                    error.to_string(),
                );
            }
        },
    };
    if let Err(error) = hold_global_definitions(state, &ctx).await {
        return exec_setup_failure_or_stop(state, &ctx, lash_core::CellFailureKind::Host, error);
    }
    let recovery = match state
        .vm
        .state()
        .service()
        .begin_execution(&identities.scope())
        .await
    {
        Ok(recovery) => recovery,
        Err(error) => return worker_setup_failure(state, &ctx, error),
    };
    let previous_service = state
        .vm
        .state_mut()
        .replace_service(recovery.service().clone());
    let response = Box::pin(execute_code_in_worker_scope(
        dialect,
        state,
        ctx.clone(),
        cell,
        code,
        artifact_store,
        lashlang_surface,
        deferred_tool_resolver,
        deferred_trigger_resolver,
        session_projected_bindings,
        execution_bounds,
        channel,
        prints,
        recovery.service().clone(),
        resumed,
    ))
    .await;
    state.vm.state_mut().replace_service(previous_service);
    match recovery.settle().await {
        Ok(()) => response,
        Err(error) => worker_setup_failure(state, &ctx, error),
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_code_in_worker_scope(
    dialect: &dyn crate::dialect::Dialect,
    state: &mut RlmExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    cell: Arc<Result<cell_run::CellRun, cell_run::LashlangCellOpener>>,
    code: &str,
    artifact_store: lashlang::LashlangArtifacts,
    lashlang_surface: LashlangSurface,
    deferred_tool_resolver: Option<lash_lashlang_runtime::SharedDeferredToolResolver>,
    deferred_trigger_resolver: Option<lash_lashlang_runtime::SharedDeferredTriggerResolver>,
    session_projected_bindings: RlmProjectedBindings,
    execution_bounds: lashlang::ExecutionBounds,
    channel: crate::plugin::RlmChannel,
    prints: Arc<std::sync::Mutex<Vec<lashlang::Value>>>,
    workers: lash_vm_client::service::Service,
    resumed: Option<Box<cell_segment::CellSegmentState>>,
) -> ExecResponse {
    state.mark_execution_started();
    let execution_checkpoint = state.execution_checkpoint();
    state.begin_code_execution(execution_checkpoint);
    // The suspended cell, if the session holds one, runs again from here or
    // is abandoned by the cell that runs in its place. A discarded execution
    // restores it with the rest of the checkpoint.
    state.take_suspended_cell();
    select_deferred_resolution_link(state, &ctx);
    if let Some(resumed) = &resumed {
        resumed.restore_context(&ctx);
    }
    // A resumed cell links against what its first segment recorded: those
    // records live in that segment's journal, so their contents came with
    // the cell and nothing here journals them again (FIG-4739).
    let (session_projected_bindings, cell_bindings, host_environment) = if let Some(resumed) =
        &resumed
    {
        let linked = crate::projection::RlmProjectedBindings::from_recorded(
            resumed.projected_bindings.clone(),
        )
        .map_err(|error| error.to_string())
        .and_then(|projected| {
            lash_lashlang_runtime::CellToolBindings::from_record(
                resumed.cell_bindings.clone(),
                ctx.live_tool_catalog().as_ref(),
            )
            .map(|bindings| (projected, bindings))
            .map_err(|error| error.message)
        });
        match linked {
            Ok((projected, bindings)) => (projected, bindings, resumed.host_environment.clone()),
            Err(error) => {
                return exec_setup_failure_or_stop(
                    state,
                    &ctx,
                    lash_core::CellFailureKind::Host,
                    format!("the suspended cell cannot be resumed: {error}"),
                );
            }
        }
    } else {
        // A frame handoff changes the session's live projections. Re-execution
        // links against this cell's recorded inputs, under the exec_code address
        // its parent installed, before it reaches its recorded command keys.
        let session_projected_bindings = match ctx
            .parent_invocation()
            .and_then(lash_core::RuntimeInvocation::effect_address)
        {
            Some(address) => match session_projected_bindings
                .journaled(&ctx, &address.replay_key)
                .await
            {
                Ok(bindings) => bindings,
                Err(error) => {
                    let message = error.message.clone();
                    ctx.record_nested_runtime_effect_error(cell_run::setup_effect_error(
                        &cell, error,
                    ));
                    return exec_setup_failure_or_stop(
                        state,
                        &ctx,
                        lash_core::CellFailureKind::Host,
                        message,
                    );
                }
            },
            None => session_projected_bindings,
        };
        let (parsed, referenced) = match workers
            .request_accounted(lash_vm_client::service::Request::References {
                source: code.to_string(),
            })
            .await
        {
            Ok(lash_vm_client::service::Response::References(referenced)) => (true, referenced),
            Ok(lash_vm_client::service::Response::CompileRefused { .. }) => {
                (false, BTreeSet::new())
            }
            Ok(other) => {
                return exec_setup_failure_or_stop(
                    state,
                    &ctx,
                    lash_core::CellFailureKind::Host,
                    format!("unexpected source analysis response: {other:?}"),
                );
            }
            Err(error) => return worker_setup_failure(state, &ctx, error),
        };

        if let Err(error) = workers.checkpoint().await {
            return worker_setup_failure(state, &ctx, error);
        }

        // gather → journal → mask → fold: every parsed resource-bearing cell first
        // consults the deferred journal, even if no live resolver and no checkpoint
        // projection are available. This is what closes the postcommit / before-
        // projection crash window. Journal outcomes then mask exact paths while the
        // ambient Tool Catalog is built, before its collision validation can
        // preempt recorded authority. Unrelated catalog errors remain ordinary host
        // failures.
        let mut effective_surface = lashlang_surface;
        if !referenced.is_empty() && state.deferred_trigger_resolutions.link_key.is_some() {
            let _phase = ctx.named_phase("rlm_lashlang.deferred_trigger_resolve");
            match lash_lashlang_runtime::resolve_and_fold_deferred_triggers(
                &referenced,
                effective_surface,
                deferred_trigger_resolver.as_ref(),
                &state.deferred_trigger_resolutions,
                &ctx,
            )
            .await
            {
                Ok((surface, record)) => {
                    effective_surface = surface;
                    state.deferred_trigger_resolutions = record;
                }
                Err(error) => {
                    ctx.record_nested_runtime_effect_error(cell_run::setup_effect_error(
                        &cell,
                        error.runtime_effect_error(),
                    ));
                    return exec_setup_failure_or_stop(
                        state,
                        &ctx,
                        lash_core::CellFailureKind::Host,
                        error.to_string(),
                    );
                }
            }
        }

        // The cell's ambient binding set is journaled before its first effect
        // and a redrive links against the recorded set, not the live registry
        // (FIG-3587): a tool removed or changed since keeps its recorded binding,
        // and calls on it are served only from their recorded results.
        let cell_bindings = match state
            .deferred_link
            .as_ref()
            .filter(|_| !referenced.is_empty())
        {
            Some(link) => {
                let _phase = ctx.named_phase("rlm_lashlang.cell_tool_bindings");
                let excluded = link.outcomes.keys().cloned().collect::<BTreeSet<_>>();
                match lash_lashlang_runtime::journal_cell_tool_bindings(
                    &referenced,
                    ctx.tool_catalog().as_ref(),
                    &excluded,
                    &link.key.address.replay_key,
                    &ctx,
                )
                .await
                {
                    Ok(bindings) => bindings,
                    Err(error) => {
                        let message = error.message.clone();
                        ctx.record_nested_runtime_effect_error(cell_run::setup_effect_error(
                            &cell, error,
                        ));
                        return exec_setup_failure_or_stop(
                            state,
                            &ctx,
                            lash_core::CellFailureKind::Host,
                            message,
                        );
                    }
                }
            }
            None => lash_lashlang_runtime::CellToolBindings::default(),
        };
        let live_catalog = ctx.tool_catalog();
        let link_catalog = cell_bindings.link_catalog(&live_catalog);

        let mut host_environment =
            if let Some(link) = state.deferred_link.as_mut().filter(|_| parsed) {
                let _phase = ctx.named_phase("rlm_lashlang.deferred_resolve");
                match lash_lashlang_runtime::resolve_and_build_deferred_environment_from_references(
                    &referenced,
                    &effective_surface,
                    &link_catalog,
                    deferred_tool_resolver.as_ref(),
                    link,
                    &ctx,
                )
                .await
                {
                    Ok(environment) => environment,
                    Err(error) => {
                        ctx.record_nested_runtime_effect_error(cell_run::setup_effect_error(
                            &cell,
                            error.runtime_effect_error(),
                        ));
                        return exec_setup_failure_or_stop(
                            state,
                            &ctx,
                            lash_core::CellFailureKind::Host,
                            error.to_string(),
                        );
                    }
                }
            } else {
                match effective_surface.host_environment(&link_catalog) {
                    Ok(environment) => environment,
                    Err(error) => {
                        emit_step_trace(
                            &ctx,
                            Err(&format!("invalid Lashlang host tool surface: {error}")),
                        );
                        return exec_setup_failure_or_stop(
                            state,
                            &ctx,
                            lash_core::CellFailureKind::Host,
                            format!("invalid Lashlang host tool surface: {error}"),
                        );
                    }
                }
            };

        // Every binding the session holds, read from the roots that own them: a
        // `Map` or a `Date` has no host view, but a later cell names it all the
        // same (ADR 0076: no existence decision reads the view).
        let mut live_global_names = state
            .vm
            .state()
            .binding_names()
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        live_global_names.insert("history".to_string());
        live_global_names.extend(session_projected_bindings.names());
        host_environment = host_environment.with_globals(live_global_names);
        host_environment =
            host_environment.with_process_handles(process_handle_names(state.vm.state().globals()));
        // A function an earlier cell bound did not survive its cell, and a
        // reference to it is refused by name rather than as a name never bound.
        host_environment = host_environment
            .with_expired_functions(state.vm.state().expired_functions().iter().cloned());
        (session_projected_bindings, cell_bindings, host_environment)
    };
    if resumed.is_some()
        && let Err(error) = workers.checkpoint().await
    {
        return worker_setup_failure(state, &ctx, error);
    }
    // What the cell linked against, as a segment boundary inside it hands it
    // over.
    let linked = (
        session_projected_bindings.recorded(),
        cell_bindings.record(),
        host_environment.clone(),
    );

    // The kind is decided here, while the failure is still a typed diagnostic.
    // "Compilation failed" is not enough to classify it: a misspelled name and a
    // forbidden construct both fail here and need opposite advice.
    let compile_result = match workers
        .request_accounted(lash_vm_client::service::Request::CompileModule {
            source: code.to_string(),
            environment: host_environment.clone(),
            cell: true,
        })
        .await
    {
        Ok(lash_vm_client::service::Response::Module(module)) => Ok(*module),
        Ok(lash_vm_client::service::Response::CompileRefused { error, policy }) => {
            let message = match error {
                lashlang::ModuleCompileError::Parse(_) => format_rlm_parse_diagnostic(
                    dialect.render_parse_diagnostic(&error),
                    channel,
                    dialect.prompt_vocabulary().cell_tags,
                ),
                _ => error.to_string(),
            };
            Err((
                if policy {
                    lash_core::CellFailureKind::Policy
                } else {
                    lash_core::CellFailureKind::Program
                },
                message,
            ))
        }
        Ok(_) => {
            return worker_setup_failure(
                state,
                &ctx,
                lash_vm_client::PoolError::breach(
                    lash_vm_protocol::SequenceFault::UnexpectedServiceResponse,
                ),
            );
        }
        Err(error) => {
            emit_step_trace(&ctx, Err(&error.to_string()));
            return worker_setup_failure(state, &ctx, error);
        }
    };
    emit_step_trace(
        &ctx,
        compile_result
            .as_ref()
            .map(|_| ())
            .map_err(|(_, diagnostic)| diagnostic.as_str()),
    );
    let linked_module = match compile_result {
        Ok(program) => program,
        Err((kind, error)) => {
            return exec_setup_failure_or_stop(state, &ctx, kind, error);
        }
    };
    if let Ok(cell) = cell.as_ref() {
        cell.ran_module(linked_module.artifact.module_ref().to_string());
    }
    // A resumed cell's module was published, and held in the frame, by the
    // segment that first ran the cell; the continuation holds what it names
    // through its definitions. Publishing again would journal a step its
    // first execution may have skipped on a frame hold only that worker
    // knew of, and a replay rebuilt from the committed state would not.
    if resumed.is_none() && !linked_module.artifact.exports().processes.is_empty() {
        let stored = {
            let _phase = ctx.named_phase("rlm_lashlang.store_module_artifact");
            publish_cell_module(state, &ctx, &artifact_store, &linked_module.artifact).await
        };
        if let Err(err) = stored {
            return exec_setup_failure_or_stop(
                state,
                &ctx,
                lash_core::CellFailureKind::Host,
                format!("failed to store lashlang module artifact: {err}"),
            );
        }
    }

    let projected = {
        let _phase = ctx.named_phase("rlm_lashlang.resolve_projected_bindings");
        match projected_bindings(&ctx, session_projected_bindings) {
            Ok(projected) => projected,
            Err(err) => {
                return exec_setup_failure_or_stop(
                    state,
                    &ctx,
                    lash_core::CellFailureKind::Host,
                    err,
                );
            }
        }
    };
    let projected_names = projected.names().collect::<Vec<_>>();
    if let Err(error) = state
        .vm
        .state_mut()
        .remove_names(projected_names.iter().cloned().collect())
        .await
    {
        return worker_setup_failure(state, &ctx, error);
    }
    let deferred_execution_grants = match &resumed {
        Some(resumed) => resumed.deferred_execution_grants.clone(),
        None => state
            .deferred_link
            .as_ref()
            .map(deferred_execution_grants)
            .unwrap_or_default(),
    };
    let lashlang_execution_trace =
        foreground_lashlang_execution_trace(&ctx, &linked_module.artifact, dialect.language_id());
    if let Some(trace) = &lashlang_execution_trace {
        emit_foreground_execution_started(trace, &linked_module.artifact);
    }
    let host = HostBridge::new(HostBridgeConfig {
        ctx: ctx.clone(),
        cell: Arc::clone(&cell),
        prints: Arc::clone(&prints),
        lashlang_execution_trace: lashlang_execution_trace.clone(),
        host_environment,
        deferred_execution_grants: deferred_execution_grants.clone(),
        cell_bindings,
        artifact_store: artifact_store.clone(),
        workers: workers.clone(),
        ledgers: resumed
            .as_ref()
            .map(|resumed| resumed.host.clone())
            .unwrap_or_default(),
    });
    let scope = match ctx.session_scope() {
        Ok(scope) => scope,
        Err(error) => {
            return exec_setup_failure_or_stop(
                state,
                &ctx,
                lash_core::CellFailureKind::Host,
                error.to_string(),
            );
        }
    };
    let owner = lash_vm_protocol::VmOwner::new(format!(
        "rlm:{}:{:?}",
        scope.session_id, scope.agent_frame_id
    ));
    let (start_state, projection_namespace) = match resumed {
        Some(resumed) => {
            if let Err(error) = hold_continuation_definitions(&ctx, &resumed.vm).await {
                return exec_setup_failure_or_stop(
                    state,
                    &ctx,
                    lash_core::CellFailureKind::Host,
                    error,
                );
            }
            (
                lash_vm_protocol::StartState::Continuation(resumed.vm),
                resumed.projection_namespace,
            )
        }
        None => (
            state
                .vm
                .state()
                .bytes()
                .map(|bytes| {
                    lash_vm_protocol::StartState::Snapshot(lash_vm_protocol::OpaqueVmState::seal(
                        lash_vm_protocol::VmStateKind::Snapshot,
                        owner.clone(),
                        lashlang::vm_contract_versions(),
                        bytes.to_vec(),
                    ))
                })
                .unwrap_or(lash_vm_protocol::StartState::Fresh),
            cell.as_ref()
                .as_ref()
                .ok()
                .map(|cell| cell_segment::projection_namespace(&cell.identities().namespace())),
        ),
    };
    let identities = match cell.as_ref() {
        Ok(cell) => cell.identities().code().clone(),
        Err(_) => match lash_core::EffectOpener::for_scope(&ctx.admitted_scope()) {
            Ok(opener) => lash_vm_broker::CodeCallIdentities::cell(opener, "pure-cell"),
            Err(error) => {
                return exec_setup_failure_or_stop(
                    state,
                    &ctx,
                    lash_core::CellFailureKind::Host,
                    error.to_string(),
                );
            }
        },
    };
    let run = lash_lashlang_runtime::WorkerRun {
        service: &workers,
        host: &host,
        identities,
        owner,
        frame_epoch: lash_vm_protocol::FrameEpoch(0),
        program: lash_vm_protocol::ProgramSource::Source {
            dialect: dialect.language_id().into(),
            text: code.to_string(),
        },
        context: lash_vm_client::RunContext {
            environment: host.host_environment_description(),
            mode: lashlang::ExecutionMode::Foreground,
            capture_state_view: true,
            projected: Vec::new(),
            observe_execution: lashlang_execution_trace.is_some(),
            ..Default::default()
        },
        projected,
        bounds: execution_bounds,
        state: start_state,
        boundary: &|| false,
        hand_over: Some(host.hand_over_gate()),
        projection_namespace,
    }
    .run()
    .await;
    let (result, runtime_failure) = match run {
        Ok(lash_vm_broker::BrokeredEnd::Complete { value, checkpoint }) => {
            let outcome = match state
                .vm
                .state_mut()
                .install_completion(&checkpoint.vm, &value)
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    let mut response = exec_setup_failure(lash_core::CellFailure::new(
                        lash_core::CellFailureKind::Host,
                        format!("invalid worker completion: {error}"),
                    ));
                    fail_cell_on_nested_error(
                        &ctx,
                        &mut response,
                        lash_core::RuntimeEffectControllerError::new(
                            lash_core::RuntimeErrorCode::ExecutionStateCaptureFailed,
                            format!("invalid worker completion: {error}"),
                        ),
                    );
                    return response;
                }
            };
            (Ok(outcome), None)
        }
        Ok(lash_vm_broker::BrokeredEnd::GuestError { error, checkpoint }) => {
            if checkpoint.is_some()
                && let Err(error) = workers.mark_running().await
            {
                return worker_setup_failure(state, &ctx, error);
            }
            if let Some(checkpoint) = checkpoint
                && let Err(error) = state
                    .vm
                    .state_mut()
                    .install_bytes(checkpoint.vm.bytes().to_vec())
                    .await
            {
                return worker_setup_failure(state, &ctx, error);
            }
            let failure: lashlang::RuntimeFailure = match rmp_serde::from_slice(&error.0) {
                Ok(failure) => failure,
                Err(error) => {
                    ctx.record_nested_effect_error(
                        lash_core::RuntimeEffectControllerError::retryable_response_derivation(
                            error.to_string(),
                        ),
                    );
                    return exec_response_from(
                        host.into_collected(),
                        Some(lash_core::CellFailure::new(
                            lash_core::CellFailureKind::Host,
                            error.to_string(),
                        )),
                        None,
                    );
                }
            };
            (Err(failure.error.clone()), Some(failure))
        }
        Ok(lash_vm_broker::BrokeredEnd::Cancelled) => {
            (Err(lashlang::RuntimeError::HostCancelled), None)
        }
        // The cell stopped on a wait its Run's successor segment takes over
        // (FIG-4739): everything the cell holds is handed to that segment
        // with the session's execution state, and the cell has no answer yet.
        Ok(lash_vm_broker::BrokeredEnd::Suspended { checkpoint }) => {
            let captured = suspend_cell(
                state,
                &ctx,
                &cell,
                &host,
                code,
                checkpoint.vm,
                linked,
                deferred_execution_grants,
                &prints,
            )
            .await;
            return match captured {
                Ok(()) => ExecResponse {
                    suspended: true,
                    ..exec_response_from(host.into_collected(), None, None)
                },
                Err(error) => {
                    let error = format!("the cell's segment state was not captured: {error}");
                    ctx.record_nested_effect_error(lash_core::RuntimeEffectControllerError::new(
                        lash_core::RuntimeErrorCode::ExecutionStateCaptureFailed,
                        error.clone(),
                    ));
                    exec_response_from(
                        host.into_collected(),
                        Some(lash_core::CellFailure::new(
                            lash_core::CellFailureKind::Host,
                            error,
                        )),
                        None,
                    )
                }
            };
        }
        Err(error) => {
            // Only a limit the run itself exhausted is the cell's result. A
            // deadline, or the host's cumulative CPU or attempt accounting,
            // is this host's live verdict: it is retryable, so the attempt
            // fails below and the cell seals nothing (FIG-4451).
            let run_limit = match &error {
                lash_vm_broker::BrokerFailure::WorkerLost {
                    outcome: lash_vm_protocol::InfrastructureOutcome::WorkerLimitExceeded { limit },
                    ..
                }
                | lash_vm_broker::BrokerFailure::Unavailable {
                    refusal:
                        lash_vm_broker::CheckoutRefusal::Infrastructure(
                            lash_vm_protocol::InfrastructureOutcome::WorkerLimitExceeded { limit },
                        ),
                } if !limit.is_host_verdict() => Some(*limit),
                _ => None,
            };
            if let Some(limit) = run_limit {
                let message = match limit {
                    lash_vm_protocol::WorkerLimit::Fuel => "instruction budget exceeded".to_owned(),
                    lash_vm_protocol::WorkerLimit::Heap => {
                        "logical memory limit exceeded".to_owned()
                    }
                    lash_vm_protocol::WorkerLimit::Depth => "frame depth limit exceeded".to_owned(),
                    lash_vm_protocol::WorkerLimit::Observations => {
                        "execution observations exceeded their bound".to_owned()
                    }
                    limit => limit.to_string(),
                };
                #[cfg(any(test, feature = "testing"))]
                assert!(
                    !EXECUTION_BOUND_EXHAUSTION_LOUD.load(Ordering::SeqCst),
                    "confidence execution exhausted a required Lashlang bound: {message}"
                );
                return exec_response_from(
                    host.into_collected(),
                    Some(
                        lash_core::CellFailure::new(lash_core::CellFailureKind::Program, message)
                            .with_worker_limit(limit),
                    ),
                    None,
                );
            }
            let deployment = match &error {
                lash_vm_broker::BrokerFailure::WorkerLost { outcome, .. }
                | lash_vm_broker::BrokerFailure::Unavailable {
                    refusal: lash_vm_broker::CheckoutRefusal::Infrastructure(outcome),
                } if outcome.deployment_fault().is_some() => Some(outcome.clone()),
                _ => None,
            };
            if let Some(outcome) = deployment {
                fail_attempt_on_host_verdict(&ctx, &outcome.into());
            } else if error.is_retryable() {
                ctx.record_nested_effect_error(
                    lash_core::RuntimeEffectControllerError::retryable_response_derivation(
                        error.to_string(),
                    ),
                );
            }
            return exec_response_from(
                host.into_collected(),
                Some(lash_core::CellFailure::new(
                    lash_core::CellFailureKind::Host,
                    error.to_string(),
                )),
                None,
            );
        }
    };
    if let Some(trace) = &lashlang_execution_trace {
        emit_foreground_execution_finished(trace, &result, runtime_failure.as_ref());
    }
    let terminal_finish = match result {
        Ok(ExecutionOutcome::Finished(value)) => Some(flow_to_json_value(&value)),
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
            // A `max_tool_calls` refusal is reported in its own words, at the
            // call that met it: the model reads the limit, not the channel the
            // refusal travelled on.
            let tool_call_limit = match &failure.error {
                lashlang::RuntimeError::AggregateHostControl { source }
                    if lash_lashlang_runtime::is_tool_call_limit_failure(&failure.error) =>
                {
                    Some(source.message().to_owned())
                }
                _ => None,
            };
            let message = match &tool_call_limit {
                Some(refusal) => {
                    lashlang::format_source_diagnostic(code, failure.span, refusal, &[])
                }
                None => crate::feedback::render_runtime_failure(code, &failure),
            };
            let mut cell_failure = lash_core::CellFailure::new(kind, message);
            if tool_call_limit.is_some() {
                cell_failure.tool_call_limit = ctx.tool_call_limit_refusal();
            }
            return exec_response_from(host.into_collected(), Some(cell_failure), None);
        }
    };
    exec_response_from(host.into_collected(), None, terminal_finish)
}

/// Records the cell as stopped at a segment boundary: its continuation and
/// every parent ledger of it, as the session's suspended cell.
#[allow(clippy::too_many_arguments)]
async fn suspend_cell(
    state: &mut RlmExecutionState,
    ctx: &RuntimeExecutionContext<'_>,
    cell: &Result<cell_run::CellRun, cell_run::LashlangCellOpener>,
    host: &HostBridge<'_>,
    code: &str,
    vm: lash_vm_protocol::OpaqueVmState,
    linked: (
        Result<serde_json::Value, serde_json::Error>,
        Result<serde_json::Value, serde_json::Error>,
        lashlang::LashlangHostEnvironment,
    ),
    deferred_execution_grants: BTreeMap<lash_core::ToolId, lash_core::ToolExecutionGrant>,
    prints: &std::sync::Mutex<Vec<lashlang::Value>>,
) -> Result<(), String> {
    let cell = cell.as_ref().map_err(|error| error.to_string())?;
    let (projected_bindings, cell_bindings, host_environment) = linked;
    hold_continuation_definitions(ctx, &vm).await?;
    let ctx = host.ctx();
    let segment = cell_segment::CellSegmentState {
        version: cell_segment::CELL_SEGMENT_STATE_VERSION,
        code: cell_segment::CellSegmentState::code_digest(code),
        vm,
        ordinals: cell.ordinals(),
        projection_namespace: host.hand_over_gate().projection_namespace(),
        projected_bindings: projected_bindings.map_err(|error| error.to_string())?,
        host_environment,
        cell_bindings: cell_bindings.map_err(|error| error.to_string())?,
        deferred_execution_grants,
        prints: prints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .map(cell_segment::RecordedPrint)
            .collect(),
        host: host.ledgers(),
        cell_tool_calls: ctx.cell_tool_calls_snapshot(),
        started_process_ids: ctx.started_process_ids(),
        incorporation_ledger: ctx.incorporation_ledger_snapshot(),
        outstanding_groups: ctx.outstanding_groups_snapshot(),
        held_tool_calls: ctx.held_tool_calls_snapshot(),
    };
    state.suspend_cell(segment.encode()?);
    Ok(())
}

/// The environment of the frame this execution was admitted on: the
/// referrer that holds every module the frame's globals reference.
fn frame_environment(ctx: &RuntimeExecutionContext<'_>) -> Option<lash_core::FrameEnvironmentId> {
    let scope = ctx.session_scope().ok()?;
    Some(lash_core::FrameEnvironmentId::new(
        scope.session_id,
        scope.agent_frame_id?,
    ))
}

/// Why a frame could not hold a module.
enum FrameHoldError {
    /// The frame has ended: only a replay of the turn that switched away
    /// from it runs here, and the frame's globals are gone, so no reader
    /// needs the edge (ADR 0113 §4.1).
    Ended,
    Store(lash_core::ArtifactStoreError),
}

/// Add the frame's edge to a stored module.
async fn acquire_frame_edge(
    frame: &lash_core::FrameEnvironmentId,
    artifact_store: &lashlang::LashlangArtifacts,
    module_ref: &lashlang::ModuleRef,
) -> Result<(), FrameHoldError> {
    let claim = lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::FrameEnvironment(
        frame.clone(),
    ))
    .map_err(|error| {
        FrameHoldError::Store(lash_core::ArtifactStoreError::Backend(error.to_string()))
    })?;
    match artifact_store
        .acquire_module_artifact(&claim, module_ref)
        .await
    {
        Ok(()) => Ok(()),
        Err(lash_core::ArtifactStoreError::ReferrerEnded { .. }) => Err(FrameHoldError::Ended),
        Err(error) => Err(FrameHoldError::Store(error)),
    }
}

/// Classifies a typed Lashlang runtime outcome before it enters the response.
fn lashlang_runtime_feedback_kind(
    error: &lashlang::RuntimeError,
    host_cancelled: bool,
) -> lash_core::CellFailureKind {
    match error {
        _ if host_cancelled => lash_core::CellFailureKind::Host,
        lashlang::RuntimeError::HostCancelled => lash_core::CellFailureKind::Host,
        // The session's `max_tool_calls` refused the cell's tool calls
        // (FIG-4546): the program asked for more than its recorded limit, and
        // every replay refuses the same call.
        error if lash_lashlang_runtime::is_tool_call_limit_failure(error) => {
            lash_core::CellFailureKind::Program
        }
        // An aggregate with no members is the program's defect: the host ends
        // the cell uncatchably (ADR 0099 §11 clause 5), but a retry of the
        // identical cell fails the same way (FIG-4547).
        lashlang::RuntimeError::AggregateAwaitUnsettled { .. } => {
            lash_core::CellFailureKind::Program
        }
        // An aggregate's infrastructure failure or host stop travels on the
        // host-control channel (ADR 0099 §10 L3): the host's, not the program's.
        lashlang::RuntimeError::AggregateHostControl { .. } => lash_core::CellFailureKind::Host,
        error if error.is_execution_bound_exhausted() => lash_core::CellFailureKind::Policy,
        _ => lash_core::CellFailureKind::Program,
    }
}

fn exec_setup_failure(error: lash_core::CellFailure) -> ExecResponse {
    ExecResponse {
        observations: Vec::new(),
        calls: Vec::new(),
        printed_images: Vec::new(),
        error: Some(error),
        degraded_bindings: Vec::new(),
        terminal_finish: None,
        terminal_finish_retained: None,
        suspended: false,
    }
}

fn exec_setup_failure_or_stop(
    state: &mut RlmExecutionState,
    ctx: &RuntimeExecutionContext<'_>,
    kind: lash_core::CellFailureKind,
    error: impl Into<String>,
) -> ExecResponse {
    if ctx.is_cancelled() {
        state.cancel_code_execution();
        return exec_setup_failure(lash_core::CellFailure::new(
            lash_core::CellFailureKind::Host,
            "foreground execution stopped during setup",
        ));
    }
    exec_setup_failure(lash_core::CellFailure::new(kind, error))
}

/// A worker service fault in a cell. A host verdict — its retryable worker
/// failure, worker budget, pool capacity or recovery store
/// ([`lash_vm_client::PoolError::is_host_verdict`]) — is read live, outside
/// any recorded step, and a replay or another host with capacity answers it
/// differently: it fails the attempt retryably, so the cell seals nothing
/// and the model never sees it (FIG-4451, FIG-4459). Any other fault is the cell's
/// host failure.
fn worker_setup_failure(
    state: &mut RlmExecutionState,
    ctx: &RuntimeExecutionContext<'_>,
    error: lash_vm_client::PoolError,
) -> ExecResponse {
    if let lash_vm_client::PoolError::Infrastructure(
        lash_vm_protocol::InfrastructureOutcome::WorkerLimitExceeded { limit },
    ) = &error
        && !limit.is_host_verdict()
    {
        let mut response = exec_setup_failure_or_stop(
            state,
            ctx,
            lash_core::CellFailureKind::Program,
            limit.to_string(),
        );
        if let Some(failure) = &mut response.error {
            failure.worker_limit = Some(*limit);
        }
        return response;
    }
    if let lash_vm_client::PoolError::Infrastructure(
        lash_vm_protocol::InfrastructureOutcome::RunRefused {
            refusal: lash_vm_protocol::RunRefusal::UnusableSchema { source },
        },
    ) = &error
    {
        if ctx.is_cancelled() {
            state.cancel_code_execution();
        }
        return exec_setup_failure(
            lash_core::CellFailure::new(lash_core::CellFailureKind::Host, source.to_string())
                .with_schema_admission(source.as_ref().clone()),
        );
    }
    fail_attempt_on_host_verdict(ctx, &error);
    exec_setup_failure_or_stop(
        state,
        ctx,
        lash_core::CellFailureKind::Host,
        error.to_string(),
    )
}

/// Record `error` as the attempt's retryable nested error when it is a host
/// verdict (see [`worker_setup_failure`]).
fn fail_attempt_on_host_verdict(
    ctx: &RuntimeExecutionContext<'_>,
    error: &lash_vm_client::PoolError,
) {
    if error.is_host_verdict() {
        ctx.record_nested_effect_error(
            lash_core::RuntimeEffectControllerError::from(error.clone().into_runtime_error())
                .retryable_uncommitted_derivation(),
        );
    }
}

fn exec_response_from(
    collected: CollectedExecutionOutput,
    error: Option<lash_core::CellFailure>,
    terminal_finish: Option<serde_json::Value>,
) -> ExecResponse {
    ExecResponse {
        observations: collected.observations,
        calls: collected.calls,
        printed_images: collected.printed_images,
        error,
        degraded_bindings: Vec::new(),
        terminal_finish,
        terminal_finish_retained: None,
        suspended: false,
    }
}

/// Render a parse failure for the model, with the cell-delimiter warning only
/// where a cell delimiter exists.
///
/// The warning explains a truncation the model cannot see: a closing delimiter
/// line inside multiline source closes the cell early, so the executor
/// receives a program that stops mid-literal. Native `execute_code` calls (ADR
/// 0083) carry the program as a tool argument, where no delimiter can truncate
/// anything — there the sentence names syntax the model never wrote and sends
/// it looking for a cause that does not exist.
///
/// Gated on the channel alone, not on the source containing the closing tag:
/// by the time the executor sees the code, cell extraction has already consumed
/// the delimiter that truncated it, so an implicated delimiter is exactly the
/// case where the source cannot mention one.
fn format_rlm_parse_diagnostic(
    diagnostic: String,
    channel: crate::plugin::RlmChannel,
    tags: crate::dialect::CellTags,
) -> String {
    match channel {
        crate::plugin::RlmChannel::Cell => format!(
            "{diagnostic}\n\nA standalone `{}` line terminates the outer cell even inside multiline source text; construct that content without a standalone delimiter line.",
            tags.close,
        ),
        crate::plugin::RlmChannel::NativeTool => diagnostic,
    }
}

fn select_deferred_resolution_link(
    state: &mut RlmExecutionState,
    ctx: &RuntimeExecutionContext<'_>,
) {
    let Some(invocation) = ctx.parent_invocation() else {
        state.deferred_link = None;
        state.deferred_trigger_resolutions.clear_link();
        return;
    };
    let Some(link_key) =
        lash_lashlang_runtime::DeferredResolutionLinkKey::from_exec_code_invocation(invocation)
    else {
        state.deferred_link = None;
        state.deferred_trigger_resolutions.clear_link();
        return;
    };

    match state.deferred_link.as_mut() {
        Some(link) => link.select_link(link_key.clone()),
        None => {
            state.deferred_link = Some(lash_lashlang_runtime::DeferredLink::new(link_key.clone()));
        }
    }
    state.deferred_trigger_resolutions.select_link(link_key);
}

fn deferred_execution_grants(
    record: &lash_lashlang_runtime::DeferredLink,
) -> BTreeMap<lash_core::ToolId, lash_core::ToolExecutionGrant> {
    record
        .outcomes
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

fn emit_step_trace(ctx: &RuntimeExecutionContext<'_>, result: Result<(), &str>) {
    let Some(invocation) = ctx.parent_invocation() else {
        return;
    };
    let Some(step_index) = invocation.attribution.protocol_iteration else {
        return;
    };
    let Some(standing) = ctx.trace_standing() else {
        return;
    };
    let tracing = lash_core::plugin::PluginExecutionTrace::new(standing);
    tracing.emit(|| {
        let context = lash_core::facade_support::trace_context_for_runtime_invocation(
            tracing.trace_runtime().base_context().clone(),
            invocation,
        );
        let outcome = match result {
            Ok(()) => lash_trace::TraceRlmStepOutcome::Ok,
            Err(diagnostic) => lash_trace::TraceRlmStepOutcome::Failure {
                diagnostic: lash_sansio::session_model::truncate_raw_error(diagnostic),
            },
        };
        (
            context,
            lash_trace::TraceEvent::RlmStep {
                step_index,
                outcome,
            },
        )
    });
}

fn foreground_lashlang_execution_trace(
    ctx: &RuntimeExecutionContext<'_>,
    artifact: &lash_vm_client::InspectedArtifact,
    language: &'static str,
) -> Option<LashlangExecutionTrace> {
    let tracing = lash_core::plugin::PluginExecutionTrace::new(ctx.trace_standing()?);
    if !tracing.observes_language() {
        return None;
    }
    let invocation = ctx.parent_invocation()?;
    let effect_id = invocation.effect_id()?;
    let address = invocation.effect_address()?.clone();
    let generation = match ctx.admitted_process() {
        Some(_) => Some(TraceLanguageExecutionGeneration::new(
            ctx.admitted_process_attempt()?,
        )),
        None => None,
    };
    Some(LashlangExecutionTrace::new(
        tracing,
        language,
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
            source_identity: artifact.source_identity(),
            module_ref: artifact.module_ref().to_string(),
            entry_kind: "main".to_string(),
            entry_ref: None,
            entry_name: "main".to_string(),
            engine_execution_id: ctx.engine_execution_id().map(str::to_string),
            generation,
        },
    ))
}

fn emit_foreground_execution_started(
    trace: &LashlangExecutionTrace,
    artifact: &lash_vm_client::InspectedArtifact,
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

fn trace_main_map(artifact: &lash_vm_client::InspectedArtifact) -> TraceLanguageExecutionMap {
    lash_lashlang_runtime::trace_lashlang_main_map(&artifact.graph)
}

/// Applies a `set_default` patch as one transaction.
///
/// Every key is checked before any of them is applied, and the accepted
/// operations then go to the state as a single batch. A rejected patch — a
/// protected or reserved name anywhere in it — therefore leaves the state
/// exactly as it was, instead of committing the defaults that happened to come
/// first while the caller's dirty tracking records nothing.
#[cfg(test)]
mod tests;

#[cfg(test)]
mod parked_tests;

#[cfg(test)]
pub(crate) use parked_tests::{
    ParkedCellEvidence, execute_parked_cell_for_tests, parked_cell_context_for_tests,
};
