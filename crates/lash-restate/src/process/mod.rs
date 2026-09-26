//! Process ownership at the Restate tier.
//!
//! One responsibility: everything that decides *who* runs a Lash process
//! segment and how its terminal is delivered — the runner seam a host
//! implements, the ingress driver that submits pending rows, the deployment
//! wiring that binds both, and the durable promise/workflow-key vocabulary the
//! workflow and the ingress side must agree on. The workflow handler itself
//! lives in [`workflow`].

use lash_sansio::ProcessId;
mod admission;
mod park_reconcile;
mod workflow;

mod stamped_requests;
pub use admission::{JOURNAL_LOGIC_EPOCH, RESTATE_PROCESS_JOURNAL_VERSION, SegmentStarted};
pub(crate) use admission::{SegmentAdmission, admit_segment, handover_digest};
pub use park_reconcile::{
    ProcessParkReconcileReport, reconcile_process_parks, resume_parked_process,
};
pub(crate) use stamped_requests::attach::StampedAttachRequest;

use std::sync::Arc;

use lash_core::{
    AbandonEvidence, AbandonWriter, AwaitEventKey, AwaitEventWaitIdentity, ExecutionScope,
    PluginError, ProcessAwaitOutput, ProcessCompletionAuthority, ProcessExecutionContext,
    ProcessExternalRef, ProcessRecord, ProcessRegistration, ProcessRegistry, ProcessStatus,
    ProcessTerminalWait, ProcessWorkSubstrate, ProcessWorkWiring, RecoveryContract, Resolution,
    RuntimeError, RuntimeErrorCode, ScopedEffectController,
    facade_support::ProcessAdmissionDeferred, facade_support::ProcessAdmissionReport,
    facade_support::ProcessEventSink, facade_support::ProcessRecoveryAttemptOutcome,
    facade_support::ProcessRecoveryOperation, facade_support::ProcessWorkerFault,
    facade_support::watch_process_registry_with_sink,
};
use lash_core_worker::DurableProcessWorker;
use restate_sdk::context::ContextPromises;
use restate_sdk::errors::{HandlerError, HandlerResult, TerminalError};
use serde::Serialize;

use crate::durable_wait::restate_await_event_key_for_authority;
use crate::ingress::{RestateConnection, RestateIngressClient};

#[cfg(test)]
pub(crate) use workflow::complete_process_outcome;
pub(crate) use workflow::{
    LashProcessWorkflow, LashProcessWorkflowClient, LashProcessWorkflowImpl,
};

/// Attempts a process segment's `run` invocation makes before it pauses, by
/// default: the same bound a turn handler has
/// ([`TURN_HANDLER_MAX_ATTEMPTS`](crate::TURN_HANDLER_MAX_ATTEMPTS)). A
/// deployment sets its own with [`RestateProcessServing::with_retry_max_attempts`].
pub const PROCESS_HANDLER_MAX_ATTEMPTS: u64 = crate::TURN_HANDLER_MAX_ATTEMPTS;

pub(crate) const PROCESS_CANCEL_PROMISE_KEY: &str = "process_cancel_requested";
/// Wall-clock epoch milliseconds for terminal evidence written at the Restate
/// tier (ADR 0019 recovery enforcement). The Restate boundary carries no
/// injected Lash clock — its durability comes from the engine and workflow-key
/// coalescing rather than a Lash lease — so it reads the system clock directly,
/// and only inside a journaled step or before the handler's first command
/// (FIG-3673): the stamp a step journals is the one every redrive publishes.
fn restate_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Restate's single-writer discipline is per-`process_id` workflow-key coalescing, not a Lash
/// lease (ADR 0027); the workflow key is that `process_id`.
pub(crate) fn workflow_key_authority(process_id: &ProcessId) -> ProcessCompletionAuthority {
    ProcessCompletionAuthority::WorkflowKey {
        workflow_key: process_id.to_string(),
    }
}

pub(crate) fn process_segment_workflow_key(process_id: &ProcessId, segment_ordinal: u64) -> String {
    if segment_ordinal == 0 {
        process_id.to_string()
    } else {
        format!("{process_id}#{segment_ordinal}")
    }
}

pub(crate) fn terminal_completion_workflow_key(
    process_id: &ProcessId,
    segment_ordinal: u64,
) -> Option<String> {
    (segment_ordinal > 0).then(|| process_id.to_string())
}

/// Converts Lash failures at the Restate process-handler boundary without
/// inheriting the SDK's blanket retry policy for arbitrary Rust errors.
///
/// Lash's runtime classification is the authority: explicitly retryable
/// runtime errors request redelivery, while every other plugin/runtime failure
/// terminates the invocation so deterministic failures cannot loop forever.
pub(crate) fn handler_error_from_plugin(error: PluginError) -> HandlerError {
    if error.is_retryable() {
        HandlerError::from(error)
    } else {
        HandlerError::from(TerminalError::from_error(error))
    }
}

fn is_replay_mismatch(error: &PluginError) -> bool {
    match error {
        PluginError::Runtime(error) => error.code.is_replay_mismatch(),
        PluginError::RuntimeEffectController(error) => error.code.is_replay_mismatch(),
        _ => false,
    }
}

fn terminal_process_output(error: PluginError) -> ProcessAwaitOutput {
    let error = lash_core::RuntimeEffectControllerError::from(error);
    ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::failure(
        lash_core::ToolFailure::runtime(
            lash_core::ToolFailureClass::Execution,
            error.code.as_str(),
            error.message,
        ),
    ))
}

