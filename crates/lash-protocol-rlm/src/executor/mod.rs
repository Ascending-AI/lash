//! A session cell on the kernel.
//!
//! A cell is one kernel program (`K-SES-001`). Its source is lowered by the
//! session's dialect, in a worker, to a document against the effects the
//! session offers; a worker-hosted machine runs the document's `main` from
//! the session's bindings; and the kernel broker commits each park with the
//! admission of every tool call the cell asked for since the last. The
//! cell's end leaves the session its bindings.
//!
//! A cell is durable through its parked state (ADR 0132 §8): one with a
//! committed park under its execution resumes from it, on the document and
//! with the envelope its first activation recorded, and runs no saved
//! statement again.

mod carry;
mod cell_outputs;
use cell_outputs::record_cell_outputs;
mod cell_run;
pub(crate) mod envelope;
mod host;
#[cfg(feature = "testing")]
mod perf_fixture;
#[cfg(feature = "testing")]
pub use perf_fixture::{CodeModeCheckpointPerfFixture, CodeModeCheckpointPerfPublisher};
pub(crate) mod host_graph;
mod pages;
mod processes;
mod reads;
mod session;
mod snapshot;
mod trace;

pub(crate) use carry::KernelCarry;
pub use carry::{cell_migration_refusal, cell_snapshot_functions};
pub(crate) use envelope::{
    check_cell_snapshot, recorded_call, recorded_cell_ledger, snapshot_tool_calls,
};
pub use host::{CONTROL_REFUSED, TOOL_ARGUMENTS, TOOL_CALL_LIMIT, TOOL_FAILED, UNKNOWN_EFFECT};
pub use session::{CodeModeExecutionState, saved_function_pins};
pub(crate) use session::{stored_binding_roots, stored_session_descriptor};
pub use snapshot::{CODEMODE_SNAPSHOT_VERSION, SessionOpenError};

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use lash_core::{ExecRequest, ExecResponse, RuntimeExecutionContext};
use lash_kernel_doc::{Datum, IdentifiedDocument, Name};
use lash_kernel_vm::{Bounds, End, Target};
use lash_protocol_types::sync::MutexExt;
use lash_vm_broker::kernel::run::PagePlan;
use lash_vm_broker::kernel::{KernelBroker, KernelCeilings, KernelEnd, KernelFailure};
use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
use lash_vm_client::service::{Request, Response};
use lash_vm_runtime::HostBoundary;

use self::envelope::CellEnvelope;
use self::host::{CellHost, CellHostLedgers};
use crate::cell_value::datum_json;
use crate::feedback::CellObservation;
use crate::projection::CodeModeProjectedBindings;

/// What a cell needs of the session that runs it, beside its state.
#[derive(Clone)]
pub(crate) struct CellServices {
    pub events: Arc<crate::projection::EventDecoder>,
    /// The workers that lower the cell and host its machine.
    pub workers: lash_vm_client::service::Service,
    pub execution_bounds: crate::plugin::ExecutionBounds,
    /// The transport the program arrived on.
    pub channel: crate::plugin::RlmChannel,
    pub code_renderer: crate::render::CodeRendererSlot,
    /// How the session's dialect words what a model reads.
    pub prompts: Arc<dyn crate::dialect::DialectPrompts>,
    /// Which helper release the cell is lowered against; `None` is the
    /// build's own (FIG-5799).
    pub helpers: Option<Arc<dyn crate::plugin::HelperReleaseGate>>,
    /// The library the workers hold, which a cell's document is retained
    /// with the declarations of.
    pub kernel: KernelCarry,
}

/// Runs one cell of `state`'s session, or resumes it from its parked state.
pub(crate) async fn execute_cell(
    state: &mut CodeModeExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    request: ExecRequest,
    services: &CellServices,
    session_projected_bindings: CodeModeProjectedBindings,
) -> ExecResponse {
    let clean_code = clean_model_code(&request.code);
    let opened = cell_run::CellRun::open(&ctx);
    let identities = match &opened {
        Ok(cell) => Ok(cell.identities().clone()),
        Err(_) => cell_run::pure_cell_identities(&ctx),
    };
    let exec = identities
        .as_ref()
        .map_err(Clone::clone)
        .and_then(|identities| cell_run::cell_exec(&ctx, identities));
    let (identities, exec) = match (identities, exec) {
        (Ok(identities), Ok(exec)) => (identities, exec),
        (Err(error), _) | (_, Err(error)) => {
            return exec_setup_failure(lash_core::CellFailure::new(
                lash_core::CellFailureKind::Host,
                format!("the cell has no execution to file its parked state under: {error}"),
            ));
        }
    };
    let snapshots = Arc::new(lash_vm_broker::DurableSnapshotStore::new(
        ctx.actor_context(),
        exec.clone(),
    ));
    let resumed = match &opened {
        Ok(_) => match envelope::resumed_cell(&snapshots, &clean_code).await {
            Ok(resumed) => resumed,
            Err(error) => {
                let message = format!("the cell's parked state cannot be resumed: {error}");
                let mut response = exec_setup_failure(lash_core::CellFailure::new(
                    lash_core::CellFailureKind::Host,
                    message.clone(),
                ));
                fail_cell_on_nested_error(
                    &ctx,
                    &mut response,
                    lash_core::RuntimeEffectControllerError::new(
                        lash_core::RuntimeErrorCode::ExecutionStateCaptureFailed,
                        message,
                    ),
                );
                return response;
            }
        },
        Err(_) => None,
    };
    // A cell with no parked state enters its program from the start: the
    // one entry no resume may repeat (ADR 0132 §8).
    if resumed.is_none() && opened.is_ok() {
        ctx.actor_context().probe().vm_program_entered(&exec);
    }
    // Boxed: the cell's outputs are recorded under the same context after
    // the cell, and an unboxed context held across the cell would size
    // every caller's future.
    let seal_ctx = Box::new(ctx.clone());
    let prints = Arc::new(Mutex::new(
        resumed
            .as_ref()
            .map(|resumed| resumed.prints.clone())
            .unwrap_or_default(),
    ));
    let mut response = Box::pin(run_cell(
        state,
        ctx,
        &clean_code,
        services,
        session_projected_bindings,
        identities,
        Arc::clone(&prints),
        &snapshots,
        resumed,
    ))
    .await;
    // A cell parked beyond this activation has no answer yet: it records no
    // outputs. The activation that ends it does.
    if response.suspended {
        return response;
    }
    // A cell that answers has no runs left to lose (FIG-6806).
    if !seal_ctx.has_nested_effect_error() {
        services.workers.forget_lost_runs(&exec);
    }
    if let Ok(cell) = &opened
        && !seal_ctx.is_cancelled()
        && !seal_ctx.has_nested_effect_error()
    {
        let values = prints.lock_recover().clone();
        if !values.is_empty() || response.finish_value().is_some() {
            record_cell_outputs(
                &seal_ctx,
                cell,
                &services.code_renderer,
                &services.events,
                values,
                &mut response,
            )
            .await;
        }
    }
    response
}

