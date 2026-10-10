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
mod envelope;
mod host;
mod processes;
mod session;
mod snapshot;
mod trace;

pub(crate) use carry::KernelCarry;
pub(crate) use envelope::{check_cell_snapshot, snapshot_tool_calls};
pub(crate) use host::site_label;
pub use host::{TOOL_ARGUMENTS, TOOL_CALL_LIMIT, TOOL_FAILED, UNKNOWN_EFFECT};
pub use session::RlmExecutionState;
pub use snapshot::{RLM_SNAPSHOT_VERSION, RlmSnapshotError};

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use lash_core::{ExecRequest, ExecResponse, RuntimeExecutionContext};
use lash_kernel_doc::{Datum, Document, Name};
use lash_kernel_vm::{Bindings, Bounds, End, Start, Target};
use lash_sansio::sync::MutexExt;
use lash_vm_broker::kernel::{KernelBroker, KernelCeilings, KernelEnd, KernelFailure};
use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
use lash_vm_client::service::{Request, Response};
use lash_vm_runtime::HostBoundary;

use self::envelope::CellEnvelope;
use self::host::{CellHost, CellHostLedgers};
use crate::cell_value::datum_json;
use crate::feedback::CellObservation;
use crate::projection::RlmProjectedBindings;

/// What a cell needs of the session that runs it, beside its state.
#[derive(Clone)]
pub(crate) struct CellServices {
    /// The workers that lower the cell and host its machine.
    pub workers: lash_vm_client::service::Service,
    pub deferred_tool_resolver: Option<crate::deferred::SharedDeferredToolResolver>,
    pub execution_bounds: crate::plugin::ExecutionBounds,
    /// The transport the program arrived on.
    pub channel: crate::plugin::RlmChannel,
    pub code_renderer: crate::render::CodeRendererSlot,
    /// How the session's dialect words what a model reads.
    pub prompts: Arc<dyn crate::dialect::DialectPrompts>,
}