/// A 404 here is a deployment that never bound the process workflow, not a busy engine:
/// terminal by construction, because retrying cannot make an unbound service appear
/// (FIG-1579).
/// Every other failure stays in the retryable ingress class.
pub(crate) fn process_ingress_submit_error(
    process_id: &ProcessId,
    err: crate::RestateHttpError,
) -> PluginError {
    if err.is_service_unregistered() {
        PluginError::Runtime(RuntimeError::new(
            RuntimeErrorCode::EngineServiceUnregistered,
            crate::ingress::unregistered_service_message(
                crate::LashService::ProcessWorkflow.name(),
                "run",
                &err,
            ),
        ))
    } else {
        PluginError::Runtime(RuntimeError::new(
            RuntimeErrorCode::EngineProcessIngressSubmit,
            format!("ingress submit for process `{process_id}` failed: {err}"),
        ))
    }
}

pub(crate) fn boundary_must_be_declined(record: Option<&ProcessRecord>) -> bool {
    record.is_some_and(|record| record.wait.is_some())
}

pub(crate) fn restate_process_terminal_await_key(
    authority_id: &crate::RestateAuthorityId,
    process_id: &ProcessId,
) -> Result<AwaitEventKey, RuntimeError> {
    restate_await_event_key_for_authority(
        authority_id,
        &ExecutionScope::process(process_id.clone()),
        AwaitEventWaitIdentity::Custom {
            key: "process_terminal".to_string(),
        },
    )
}

pub(crate) fn restate_process_terminal_resolution(
    output: &ProcessAwaitOutput,
) -> Result<Resolution, RuntimeError> {
    serde_json::to_value(output)
        .map(Resolution::Ok)
        .map_err(|err| {
            RuntimeError::new(
                lash_core::RuntimeErrorCode::EngineProcessTerminalEncode,
                err.to_string(),
            )
        })
}

pub(crate) fn restate_process_terminal_output(
    process_id: &ProcessId,
    resolution: Resolution,
) -> Result<ProcessAwaitOutput, PluginError> {
    match resolution {
        Resolution::Ok(value) => serde_json::from_value(value).map_err(|err| {
            PluginError::Session(format!(
                "invalid terminal output for process `{process_id}`: {err}"
            ))
        }),
        Resolution::Err(err) => Ok(ProcessAwaitOutput::from_tool_output(
            lash_core::ToolCallOutput::failure(lash_core::ToolFailure::runtime(
                lash_core::ToolFailureClass::Execution,
                err.code.namespaced(),
                err.message,
            )),
        )),
        Resolution::Timeout => Ok(ProcessAwaitOutput::from_tool_output(
            lash_core::ToolCallOutput::failure(lash_core::ToolFailure::runtime(
                lash_core::ToolFailureClass::Execution,
                "process_await_timeout",
                format!("awaiting process `{process_id}` timed out"),
            )),
        )),
        Resolution::Cancelled => Ok(ProcessAwaitOutput::from_tool_output(
            lash_core::ToolCallOutput::failure(lash_core::ToolFailure::runtime(
                lash_core::ToolFailureClass::Execution,
                "process_await_cancelled",
                format!("awaiting process `{process_id}` was cancelled"),
            )),
        )),
    }
}

fn resolve_process_terminal_promise<'ctx, C>(
    context: &C,
    authority_id: &crate::RestateAuthorityId,
    process_id: &ProcessId,
    output: &ProcessAwaitOutput,
) -> HandlerResult<()>
where
    C: ContextPromises<'ctx>,
{
    let key = restate_process_terminal_await_key(authority_id, process_id)
        .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
    let resolution = restate_process_terminal_resolution(output)
        .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
    let payload = serde_json::to_string(&resolution)
        .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
    context.resolve_promise(&key.promise_key(), payload);
    Ok(())
}

/// Decodes one process cancellation promise payload into the wake verdict a
/// journaled peek records (FIG-3149).
///
/// An unresolved promise and a retired segment observer both read as "not
/// cancelled"; only an accepted cancel request settles the sleep.
pub(crate) fn process_cancel_promise_verdict(payload: Option<String>) -> bool {
    payload
        .and_then(|payload| serde_json::from_str::<RestateProcessCancelSignal>(&payload).ok())
        .is_some_and(|signal| signal == RestateProcessCancelSignal::CancelRequested)
}

fn resolve_process_cancel_signal<'ctx, C>(
    context: &C,
    signal: RestateProcessCancelSignal,
) -> HandlerResult<()>
where
    C: ContextPromises<'ctx>,
{
    let payload = serde_json::to_string(&signal)
        .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
    context.resolve_promise(PROCESS_CANCEL_PROMISE_KEY, payload);
    Ok(())
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(try_from = "serde_json::Value")]
pub struct RestateProcessCancelRequest {
    pub process_id: ProcessId,
    pub request: lash_core::CancelRequest,
    /// The generation of the `cancel` and `deliver_cancel` handlers' journaled
    /// commands the sender built this request for (FIG-3673): the request is
    /// refused by any other generation before its shape is decoded, and the
    /// handlers refuse it before journaling anything. An unstamped request is
    /// generation 1.
    pub journal_version: u32,
}