/// Stops the cell on a nested effect's failure: the error is recorded for
/// the turn, and the cell answers it as a host failure.
fn fail_cell_on_nested_error(
    ctx: &RuntimeExecutionContext<'_>,
    response: &mut ExecResponse,
    error: lash_core::RuntimeEffectControllerError,
) {
    let message = error.to_string();
    ctx.record_nested_runtime_effect_error(error);
    fail_cell(
        response,
        lash_core::CellFailure::new(lash_core::CellFailureKind::Host, message),
    );
}

/// Resolves the cell to `failure`, whatever it had resolved to: a cell that
/// finished and then failed is a failed cell, and keeps no finish value.
fn fail_cell(response: &mut ExecResponse, failure: lash_core::CellFailure) {
    response.result = lash_core::CellOutcome::Failed(failure);
    response.retained_finish_value = None;
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

/// A cell that was lowered and admitted: what it runs and what its host
/// holds of it.
struct LinkedCell {
    envelope: CellEnvelope,
    document: IdentifiedDocument,
    boundary: HostBoundary,
    /// The host's read-only bindings, which `main` starts with over the
    /// session's.
    host: Vec<(Name, Datum)>,
    ledgers: CellHostLedgers,
    /// The host graph a resumed cell's root holds.
    graph: host::HostGraph,
    /// The effect identity each call a resumed cell holds open was
    /// performed at.
    open_sites: BTreeMap<lash_core::ToolCallId, lash_kernel_doc::EffectId>,
    resumed: bool,
}

/// A cell that did not get as far as running.
enum Refused {
    /// The cell's answer.
    Cell(Box<lash_core::CellFailure>),
    /// A nested effect of the cell's setup failed: recorded for the turn.
    Nested(Box<lash_core::RuntimeEffectControllerError>),
    /// The fleet cannot yet supply definitions every co-serving node reads.
    Helpers(Box<lash_core::RuntimeEffectControllerError>),
    /// A worker fault.
    Worker(Box<lash_vm_client::PoolError>),
}

impl From<lash_core::CellFailure> for Refused {
    fn from(failure: lash_core::CellFailure) -> Self {
        Self::Cell(Box::new(failure))
    }
}

fn host_failure(message: impl Into<String>) -> Refused {
    Refused::Cell(Box::new(lash_core::CellFailure::new(
        lash_core::CellFailureKind::Host,
        message,
    )))
}

/// Where the session an ended cell's recorded end names stands: under a
/// lease on the cell's ended root `root`, fenced by the activation that
/// resumes it. None for a store that publishes no heap pages.
fn ended_root_pages(
    snapshots: &lash_vm_broker::DurableSnapshotStore,
    root: lash_core_store::checkpoint_graph::NodeId,
) -> Option<lash_core::plugin::SessionPages> {
    let cx = snapshots.context();
    Some(lash_core::plugin::SessionPages {
        store: cx.pages().ok()?,
        actor: cx.actor().clone(),
        epoch: cx.epoch(),
        owner: lash_durable::domain::graph::RootOwner::Exec(snapshots.exec().clone()),
        root,
    })
}

/// Lowers a fresh cell against what the session offers now, or takes a
/// resumed cell as its first activation recorded it.
async fn link_cell(
    state: &mut CodeModeExecutionState,
    ctx: &RuntimeExecutionContext<'_>,
    code: &str,
    services: &CellServices,
    session_projected_bindings: CodeModeProjectedBindings,
    snapshots: &lash_vm_broker::DurableSnapshotStore,
    resumed: Option<envelope::ResumedCell>,
) -> Result<LinkedCell, Refused> {
    if let Some(resumed) = resumed {
        // A resumed cell runs what its first activation admitted: the
        // envelope carries the document and everything it was lowered
        // against, and nothing here is journaled or lowered again.
        ctx.restore_started_process_ids(&resumed.started_process_ids);
        let envelope = resumed.cell;
        if envelope.dialect != state.dialect() {
            return Err(host_failure(format!(
                "the cell was parked as `{}` and this session's dialect is `{}`",
                envelope.dialect,
                state.dialect()
            )));
        }
        // An end an earlier activation recorded names the session as that
        // activation left it, which a read-back end does not carry: the
        // session adopts it, and reads its pages under a lease on the
        // cell's ended root (FIG-6315), which the turn names once the cell
        // answers. The session names that root itself as it adopts the
        // end: its own reads before the turn's next one (the live catalog
        // check of the next sync) reach the pages no run of it holds
        // (FIG-6442).
        if let Some(session) = &resumed.session {
            state
                .restore_execution_graph(
                    session,
                    lash_core_store::execution_state::ExecutionGraphPurpose::Working,
                )
                .map_err(|error| {
                    host_failure(format!(
                        "the session the cell's recorded end names does not restore: {error}"
                    ))
                })?;
            state.note_pages(ended_root_pages(snapshots, resumed.root).as_ref());
        }
        return Ok(LinkedCell {
            document: envelope.document.clone(),
            boundary: envelope.boundary().map_err(host_failure)?,
            envelope,
            // The machine resumes from its parked state, not from a start.
            host: Vec::new(),
            ledgers: resumed.ledgers,
            graph: resumed.graph,
            open_sites: resumed.open_sites,
            resumed: true,
        });
    }

    // The host's read-only bindings are recorded under the cell's effect
    // before it runs, so a redrive of the cell starts with the same ones.
    let projected = {
        let _phase = ctx.named_phase("codemode_lash_vm.resolve_projected_bindings");
        crate::projection::cell_host_bindings(ctx, session_projected_bindings, code)
            .await
            .map_err(|error| match error {
                crate::projection::HostBindingsError::Journal(error) => {
                    Refused::Nested(Box::new(error))
                }
                crate::projection::HostBindingsError::Refused(message) => host_failure(message),
            })?
    };

    // Each catalog tool is an effect, under the name a cell of this
    // dialect calls it by.
    let mut boundary = HostBoundary::new();
    let mut tool_roots = BTreeSet::new();
    // The catalog tools that declare a read, by effect: what the cell's
    // read plan is drawn from.
    let mut read_tools = BTreeMap::new();
    let mut grant_calls = BTreeMap::new();
    for tool in &ctx.tool_catalog().tools {
        let surface = |error: String| host_failure(format!("invalid host tool surface: {error}"));
        let binding = lash_vm_runtime::required_tool_executable(&tool.manifest)
            .map_err(|error| surface(error.to_string()))?;
        if let Some(root) = binding.module_path.first() {
            tool_roots.insert(Name::new(root));
        }
        let path = services
            .prompts
            .tool_call_path(&binding)
            .map_err(|refusal| surface(refusal.to_string()))?;
        grant_calls.insert(tool.manifest.name.clone(), path.clone());
        boundary
            .offer_tool(
                &path,
                tool.manifest.id.clone(),
                &tool.contract.input_schema.canonical,
                &tool.contract.output_schema.canonical,
                tool.manifest.declaration().controls.clone(),
            )
            .map_err(|error| surface(error.to_string()))?;
        if tool.manifest.execution_policy == lash_core::ExecutionPolicy::Read
            && let Ok(effect) = lash_kernel_doc::EffectName::new(path.as_str())
        {
            read_tools.insert(effect, tool.manifest.id.clone());
        }
    }

    state
        .prepare_cell(ctx.session_pages())
        .await
        .map_err(|error| host_failure(format!("the session's state is not readable: {error}")))?;
    let mut names = state.names();
    names.extend(projected.keys().map(|name| Name::new(name.as_str())));

    // A cell is written against the newest helper release every live node
    // holds (FIG-5799).
    let helpers = match &services.helpers {
        Some(gate) => gate
            .writable()
            .await
            .map_err(|error| Refused::Helpers(Box::new(error)))?,
        None => lash_vm_runtime::KERNEL_HELPER_RELEASE,
    };

    // gather → decide → offer: the run's deferred provider matches the
    // paths no surface tool answers, and the matches commit before the cell
    // is lowered. A committed match is never resolved again.
    let mut discovery_failure = None;
    let outcomes =
        if ctx.parent_invocation().is_some() && !matches!(ctx.deferred_provider(), Ok(None)) {
            let _phase = ctx.named_phase("codemode_lash_vm.deferred_resolve");
            let offered = boundary.signatures();
            let calls = services
                .workers
                .request_accounted(Request::ToolCalls {
                    dialect: state.dialect().to_owned(),
                    source: code.to_owned(),
                    bindings: names.clone(),
                    functions: state.lowering_functions(),
                    effects: offered.clone(),
                    controls: boundary.controls(),
                    helpers,
                })
                .await;
            let calls = match calls {
                Ok(Response::ToolCalls(calls)) => calls,
                Ok(Response::Lowered { .. } | Response::Printed(_) | Response::Read { .. }) => {
                    discovery_failure = Some(Err(lash_vm_client::PoolError::breach(
                        lash_vm_protocol::SequenceFault::UnexpectedServiceResponse,
                    )));
                    BTreeSet::new()
                }
                other => {
                    discovery_failure = Some(other);
                    BTreeSet::new()
                }
            };
            let candidates = calls
                .into_iter()
                .filter(|path| {
                    !lash_kernel_doc::EffectName::new(path.as_str())
                        .is_ok_and(|name| offered.contains_key(&name))
                })
                .collect::<BTreeSet<_>>();
            if candidates.is_empty() {
                BTreeMap::new()
            } else {
                crate::deferred::journal_deferred_matches(candidates, ctx)
                    .await
                    .map_err(|error| Refused::Nested(Box::new(error.runtime_effect_error())))?
                    .unwrap_or_default()
            }
        } else {
            BTreeMap::new()
        };
    let grants = crate::deferred::offer_deferred_matches(&mut boundary, &outcomes, ctx)
        .map_err(|error| Refused::Nested(Box::new(error.runtime_effect_error())))?;
    for grant in grants.values() {
        let binding = lash_vm_runtime::required_tool_executable(grant.manifest())
            .map_err(|error| host_failure(error.to_string()))?;
        if let Ok(path) = services.prompts.tool_call_path(&binding) {
            grant_calls.insert(grant.manifest().name.clone(), path);
        }
    }
    let candidates = read_candidates(ctx, &boundary, &grants, read_tools);

    let mut effects = boundary.signatures();
    let unavailable_notes = crate::deferred::offer_unavailable_notes(&outcomes, &mut effects);
    // The worker that lowers the cell starts its run (FIG-5921).
    let lowered = match discovery_failure {
        Some(failure) => failure,
        None => {
            services
                .workers
                .request_for_run(Request::Lower {
                    dialect: state.dialect().to_owned(),
                    source: code.to_owned(),
                    effects,
                    tool_roots,
                    controls: boundary.controls(),
                    bindings: names,
                    functions: state.lowering_functions(),
                    helpers,
                    reads: candidates.keys().cloned().collect(),
                })
                .await
        }
    };
    let (document, source_map, written, function_values, reached) = match lowered {
        Ok(Response::Lowered {
            document,
            source_map,
            written,
            function_values,
            reads,
            ..
        }) => (document, source_map, written, function_values, reads),
        Ok(Response::DialectRefused(mut refusal)) => {
            let feedback =
                crate::feedback::contribute_repairs(&mut refusal, code, &grant_calls, ctx);
            let observation = CellObservation::of_refusal(refusal);
            emit_step_trace(ctx, Err(&observation.code()));
            let mut failure =
                observation.failure(code, None, services.channel, services.prompts.as_ref());
            failure.feedback = feedback;
            return Err(Refused::Cell(Box::new(failure)));
        }
        Ok(Response::UnknownDialect { dialect }) => {
            return Err(host_failure(format!(
                "this session's cells are written in `{dialect}`, and no worker has that dialect installed"
            )));
        }
        Ok(Response::HelperReleaseNotHeld { release }) => {
            return Err(lash_core::CellFailure::new(
                lash_core::CellFailureKind::HelperReleaseNotHeld { release },
                format!("this session's cells are written against helper release {release}, and the worker does not hold it"),
            ).into());
        }
        Ok(Response::Printed(_) | Response::Read { .. } | Response::ToolCalls(_)) => {
            return Err(Refused::Worker(Box::new(
                lash_vm_client::PoolError::breach(
                    lash_vm_protocol::SequenceFault::UnexpectedServiceResponse,
                ),
            )));
        }
        Err(error) => {
            emit_step_trace(ctx, Err(&error.to_string()));
            return Err(Refused::Worker(Box::new(error)));
        }
    };
    emit_step_trace(ctx, Ok(()));
    // The worker's token body is decoded here, once: the identity is its
    // digest, and the envelope, the store and every checkout share all
    // three.
    let document =
        IdentifiedDocument::decode(&document, lash_kernel_doc::KernelDataLimits::standard())
            .map_err(|error| host_failure(format!("the worker's document is not one: {error}")))?;
    let envelope = CellEnvelope {
        code: CellEnvelope::code_digest(code),
        dialect: state.dialect().to_owned(),
        document,
        source_map,
        written,
        function_values,
        effects: CellEnvelope::record_effects(&boundary),
        grants,
        unavailable_notes,
        projected: projected.keys().cloned().collect(),
        reads: candidates
            .into_iter()
            .filter(|(effect, _)| reached.contains(effect))
            .collect(),
    };
    let document = envelope.document.clone();
    if let Some(map) = &envelope.source_map {
        map.validate_identified(&document, code).map_err(|error| {
            host_failure(format!(
                "the worker's source map is not the cell's: {error}"
            ))
        })?;
    }

    // `main` starts with the session's bindings and, over them, the host's
    // read-only ones.
    let host = projected
        .iter()
        .map(|(name, value)| {
            (
                Name::new(name.as_str()),
                crate::cell_value::json_datum(value, document.manifest.numbers),
            )
        })
        .collect();
    Ok(LinkedCell {
        envelope,
        document,
        boundary,
        host,
        ledgers: CellHostLedgers::default(),
        graph: host::HostGraph::default(),
        open_sites: BTreeMap::new(),
        resumed: false,
    })
}

/// The effects a cell of this session may perform as reads (ADR 0144),
/// each with the provider the run's surface retained for its tool: every
/// catalog tool that declares a read and whose read this node can route,
/// unless a plugin sees tool calls through a hook, which a read would pass
/// by. The protocol's own argument projection is no such hook: a read
/// applies it itself. A deferred match is no candidate: it is recorded as a
/// grant and its calls stay recorded. The worker keeps the candidates the
/// cell's `main` certifiably reaches; any other `perform` stays recorded.
fn read_candidates(
    ctx: &RuntimeExecutionContext<'_>,
    boundary: &HostBoundary,
    grants: &BTreeMap<lash_core::ToolId, lash_core::ToolExecutionGrant>,
    read_tools: BTreeMap<lash_kernel_doc::EffectName, lash_core::ToolId>,
) -> BTreeMap<lash_kernel_doc::EffectName, lash_core::ProviderKey> {
    let projection = lash_core::plugin::PluginCallbackIdentity {
        owner: crate::RLM_PROTOCOL_PLUGIN_ID.to_owned(),
        key: format!(
            "{}:{}",
            lash_core_store::tool_run::CallbackSlot::ToolArgsTransform.key_prefix(),
            crate::plugin::PROJECTED_ARGS.as_str()
        ),
    };
    if ctx.tool_call_hooks().iter().any(|hook| *hook != projection) {
        return BTreeMap::new();
    }
    read_tools
        .into_iter()
        .filter(|(effect, tool)| {
            !grants.contains_key(tool)
                && boundary
                    .effect(effect)
                    .is_some_and(|offered| &offered.tool == tool)
        })
        .filter_map(|(effect, tool)| Some((effect, ctx.read_route(&tool).ok()?.provider)))
        .collect()
}

/// The bounds a cell's run asks for: the session's instruction and memory
/// bounds under the worker pool's ceiling, and the pool's own for the rest.
fn cell_bounds(services: &CellServices) -> Bounds {
    let pool = services.workers.config().run_bounds;
    Bounds {
        memory_meter: services.execution_bounds.memory_meter,
        charge: services.execution_bounds.charge,
        charge_limit: services.execution_bounds.charge_limit_under(&pool),
        memory: services.execution_bounds.memory_under(&pool),
        call_depth: pool.call_depth,
        live_tasks: pool.live_tasks,
        requests_per_park: pool.requests_per_park,
        join_members: pool.join_members,
    }
}

/// The executable generation a session's cells run under (FIG-3571): how
/// a recorded cell is run on, which is its envelope. A cell's document
/// and everything it was lowered against are recorded there, and a kernel
/// version that replaces the one the document is written in carries the
/// cell forward by migration when its turn is restored (kernel spec §6),
/// so the kernel version is no part of the generation: a build that no
/// longer reads a cell's kernel version refuses its snapshot instead.
pub(crate) fn cell_generation() -> lash_core::ExecutableGeneration {
    lash_core::ExecutableGeneration::new("kernel:1")
}

#[expect(clippy::too_many_arguments, reason = "one cell's whole run")]
async fn run_cell(
    state: &mut CodeModeExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    code: &str,
    services: &CellServices,
    session_projected_bindings: CodeModeProjectedBindings,
    identities: lash_vm_broker::CodeCallIdentities,
    prints: Arc<Mutex<Vec<Datum>>>,
    snapshots: &Arc<lash_vm_broker::DurableSnapshotStore>,
    resumed: Option<envelope::ResumedCell>,
) -> ExecResponse {
    // The model response the turn holds for the cell's first save is not
    // held across a wait for a worker: it commits on its own first
    // (FIG-6000).
    if let Some(held) = ctx.held_response()
        && !services.workers.worker_ready()
        && let Err(error) = held.commit_alone(ctx.actor_context()).await
    {
        let message = format!("the model response that asked for the cell did not commit: {error}");
        ctx.record_nested_effect_error(
            lash_core::RuntimeEffectControllerError::retryable_response_derivation(message.clone()),
        );
        return exec_setup_failure(lash_core::CellFailure::new(
            lash_core::CellFailureKind::Host,
            message,
        ));
    }
    state.begin_code_execution();
    state.note_pages(ctx.session_pages());
    // The wall time the cell held workers before this activation counts
    // against its cap (FIG-6786).
    let held = resumed.as_ref().map_or(std::time::Duration::ZERO, |cell| {
        std::time::Duration::from_millis(cell.graph.state.held_ms)
    });
    let workers = services.workers.begin_execution(held);
    let services = CellServices {
        workers,
        ..services.clone()
    };
    let response = Box::pin(run_linked_cell(
        state,
        ctx,
        code,
        &services,
        session_projected_bindings,
        identities,
        prints,
        snapshots,
        resumed,
    ))
    .await;
    // The worker that lowered a cell whose run never started on it goes
    // back to the pool.
    services.workers.release_retained().await;
    response
}

#[expect(clippy::too_many_arguments, reason = "one cell's whole run")]
async fn run_linked_cell(
    state: &mut CodeModeExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    code: &str,
    services: &CellServices,
    session_projected_bindings: CodeModeProjectedBindings,
    identities: lash_vm_broker::CodeCallIdentities,
    prints: Arc<Mutex<Vec<Datum>>>,
    snapshots: &Arc<lash_vm_broker::DurableSnapshotStore>,
    resumed: Option<envelope::ResumedCell>,
) -> ExecResponse {
    let linked = match link_cell(
        state,
        &ctx,
        code,
        services,
        session_projected_bindings,
        snapshots,
        resumed,
    )
    .await
    {
        Ok(linked) => linked,
        Err(Refused::Cell(failure)) => return exec_setup_failure_or_stop(state, &ctx, *failure),
        Err(Refused::Nested(error)) => {
            let message = error.to_string();
            ctx.record_nested_runtime_effect_error(*error);
            return exec_setup_failure_or_stop(
                state,
                &ctx,
                lash_core::CellFailure::new(lash_core::CellFailureKind::Host, message),
            );
        }
        Err(Refused::Helpers(error)) => {
            let message = error.to_string();
            ctx.record_nested_effect_error(*error);
            return exec_setup_failure_or_stop(
                state,
                &ctx,
                lash_core::CellFailure::new(lash_core::CellFailureKind::Host, message),
            );
        }
        Err(Refused::Worker(error)) => return worker_setup_failure(state, &ctx, *error),
    };
    // A process the cell starts captures the tools its committed deferred
    // matches resolved from them, with their bindings, never resolving
    // again; a resumed cell's are its envelope's.
    let ctx = ctx.with_recorded_grants(Arc::new(linked.envelope.recorded_grants()));

    // Every call the cell makes is its own admitted execution, run from
    // these bodies (ADR 0132 §5): a resumed cell knows each call its ledger
    // holds open, so a call still running settles on this owner.
    let members =
        match lash_core::tool_dispatch::CellMembers::new(&ctx, identities.opener().clone()) {
            Ok(members) => Arc::new(members),
            Err(error) => {
                return exec_setup_failure_or_stop(
                    state,
                    &ctx,
                    lash_core::CellFailure::new(
                        lash_core::CellFailureKind::Host,
                        error.to_string(),
                    ),
                );
            }
        };
    snapshots
        .bind_members(Arc::clone(&members) as _, members.policies())
        .await;

    let scope = match ctx.session_scope() {
        Ok(scope) => scope,
        Err(error) => {
            return exec_setup_failure_or_stop(
                state,
                &ctx,
                lash_core::CellFailure::new(lash_core::CellFailureKind::Host, error.to_string()),
            );
        }
    };
    let owner = lash_vm_protocol::VmOwner::new(format!(
        "codemode:{}:{:?}",
        scope.session_id, scope.agent_frame_id
    ));
    let LinkedCell {
        envelope,
        document,
        boundary,
        host: host_bindings,
        ledgers,
        graph,
        open_sites,
        resumed: runs_on,
    } = linked;
    // The run names the dialect its cell's envelope recorded: a worker
    // whose embedding lacks it refuses the cell (FIG-5990).
    let dialect = envelope.dialect.clone();
    // The map the cell was lowered with, stored in its envelope: a resumed
    // cell's failure locates its tasks by it, never by lowering again.
    let source_map = envelope.source_map.clone();
    let document_identity = document.id();
    let segment = match snapshots.root().await {
        Ok(root) => root.map_or_else(|| "start".to_owned(), |root| root.root.to_string()),
        Err(error) => {
            return exec_setup_failure_or_stop(
                state,
                &ctx,
                lash_core::CellFailure::new(lash_core::CellFailureKind::Host, error.to_string()),
            );
        }
    };
    let observation = ctx
        .run_spec()
        .map_or(lash_kernel_doc::RunObservation::Off, |spec| spec.observe);
    let trace = trace::CellTrace::new(&ctx, document_identity, &envelope.dialect, segment);
    if let Some(trace) = &trace {
        trace.started();
    }
    let published = if runs_on {
        Ok(())
    } else {
        match services.kernel.library() {
            Ok(functions) => processes::retain_document(&ctx, &document, &functions).await,
            Err(refusal) => Err(refusal.to_string()),
        }
    };
    if let Err(message) = published {
        return exec_setup_failure_or_stop(
            state,
            &ctx,
            lash_core::CellFailure::new(lash_core::CellFailureKind::Host, message),
        );
    }
    // Each planned read goes where the run's surface retained its tool,
    // and nowhere else: a node whose provider drifted or registers no
    // reader takes up none of the cell, which waits for one that does.
    let mut routes = BTreeMap::new();
    for (effect, provider) in &envelope.reads {
        let Some(recorded) = envelope.effects.get(effect) else {
            return exec_setup_failure_or_stop(
                state,
                &ctx,
                lash_core::CellFailure::new(
                    lash_core::CellFailureKind::Host,
                    format!(
                        "the cell's read plan names `{effect}`, which it was not lowered against"
                    ),
                ),
            );
        };
        match ctx.read_route(&recorded.tool) {
            Ok(route) if &route.provider == provider => {
                routes.insert(effect.clone(), route);
            }
            routed => {
                let error = routed.err().unwrap_or_else(|| {
                    lash_core::RuntimeEffectControllerError::new(
                        lash_core::RuntimeErrorCode::LashVmCellBindingDrift,
                        format!(
                            "the cell plans its reads of `{effect}` through provider `{provider}`, \
                             and the run's surface now routes tool `{}` elsewhere; nothing was read",
                            recorded.tool
                        ),
                    )
                });
                let message = error.to_string();
                ctx.record_nested_runtime_effect_error(error);
                return exec_setup_failure_or_stop(
                    state,
                    &ctx,
                    lash_core::CellFailure::new(lash_core::CellFailureKind::Host, message),
                );
            }
        }
    }
    let modes = lash_kernel_vm::EffectModes::reading(envelope.reads.keys().cloned());
    let ledgers = Arc::new(Mutex::new(ledgers));
    let cell_reads = Arc::new(reads::CellReads {
        routes,
        config: ctx.owner_config().clone(),
        cancel: ctx.cancellation(),
        parent_wait: services.workers.config().tuning.parent_wait,
        ledgers: Arc::clone(&ledgers),
        boundary: boundary.clone(),
        limit: ctx.max_tool_calls(),
        cell: "cell".to_owned(),
    });
    let host = CellHost {
        trace: trace.clone(),
        sites: Mutex::new(open_sites),
        ctx: ctx.clone(),
        boundary,
        grants: envelope.grants.clone(),
        members,
        opener: identities.opener().clone(),
        prints: Arc::clone(&prints),
        ledgers,
        envelope,
        graph: Mutex::new(graph),
        budget: services
            .workers
            .execution_budget()
            .cloned()
            .unwrap_or_default(),
    };
    let printed = Arc::clone(&prints);
    let random = std::collections::hash_map::RandomState::new();
    let drawn = std::sync::atomic::AtomicU64::new(0);
    let counts_trace = trace.clone();
    let run_host = Arc::new(lash_vm_runtime::ParentHost {
        observation,
        ran: Arc::new(move |delta| {
            if let Some(trace) = &counts_trace {
                trace.ran(delta);
            }
        }),
        store: Arc::clone(snapshots),
        random: Arc::new(move || {
            use std::hash::BuildHasher as _;
            random.hash_one(drawn.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
        }),
        providers: lash_vm_runtime::ProjectionCatalog::of_backend(
            ctx.projection_providers().as_deref(),
        ),
        printed: Arc::new(move |value| printed.lock_recover().push(value)),
        reads: cell_reads,
    });
    let bounds = cell_bounds(services);
    let save = lash_vm_client::wire::SaveWire {
        controls: host.boundary.controls(),
        written: host.envelope.written.clone(),
    };
    let machines = match lash_vm_client::RemoteMachines::new(
        services.workers.clone(),
        owner,
        lash_vm_protocol::OwnerEpoch(0),
        lash_vm_protocol::FrameEpoch(0),
        run_host,
        &document,
        &dialect,
        lash_vm_client::RunStart {
            target: Target::Main,
            args: Vec::new(),
            session: state.session(),
            modes,
            host: host_bindings,
            save: Some(save),
        },
        bounds,
    ) {
        Ok(machines) => machines,
        Err(error) => {
            return exec_setup_failure_or_stop(
                state,
                &ctx,
                lash_core::CellFailure::new(lash_core::CellFailureKind::Host, error.to_string()),
            );
        }
    };
    let plan = match cell_pages(state, services).await {
        Ok(plan) => plan,
        Err(error) => {
            return exec_setup_failure_or_stop(
                state,
                &ctx,
                lash_core::CellFailure::new(
                    lash_core::CellFailureKind::Host,
                    format!("the session's pages are not readable: {error}"),
                ),
            );
        }
    };
    let pool = services.workers.config();
    let run = Box::pin(
        KernelBroker {
            store: snapshots.as_ref(),
            effects: &host,
            identities: &identities,
            ceilings: KernelCeilings {
                requests_per_park: pool.run_bounds.requests_per_park,
                join_members: pool.run_bounds.join_members,
                effects: pool.cell_lifetime.effects,
                checkpoint_bytes: pool.cell_lifetime.checkpoint_bytes,
            },
            slice: pool.tuning.slice,
            pages: &plan,
        }
        .run(&machines, &ctx.cancellation()),
    )
    .await;
    // A read the limit refused is the turn's refusal as a call's is.
    if let Some(exceeded) = host.ledgers().tool_call_limit {
        ctx.record_tool_call_limit_refusal(exceeded);
    }

    // A finished cell's carry is the session's next version, staged in the
    // turn's spool under a descriptor of its own. The committed pages it
    // names stay under its run's lease, which the session holds until a
    // committed capture owns the carry.
    let store = snapshots.context().pages().ok();
    let staged = match &run {
        Ok(KernelEnd::Ended {
            end: End::Finished(finished),
            pages,
            ..
        }) => match finished.carry {
            Some(carry) => Some(
                state
                    .stage_cell(
                        carry,
                        finished.not_carried.clone(),
                        pages
                            .clone()
                            .zip(store.clone())
                            .map(|(pages, store)| pages::HeldRun::new(pages, store)),
                    )
                    .await
                    .map_err(|error| error.to_string()),
            ),
            None => None,
        },
        _ => None,
    };
    let held = staged.is_some();
    // A run that parked owes its end over its root: held for the commit
    // that consumes the cell's answer, or recorded with the session's
    // descriptor as the cell left it.
    let owed = match &run {
        Ok(KernelEnd::Ended {
            end,
            owed: Some(ledger),
            pages: Some(pages),
        }) => Some((ledger.as_ref().clone(), end.clone(), Arc::clone(pages))),
        _ => None,
    };
    // The last activation, which the cell's end reads and stages through;
    // it ends once the end is answered and committed.
    let activation = match &run {
        Ok(KernelEnd::Ended {
            pages: Some(pages), ..
        }) => Some(Arc::clone(pages)),
        _ => None,
    };
    // An end an earlier activation recorded stands over the cell's root.
    let recorded = matches!(
        &run,
        Ok(KernelEnd::Ended { end, owed: None, .. }) if !matches!(end, End::Cancelled)
    );
    let document_id = document_identity;
    let died = cell_run::DiedRunning {
        workers: &services.workers,
        exec: snapshots.exec(),
    };
    // The rest answers the cell from what the run left; the execution's
    // terminal fact follows its answer.
    let answer = || -> ExecResponse {
        let collected = host.ledgers();
        let respond = |result| exec_response_from(&collected, result);
        let failed = |kind, message: String| {
            respond(lash_core::CellOutcome::Failed(lash_core::CellFailure::new(
                kind, message,
            )))
        };
        let end = match run {
            Ok(KernelEnd::Ended { end, .. }) => end,
            // The cell stays parked on waits that outlive this activation: its
            // checkpoint, committed with its envelope, holds everything the
            // cell holds, and the cell has no answer yet.
            Ok(KernelEnd::Suspended) => {
                return ExecResponse {
                    suspended: true,
                    ..respond(lash_core::CellOutcome::Completed)
                };
            }
            Err(failure) => {
                return broker_failure(state, &ctx, failure, runs_on, &collected, &died);
            }
        };
        // A cancelled tool call ends the cell, whatever its program made of the
        // error; so does the turn's own cancel.
        if collected.call_cancelled() || ctx.is_cancelled() || matches!(end, End::Cancelled) {
            state.rollback_code_execution();
            return failed(
                lash_core::CellFailureKind::Host,
                "foreground execution stopped".to_owned(),
            );
        }
        // A cell over `max_tool_calls` fails with the limit, at the call that
        // met it, whatever its program made of the refusal.
        if let Some(exceeded) = collected.tool_call_limit {
            let mut failure = lash_core::CellFailure::new(
                lash_core::CellFailureKind::Program,
                exceeded.to_string(),
            );
            failure.tool_call_limit = Some(exceeded);
            return respond(lash_core::CellOutcome::Failed(failure));
        }
        match end {
            End::Finished(finished) => {
                let bindings = match staged {
                    Some(Ok(changes)) => changes.bindings,
                    Some(Err(error)) => {
                        return failed(
                            lash_core::CellFailureKind::Host,
                            format!("the cell's session state did not stage: {error}"),
                        );
                    }
                    None => lash_core::BindingChanges::default(),
                };
                // A cell ends its turn only through a control call: the
                // dialect ends `main` right after one settles, and the
                // control is the settled call's, never the program's own
                // value. `main` that ran to its end, or whose control call
                // gave no control, answered nothing: the cell completed.
                let controlled = finished
                    .finish
                    .then(|| collected.settled_control(&host.boundary))
                    .flatten()
                    .map(|(record, control)| lash_core::CellOutcome::Controlled {
                        tool_name: record.tool.clone(),
                        call_id: record.call_id.clone(),
                        control: match control {
                            lash_core::TurnControl::Finish { value, .. } => {
                                lash_core::CellControl::Finish {
                                    value: lash_core::ToolCallOutput::success_tool_value(
                                        value.clone(),
                                    )
                                    .value_for_projection()
                                    .into(),
                                }
                            }
                            lash_core::TurnControl::SwitchAgentFrame { frame_key, .. } => {
                                lash_core::CellControl::SwitchAgentFrame {
                                    frame_key: frame_key.clone(),
                                }
                            }
                        },
                    });
                ExecResponse {
                    bindings: Box::new(bindings),
                    ..respond(controlled.unwrap_or(lash_core::CellOutcome::Completed))
                }
            }
            End::Failed(reason) => failed(
                lash_core::CellFailureKind::Program,
                format!(
                    "the program failed: {}",
                    crate::feedback::datum_text(&reason)
                ),
            ),
            End::Error(error) => {
                let observation = CellObservation::of_run_error(error, &state.not_carried());
                respond(lash_core::CellOutcome::Failed(observation.failure(
                    code,
                    source_map.as_ref().filter(|map| {
                        map.validate_identified(&host.envelope.document, code)
                            .is_ok()
                    }),
                    services.channel,
                    services.prompts.as_ref(),
                )))
            }
            End::Cancelled => failed(
                lash_core::CellFailureKind::Host,
                "foreground execution stopped".to_owned(),
            ),
        }
    };
    let mut response = answer();
    let ended = match owed {
        Some((ledger, end, pages)) => {
            end_cell(state, snapshots, &host, document_id, ledger, end, &pages)
                .await
                .map(Some)
        }
        None if recorded => snapshots
            .root()
            .await
            .map(|root| {
                root.map(|root| lash_core::EndedCell {
                    exec: snapshots.exec().clone(),
                    root: root.root,
                    held: false,
                })
            })
            .map_err(KernelFailure::Checkpoint),
        None => Ok(None),
    };
    match ended {
        Ok(Some(ended)) => ctx.cell_ended(ended),
        Ok(None) => {}
        Err(failure) => {
            response = broker_failure(state, &ctx, failure, runs_on, &host.ledgers(), &died);
        }
    }
    if let Some(pages) = activation
        && !held
        && let Some(store) = store
    {
        lash_vm_broker::kernel::end_activation(store.as_ref(), &pages).await;
    }
    if let Some(trace) = &host.trace
        && !response.suspended
    {
        use lash_trace::TraceLanguageExecutionStatus as Status;
        match &response.result {
            lash_core::CellOutcome::Failed(failure) => {
                trace.finished(Status::Failed, Some(failure.message.clone()));
            }
            _ => trace.finished(Status::Completed, None),
        }
    }
    response
}

/// Ends a cell that parked. A turn that holds the end
/// ([`RuntimeExecutionContext::holds_cell_end`]) is left the cell's parked
/// root, which the commit that consumes the cell's answer retires with the
/// session's working state as the cell left it (FIG-5999): nothing commits
/// here, and a successor that finds the root runs the cell on from its last
/// park, whose final stretch issued no effect (ADR 0132 §8). A cell that
/// ends with an execution still open, or whose turn runs a callback of its
/// own before that commit, records its end ([`record_end`]).
async fn end_cell(
    state: &mut CodeModeExecutionState,
    snapshots: &lash_vm_broker::DurableSnapshotStore,
    host: &CellHost<'_>,
    document: lash_kernel_doc::DocumentId,
    ledger: lash_vm_broker::kernel::EffectLedger,
    end: End,
    pages: &lash_vm_broker::kernel::pages::SharedPages,
) -> Result<lash_core::EndedCell, KernelFailure> {
    if host.ctx.holds_cell_end()
        && let Some(held) = snapshots.held_end(&ledger).await?
    {
        return Ok(lash_core::EndedCell {
            exec: snapshots.exec().clone(),
            root: held.root,
            held: true,
        });
    }
    let root = record_end(state, snapshots, host, document, ledger, end, pages).await?;
    Ok(lash_core::EndedCell {
        exec: snapshots.exec().clone(),
        root,
        held: false,
    })
}

/// Commits the end of a cell that parked over its root: the run's
/// terminal, the session's descriptor as the cell left it, the host's
/// state and envelope, in one CellEnd, publishing the pages the cell
/// staged since its last park. The turn's consuming phase adopts the
/// descriptor and retires the root this answers.
async fn record_end(
    state: &mut CodeModeExecutionState,
    snapshots: &lash_vm_broker::DurableSnapshotStore,
    host: &CellHost<'_>,
    document: lash_kernel_doc::DocumentId,
    ledger: lash_vm_broker::kernel::EffectLedger,
    end: End,
    pages: &lash_vm_broker::kernel::pages::SharedPages,
) -> Result<lash_core_store::checkpoint_graph::NodeId, KernelFailure> {
    let session = state.end_descriptor().map_err(|error| {
        KernelFailure::Checkpoint(lash_vm_broker::QuietPointRefusal(error.to_string()))
    })?;
    snapshots
        .commit_run_end(
            lash_vm_broker::kernel::EndSave {
                document,
                ledger,
                end,
                session,
                pages,
                with: Vec::new(),
            },
            host,
        )
        .await
        .map(|saved| saved.root)
        .map_err(KernelFailure::Checkpoint)
}

/// The pages a cell's run reads and stages: the turn's spool, the
/// session's lease, and its carried roots, which the run names unread.
async fn cell_pages(
    state: &mut CodeModeExecutionState,
    services: &CellServices,
) -> Result<PagePlan, pages::PageAccessError> {
    let spool = state.turn_spool()?;
    let config = services.workers.config();
    Ok(PagePlan {
        spool: Arc::new(move || {
            Ok(Box::new(spool.clone())
                as Box<
                    dyn lash_core_store::checkpoint_graph::stage::StageSpool,
                >)
        }),
        fresh: state.lease().await?,
        references: state.session_references(),
        limits: lash_vm_broker::kernel::pages::PageLimits::within(config.protocol.decode),
        lease_ttl: pages::LEASE_TTL,
        span: std::time::Duration::from_secs(3600),
        cache: Some(services.workers.page_cache()),
        hints: state
            .actor()
            .map(|actor| services.workers.page_hints().of(actor)),
    })
}

/// How a broker failure answers the cell. Nothing about the guest: the
/// cell's last checkpoint stands.
fn broker_failure(
    state: &mut CodeModeExecutionState,
    ctx: &RuntimeExecutionContext<'_>,
    failure: KernelFailure,
    runs_on: bool,
    collected: &CellHostLedgers,
    died: &cell_run::DiedRunning<'_>,
) -> ExecResponse {
    let message = failure.to_string();
    let host = |message: String| {
        exec_response_from(
            collected,
            lash_core::CellOutcome::Failed(lash_core::CellFailure::new(
                lash_core::CellFailureKind::Host,
                message,
            )),
        )
    };
    match failure {
        KernelFailure::Worker { outcome } => {
            // A limit the run itself exhausted is the cell's result.
            if let lash_vm_protocol::InfrastructureOutcome::WorkerLimitExceeded { limit } = &outcome
                && !limit.is_host_verdict()
            {
                return exec_response_from(
                    collected,
                    lash_core::CellOutcome::Failed(crate::feedback::worker_limit_failure(*limit)),
                );
            }
            // A worker that died running the cell ends it once its death is
            // the cell's to read; until then the pass fails as the host's
            // verdict below, and the next runs the cell again (FIG-6806).
            if let lash_vm_protocol::InfrastructureOutcome::RunCrashedWorker { evidence } = &outcome
                && let Some(failure) = died.answer(*evidence, &collected.executed_calls())
            {
                return exec_response_from(collected, lash_core::CellOutcome::Failed(failure));
            }
            // The state a resumed cell runs on from is refused: another
            // build wrote it. That is no failure the model sees: the cell
            // aborts with the typed refusal, and its turn parks on the
            // cell's checkpoint.
            if runs_on
                && let lash_vm_protocol::InfrastructureOutcome::RunRefused { refusal } = &outcome
                && matches!(refusal, lash_vm_protocol::RunRefusal::Restore { .. })
            {
                let mut refused = lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::VmWorkerFailed,
                    message.clone(),
                );
                refused.cause = Some(lash_core::RuntimeErrorCause::CellSnapshotUndecodable {
                    refusal: Box::new(refusal.clone()),
                });
                ctx.record_nested_effect_error(
                    lash_core::RuntimeEffectControllerError::from(refused)
                        .retryable_uncommitted_derivation(),
                );
                return host(message);
            }
            fail_attempt_on_host_verdict(ctx, &lash_vm_client::PoolError::from(outcome));
            if ctx.is_cancelled() {
                state.rollback_code_execution();
            }
            host(message)
        }
        // The parked state is refused where the machine imports it: the
        // same account as a worker's refusal of it.
        KernelFailure::Open(_) | KernelFailure::Sealed(_) | KernelFailure::EmptyCheckpoint
            if runs_on =>
        {
            ctx.record_nested_effect_error(
                lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::ExecutionStateCaptureFailed,
                    message.clone(),
                )
                .retryable_uncommitted_derivation(),
            );
            host(message)
        }
        // The broker's own invariant failed over sound records: every pass
        // meets it again, so the turn ends with it, typed, never retried.
        KernelFailure::Invariant(_) => {
            ctx.record_nested_effect_error(lash_core::RuntimeEffectControllerError::new(
                lash_core::RuntimeErrorCode::BrokerInvariant,
                message.clone(),
            ));
            if ctx.is_cancelled() {
                state.rollback_code_execution();
            }
            host(message)
        }
        // The parent's own store or host faulted: the attempt fails
        // retryably, and the cell resumes from its last checkpoint.
        KernelFailure::Checkpoint(_) | KernelFailure::Parent(_) => {
            ctx.record_nested_effect_error(
                lash_core::RuntimeEffectControllerError::retryable_response_derivation(
                    message.clone(),
                ),
            );
            if ctx.is_cancelled() {
                state.rollback_code_execution();
            }
            host(message)
        }
        _ => host(message),
    }
}

