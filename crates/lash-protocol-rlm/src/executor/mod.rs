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

pub(crate) mod carry;
pub(crate) mod cell_carry;
mod cell_outputs;
use cell_outputs::prepare_cell_outputs;
mod cell_run;
pub(crate) mod envelope;
mod host;
mod link;
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
pub use carry::cell_migration_refusal;
pub(crate) use envelope::{
    check_cell_snapshot, recorded_call, recorded_cell_ledger, snapshot_tool_calls,
};
pub use host::{CONTROL_REFUSED, TOOL_ARGUMENTS, TOOL_CALL_LIMIT, TOOL_FAILED, UNKNOWN_EFFECT};
pub use session::{CodeModeExecutionState, saved_function_pins};
pub use snapshot::{CODEMODE_SNAPSHOT_VERSION, SessionOpenError};

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use lash_core::{ExecRequest, ExecResponse, RuntimeExecutionContext};
use lash_kernel_doc::{Datum, IdentifiedDocument};

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

/// What a cell needs of the session that runs it, beside its state.
#[derive(Clone)]
pub(crate) struct CellServices {
    pub documents: Option<lash_vm_runtime::KernelDocuments>,
    /// The workers that lower the cell and host its machine.
    pub workers: lash_vm_client::service::Service,
    pub execution_bounds: crate::plugin::ExecutionBounds,
    /// The transport the program arrived on.
    pub channel: crate::plugin::RlmChannel,
    pub code_renderer: crate::render::CodeRendererSlot,
    /// How the session's dialect words what a model reads.
    pub prompts: Arc<dyn crate::dialect::DialectPrompts>,
    /// The workers' library declarations retained with a cell's Document.
    pub kernel: KernelCarry,
}