/// Runs one cell of `state`'s session, or resumes it from its parked state.
pub(crate) async fn execute_cell(
    state: &mut RlmExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    request: ExecRequest,
    services: &CellServices,
    session_projected_bindings: RlmProjectedBindings,
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
            .map(|resumed| resumed.state.prints.clone())
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
    document: Document,
    boundary: HostBoundary,
    /// The session's bindings and the host's read-only ones, as `main`
    /// starts with them.
    start: Bindings,
    ledgers: CellHostLedgers,
    /// The requests of the calls a resumed cell's ledger holds open.
    open_calls: Vec<lash_vm_protocol::EncodedPayload>,
    /// The effect identity each of those calls was performed at.
    open_sites: BTreeMap<lash_core::ToolCallId, lash_kernel_doc::EffectIdentity>,
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

/// Lowers a fresh cell against what the session offers now, or takes a
/// resumed cell as its first activation recorded it.
async fn link_cell(
    state: &RlmExecutionState,
    ctx: &RuntimeExecutionContext<'_>,
    code: &str,
    services: &CellServices,
    session_projected_bindings: RlmProjectedBindings,
    resumed: Option<envelope::ResumedCell>,
) -> Result<LinkedCell, Refused> {
    if let Some(resumed) = resumed {
        // A resumed cell runs what its first activation admitted: the
        // envelope carries the document and everything it was lowered
        // against, and nothing here is journaled or lowered again.
        resumed.state.restore_context(ctx);
        let envelope = resumed.state.cell;
        if envelope.dialect != state.dialect() {
            return Err(host_failure(format!(
                "the cell was parked as `{}` and this session's dialect is `{}`",
                envelope.dialect,
                state.dialect()
            )));
        }
        return Ok(LinkedCell {
            document: envelope.document().map_err(host_failure)?,
            boundary: envelope.boundary().map_err(host_failure)?,
            envelope,
            // The machine resumes from its parked state, not from a start.
            start: Bindings::default(),
            ledgers: resumed.state.host,
            open_calls: resumed.open_calls,
            open_sites: resumed.open_sites,
            resumed: true,
        });
    }

    // The host's read-only bindings are recorded under the cell's effect
    // before it runs, so a redrive of the cell starts with the same ones.
    let projected = {
        let _phase = ctx.named_phase("rlm_lash_vm.resolve_projected_bindings");
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
    for tool in &ctx.tool_catalog().tools {
        let surface = |error: String| host_failure(format!("invalid host tool surface: {error}"));
        let binding = lash_vm_runtime::required_tool_executable(&tool.manifest)
            .map_err(|error| surface(error.to_string()))?;
        let path = services
            .prompts
            .tool_call_path(&binding)
            .map_err(|refusal| surface(refusal.to_string()))?;
        boundary
            .offer_tool(
                &path,
                tool.manifest.id.clone(),
                tool.contract.input_schema.canonical(),
                tool.contract.output_schema.canonical(),
            )
            .map_err(|error| surface(error.to_string()))?;
    }

    // gather → journal → offer: the call paths the source writes that no
    // catalog tool answers are decided durably before the cell is lowered,
    // and each granted tool is offered as an effect.
    let grants = match &services.deferred_tool_resolver {
        Some(_) if ctx.parent_invocation().is_some() => {
            let _phase = ctx.named_phase("rlm_lash_vm.deferred_resolve");
            let offered = boundary.signatures();
            let candidates = crate::deferred::called_paths(code)
                .into_iter()
                .filter(|path| {
                    !lash_kernel_doc::EffectName::new(path.as_str())
                        .is_ok_and(|name| offered.contains_key(&name))
                })
                .collect::<BTreeSet<_>>();
            if candidates.is_empty() {
                BTreeMap::new()
            } else {
                let resolver = services.deferred_tool_resolver.as_ref();
                let outcomes =
                    crate::deferred::journal_deferred_outcomes(candidates, resolver, ctx)
                        .await
                        .map_err(|error| Refused::Nested(Box::new(error.runtime_effect_error())))?;
                crate::deferred::offer_deferred_grants(&mut boundary, &outcomes, resolver, ctx)
                    .map_err(|error| Refused::Nested(Box::new(error.runtime_effect_error())))?
            }
        }
        _ => BTreeMap::new(),
    };

    let session = state.bindings();
    let mut names = session.names();
    names.extend(projected.keys().map(|name| Name::new(name.as_str())));
    let lowered = services
        .workers
        .request_accounted(Request::Lower {
            dialect: state.dialect().to_owned(),
            source: code.to_owned(),
            effects: boundary.signatures(),
            bindings: names,
            functions: session.functions(),
        })
        .await;
    let (document, annotations) = match lowered {
        Ok(Response::Lowered {
            document,
            annotations,
        }) => (document, annotations),
        Ok(Response::DialectRefused(refusal)) => {
            let observation = CellObservation::of_refusal(refusal);
            emit_step_trace(ctx, Err(&observation.code()));
            return Err(Refused::Cell(Box::new(observation.failure(
                code,
                None,
                services.channel,
                services.prompts.as_ref(),
            ))));
        }
        Ok(Response::UnknownDialect { dialect }) => {
            return Err(host_failure(format!(
                "this session's cells are written in `{dialect}`, and no worker has that dialect installed"
            )));
        }
        Ok(Response::Printed { .. }) => {
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
    let text = |bytes: Vec<u8>, what: &str| {
        String::from_utf8(bytes)
            .map_err(|error| host_failure(format!("the worker's {what} is not text: {error}")))
    };
    let envelope = CellEnvelope {
        code: CellEnvelope::code_digest(code),
        dialect: state.dialect().to_owned(),
        document: text(document, "document")?,
        annotations: text(annotations, "annotations")?,
        effects: CellEnvelope::record_effects(&boundary),
        grants,
        projected: projected.keys().cloned().collect(),
    };
    let document = envelope
        .document()
        .map_err(|error| host_failure(format!("the worker's document is not one: {error}")))?;

    // `main` starts with the session's bindings and, over them, the host's
    // read-only ones.
    let mut start = session.start();
    let mut next = start
        .objects
        .keys()
        .next_back()
        .map_or(0, |id| id.0.saturating_add(1));
    let mut allocate = || {
        let id = lash_kernel_doc::ObjectId(next);
        next += 1;
        id
    };
    for (name, value) in &projected {
        let value = crate::cell_value::bind_json(
            value,
            document.manifest.numbers,
            &mut allocate,
            &mut start.objects,
        );
        start.variables.insert(Name::new(name.as_str()), value);
    }
    Ok(LinkedCell {
        envelope,
        document,
        boundary,
        start,
        ledgers: CellHostLedgers::default(),
        open_calls: Vec::new(),
        open_sites: BTreeMap::new(),
        resumed: false,
    })
}

/// The bounds a cell's run is held to: the session's instruction and
/// memory bounds over the worker pool's own, which also state the rest.
fn cell_bounds(services: &CellServices) -> Bounds {
    let pool = services.workers.config().run_bounds;
    Bounds {
        charge: services
            .execution_bounds
            .instruction_limit
            .limit()
            .map_or(u64::MAX, std::num::NonZeroU64::get),
        memory: services
            .execution_bounds
            .memory_limit
            .limit()
            .map_or(u64::MAX, std::num::NonZeroU64::get),
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
    state: &mut RlmExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    code: &str,
    services: &CellServices,
    session_projected_bindings: RlmProjectedBindings,
    identities: lash_vm_broker::CodeCallIdentities,
    prints: Arc<Mutex<Vec<Datum>>>,
    snapshots: &Arc<lash_vm_broker::DurableSnapshotStore>,
    resumed: Option<envelope::ResumedCell>,
) -> ExecResponse {
    state.begin_code_execution();
    let workers = services.workers.begin_execution();
    let services = &CellServices {
        workers,
        ..services.clone()
    };
    let linked = match link_cell(
        state,
        &ctx,
        code,
        services,
        session_projected_bindings,
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
        Err(Refused::Worker(error)) => return worker_setup_failure(state, &ctx, *error),
    };

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
    for open in &linked.open_calls {
        match lash_core::tool_dispatch::CellMember::decode(&open.0) {
            // An open call this node cannot run is not taken up: the cell
            // stops unrecorded, and its turn waits for a node that can.
            Ok(member) => match members.require_capable(&member) {
                Ok(()) => members.register(member),
                Err(error) => {
                    let message = error.to_string();
                    ctx.record_nested_runtime_effect_error(error);
                    return exec_setup_failure_or_stop(
                        state,
                        &ctx,
                        lash_core::CellFailure::new(lash_core::CellFailureKind::Host, message),
                    );
                }
            },
            Err(error) => {
                return exec_setup_failure_or_stop(
                    state,
                    &ctx,
                    lash_core::CellFailure::new(
                        lash_core::CellFailureKind::Host,
                        format!("an open call of the cell cannot be read: {error}"),
                    ),
                );
            }
        }
    }
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
        "rlm:{}:{:?}",
        scope.session_id, scope.agent_frame_id
    ));
    let LinkedCell {
        envelope,
        document,
        boundary,
        start,
        ledgers,
        open_sites,
        resumed: runs_on,
        ..
    } = linked;
    let projected = envelope.projected.clone();
    let annotations =
        serde_json::from_str::<lash_kernel_doc::Annotations>(&envelope.annotations).ok();
    let document_identity = document.identity();
    let trace = document_identity
        .as_ref()
        .ok()
        .and_then(|identity| trace::CellTrace::new(&ctx, *identity, &envelope.dialect));
    if let Some(trace) = &trace {
        trace.started();
    }
    let entries = match &document_identity {
        Ok(identity) => processes::CellEntries {
            document: document.clone(),
            identity: *identity,
        },
        Err(error) => {
            return exec_setup_failure_or_stop(
                state,
                &ctx,
                lash_core::CellFailure::new(lash_core::CellFailureKind::Host, error.to_string()),
            );
        }
    };
    let published = if runs_on {
        Ok(())
    } else {
        match processes::retain_document(&ctx, &entries).await {
            Ok(()) => processes::publish_entries(&ctx, &entries).await,
            Err(message) => Err(message),
        }
    };
    if let Err(message) = published {
        return exec_setup_failure_or_stop(
            state,
            &ctx,
            lash_core::CellFailure::new(lash_core::CellFailureKind::Host, message),
        );
    }
    let host = CellHost {
        entries,
        trace,
        sites: Mutex::new(open_sites),
        ctx: ctx.clone(),
        boundary,
        grants: envelope.grants.clone(),
        members,
        opener: identities.opener().clone(),
        prints: Arc::clone(&prints),
        ledgers: Mutex::new(ledgers),
        envelope,
    };
    let printed = Arc::clone(&prints);
    let random = std::collections::hash_map::RandomState::new();
    let drawn = std::sync::atomic::AtomicU64::new(0);
    let run_host = Arc::new(lash_vm_runtime::ParentHost {
        store: Arc::clone(snapshots),
        random: Arc::new(move || {
            use std::hash::BuildHasher as _;
            random.hash_one(drawn.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
        }),
        providers: lash_vm_runtime::ProjectionCatalog::of_backend(
            ctx.projection_providers().as_deref(),
        ),
        printed: Arc::new(move |value| printed.lock_recover().push(value)),
    });
    let bounds = cell_bounds(services);
    let machines = match lash_vm_client::RemoteMachines::new(
        services.workers.clone(),
        owner,
        lash_vm_protocol::OwnerEpoch(0),
        lash_vm_protocol::FrameEpoch(0),
        run_host,
        &document,
        Start {
            target: Target::Main,
            args: Vec::new(),
            bindings: start,
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
    let pool = services.workers.config();
    let run = KernelBroker {
        store: snapshots.as_ref(),
        effects: &host,
        identities: &identities,
        ceilings: KernelCeilings {
            requests_per_park: pool.run_bounds.requests_per_park,
            join_members: pool.run_bounds.join_members,
        },
        slice: pool.tuning.slice,
    }
    .run(&machines, &ctx.cancellation())
    .await;

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
            Ok(KernelEnd::Ended(end)) => end,
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
                return broker_failure(state, &ctx, failure, runs_on, &collected);
            }
        };
        // A cancelled tool call ends the cell, whatever its program made of the
        // error; so does the turn's own cancel.
        if collected.call_cancelled || ctx.is_cancelled() || matches!(end, End::Cancelled) {
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
                let document = match document_identity {
                    Ok(document) => document,
                    Err(error) => {
                        return failed(lash_core::CellFailureKind::Host, error.to_string());
                    }
                };
                let mut left = finished.bindings;
                // A process the cell bound or answered with leaves it as its
                // definition.
                host.entries.bind_definitions(&mut left);
                let result = host
                    .entries
                    .leaving(&finished.result)
                    .unwrap_or(finished.result);
                // The host's read-only bindings are the host's: what the cell
                // made of its copy is not the session's.
                left.variables
                    .retain(|name, _| !projected.contains(name.as_str()));
                let bindings = state.settle_cell(session::CellLeft {
                    document: &host.entries.document,
                    identity: document,
                    annotations: annotations.as_ref(),
                    bindings: left,
                    not_carried: finished.not_carried,
                    closures: finished.closures,
                });
                // `main` that ran to its end without a `finish` answered
                // nothing: the cell completed. A `finish` gave the turn its
                // answer.
                ExecResponse {
                    bindings: Box::new(bindings),
                    ..respond(if finished.finish {
                        lash_core::CellOutcome::Finished(datum_json(&result).into())
                    } else {
                        lash_core::CellOutcome::Completed
                    })
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
                let observation =
                    CellObservation::of_run_error(error, state.bindings().not_carried());
                respond(lash_core::CellOutcome::Failed(observation.failure(
                    code,
                    annotations.as_ref(),
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
    let response = answer();
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

/// How a broker failure answers the cell. Nothing about the guest: the
/// cell's last checkpoint stands.
fn broker_failure(
    state: &mut RlmExecutionState,
    ctx: &RuntimeExecutionContext<'_>,
    failure: KernelFailure,
    runs_on: bool,
    collected: &CellHostLedgers,
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
                    lash_core::CellOutcome::Failed(
                        lash_core::CellFailure::new(
                            lash_core::CellFailureKind::Program,
                            limit.to_string(),
                        )
                        .with_worker_limit(*limit),
                    ),
                );
            }
            // The state a resumed cell runs on from is refused: another
            // build wrote it. That is no failure the model sees: the cell
            // aborts with the typed refusal, and its turn parks on the
            // cell's checkpoint.
            if runs_on
                && let lash_vm_protocol::InfrastructureOutcome::RunRefused { refusal } = &outcome
                && matches!(refusal, lash_vm_protocol::RunRefusal::State { .. })
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
        KernelFailure::Import(_) | KernelFailure::EmptyCheckpoint if runs_on => {
            ctx.record_nested_effect_error(
                lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::ExecutionStateCaptureFailed,
                    message.clone(),
                )
                .retryable_uncommitted_derivation(),
            );
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
    state: &mut RlmExecutionState,
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
            lash_core::CellFailure::new(lash_core::CellFailureKind::Program, limit.to_string()),
        );
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
            Ok(()) => lash_trace::TraceProgramStepOutcome::Ok,
            Err(diagnostic) => lash_trace::TraceProgramStepOutcome::Failure {
                diagnostic: tracing
                    .trace_runtime()
                    .limits()
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

/// Feature-gated fixture that lets the repository's performance harness
/// shift the production RLM execution-state capture without exposing
/// executor internals as public protocol API.
#[cfg(feature = "testing")]
pub struct RlmCheckpointPerfFixture {
    services: CellServices,
    state: RlmExecutionState,
    binding_count: usize,
    payload_bytes: usize,
}

#[cfg(feature = "testing")]
impl RlmCheckpointPerfFixture {
    /// A fixture whose session holds `binding_count` bindings of
    /// `payload_bytes` each.
    pub async fn new(
        dialect: &crate::CellDialect,
        workers: lash_vm_client::service::Service,
        binding_count: usize,
        payload_bytes: usize,
    ) -> Result<Self, lash_core::SessionError> {
        let mut state = RlmExecutionState::new(dialect.name(), dialect.numbers());
        let mut patch = lash_rlm_types::RlmGlobalsPatchPluginBody::default();
        for index in 0..binding_count {
            patch.set_default.insert(
                format!("mid_{index}"),
                serde_json::json!([format!("binding-{index}-{}", "x".repeat(payload_bytes))]),
            );
        }
        state.patch_globals(&patch, &BTreeSet::new()).await?;
        Ok(Self {
            services: CellServices {
                workers,
                deferred_tool_resolver: None,
                execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
                channel: crate::plugin::RlmChannel::Cell,
                code_renderer: crate::render::CodeRendererSlot::default(),
                prompts: dialect.prompts(),
            },
            state,
            binding_count,
            payload_bytes,
        })
    }

    pub async fn capture(
        &mut self,
    ) -> Result<lash_core::plugin::ExecutionStateCapture, lash_core::SessionError> {
        self.state
            .snapshot_execution_state(lash_core::FleetFormat::current())
            .await
    }

    pub fn acknowledge_capture(&mut self) {
        self.state.acknowledge_execution_state_capture();
    }

    /// Run the edit under the backend's scoped controller and this cell's
    /// invocation: one binding is rebound per turn, at the same order of
    /// bytes.
    pub async fn assign_one(
        &mut self,
        index: usize,
        turn: usize,
        ctx: RuntimeExecutionContext<'_>,
    ) -> Result<(), lash_core::SessionError> {
        let binding = index % self.binding_count.max(1);
        let code = format!(
            "mid_{binding} = [\"binding-{binding}-{}\", \"turn-{turn}-{}\"];",
            "x".repeat(self.payload_bytes),
            "y".repeat(self.payload_bytes / 8)
        );
        let response = execute_cell(
            &mut self.state,
            ctx,
            ExecRequest { code },
            &self.services,
            RlmProjectedBindings::default(),
        )
        .await;
        self.state.accept_code_execution();
        if let Some(error) = response.error() {
            return Err(lash_core::SessionError::Protocol(format!(
                "RLM checkpoint perf assignment failed: {}",
                error.message,
            )));
        }
        Ok(())
    }

    pub async fn restore(
        dialect: &crate::CellDialect,
        state: &lash_core::plugin::HydratedExecutionState,
    ) -> Result<(), lash_core::SessionError> {
        let mut restored = RlmExecutionState::new(dialect.name(), dialect.numbers());
        restored
            .restore_execution_state(state, lash_core::FleetFormat::current())
            .await
            .map_err(lash_core::SessionError::from)
    }
}

#[cfg(test)]
mod tests;