fn exec_setup_failure(error: lash_core::CellFailure) -> ExecResponse {
    ExecResponse {
        prints_retained: None,
        prints: Vec::new(),
        calls: Vec::new(),
        tool_calls: Vec::new(),
        printed_images: Vec::new(),
        result: lash_core::CellOutcome::Failed(error),
        retained_finish_value: None,
        degraded_bindings: Vec::new(),
        bindings: Default::default(),
        suspended: false,
    }
}

fn exec_setup_failure_or_stop(
    state: &mut CodeModeExecutionState,
    ctx: &RuntimeExecutionContext<'_>,
    failure: lash_core::CellFailure,
) -> ExecResponse {
    if ctx.is_cancelled() {
        state.rollback_code_execution();
        return exec_setup_failure(lash_core::CellFailure::new(
            lash_core::CellFailureKind::Host,
            "foreground execution stopped during setup",
        ));
    }
    exec_setup_failure(failure)
}

/// A worker service fault in a cell's setup. A host verdict (its retryable
/// worker failure, worker budget or pool capacity) is read live, outside
/// any recorded step, and a replay or another host with capacity answers it
/// differently: it fails the attempt retryably, so the cell seals nothing
/// and the model never sees it. Any other fault is the cell's host failure.
fn worker_setup_failure(
    state: &mut CodeModeExecutionState,
    ctx: &RuntimeExecutionContext<'_>,
    error: lash_vm_client::PoolError,
) -> ExecResponse {
    if let lash_vm_client::PoolError::Infrastructure(
        lash_vm_protocol::InfrastructureOutcome::WorkerLimitExceeded { limit },
    ) = &error
        && !limit.is_host_verdict()
    {
        let mut response =
            exec_setup_failure_or_stop(state, ctx, crate::feedback::worker_limit_failure(*limit));
        if let lash_core::CellOutcome::Failed(failure) = &mut response.result {
            failure.worker_limit = Some(*limit);
        }
        return response;
    }
    fail_attempt_on_host_verdict(ctx, &error);
    exec_setup_failure_or_stop(
        state,
        ctx,
        lash_core::CellFailure::new(lash_core::CellFailureKind::Host, error.to_string()),
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

fn exec_response_from(collected: &CellHostLedgers, result: lash_core::CellOutcome) -> ExecResponse {
    ExecResponse {
        prints_retained: None,
        prints: Vec::new(),
        calls: collected.executed_calls(),
        tool_calls: collected.tool_call_records(),
        printed_images: Vec::new(),
        result,
        retained_finish_value: None,
        degraded_bindings: Vec::new(),
        bindings: Default::default(),
        suspended: false,
    }
}

fn emit_step_trace(ctx: &RuntimeExecutionContext<'_>, result: Result<(), &str>) {
    let Some(invocation) = ctx.parent_invocation().cloned() else {
        return;
    };
    let Some(step_index) = invocation.attribution.protocol_iteration else {
        return;
    };
    let Some(standing) = ctx.trace_standing() else {
        return;
    };
    let tracing = lash_core::plugin::PluginExecutionTrace::new(standing);
    let failure = result.err().map(str::to_owned);
    let emit = move || step_trace(&tracing, &invocation, step_index, failure.as_deref());
    // Nothing of the cell is traced before a commit holds the model
    // response that asked for it (FIG-6000).
    match ctx.held_response() {
        Some(held) => held.after_commit(Box::new(emit)),
        None => emit(),
    }
}

fn step_trace(
    tracing: &lash_core::plugin::PluginExecutionTrace,
    invocation: &lash_core::RuntimeInvocation,
    step_index: usize,
    failure: Option<&str>,
) {
    tracing.emit(|| {
        let context = lash_core::facade_support::trace_context_for_runtime_invocation(
            lash_trace::TraceContext::default(),
            invocation,
        );
        let outcome = match failure {
            None => lash_trace::TraceProgramStepOutcome::Ok,
            Some(diagnostic) => lash_trace::TraceProgramStepOutcome::Failure {
                diagnostic: tracing
                    .trace_runtime()
                    .diagnostic_limits()
                    .diagnostic_error(diagnostic),
            },
        };
        (
            context,
            lash_trace::TraceEvent::ProgramStep {
                step_index,
                outcome,
            },
        )
    });
}

#[cfg(test)]
mod tests;