impl RestateProcessCancelRequest {
    /// A request for the handlers of this build's generation.
    pub fn new(process_id: ProcessId, request: lash_core::CancelRequest) -> Self {
        Self {
            process_id,
            request,
            journal_version: RESTATE_PROCESS_JOURNAL_VERSION,
        }
    }

    pub(crate) fn from_record(record: &lash_core::ProcessRecord) -> Result<Self, PluginError> {
        let request = record.cancel_request.as_deref().cloned().ok_or_else(|| {
            PluginError::Session(format!(
                "process `{}` has no cancellation request",
                record.id
            ))
        })?;
        Ok(Self::new(record.id.clone(), request))
    }
}

#[async_trait::async_trait]
pub(crate) trait RestateProcessRunner: Send + Sync + 'static {
    /// Run one admitted segment. `started` is the proof that the segment's
    /// start marker committed (FIG-3588); a runner cannot be driven without it.
    #[allow(clippy::too_many_arguments)]
    async fn run_process_segment(
        &self,
        started: &SegmentStarted,
        process_id: ProcessId,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        scoped_effect_controller: ScopedEffectController<'_>,
        handover: Option<lash_core::SegmentHandover>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError>;

    /// The executable generation the engine running `registration` runs it
    /// as (FIG-3571). Admission stamps it on segment 0's start marker, which
    /// is the incarnation's start record, and every later segment is held to
    /// the stamp before its runner is entered. A runner whose engine carries
    /// no generation answers `None`.
    fn executable_generation(
        &self,
        registration: &ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration>;

    /// Ask the child turn a `SessionTurn` process drives to stop now, as a
    /// durable request on the turn's gate (FIG-3673). A runner whose
    /// processes drive no child turn answers `Ok`.
    async fn stop_child_turn(
        &self,
        _record: &lash_core::ProcessRecord,
        _request: &lash_core::CancelRequest,
    ) -> Result<(), PluginError> {
        Ok(())
    }

    /// The live trace observer and context segment controllers report to,
    /// when the runner's worker has one. A workflow given its own sink uses
    /// that instead.
    fn trace(&self) -> Option<(Arc<dyn lash_trace::TraceSink>, lash_trace::TraceContext)> {
        None
    }
}

/// Runs process segments on a deployment's [`DurableProcessWorker`]: one
/// given up front, or one installed in a [`RestateProcessWorkerSlot`] after
/// the endpoint exists.
#[derive(Clone)]
pub(crate) struct RestateCoreProcessRunner {
    worker: ProcessWorkerSource,
}

#[derive(Clone)]
enum ProcessWorkerSource {
    Ready(DurableProcessWorker),
    Slot(RestateProcessWorkerSlot),
}

impl RestateCoreProcessRunner {
    #[cfg(test)]
    pub(crate) fn new(worker: DurableProcessWorker) -> Self {
        Self {
            worker: ProcessWorkerSource::Ready(worker),
        }
    }

    /// The worker segments run on. A slot nothing is installed in yet refuses:
    /// the endpoint was bound before its core existed, and the host has not
    /// installed the core's worker.
    fn worker(&self) -> Result<DurableProcessWorker, PluginError> {
        match &self.worker {
            ProcessWorkerSource::Ready(worker) => Ok(worker.clone()),
            ProcessWorkerSource::Slot(slot) => slot.installed().ok_or_else(|| {
                PluginError::Invoke(
                    "no process worker is installed in this deployment's \
                     RestateProcessWorkerSlot yet"
                        .to_owned(),
                )
            }),
        }
    }
}

#[async_trait::async_trait]
impl RestateProcessRunner for RestateCoreProcessRunner {
    fn executable_generation(
        &self,
        registration: &ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        self.worker()
            .ok()
            .and_then(|worker| worker.executable_generation(registration))
    }

    async fn stop_child_turn(
        &self,
        record: &lash_core::ProcessRecord,
        request: &lash_core::CancelRequest,
    ) -> Result<(), PluginError> {
        self.worker()?
            .request_session_turn_child_stop(record, request)
            .await
    }

    fn trace(&self) -> Option<(Arc<dyn lash_trace::TraceSink>, lash_trace::TraceContext)> {
        let worker = self.worker().ok()?;
        let tracing = &worker.config().runtime_host.tracing;
        tracing
            .trace_sink
            .clone()
            .map(|sink| (sink, tracing.trace_context.clone()))
    }

    async fn run_process_segment(
        &self,
        started: &SegmentStarted,
        process_id: ProcessId,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        scoped_effect_controller: ScopedEffectController<'_>,
        handover: Option<lash_core::SegmentHandover>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        let worker = self.worker()?;
        let execution_write_authority = started.write_authority().clone();
        Box::pin(worker.run_process_segment_with_scoped_effect_controller(
            process_id,
            registration,
            execution_context,
            execution_write_authority,
            scoped_effect_controller,
            cancellation,
            handover,
        ))
        .await
    }
}

/// [`ProcessWorkSubstrate`] that drives pending processes by submitting their
/// `LashProcessWorkflow` through the Restate ingress instead of running them
/// in-process.
///
/// This is the controller-owned process work handle: a host-owned
/// The process-work port calls it on ingress-relevant events. Per row, it POSTs
/// `LashProcessWorkflow/{process_id}/run/send` to the ingress. Restate
/// coalesces by workflow key, so duplicate submits are idempotent and no Lash
/// registry lease is needed at the Restate tier.
pub struct RestateProcessIngressRunner {
    ingress: RestateIngressClient,
    registry: Arc<dyn ProcessRegistry>,
    continuations: Arc<dyn lash_core::ProcessContinuationStore>,
    event_sink: Option<Arc<dyn ProcessEventSink>>,
    park_reconciler: ParkReconciler,
}

/// The admin client a deployment's sweep reconciles engine-paused segments
/// into process parks through, once the host installs one.
type ParkReconciler = Arc<std::sync::OnceLock<crate::RestateAdminClient>>;

impl RestateProcessIngressRunner {
    pub fn new(
        connection: impl Into<RestateConnection>,
        registry: Arc<dyn ProcessRegistry>,
        continuations: Arc<dyn lash_core::ProcessContinuationStore>,
    ) -> Self {
        Self {
            ingress: RestateIngressClient::new(connection),
            registry,
            continuations,
            event_sink: None,
            park_reconciler: ParkReconciler::default(),
        }
    }

    fn with_park_reconciler(mut self, reconciler: ParkReconciler) -> Self {
        self.park_reconciler = reconciler;
        self
    }

    /// `RestateProcessDeployment::new_with_sink` installs the host's sink here,
    /// because a per-row deferral only reaches a host that reads the report —
    /// and every in-tree caller of `claim_and_run_pending` discards it. The
    /// fault surface is the path that does not depend on anyone reading a
    /// return value.
    pub(crate) fn with_event_sink(mut self, sink: Option<Arc<dyn ProcessEventSink>>) -> Self {
        self.event_sink = sink;
        self
    }

    /// Push one worker fault to the host-facing sink, or to `tracing` when this
    /// handle has none — the same floor the native worker keeps.
    async fn emit_worker_fault(
        &self,
        process_id: &ProcessId,
        operation: ProcessRecoveryOperation,
        error: &PluginError,
    ) {
        let fault = ProcessWorkerFault::RecoveryBackendError {
            process_id: process_id.clone(),
            operation,
            error: error.to_string(),
        };
        let Some(sink) = self.event_sink.as_ref() else {
            tracing::error!(
                target: "lash_restate::process",
                event = "process_worker.fault",
                fault = "recovery_backend_error",
                process_id = %process_id,
                operation = operation.label(),
                error = %error,
                "restate ingress sweep fault (no process event sink wired)"
            );
            return;
        };
        sink.emit_worker_fault(&fault).await;
    }

    async fn submit_record(
        &self,
        record: ProcessRecord,
    ) -> Result<IngressSubmitOutcome, PluginError> {
        let process_id = record.id.clone();
        // ExternallyOwned rows are never executed by Lash (ADR 0019). Defensively
        // refuse to POST a run for one even when reached directly, so both the
        // sweep and any direct caller are safe; their closure comes from an
        // external actor calling `complete_process` or a reconciled Abandon
        // Request (see `claim_and_run_pending`).
        if record.disposition == RecoveryContract::ExternallyOwned {
            return Ok(IngressSubmitOutcome::ExternallyOwned);
        }
        // Re-read before submitting: the worklist page is a snapshot, and the
        // row may have moved under it.
        let current = self.registry.get_process(&process_id).await?;
        // Idempotent by process id: never re-submit a finished process.
        if let Some(current) = current.as_ref().filter(|current| current.is_terminal()) {
            return Ok(IngressSubmitOutcome::SettledByPeer(current.status));
        }
        // A standing cancel request is not a reason to withhold the submission.
        // Only a `StartFailed` request is terminal on the spot; every other
        // origin is recorded and waits for the run to honour it. Since the
        // sweep never writes a terminal of its own, skipping here would leave a
        // cancel-requested row that was never submitted permanently
        // non-terminal: `await_process_terminal` would never return and
        // retention would never reclaim it. The row is submitted, and the
        // workflow's own journaled cancellation step settles it.
        //
        // Every non-terminal row is submitted under the workflow key of its
        // latest handover, whatever its external reference says (FIG-3588).
        // Restate coalesces a submission onto a live or retained workflow of
        // that key. A key it no longer holds runs the segment's admission,
        // which starts a segment that never started, ends a started one whose
        // journal is gone as `SubstrateLost`, and ignores a segment that has
        // already handed over. So the sweep reaches every kind of substrate
        // loss, and the reference is observational only.
        let latest_handover = self
            .continuations
            .latest_segment_handover(&process_id)
            .await?;
        let segment_ordinal = latest_handover
            .as_ref()
            .map_or(0, |handover| handover.segment_ordinal);
        let workflow_key = process_segment_workflow_key(&process_id, segment_ordinal);
        let registration = ProcessRegistration {
            start_key: record.start_key,
            input: record.input,
            disposition: record.disposition,
            lifecycle: record.lifecycle,
            max_attempts: record.max_attempts,
            identity: record.identity,
            event_types: record.event_types,
            provenance: record.provenance.clone(),
            env_ref: record.env_ref,
            wake_session_id: None,
        };
        let execution_context = ProcessExecutionContext::default();
        let invocation_id = self
            .ingress
            .send_workflow_json(
                crate::LashService::ProcessWorkflow.name(),
                &workflow_key,
                "run",
                &RestateProcessWorkflowInput {
                    process_id: process_id.clone(),
                    registration,
                    execution_context,
                    segment_ordinal,
                    journal_version: RESTATE_PROCESS_JOURNAL_VERSION,
                },
            )
            .await
            .map_err(|err| process_ingress_submit_error(&process_id, err))?;
        // Record the durable backend reference so the process is observably
        // owned by Restate, mirroring `schedule_restate_process`.
        self.registry
            .set_external_ref(
                &process_id,
                ProcessExternalRef {
                    backend: "restate".to_string(),
                    id: format!(
                        "{}/{workflow_key}",
                        crate::LashService::ProcessWorkflow.name()
                    ),
                    metadata: Some(serde_json::json!({ "invocation_id": invocation_id })),
                    segment_ordinal: Some(segment_ordinal),
                },
            )
            .await
            .map(|_| IngressSubmitOutcome::Submitted)
    }

    /// Reconcile a pending Abandon Request on an externally-owned row into an
    /// `Abandoned{ReconciledRequest}` terminal, mirroring the core sweep's
    /// `reconcile_externally_owned_abandon`.
    ///
    /// Lash never executed the row, so there is no execution owner to name
    /// (`owner: None`). The Restate tier holds no Lash lease — workflow-key
    /// coalescing is its single-writer discipline — so the terminal is written
    /// directly after re-checking the row is still non-terminal (it may have
    /// been completed between the worklist scan and here). The decorated
    /// registry emits the resulting terminal append through the event sink.
    async fn reconcile_externally_owned_abandon(
        &self,
        process_id: &ProcessId,
    ) -> Result<(), PluginError> {
        if self
            .registry
            .get_process(process_id)
            .await?
            .is_some_and(|current| current.is_terminal())
        {
            return Ok(());
        }
        self.registry
            .complete_process(
                process_id,
                ProcessAwaitOutput::Abandoned {
                    evidence: Box::new(AbandonEvidence {
                        writer: AbandonWriter::ReconciledRequest,
                        owner: None,
                        epoch_ms: restate_now_ms(),
                    }),
                    control: None,
                },
                ProcessCompletionAuthority::ReconciledAbandon,
            )
            .await
            .map(|_| ())
    }
}

impl RestateProcessIngressRunner {
    async fn claim_and_run_pending(&self) -> Result<ProcessAdmissionReport, PluginError> {
        // Engine-paused segments become process parks before the pass reads
        // its worklist (FIG-3675). A failed reconcile is reported and retried
        // by the next pass; it never stops this one.
        if let Some(admin) = self.park_reconciler.get()
            && let Err(error) = reconcile_process_parks(admin, &self.registry).await
        {
            tracing::warn!(
                error = %error,
                "restate process park reconcile failed; the next sweep retries it"
            );
        }
        let mut report = ProcessAdmissionReport::default();
        let limit = std::num::NonZeroUsize::MIN.saturating_add(255);
        let mut continuation = None;
        loop {
            let page = match self
                .registry
                .list_non_terminal_page(limit, continuation)
                .await
            {
                Ok(page) => page,
                Err(error) => {
                    // The only remaining escape: a page read that fails after
                    // earlier pages already admitted rows. An `Err` may follow
                    // partial admission; name the admitted ids so they are not
                    // silently lost.
                    if !report.admitted.is_empty() {
                        tracing::error!(
                            admitted = report.admitted.len(),
                            error = %error,
                            "restate process worklist scan failed after partial admission"
                        );
                    }
                    return Err(error);
                }
            };
            let next = page.continuation;
            for record in page.records {
                // ExternallyOwned rows are never submitted to ingress (ADR 0019):
                // Lash does not execute them at the Restate tier either. A pending
                // Abandon Request on such a row is reconciled into an Abandoned
                // terminal here, mirroring the core sweep's
                // `reconcile_externally_owned_abandon`; rows without a request are
                // left untouched for their external owner to complete.
                if record.disposition == RecoveryContract::ExternallyOwned {
                    let process_id = record.id.clone();
                    if record.abandon_request.is_some()
                        && let Err(error) =
                            self.reconcile_externally_owned_abandon(&process_id).await
                    {
                        // A failed reconcile is this row's outcome, not the
                        // whole pass's: rows already submitted to the ingress
                        // stay in the report instead of being discarded by `?`.
                        report.deferred.push(ProcessAdmissionDeferred {
                            process_id: process_id.clone(),
                            disposition: ProcessRecoveryAttemptOutcome::BackendError {
                                operation: ProcessRecoveryOperation::WriteTerminal,
                                error: error.to_string(),
                            },
                        });
                        self.emit_worker_fault(
                            &process_id,
                            ProcessRecoveryOperation::WriteTerminal,
                            &error,
                        )
                        .await;
                        continue;
                    }
                    // Lash never executes an externally-owned row on any tier;
                    // the native worker reports the same typed deferral.
                    report.deferred.push(ProcessAdmissionDeferred {
                        process_id,
                        disposition: ProcessRecoveryAttemptOutcome::ExternallyOwned,
                    });
                    continue;
                }
                let process_id = record.id.clone();
                match self.submit_record(record).await {
                    Ok(IngressSubmitOutcome::Submitted) => report.admitted.push(process_id),
                    Ok(IngressSubmitOutcome::ExternallyOwned) => {
                        report.deferred.push(ProcessAdmissionDeferred {
                            process_id,
                            disposition: ProcessRecoveryAttemptOutcome::ExternallyOwned,
                        });
                    }
                    Ok(IngressSubmitOutcome::SettledByPeer(terminal_status)) => {
                        report.deferred.push(ProcessAdmissionDeferred {
                            process_id,
                            disposition: ProcessRecoveryAttemptOutcome::SettledByPeer {
                                terminal_status,
                            },
                        });
                    }
                    Err(error) => {
                        // Per-row submit failure is a per-row deferral. Failing
                        // the whole call here would throw away the ids that
                        // already reached the ingress in this same pass.
                        report.deferred.push(ProcessAdmissionDeferred {
                            process_id: process_id.clone(),
                            disposition: ProcessRecoveryAttemptOutcome::BackendError {
                                operation: ProcessRecoveryOperation::SubmitRun,
                                error: error.to_string(),
                            },
                        });
                        // The deferral only reaches a host that reads the
                        // report; the fault surface reaches one that does not.
                        self.emit_worker_fault(
                            &process_id,
                            ProcessRecoveryOperation::SubmitRun,
                            &error,
                        )
                        .await;
                    }
                }
            }
            let Some(next) = next else {
                break;
            };
            continuation = Some(next);
        }
        Ok(report)
    }
}

/// What one ingress submit attempt did with a row.
enum IngressSubmitOutcome {
    /// The row's workflow run was submitted to the ingress.
    Submitted,
    /// Lash never executes the row (externally owned); nothing was submitted.
    ExternallyOwned,
    /// The row was already terminal when re-read just before submitting.
    SettledByPeer(ProcessStatus),
}

impl RestateProcessIngressRunner {
    pub(crate) async fn await_terminal_wait(
        &self,
        process_id: &ProcessId,
    ) -> Result<ProcessTerminalWait, PluginError> {
        let record = self.registry.get_process(process_id).await?;
        if let Some(output) = record.as_ref().and_then(|record| record.outcome.as_ref()) {
            return Ok(ProcessTerminalWait::Terminal(output.clone()));
        }
        let outcome = self
            .ingress
            .call_workflow_json::<_, ProcessAwaitOutput>(
                crate::LashService::ProcessWorkflow.name(),
                process_id.as_str(),
                "await_terminal",
                &RestateProcessAwaitRequest {
                    process_id: process_id.clone(),
                },
            )
            .await;
        let wait = outcome.map(ProcessTerminalWait::Terminal).or_else(|err| {
            if err.is_timeout() {
                Ok(ProcessTerminalWait::Reattach)
            } else if err.is_service_unregistered() {
                // A shared handler, so the 404 has two readings and this
                // client cannot tell them apart: nothing binds the service,
                // or nothing is left of this process's invocation.
                Err(PluginError::Runtime(RuntimeError::new(
                    RuntimeErrorCode::EngineProcessAwait,
                    crate::ingress::unresolvable_call_target_message(
                        crate::LashService::ProcessWorkflow.name(),
                        "await_terminal",
                        &err,
                    ),
                )))
            } else {
                Err(PluginError::Runtime(RuntimeError::new(
                    RuntimeErrorCode::EngineProcessAwait,
                    format!("ingress await for process `{process_id}` failed: {err}"),
                )))
            }
        })?;
        if matches!(&wait, ProcessTerminalWait::Terminal(_)) {
            self.registry.get_process(process_id).await?;
        }
        Ok(wait)
    }
}

#[async_trait::async_trait]
impl ProcessWorkSubstrate for RestateProcessIngressRunner {
    async fn admit_pending_processes(
        &self,
        _reason: &str,
    ) -> Result<ProcessAdmissionReport, PluginError> {
        self.claim_and_run_pending().await
    }

    async fn await_process_terminal(
        &self,
        process_id: &ProcessId,
    ) -> Result<ProcessTerminalWait, PluginError> {
        lash_core::facade_support::release_process_execution_permit_while(
            self.await_terminal_wait(process_id),
        )
        .await
    }
}

/// Bundled Restate process deployment wiring for a Lash core.
pub struct RestateProcessDeployment {
    #[cfg(test)]
    process_work: Arc<RestateProcessIngressRunner>,
    wiring: ProcessWorkWiring,
    registry: Arc<dyn ProcessRegistry>,
    ingress: RestateIngressClient,
    continuations: Arc<dyn lash_core::ProcessContinuationStore>,
    authority_id: crate::RestateAuthorityId,
    park_reconciler: ParkReconciler,
}

impl RestateProcessDeployment {
    pub fn new(
        connection: impl Into<RestateConnection>,
        authority_id: crate::RestateAuthorityId,
        registry: Arc<dyn ProcessRegistry>,
        continuations: Arc<dyn lash_core::ProcessContinuationStore>,
    ) -> Self {
        Self::new_with_sink(connection, authority_id, registry, continuations, None)
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        connection: impl Into<RestateConnection>,
        registry: Arc<dyn ProcessRegistry>,
        continuations: Arc<dyn lash_core::ProcessContinuationStore>,
    ) -> Self {
        Self::new(
            connection,
            crate::RestateAuthorityId::new("lash-restate-tests").expect("valid test authority"),
            registry,
            continuations,
        )
    }

    /// Like [`new`](Self::new), but installs a host-facing
    /// [`ProcessEventSink`] on the registry decorator this deployment wraps.
    ///
    /// The wrap happens inside the constructor, so the sink must be supplied
    /// here; each appended event is pushed best-effort after its durable write.
    /// See [`ProcessEventSink`] for the freshness-not-truth contract.
    pub fn new_with_sink(
        connection: impl Into<RestateConnection>,
        authority_id: crate::RestateAuthorityId,
        registry: Arc<dyn ProcessRegistry>,
        continuations: Arc<dyn lash_core::ProcessContinuationStore>,
        sink: Option<Arc<dyn ProcessEventSink>>,
    ) -> Self {
        let connection = connection.into();
        let fault_sink = sink.clone();
        let park_reconciler = ParkReconciler::default();
        let watched = watch_process_registry_with_sink(registry, sink);
        let registry = Arc::clone(watched.registry());
        let ingress_runner = Arc::new(
            RestateProcessIngressRunner::new(
                connection.clone(),
                Arc::clone(&registry),
                Arc::clone(&continuations),
            )
            .with_event_sink(fault_sink)
            .with_park_reconciler(Arc::clone(&park_reconciler)),
        );
        let process_work = ingress_runner;
        let port: Arc<dyn ProcessWorkSubstrate> = process_work.clone();
        let wiring = ProcessWorkWiring::new(watched, port);
        Self {
            #[cfg(test)]
            process_work,
            wiring,
            registry,
            ingress: RestateIngressClient::new(connection),
            continuations,
            authority_id,
            park_reconciler,
        }
    }

    /// Reconcile engine-paused process segments into process parks on every
    /// admission sweep, reading Restate's admin API through `admin`
    /// (FIG-3675). Installed once; a second call keeps the first client.
    pub fn install_park_reconciler(&self, admin: crate::RestateAdminClient) {
        let _ = self.park_reconciler.set(admin);
    }

    #[cfg(test)]
    pub(crate) fn new_with_sink_for_test(
        connection: impl Into<RestateConnection>,
        registry: Arc<dyn ProcessRegistry>,
        continuations: Arc<dyn lash_core::ProcessContinuationStore>,
        sink: Option<Arc<dyn ProcessEventSink>>,
    ) -> Self {
        Self::new_with_sink(
            connection,
            crate::RestateAuthorityId::new("lash-restate-tests").expect("valid test authority"),
            registry,
            continuations,
            sink,
        )
    }

    pub fn process_work(&self) -> ProcessWorkWiring {
        self.wiring.clone()
    }

    #[cfg(test)]
    pub(crate) fn test_registry(&self) -> Arc<dyn ProcessRegistry> {
        Arc::clone(&self.registry)
    }

    #[cfg(test)]
    pub(crate) fn test_process_work(&self) -> Arc<RestateProcessIngressRunner> {
        Arc::clone(&self.process_work)
    }

    /// The process workflow over `serving`'s worker, under its segment
    /// policy. Only [`crate::services::bind_lash_services`] binds it.
    pub(crate) fn workflow(
        &self,
        serving: RestateProcessServing,
    ) -> LashProcessWorkflowImpl<RestateCoreProcessRunner> {
        let RestateProcessServing {
            worker,
            segment_effect_budget,
            retry_max_attempts,
        } = serving;
        let mut workflow = LashProcessWorkflowImpl::new(
            Arc::new(RestateCoreProcessRunner { worker }),
            Arc::clone(&self.registry),
            Arc::clone(&self.continuations),
            self.ingress.clone(),
            self.authority_id.clone(),
        );
        if let Some(selector) = segment_effect_budget {
            workflow = workflow.with_segment_effect_budget(selector);
        }
        workflow.with_retry_max_attempts(retry_max_attempts)
    }
}

/// How a Restate deployment serves lash processes: the worker that runs their
/// segments and the policy that bounds each segment. A host hands it to
/// [`RestateEngine::endpoint_builder`](crate::RestateEngine::endpoint_builder);
/// a bare [`DurableProcessWorker`] or a [`RestateProcessWorkerSlot`] converts
/// into one with the default policy.
pub struct RestateProcessServing {
    worker: ProcessWorkerSource,
    segment_effect_budget: Option<SegmentEffectBudget>,
    retry_max_attempts: u64,
}

/// Picks a process's completed-effect budget per segment from its
/// registration.
pub(crate) type SegmentEffectBudget = Arc<dyn Fn(&ProcessRegistration) -> u64 + Send + Sync>;

impl RestateProcessServing {
    /// Serve processes on `worker` under the default segment policy: 10,000
    /// completed effects per incarnation.
    pub fn new(worker: DurableProcessWorker) -> Self {
        Self::from_source(ProcessWorkerSource::Ready(worker))
    }

    /// Serve processes on whatever worker `slot` holds when a segment runs,
    /// under the default segment policy.
    pub fn from_slot(slot: RestateProcessWorkerSlot) -> Self {
        Self::from_source(ProcessWorkerSource::Slot(slot))
    }

    fn from_source(worker: ProcessWorkerSource) -> Self {
        Self {
            worker,
            segment_effect_budget: None,
            retry_max_attempts: PROCESS_HANDLER_MAX_ATTEMPTS,
        }
    }

    /// Select a deterministic completed-effect budget from immutable process
    /// registration data. This is primarily useful for conformance/e2e pairs
    /// that run the same artifact with and without forced segmentation; the
    /// production default remains 10,000 completed effects per incarnation.
    /// Pause a segment's `run` invocation after `max_attempts` attempts
    /// instead of the default [`PROCESS_HANDLER_MAX_ATTEMPTS`]. A paused
    /// segment's process is parked by the park reconcile
    /// ([`RestateProcessDeployment::install_park_reconciler`]) with
    /// `ParkReason::EngineRetryExhausted`, and a resume retries it.
    pub fn with_retry_max_attempts(mut self, max_attempts: u64) -> Self {
        self.retry_max_attempts = max_attempts.max(1);
        self
    }

    pub fn with_segment_effect_budget_selector(
        mut self,
        selector: impl Fn(&ProcessRegistration) -> u64 + Send + Sync + 'static,
    ) -> Self {
        self.segment_effect_budget = Some(Arc::new(selector));
        self
    }
}

impl From<DurableProcessWorker> for RestateProcessServing {
    fn from(worker: DurableProcessWorker) -> Self {
        Self::new(worker)
    }
}

impl From<RestateProcessWorkerSlot> for RestateProcessServing {
    fn from(slot: RestateProcessWorkerSlot) -> Self {
        Self::from_slot(slot)
    }
}

/// The process worker of a deployment whose endpoint must exist before the
/// core that makes the worker does: bind the endpoint over the slot, build the
/// core, then [`install`](Self::install) its worker. A segment that runs
/// before the install fails, naming the empty slot. Clones share one slot.
#[derive(Clone, Default)]
pub struct RestateProcessWorkerSlot {
    worker: Arc<std::sync::RwLock<Option<DurableProcessWorker>>>,
}

impl RestateProcessWorkerSlot {
    /// An empty slot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Serve every later segment on `worker`, replacing any worker installed
    /// before.
    pub fn install(&self, worker: DurableProcessWorker) {
        *self
            .worker
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(worker);
    }

    fn installed(&self) -> Option<DurableProcessWorker> {
        self.worker
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl std::fmt::Debug for RestateProcessWorkerSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RestateProcessWorkerSlot")
            .field("installed", &self.installed().is_some())
            .finish()
    }
}

impl std::fmt::Debug for RestateProcessServing {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RestateProcessServing")
            .field(
                "segment_effect_budget",
                &self.segment_effect_budget.as_ref().map(|_| "selector"),
            )
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateProcessWorkflowInput {
    /// The minted id of the process this workflow runs: the registration
    /// carries no id of its own (ADR 0107).
    pub process_id: ProcessId,
    pub registration: ProcessRegistration,
    #[serde(default, skip_serializing_if = "ProcessExecutionContext::is_empty")]
    pub execution_context: ProcessExecutionContext,
    #[serde(default)]
    pub segment_ordinal: u64,
    /// The generation of the handler's journaled command prefix the submitter
    /// was built for ([`RESTATE_PROCESS_JOURNAL_VERSION`]). The handler refuses
    /// any other generation before it journals anything.
    #[serde(default = "admission::unstamped_journal_version")]
    pub journal_version: u32,
}

/// What a process workflow invocation was submitted with, read generation
/// first.
///
/// The handler must refuse an input of another journal generation typed, and
/// it can only do so if the input reaches it: an input of a retired generation
/// is not decoded against this generation's shape, so its missing or retired
/// fields cannot fail the invocation before the handler runs.
#[derive(Clone, Debug)]
pub enum RestateProcessWorkflowPayload {
    /// An input of this handler's generation.
    Current(Box<RestateProcessWorkflowInput>),
    /// An input stamped with another generation, with the process it names
    /// when its id is one this build reads.
    Retired {
        journal_version: u32,
        process_id: Option<ProcessId>,
    },
}

impl From<RestateProcessWorkflowInput> for RestateProcessWorkflowPayload {
    fn from(input: RestateProcessWorkflowInput) -> Self {
        Self::Current(Box::new(input))
    }
}

impl Serialize for RestateProcessWorkflowPayload {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Current(input) => input.serialize(serializer),
            Self::Retired {
                journal_version,
                process_id,
            } => serde_json::json!({
                "process_id": process_id,
                "journal_version": journal_version,
            })
            .serialize(serializer),
        }
    }
}

