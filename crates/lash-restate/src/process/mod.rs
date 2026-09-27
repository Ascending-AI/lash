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
pub(crate) mod park_reconcile;
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
    AwaitEventKey, AwaitEventWaitIdentity, ExecutionScope, PluginError, ProcessAwaitOutput,
    ProcessCompletionAuthority, ProcessExecutionContext, ProcessExternalRef, ProcessRecord,
    ProcessRegistration, ProcessRegistry, ProcessStatus, ProcessTerminalWait, ProcessWorkSubstrate,
    ProcessWorkWiring, Resolution, RuntimeError, RuntimeErrorCode, ScopedEffectController,
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
pub(crate) use workflow::{LashProcessWorkflow, LashProcessWorkflowImpl};

/// Attempts a process segment's `run` invocation makes before it pauses, by
/// default: the same bound a turn handler has
/// ([`TURN_HANDLER_MAX_ATTEMPTS`](crate::TURN_HANDLER_MAX_ATTEMPTS)). A
/// deployment sets its own with [`RestateProcessServing::with_retry_max_attempts`].
pub const PROCESS_HANDLER_MAX_ATTEMPTS: u64 = crate::TURN_HANDLER_MAX_ATTEMPTS;

pub(crate) const PROCESS_CANCEL_PROMISE_KEY: &str = "process_cancel_requested";
/// One segment's hand-over promise (FIG-3799): the drain's wake resolves it
/// with the generation it drains, and a signal wait of a segment admitted
/// under that generation loses its race to it and hands the wait over. It is
/// a promise of its own, never the cancel promise: a wake that lands while
/// the segment is not waiting stays latched for its next wait, and a cancel
/// arriving after it still reaches the segment.
pub(crate) const PROCESS_HAND_OVER_PROMISE_KEY: &str = "process_hand_over_requested";

/// Whether a hand-over promise's `payload` names `own`, the generation that
/// admitted the waiting segment. A wake naming another generation — a stale
/// read that reached a segment already on the newest build — is not a
/// hand-over: the wait goes on.
pub(crate) fn process_hand_over_verdict(
    payload: &str,
    own: &lash_core::engine::BuildGeneration,
) -> bool {
    serde_json::from_str::<lash_core::engine::BuildGeneration>(payload)
        .is_ok_and(|generation| &generation == own)
}
/// Wall-clock epoch milliseconds for terminal evidence written at the Restate
/// tier (ADR 0110). The Restate boundary carries no
/// injected Lash clock — its durability comes from the engine and workflow-key
/// coalescing rather than a Lash lease — so it reads the system clock through
/// [`crate::system_clock`], and only inside a journaled step or before the
/// handler's first command (FIG-3673): the stamp a step journals is the one
/// every redrive publishes.
fn restate_now_ms() -> u64 {
    crate::system_clock().timestamp_ms()
}

/// Restate's single-writer discipline is per-`process_id` workflow-key coalescing;
/// the workflow key is that `process_id` (ADR 0027).
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

/// The handler-side call that awaits `process_id`'s terminal: on the stable
/// root `LashProcessWorkflow/<pid>`, whose terminal promise outlives every
/// segment's lane (FIG-3795). A segment running under a generation lane
/// still completes the terminal there.
pub(crate) fn await_terminal_on_stable_root<'ctx, C>(
    ctx: &C,
    namespace: &crate::RestateNamespace,
    process_id: ProcessId,
) -> restate_sdk::context::Request<
    'ctx,
    restate_sdk::serde::Json<RestateProcessAwaitRequest>,
    restate_sdk::serde::Json<ProcessAwaitOutput>,