/// Runs one cell of `state`'s session, or resumes it from its parked state.
pub(crate) async fn execute_cell(
    state: &mut CodeModeExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    request: ExecRequest,
    services: &CellServices,
) -> ExecResponse {
    let backend = ctx.actor_context().backend();
    let library = match services.workers.config().worker_library() {
        Ok(library) => library,
        Err(error) => {
            return exec_setup_failure(lash_core::CellFailure::new(
                lash_core::CellFailureKind::Host,
                error.to_string(),
            ));
        }
    };
    let previous = services.documents.as_ref().or(state.documents.as_ref());
    if previous.is_some_and(|owner| !Arc::ptr_eq(owner.helpers(), library.helpers())) {
        return exec_setup_failure(lash_core::CellFailure::new(
            lash_core::CellFailureKind::Host,
            "the document owner must share the workers' helper image",
        ));
    }
    state.documents = Some(match previous {
        Some(owner) => owner.bind(backend.module_artifacts(), backend.package_store()),
        None => lash_vm_runtime::KernelDocuments::new(
            backend.module_artifacts(),
            backend.package_store(),
            Arc::clone(library.helpers()),
        ),
    });
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
    let opened = match opened {
        Ok(value) => {
            match Box::pin(cell_carry::carry(
                &snapshots,
                &services.kernel,
                services,
                state.documents.as_ref(),
                &ctx,
            ))
            .await
            {
                Ok(()) => Ok(value),
                Err(error) => {
                    let message = error.to_string();
                    ctx.record_nested_effect_error(error.into_runtime());
                    return exec_setup_failure(lash_core::CellFailure::new(
                        lash_core::CellFailureKind::Host,
                        message,
                    ));
                }
            }
        }
        Err(error) => Err(error),
    };
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
    let resumes = resumed.is_some();
    // A cell with no parked state enters its program from the start: the
    // one entry no resume may repeat (ADR 0132 §8).
    if !resumes && opened.is_ok() {
        ctx.actor_context().probe().vm_program_entered(&exec);
    }
    // Boxed: the cell's outputs are prepared under the same context after
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
            prepare_cell_outputs(
                &seal_ctx,
                cell,
                &services.code_renderer,
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
    /// Certifiable invocations, derived from the envelope's immutable Link.
    invoked: std::collections::BTreeSet<lash_vocabulary::OperationUse>,
    envelope: CellEnvelope,
    document: IdentifiedDocument,
    ledgers: CellHostLedgers,
    /// The host graph a resumed cell's root holds.
    graph: host::HostGraph,
    /// The effect identity each call a resumed cell holds open was
    /// performed at.
    open_sites: BTreeMap<lash_core::ToolCallId, lash_vocabulary::sites::EffectId>,
    resumed: bool,
}

/// A cell that did not get as far as running.
enum Refused {
    /// The cell's answer.
    Cell(Box<lash_core::CellFailure>),
    /// A nested effect of the cell's setup failed: recorded for the turn.
    Nested(Box<lash_core::RuntimeEffectControllerError>),
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
    state.page_clock(Arc::clone(ctx.actor_context().clock()));
    state.note_pages(ctx.session_pages());
    // The wall time the cell held workers before this activation counts
    // against its cap (FIG-6786).
    let held = resumed.as_ref().map_or(std::time::Duration::ZERO, |cell| {
        std::time::Duration::from_millis(cell.graph.state.held_ms)
    });
    let workers = services
        .workers
        .begin_execution(held, Arc::clone(ctx.actor_context().clock()));
    let services = CellServices {
        workers,
        ..services.clone()
    };
    let response = Box::pin(run_linked_cell(
        state, ctx, code, &services, identities, prints, snapshots, resumed,
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

    identities: lash_vm_broker::CodeCallIdentities,
    prints: Arc<Mutex<Vec<Datum>>>,
    snapshots: &Arc<lash_vm_broker::DurableSnapshotStore>,
    resumed: Option<envelope::ResumedCell>,
) -> ExecResponse {
    let linked = match link::link_cell(state, &ctx, code, services, snapshots, resumed).await {
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
        Err(Refused::Worker(error)) => return worker_setup_failure(state, &ctx, *error),
    };
    let execution = link::execution_id(snapshots.exec());
    let mut pins = BTreeMap::new();
    let resolutions = ctx
        .actor_context()
        .backend()
        .stores()
        .execution_resolutions();
    for use_ in linked.envelope.environment.uses.keys() {
        match resolutions
            .load(&lash_core_store::resolutions::ResolutionKey {
                execution: execution.clone(),
                use_: *use_,
            })
            .await
        {
            Ok(Some(record)) => {
                pins.insert(*use_, record.entry);
            }
            Ok(None) => {}
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
        }
    }
    let ctx = ctx.with_linked_execution(lash_core::LinkedExecution {
        execution: execution.clone(),
        environment: Arc::new(linked.envelope.environment.clone()),
        resolutions: Arc::new(Mutex::new(pins)),
    });

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
        invoked,
        envelope,
        document,
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
        .map_or(lash_vocabulary::observation::RunObservation::Off, |spec| {
            spec.observe
        });
    let trace = trace::CellTrace::new(&ctx, envelope.link, &envelope.dialect, segment);
    if let Some(trace) = &trace {
        trace.started();
    }
    let mut routes = BTreeMap::new();
    for (use_, binding) in &envelope.environment.uses {
        if invoked.contains(use_)
            && let lash_core_store::tool_surface::UseBinding::Fixed(operation) = binding
            && operation.offer.mode == lash_vocabulary::package::OperationMode::Read
        {
            match ctx.operation_read_route(operation) {
                Ok(route) => {
                    routes.insert(*use_, route);
                }
                Err(error) => {
                    let message = error.to_string();
                    ctx.record_nested_effect_error(error);
                    return exec_setup_failure_or_stop(
                        state,
                        &ctx,
                        lash_core::CellFailure::new(lash_core::CellFailureKind::Host, message),
                    );
                }
            }
        }
    }
    let modes = envelope
        .environment
        .uses
        .keys()
        .map(|use_| {
            (
                *use_,
                if invoked.contains(use_) {
                    envelope
                        .environment
                        .mode(use_)
                        .unwrap_or(lash_vocabulary::package::OperationMode::Waited)
                } else {
                    lash_vocabulary::package::OperationMode::Waited
                },
            )
        })
        .collect();
    let ledgers = Arc::new(Mutex::new(ledgers));
    let plans_reads = !routes.is_empty();
    let cell_reads = Arc::new(reads::CellReads {
        clock: Arc::clone(ctx.actor_context().clock()),
        routes,
        history: ctx.read_history(),
        config: ctx.owner_config().clone(),
        cancel: ctx.cancellation(),
        parent_wait: services.workers.config().tuning.parent_wait,
        ledgers: Arc::clone(&ledgers),
        limit: ctx.max_tool_calls(),
        cell: "cell".to_owned(),
    });
    let host = CellHost {
        plans_reads,
        trace: trace.clone(),
        sites: Mutex::new(open_sites),
        ctx: ctx.clone(),
        execution,
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
    let counts_trace = trace.clone();
    let run_host = Arc::new(lash_vm_runtime::ParentHost {
        observation,
        ran: Arc::new(move |delta| {
            if let Some(trace) = &counts_trace {
                trace.ran(delta);
            }
        }),
        store: Arc::clone(snapshots),
        printed: Arc::new(move |value| printed.lock_recover().push(value)),
        reads: cell_reads,
    });
    let bounds = cell_bounds(services);
    let save = lash_vm_client::wire::SaveWire {
        saved_written: host.envelope.saved_written.clone(),
        controls: host
            .envelope
            .environment
            .uses
            .iter()
            .filter_map(|(use_, binding)| match binding {
                lash_core_store::tool_surface::UseBinding::Fixed(operation) => Some((
                    *use_,
                    lash_vm_runtime::effect_controls(&operation.offer.controls),
                )),
                _ => None,
            })
            .collect(),
        written: host.envelope.written.clone(),
    };
    // The cell's roots pin its document's execution closure, and a saved
    // function's record names it: a capture stages it with them.
    // A resumed cell runs the definitions it was written against, the
    // store's where this node lacks them (FIG-6985).
    let start = lash_vocabulary::names::LinkedStart {
        link: host.envelope.link,
        selector: lash_vocabulary::names::StartSelector::Main,
    };
    let helpers = match services.workers.config().worker_library() {
        Ok(library) => Arc::clone(library.helpers()),
        Err(error) => {
            return exec_setup_failure_or_stop(
                state,
                &ctx,
                lash_core::CellFailure::new(lash_core::CellFailureKind::Host, error.to_string()),
            );
        }
    };
    let loaded = match state.documents.as_ref() {
        Some(documents) => {
            processes::load(documents, &start, &host.envelope.environment.authored).await
        }
        None => Err("the cell has no document owner".into()),
    };
    let loaded = match loaded {
        Ok(loaded) => loaded,
        Err(message) => {
            return exec_setup_failure_or_stop(
                state,
                &ctx,
                lash_core::CellFailure::new(lash_core::CellFailureKind::Host, message),
            );
        }
    };
    let selection = match cell_helper_selection(snapshots, &loaded.content, Arc::clone(&helpers))
        .await
    {
        Ok(selection) => selection,
        Err(error) => {
            if let KernelFailure::Worker {
                outcome:
                    lash_protocol_types::engine_fault::InfrastructureOutcome::RunRefused { refusal },
            } = &error
            {
                let mut typed = lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::VmWorkerFailed,
                    error.to_string(),
                );
                typed.cause = Some(lash_core::RuntimeErrorCause::CellSnapshotUndecodable {
                    refusal: Box::new(refusal.clone()),
                });
                ctx.record_nested_effect_error(
                    lash_core::RuntimeEffectControllerError::from(typed)
                        .retryable_uncommitted_derivation(),
                );
            }
            return exec_setup_failure_or_stop(
                state,
                &ctx,
                lash_core::CellFailure::new(lash_core::CellFailureKind::Host, error.to_string()),
            );
        }
    };
    let prepared = match (services.kernel.library(), state.documents.as_ref()) {
        (Ok(functions), Some(documents)) => processes::prepare(
            loaded,
            &start,
            &functions,
            &host.envelope.environment.authored,
            documents,
            &selection,
        ),
        (Err(error), _) => Err(error.to_string()),
        (Ok(_), None) => Err("the cell has no document owner".into()),
    };
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(message) => {
            return exec_setup_failure_or_stop(
                state,
                &ctx,
                lash_core::CellFailure::new(lash_core::CellFailureKind::Host, message),
            );
        }
    };
    let closure = prepared.loaded.closure;
    let shared = prepared
        .documents
        .iter()
        .map(|(id, document)| (*id, Arc::clone(document.shared())))
        .collect();
    let instances =
        match lash_kernel_vm::Program::prepare_instances(&shared, &prepared.content.links) {
            Ok(instances) => instances,
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
    let carried = state.carried_closures();
    state.runs_under(&closure);
    let machines = match lash_vm_client::RemoteMachines::new(
        services.workers.clone(),
        owner,
        lash_protocol_types::engine_fault::OwnerEpoch(0),
        lash_protocol_types::engine_fault::FrameEpoch(0),
        run_host,
        &document,
        &dialect,
        lash_vm_client::RunStart {
            link: start.link,
            instances,
            documents: prepared.documents,
            target: Target::Main,
            args: Vec::new(),
            session: state.session(),
            host: Vec::new(),
            modes,
            save: Some(save),
        },
        bounds,
        closure,
    ) {
        Ok(machines) => {
            let machines = machines.with_carried(carried);
            match selection {
                lash_kernel_doc::Selection::Fresh => machines,
                lash_kernel_doc::Selection::Restored(selection) => {
                    machines.with_restore_selection(selection)
                }
            }
        }
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
    let mut staged = match &run {
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
    if matches!(staged, Some(Ok(_))) {
        // A saved definition outlives the journal of the cell that made it.
        // Transfer its artifact lifetime to the frame before that journal ends;
        // FIG-6926 still fulfils a fresh Link at every subsequent cell start.
        let held = async {
            let claim = ctx.frame_claim().map_err(|error| error.to_string())?;
            let documents = state
                .documents
                .as_ref()
                .ok_or("saved code has no document owner")?;
            let links: BTreeSet<_> = state
                .lowering_functions()
                .values()
                .map(|function| function.link)
                .collect();
            for link in links {
                documents
                    .hold(
                        &claim,
                        &lash_execution_contracts::process::linking::ContentRef::Link(link),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
            }
            Ok::<(), String>(())
        }
        .await;
        if let Err(message) = held {
            ctx.record_nested_effect_error(
                lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::ExecutionStateCaptureFailed,
                    message.clone(),
                )
                .retryable_uncommitted_derivation(),
            );
            staged = Some(Err(message));
        }
    }
    let held = staged.is_some()
        || match &run {
            Ok(KernelEnd::Ended {
                pages: Some(pages), ..
            }) => {
                if let Some(store) = store.clone() {
                    // Recording a failed cell can create and publish the
                    // first session descriptor too. Retain its activation
                    // before that publication, until a capture owns it.
                    state.hold_run(pages::HeldRun::new(Arc::clone(pages), store));
                    true
                } else {
                    false
                }
            }
            _ => false,
        };
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
        // A turn-stopping call cancellation ends the cell, whatever its
        // program made of the error; Closing a race loser does not.
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
                    .then(|| collected.settled_control())
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
                respond(lash_core::CellOutcome::Failed(
                    observation.failure_with_repairs(
                        code,
                        source_map.as_ref().filter(|map| {
                            map.validate_identified(&host.envelope.document, code)
                                .is_ok()
                        }),
                        services.channel,
                        services.prompts.as_ref(),
                        state
                            .shown_packages()
                            .as_ref()
                            .map(|packages| &packages.snapshot),
                        &ctx,
                    ),
                ))
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
            .map_err(KernelFailure::from),
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
    // Only the pass that answers the cell publishes its end. A pass that
    // aborts, as one whose worker was lost from outside does, journals no
    // response: the cell has not ended, and the pass that runs it again
    // publishes its end under the same key (FIG-6891).
    if let Some(trace) = &host.trace
        && !response.suspended
        && !ctx.has_nested_effect_error()
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
    document: lash_vocabulary::names::DocumentId,
    ledger: lash_vm_broker::kernel::EffectLedger,
    end: End,
    pages: &lash_vm_broker::kernel::pages::SharedPages,
) -> Result<lash_core::EndedCell, KernelFailure> {
    // Runtime-settled cancellations have no standalone call material.
    // Record their host ledger before the turn consumes the cell, so its
    // durable report retains the cancelled losers as well as the winner.
    let cancelled_call = host.ledgers().tool_call_records().iter().any(|call| {
        matches!(
            call.output.outcome,
            lash_core::ToolCallOutcome::Cancelled(_)
        )
    });
    if host.ctx.holds_cell_end()
        && !cancelled_call
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
    document: lash_vocabulary::names::DocumentId,
    ledger: lash_vm_broker::kernel::EffectLedger,
    end: End,
    pages: &lash_vm_broker::kernel::pages::SharedPages,
) -> Result<lash_core_store::checkpoint_graph::NodeId, KernelFailure> {
    let session = state.end_descriptor().map_err(|error| {
        KernelFailure::Checkpoint(lash_vm_broker::QuietPointRefusal::Refused(
            error.to_string(),
        ))
    })?;
    let carried = state.carried_closures();
    snapshots
        .commit_run_end(
            lash_vm_broker::kernel::EndSave {
                document,
                ledger,
                end,
                session,
                pages,
                carried: &carried,
                with: Vec::new(),
            },
            host,
        )
        .await
        .map(|saved| saved.root)
        .map_err(KernelFailure::from)
}

/// Check the authenticated parked root before any helper admission or assembly.
async fn cell_helper_selection(
    snapshots: &lash_vm_broker::DurableSnapshotStore,
    content: &lash_kernel_package::LinkedContent,
    helpers: Arc<lash_kernel_doc::BuildHelpers>,
) -> Result<lash_kernel_doc::Selection, KernelFailure> {
    let Some(read) = snapshots
        .latest_root()
        .await
        .map_err(KernelFailure::Checkpoint)?
    else {
        return Ok(lash_kernel_doc::Selection::Fresh);
    };
    if read.body.status != lash_core_store::checkpoint_graph::bodies::ExecStatus::Parked {
        return Ok(lash_kernel_doc::Selection::Fresh);
    }
    let cx = snapshots.context();
    let store = cx
        .pages()
        .map_err(|error| KernelFailure::Parent(lash_vm_broker::ParentFault(error.to_string())))?;
    let binding = lash_vm_broker::kernel::graph::owner_binding_node(snapshots.exec())
        .map_err(|error| KernelFailure::State(error.into()))?;
    lash_vm_runtime::process::check_parked_helpers(
        store,
        lash_durable::domain::pages::RootLeaseRequest {
            owner: lash_durable::domain::graph::RootOwner::Exec(snapshots.exec().clone()),
            expected: read.held.root,
            fence: lash_durable::domain::pages::LeaseFence::Actor {
                actor: cx.actor().clone(),
                epoch: cx.epoch(),
            },
            ttl_millis: i64::try_from(pages::LEASE_TTL.as_millis()).unwrap_or(i64::MAX),
        },
        read.body.state,
        binding,
        content,
        helpers,
    )
    .await
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
        KernelFailure::Durable(error) => {
            let fault = match error {
                lash_durable::DurableError::RevisionConflict {
                    actor,
                    epoch,
                    expected,
                    actual,
                } => lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::OwnerRevisionConflict,
                    message.clone(),
                )
                .with_cause(lash_core::RuntimeErrorCause::OwnerRevisionConflict(
                    Box::new(lash_core::OwnerRevisionConflict {
                        actor: actor.as_str().to_owned(),
                        epoch: epoch.0,
                        expected: expected.0,
                        actual: actual.0,
                    }),
                )),
                lash_durable::DurableError::OwnershipLost(_) => lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::SessionExecutionLeaseLost,
                    message.clone(),
                ),
                _ => lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::RuntimeStore,
                    message.clone(),
                ),
            };
            ctx.record_nested_effect_error(
                lash_core::RuntimeEffectControllerError::from(fault)
                    .retryable_uncommitted_derivation(),
            );
            if ctx.is_cancelled() {
                state.rollback_code_execution();
            }
            host(message)
        }
        KernelFailure::Worker { outcome } => {
            // A limit the run itself exhausted is the cell's result.
            if let lash_protocol_types::engine_fault::InfrastructureOutcome::WorkerLimitExceeded {
                limit,
            } = &outcome
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
            if let lash_protocol_types::engine_fault::InfrastructureOutcome::RunCrashedWorker {
                evidence,
            } = &outcome
                && let Some(failure) = died.answer(*evidence, &collected.executed_calls())
            {
                return exec_response_from(collected, lash_core::CellOutcome::Failed(failure));
            }
            // The state a resumed cell runs on from is refused: another
            // build wrote it. That is no failure the model sees: the cell
            // aborts with the typed refusal, and its turn parks on the
            // cell's checkpoint.
            if runs_on
                && let lash_protocol_types::engine_fault::InfrastructureOutcome::RunRefused {
                    refusal,
                } = &outcome
                && matches!(
                    refusal,
                    lash_protocol_types::engine_fault::RunRefusal::Restore { .. }
                )
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
        lash_protocol_types::engine_fault::InfrastructureOutcome::WorkerLimitExceeded { limit },
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

#[cfg(test)]
mod tests;