impl<'de> serde::Deserialize<'de> for RestateProcessWorkflowPayload {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let payload = serde_json::Value::deserialize(deserializer)?;
        let journal_version = admission::stamped_journal_version(&payload);
        if journal_version != RESTATE_PROCESS_JOURNAL_VERSION {
            let process_id = payload
                .get("process_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|process_id| ProcessId::parse(process_id).ok());
            return Ok(Self::Retired {
                journal_version,
                process_id,
            });
        }
        serde_json::from_value(payload)
            .map(|input| Self::Current(Box::new(input)))
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RestateProcessWorkflowOutput {
    Terminal { output: Box<ProcessAwaitOutput> },
    SegmentChained { next_segment_ordinal: u64 },
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
#[serde(
    into = "stamped_requests::StampedCompleteRequest",
    try_from = "serde_json::Value"
)]
pub struct RestateProcessCompleteRequest {
    pub process_id: ProcessId,
    pub output: ProcessAwaitOutput,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
#[serde(
    into = "stamped_requests::StampedAwaitRequest",
    try_from = "serde_json::Value"
)]
pub struct RestateProcessAwaitRequest {
    pub process_id: ProcessId,
}

/// Terminal value for one process segment's durable cancellation observer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestateProcessCancelSignal {
    /// The cancel endpoint accepted a request and wrote its durable registry event.
    CancelRequested,
    /// The segment ended normally, so its cancellation observer can retire.
    SegmentFinished,
}