>
where
    C: restate_sdk::context::ContextClient<'ctx>,
{
    crate::services::routed_workflow(
        ctx,
        &namespace.stable(crate::LashService::ProcessWorkflow),
        process_id.to_string(),
        "await_terminal",
        RestateProcessAwaitRequest { process_id },
    )
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
    namespace: &crate::RestateNamespace,
    process_id: &ProcessId,
    err: crate::RestateHttpError,
) -> PluginError {
    if err.is_service_unregistered() {
        PluginError::Runtime(RuntimeError::new(
            RuntimeErrorCode::EngineServiceUnregistered,
            crate::ingress::unregistered_service_message(
                &namespace.stable(crate::LashService::ProcessWorkflow).name(),
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

/// Whether a segment boundary is declined, and the runner re-entered in the
/// same invocation: a budget boundary taken while the process's wait state is
/// armed is. A hand-over boundary never is — it exists to carry an open wait
/// to a successor (FIG-3799), and re-entering would only hand it over again.
pub(crate) fn boundary_must_be_declined(
    reason: lash_core::BoundaryReason,
    record: Option<&ProcessRecord>,
) -> bool {
    reason != lash_core::BoundaryReason::HandOver
        && record.is_some_and(|record| record.wait.is_some())
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
    /// The namespace the deployment's process workflow is named in
    /// (FIG-3898).
    namespace: crate::RestateNamespace,
    registry: Arc<dyn ProcessRegistry>,
    continuations: Arc<dyn lash_core::ProcessContinuationStore>,
    event_sink: Option<Arc<dyn ProcessEventSink>>,
    park_reconciler: ParkReconciler,
}

/// The admin client a deployment's sweep reconciles engine-paused segments
/// into process parks through, once the host installs one.
type ParkReconciler = Arc<std::sync::OnceLock<crate::RestateAdminClient>>;

impl RestateProcessIngressRunner {
    /// The runner of a deployment in the default namespace.
    pub fn new(
        connection: impl Into<RestateConnection>,
        registry: Arc<dyn ProcessRegistry>,
        continuations: Arc<dyn lash_core::ProcessContinuationStore>,
    ) -> Self {
        Self::in_namespace(
            connection,
            registry,
            continuations,
            crate::RestateNamespace::default(),
        )
    }

    /// [`new`](Self::new) for a deployment in `namespace` (FIG-3898).
    pub fn in_namespace(
        connection: impl Into<RestateConnection>,
        registry: Arc<dyn ProcessRegistry>,
        continuations: Arc<dyn lash_core::ProcessContinuationStore>,
        namespace: crate::RestateNamespace,
    ) -> Self {
        Self {
            ingress: RestateIngressClient::new(connection),
            namespace,
            registry,
            continuations,
            event_sink: None,
            park_reconciler: ParkReconciler::default(),
        }
    }

    /// A runner over an ingress client the caller already holds: the process
    /// workflow's, to deliver its ended scope's cancels (FIG-3822).
    pub(crate) fn over_ingress(
        ingress: RestateIngressClient,
        namespace: crate::RestateNamespace,
        registry: Arc<dyn ProcessRegistry>,
        continuations: Arc<dyn lash_core::ProcessContinuationStore>,
    ) -> Self {
        Self {
            ingress,
            namespace,
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
    /// handle has none, so a fault is never silent.
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
        // Externally-owned rows are never executed by Lash (ADR 0110).
        // Defensively refuse to POST a run for one even when reached directly,
        // so both the sweep and any direct caller are safe; their closure comes
        // from their external owner calling `complete_process`.
        if record.input.is_externally_owned() {
            return Ok(IngressSubmitOutcome::ExternallyOwned);
        }
        // Re-read before submitting: the registry page is a snapshot, and the
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
        // The route is data (FIG-3795 S3/S5): a redrive addresses the route
        // the latest handover recorded rather than recomputing a name, and
        // carries the handover writer's generation as its sender, so the lane
        // it reaches judges it as the send it repeats. A root segment, which
        // has no handover, was sent under the stable name, where a redrive of
        // segment 0 is admitted from any sender.
        let route = latest_handover.as_ref().map_or_else(
            || {
                self.namespace
                    .stable(crate::LashService::ProcessWorkflow)
                    .to_string()
            },
            |handover| handover.route.clone(),
        );
        let sender_generation = latest_handover
            .as_ref()
            .and_then(|handover| handover.written_generation.clone());
        let registration = ProcessRegistration {
            start_key: record.start_key,
            input: record.input,
            lifetime: record.lifetime,
            ancestry: record.ancestry,
            session_capability: record.session_capability,
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
                route.as_str(),
                &workflow_key,
                "run",
                &RestateProcessWorkflowInput {
                    process_id: process_id.clone(),
                    registration,
                    execution_context,
                    segment_ordinal,
                    sender_generation,
                },
            )
            .await
            .map_err(|err| process_ingress_submit_error(&self.namespace, &process_id, err))?;
        // Record the durable backend reference so the process is observably
        // owned by Restate, mirroring `schedule_restate_process`.
        self.registry
            .set_external_ref(
                &process_id,
                ProcessExternalRef {
                    backend: "restate".to_string(),
                    id: format!("{route}/{workflow_key}"),
                    metadata: Some(serde_json::json!({ "invocation_id": invocation_id })),
                    segment_ordinal: Some(segment_ordinal),
                },
            )
            .await
            .map(|_| IngressSubmitOutcome::Submitted)
    }
}

impl RestateProcessIngressRunner {
    async fn claim_and_run_pending(&self) -> Result<ProcessAdmissionReport, PluginError> {
        // Engine-paused segments become process parks before the pass reads
        // its registry page (FIG-3675). A failed reconcile is reported and retried
        // by the next pass; it never stops this one.
        if let Some(admin) = self.park_reconciler.get()
            && let Err(error) =
                reconcile_process_parks(admin, &self.namespace, &self.registry, &self.continuations)
                    .await
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
                .list_non_terminal_processes_page(limit, continuation)
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
                            "restate non-terminal registry scan failed after partial admission"
                        );
                    }
                    return Err(error);
                }
            };
            let next = page.continuation;
            for record in page.records {
                // Externally-owned rows are never submitted to ingress (ADR
                // 0110): Lash does not execute them on any tier, and their
                // external owner closes them. The pass reports each as
                // deferred.
                if record.input.is_externally_owned() {
                    report.deferred.push(ProcessAdmissionDeferred {
                        process_id: record.id.clone(),
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
        // FIG-1383: a row whose caller departed before any outcome has no
        // actor left to end it, and lash never invents its outcome, so a
        // wait on it is refused rather than parked.
        if record
            .as_ref()
            .is_some_and(|record| record.status == ProcessStatus::CallerDeparted)
        {
            return Err(PluginError::ProcessCallerDeparted {
                process_id: process_id.clone(),
            });
        }
        let outcome = self
            .ingress
            .call_workflow_json::<_, ProcessAwaitOutput>(
                &self
                    .namespace
                    .stable(crate::LashService::ProcessWorkflow)
                    .name(),
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
                        &self
                            .namespace
                            .stable(crate::LashService::ProcessWorkflow)
                            .name(),
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
        self.await_terminal_wait(process_id).await
    }

    /// A one-way send to the process's `cancel` handler under
    /// `delivery_key`: the handler journals the registry request and
    /// resolves the cancel promise its segment watches (FIG-3673), and
    /// routes the request to a later segment itself. A repeat of the key
    /// names the first invocation; past the engine's dedupe window the
    /// handler is idempotent anyway, since the same origin and requester is
    /// a no-op and a resolved promise stays resolved.
    async fn deliver_cancel(
        &self,
        process_id: &ProcessId,
        request: &lash_core::CancelRequest,
        delivery_key: &str,
    ) -> Result<(), PluginError> {
        deliver_process_cancel(
            &self.ingress,
            &self.namespace,
            process_id,
            request,
            delivery_key,
        )
        .await
    }

    /// The drain's wake (FIG-3799): a one-way send to the live segment's
    /// `deliver_hand_over` handler, under the route its handover recorded,
    /// keyed by generation and segment so a repeated wake names the first
    /// invocation. The handler resolves the segment's hand-over promise.
    async fn deliver_hand_over(
        &self,
        process_id: &ProcessId,
        generation: &lash_core::engine::BuildGeneration,
    ) -> Result<(), PluginError> {
        deliver_process_hand_over(
            &self.ingress,
            &self.namespace,
            self.continuations.as_ref(),
            process_id,
            generation,
        )
        .await
    }

    /// A call to the root workflow's `complete_terminal`, which resolves the
    /// process's terminal promise unless it already holds a terminal: the
    /// first published terminal stands, so a repeat is a no-op and the
    /// promise itself is the dedupe. No idempotency key is sent, so an
    /// attempt never attaches to an earlier attempt's invocation that
    /// stopped.
    async fn publish_process_terminal(
        &self,
        process_id: &ProcessId,
        output: &ProcessAwaitOutput,
        _key: &str,
    ) -> Result<(), PluginError> {
        publish_process_terminal(&self.ingress, &self.namespace, process_id, output).await
    }
}

/// Wake `process_id`'s live segment to hand its wait over from `generation`
/// ([`ProcessWorkSubstrate::deliver_hand_over`] on Restate). The live segment
/// is the latest handover's successor under the route recorded with it
/// (FIG-3795 S3), or the root on the stable lane when nothing was handed
/// over yet.
pub(crate) async fn deliver_process_hand_over(
    ingress: &RestateIngressClient,
    namespace: &crate::RestateNamespace,
    continuations: &dyn lash_core::ProcessContinuationStore,
    process_id: &ProcessId,
    generation: &lash_core::engine::BuildGeneration,
) -> Result<(), PluginError> {
    let (segment_ordinal, route) = match continuations.latest_segment_handover(process_id).await? {
        Some(handover) if handover.segment_ordinal > 0 => {
            (handover.segment_ordinal, handover.route)
        }
        _ => (
            0,
            namespace
                .stable(crate::LashService::ProcessWorkflow)
                .to_string(),
        ),
    };
    ingress
        .send_workflow_json_idempotent(
            &route,
            &process_segment_workflow_key(process_id, segment_ordinal),
            "deliver_hand_over",
            &RestateProcessHandOverRequest {
                process_id: process_id.clone(),
                generation: generation.clone(),
            },
            &format!(
                "hand-over:{}:{process_id}:{segment_ordinal}",
                generation.as_str()
            ),
        )
        .await
        .map(|_| ())
        .map_err(|error| {
            PluginError::Runtime(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::EngineProcessIngressSubmit,
                format!(
                    "the drain's hand-over wake of process `{process_id}` segment \
                     {segment_ordinal} failed: {error}"
                ),
            ))
        })
}

/// Publish `output` to `process_id`'s terminal promise through the stable
/// root workflow's `complete_terminal`, whose promise outlives every
/// segment's lane (FIG-3795) (the `ProcessTerminal` obligation's
/// delivery on Restate, ADR 0109 §3).
pub(crate) async fn publish_process_terminal(
    ingress: &RestateIngressClient,
    namespace: &crate::RestateNamespace,
    process_id: &ProcessId,
    output: &ProcessAwaitOutput,
) -> Result<(), PluginError> {
    ingress
        .call_workflow_json::<_, ()>(
            &namespace.stable(crate::LashService::ProcessWorkflow).name(),
            process_id.as_str(),
            "complete_terminal",
            &RestateProcessCompleteRequest {
                process_id: process_id.clone(),
                output: output.clone(),
            },
        )
        .await
        .map_err(|error| {
            if error.is_service_unregistered() {
                PluginError::Runtime(RuntimeError::new(
                    RuntimeErrorCode::EngineServiceUnregistered,
                    crate::ingress::unresolvable_call_target_message(
                        &namespace.stable(crate::LashService::ProcessWorkflow).name(),
                        "complete_terminal",
                        &error,
                    ),
                ))
            } else {
                PluginError::Runtime(RuntimeError::new(
                    RuntimeErrorCode::EngineProcessAwait,
                    format!("publishing process `{process_id}`'s terminal failed: {error}"),
                ))
            }
        })
}

/// Send `request` to `process`'s `cancel` handler under `delivery_key`
/// ([`ProcessWorkSubstrate::deliver_cancel`] on Restate).
pub(crate) async fn deliver_process_cancel(
    ingress: &RestateIngressClient,
    namespace: &crate::RestateNamespace,
    process_id: &ProcessId,
    request: &lash_core::CancelRequest,
    delivery_key: &str,
) -> Result<(), PluginError> {
    ingress
        .send_workflow_json_idempotent(
            &namespace.stable(crate::LashService::ProcessWorkflow).name(),
            &process_segment_workflow_key(process_id, 0),
            "cancel",
            &RestateProcessCancelRequest::new(process_id.clone(), request.clone()),
            delivery_key,
        )
        .await
        .map(|_| ())
        .map_err(|error| {
            PluginError::Runtime(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::EngineProcessCancel,
                format!("parent-end cancel delivery to process `{process_id}` failed: {error}"),
            ))
        })
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
    namespace: crate::RestateNamespace,
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
        Self::in_namespace(
            connection,
            authority_id,
            registry,
            continuations,
            sink,
            crate::RestateNamespace::default(),
        )
    }

    /// [`new_with_sink`](Self::new_with_sink) for a deployment in
    /// `namespace` (FIG-3898): the process workflow it submits to, awaits
    /// and reconciles is that namespace's.
    pub fn in_namespace(
        connection: impl Into<RestateConnection>,
        authority_id: crate::RestateAuthorityId,
        registry: Arc<dyn ProcessRegistry>,
        continuations: Arc<dyn lash_core::ProcessContinuationStore>,
        sink: Option<Arc<dyn ProcessEventSink>>,
        namespace: crate::RestateNamespace,
    ) -> Self {
        let connection = connection.into();
        let fault_sink = sink.clone();
        let park_reconciler = ParkReconciler::default();
        let watched = watch_process_registry_with_sink(registry, sink);
        let registry = Arc::clone(watched.registry());
        let ingress_runner = Arc::new(
            RestateProcessIngressRunner::in_namespace(
                connection.clone(),
                Arc::clone(&registry),
                Arc::clone(&continuations),
                namespace.clone(),
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
            namespace,
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
    /// policy, stamping `build_generation` on every admission, handover and
    /// park it writes (FIG-3795). Only
    /// [`crate::services::bind_lash_services`] binds it.
    pub(crate) fn workflow(
        &self,
        serving: RestateProcessServing,
        build_generation: lash_core::engine::BuildGeneration,
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
            build_generation,
            &self.namespace,
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
    /// The drain generation of the build that sent this segment (FIG-3795
    /// S6): for a successor, the build that wrote the handover it resumes
    /// from; for a new process, the build whose turn or segment started it.
    /// A generation lane admits only its own generation's inputs, and the
    /// stable lane holds a successor from another build to its successor
    /// window. `None` names no build: a controller a host built, or a
    /// redrive of a handover written before generations were stamped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_generation: Option<lash_core::engine::BuildGeneration>,
}

/// What a process workflow invocation was submitted with.
///
/// A sender of another build may send an input whose shape this build does
/// not read. The handler must still refuse it typed — park it for its
/// sender's generation — and it can only do so if the input reaches it: an
/// input this build cannot decode keeps what of it this build can read
/// rather than failing the invocation before the handler runs.
#[derive(Clone, Debug)]
pub enum RestateProcessWorkflowPayload {
    /// An input this build reads.
    Current(Box<RestateProcessWorkflowInput>),
    /// An input this build does not decode, with the process, segment and
    /// sender generation it names when this build can read them.
    Unreadable {
        process_id: Option<ProcessId>,
        segment_ordinal: u64,
        sender_generation: Option<lash_core::engine::BuildGeneration>,
        error: String,
    },
}

impl RestateProcessWorkflowPayload {
    /// The process the input names, when this build can read it.
    pub(crate) fn process_id(&self) -> Option<&ProcessId> {
        match self {
            Self::Current(input) => Some(&input.process_id),
            Self::Unreadable { process_id, .. } => process_id.as_ref(),
        }
    }

    /// The segment the input runs.
    pub(crate) fn segment_ordinal(&self) -> u64 {
        match self {
            Self::Current(input) => input.segment_ordinal,
            Self::Unreadable {
                segment_ordinal, ..
            } => *segment_ordinal,
        }
    }

    /// The generation of the build that sent the input, when it names one.
    pub(crate) fn sender_generation(&self) -> Option<&lash_core::engine::BuildGeneration> {
        match self {
            Self::Current(input) => input.sender_generation.as_ref(),
            Self::Unreadable {
                sender_generation, ..
            } => sender_generation.as_ref(),
        }
    }
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
            Self::Unreadable {
                process_id,
                segment_ordinal,
                sender_generation,
                error: _,
            } => serde_json::json!({
                "process_id": process_id,
                "segment_ordinal": segment_ordinal,
                "sender_generation": sender_generation,
            })
            .serialize(serializer),
        }
    }
}

impl<'de> serde::Deserialize<'de> for RestateProcessWorkflowPayload {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let payload = serde_json::Value::deserialize(deserializer)?;
        match serde_json::from_value::<RestateProcessWorkflowInput>(payload.clone()) {
            Ok(input) => Ok(Self::Current(Box::new(input))),
            Err(error) => Ok(Self::Unreadable {
                process_id: payload
                    .get("process_id")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|process_id| ProcessId::parse(process_id).ok()),
                segment_ordinal: payload
                    .get("segment_ordinal")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                sender_generation: payload
                    .get("sender_generation")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|generation| {
                        lash_core::engine::BuildGeneration::parse(generation).ok()
                    }),
                error: error.to_string(),
            }),
        }
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

/// The drain's wake of a process's live segment (FIG-3799): resolve its
/// hand-over promise with the generation the drain retires.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct RestateProcessHandOverRequest {
    pub process_id: ProcessId,
    /// The generation being drained. Only a segment admitted under it hands
    /// its wait over.
    pub generation: lash_core::engine::BuildGeneration,
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
