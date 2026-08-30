//! Process ownership at the Restate tier.
//!
//! One responsibility: everything that decides *who* runs a Lash process
//! segment and how its terminal is delivered — the runner seam a host
//! implements, the ingress driver that submits pending rows, the deployment
//! wiring that binds both, and the durable promise/workflow-key vocabulary the
//! workflow and the ingress side must agree on. The workflow handler itself
//! lives in [`workflow`].

mod workflow;

use std::sync::Arc;
use std::time::Duration;

use lash_core::{
    AbandonEvidence, AbandonWriter, AwaitEventKey, AwaitEventWaitIdentity, ExecutionScope,
    PluginError, ProcessAwaitOutput, ProcessCompletionAuthority, ProcessExecutionContext,
    ProcessExternalRef, ProcessRecord, ProcessRegistration, ProcessRegistry, ProcessStatus,
    ProcessTerminalWait, ProcessWorkSubstrate, ProcessWorkWiring, RecoveryContract, Resolution,
    RuntimeError, RuntimeErrorCode, ScopedEffectController, facade_support::DurableProcessWorker,
    facade_support::ProcessAdmissionDeferred, facade_support::ProcessAdmissionReport,
    facade_support::ProcessEventSink, facade_support::ProcessRecoveryAttemptOutcome,
    facade_support::ProcessRecoveryOperation, facade_support::ProcessWorkerFault,
    facade_support::watch_process_registry_with_sink,
};
use restate_sdk::context::ContextPromises;
use restate_sdk::errors::{HandlerError, HandlerResult, TerminalError};
use serde::Serialize;

use crate::durable_wait::restate_await_event_key;
use crate::ingress::{RestateConnection, RestateIngressClient};

pub use workflow::{
    LashProcessWorkflow, LashProcessWorkflowClient, LashProcessWorkflowImpl,
    ServeLashProcessWorkflow,
};

const PROCESS_CANCEL_PROMISE_KEY: &str = "process_cancel_requested";
const PROCESS_CANCEL_CONFIRM_RETRIES: usize = 5;
const PROCESS_CANCEL_CONFIRM_RETRY_DELAY: Duration = Duration::from_millis(100);
/// Wall-clock epoch milliseconds for terminal evidence written at the Restate
/// tier (ADR 0019 recovery enforcement). The Restate boundary carries no
/// injected Lash clock — its durability comes from the engine and workflow-key
/// coalescing rather than a Lash lease — so it reads the system clock directly.
fn restate_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Completion authority for a row the Restate workflow ran itself. Restate's
/// single-writer discipline is per-`process_id` workflow-key coalescing, not a
/// Lash lease (ADR 0027); the workflow key is that `process_id`.
pub(crate) fn workflow_key_authority(process_id: &str) -> ProcessCompletionAuthority {
    ProcessCompletionAuthority::WorkflowKey {
        workflow_key: process_id.to_string(),
    }
}

pub(crate) fn process_segment_workflow_key(process_id: &str, segment_ordinal: u64) -> String {
    if segment_ordinal == 0 {
        process_id.to_string()
    } else {
        format!("{process_id}#{segment_ordinal}")
    }
}

pub(crate) fn terminal_completion_workflow_key(
    process_id: &str,
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

/// Maps an ingress submit failure for `LashProcessWorkflow/run`. A 404 here is
/// a deployment that never bound the process workflow, not a busy engine:
/// terminal by construction, because retrying cannot make an unbound service
/// appear (FIG-1579). Every other failure stays in the retryable ingress class.
pub(crate) fn process_ingress_submit_error(
    process_id: &str,
    err: crate::RestateHttpError,
) -> PluginError {
    if err.is_service_unregistered() {
        PluginError::Runtime(RuntimeError::new(
            RuntimeErrorCode::RestateServiceUnregistered,
            crate::ingress::unregistered_service_message("LashProcessWorkflow", "run", &err),
        ))
    } else {
        PluginError::Runtime(RuntimeError::new(
            RuntimeErrorCode::RestateProcessIngressSubmit,
            format!("ingress submit for process `{process_id}` failed: {err}"),
        ))
    }
}

pub(crate) fn boundary_must_be_declined(record: Option<&ProcessRecord>) -> bool {
    record.is_some_and(|record| record.wait.is_some())
}

pub(crate) fn missing_segment_is_superseded(
    requested_ordinal: u64,
    latest: Option<&lash_core::PersistedSegmentHandover>,
) -> bool {
    latest.is_some_and(|handover| handover.segment_ordinal > requested_ordinal)
}

pub(crate) fn restate_process_terminal_await_key(
    process_id: &str,
) -> Result<AwaitEventKey, RuntimeError> {
    restate_await_event_key(
        &ExecutionScope::process(process_id.to_string()),
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
                lash_core::RuntimeErrorCode::RestateProcessTerminalEncode,
                err.to_string(),
            )
        })
}

pub(crate) fn restate_process_terminal_output(
    process_id: &str,
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
                err.code,
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
    process_id: &str,
    output: &ProcessAwaitOutput,
) -> HandlerResult<()>
where
    C: ContextPromises<'ctx>,
{
    let key = restate_process_terminal_await_key(process_id)
        .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
    let resolution = restate_process_terminal_resolution(output)
        .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
    let payload = serde_json::to_string(&resolution)
        .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
    context.resolve_promise(&key.promise_key(), payload);
    Ok(())
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
pub struct RestateProcessCancelRequest {
    pub process_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[async_trait::async_trait]
pub trait RestateProcessRunner: Send + Sync + 'static {
    async fn run_process_segment(
        &self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        scoped_effect_controller: ScopedEffectController<'_>,
        handover: Option<lash_core::SegmentHandover>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError>;

    async fn request_process_cancel(
        &self,
        request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError>;

    async fn finish_process_parent_end(
        &self,
        plan: lash_core::ProcessParentEndPlan,
        _scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<(), PluginError> {
        if plan.actions.is_empty() {
            Ok(())
        } else {
            Err(PluginError::Session(format!(
                "Restate process runner cannot execute {} retained parent-end actions",
                plan.actions.len()
            )))
        }
    }
}

#[derive(Clone)]
pub struct RestateCoreProcessRunner {
    worker: DurableProcessWorker,
}

impl RestateCoreProcessRunner {
    pub fn new(worker: DurableProcessWorker) -> Self {
        Self { worker }
    }

    pub fn worker(&self) -> &DurableProcessWorker {
        &self.worker
    }
}

#[async_trait::async_trait]
impl RestateProcessRunner for RestateCoreProcessRunner {
    async fn run_process_segment(
        &self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        scoped_effect_controller: ScopedEffectController<'_>,
        handover: Option<lash_core::SegmentHandover>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        let execution_write_authority = execution_context
            .execution_write_authority
            .clone()
            .ok_or_else(|| {
                PluginError::Session(format!(
                    "Restate process `{}` omitted its invocation execution identity",
                    registration.id
                ))
            })?;
        Box::pin(
            self.worker
                .run_process_segment_with_scoped_effect_controller(
                    registration,
                    execution_context,
                    execution_write_authority,
                    scoped_effect_controller,
                    cancellation,
                    handover,
                ),
        )
        .await
    }

    async fn request_process_cancel(
        &self,
        request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        self.worker
            .request_process_cancel(&request.process_id, request.reason)
            .await
    }

    async fn finish_process_parent_end(
        &self,
        plan: lash_core::ProcessParentEndPlan,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<(), PluginError> {
        self.worker
            .execute_parent_end_plan_with_scoped_effect_controller(plan, scoped_effect_controller)
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
}

impl RestateProcessIngressRunner {
    /// Build an ingress-client run handle over the given ingress base URL and
    /// process registry.
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
        }
    }

    /// Report this handle's worker faults to `sink`.
    ///
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
        process_id: &str,
        operation: ProcessRecoveryOperation,
        error: &PluginError,
    ) {
        let fault = ProcessWorkerFault::RecoveryBackendError {
            process_id: process_id.to_string(),
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
        // The record may have reached a terminal state between the list and the submit.
        // Idempotent by process_id: never re-submit a finished process.
        if let Some(current) = self
            .registry
            .get_process(&process_id)
            .await?
            .filter(|current| current.is_terminal())
        {
            return Ok(IngressSubmitOutcome::SettledByPeer(current.status));
        }
        let latest_handover = self
            .continuations
            .latest_segment_handover(&process_id)
            .await?;
        let segment_ordinal = latest_handover
            .as_ref()
            .map_or(0, |handover| handover.segment_ordinal);
        let workflow_key = process_segment_workflow_key(&process_id, segment_ordinal);
        let registration = ProcessRegistration {
            id: record.id,
            input: record.input,
            disposition: record.disposition,
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
                "LashProcessWorkflow",
                &workflow_key,
                "run",
                &RestateProcessWorkflowInput {
                    registration,
                    execution_context,
                    segment_ordinal,

                    // An ingress/sweep submission is a fresh invocation. For a
                    // mid-chain row the handler validates the durable handover,
                    // then binds this invocation as the next execution attempt.
                    execution_id: None,
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
                    id: format!("LashProcessWorkflow/{process_id}"),
                    metadata: Some(serde_json::json!({ "invocation_id": invocation_id })),
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
        process_id: &str,
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
        process_id: &str,
    ) -> Result<ProcessTerminalWait, PluginError> {
        let record = self.registry.get_process(process_id).await?;
        if let Some(output) = record.as_ref().and_then(|record| record.outcome.as_ref()) {
            return Ok(ProcessTerminalWait::Terminal(output.clone()));
        }
        self.ingress
            .call_workflow_json::<_, ProcessAwaitOutput>(
                "LashProcessWorkflow",
                process_id,
                "await_terminal",
                &RestateProcessAwaitRequest {
                    process_id: process_id.to_string(),
                },
            )
            .await
            .map(ProcessTerminalWait::Terminal)
            .or_else(|err| {
                if err.is_timeout() {
                    Ok(ProcessTerminalWait::Reattach)
                } else if err.is_service_unregistered() {
                    // A shared handler, so the 404 has two readings and this
                    // client cannot tell them apart: nothing binds the service,
                    // or nothing is left of this process's invocation.
                    Err(PluginError::Runtime(RuntimeError::new(
                        RuntimeErrorCode::RestateProcessAwait,
                        crate::ingress::unresolvable_call_target_message(
                            "LashProcessWorkflow",
                            "await_terminal",
                            &err,
                        ),
                    )))
                } else {
                    Err(PluginError::Runtime(RuntimeError::new(
                        RuntimeErrorCode::RestateProcessAwait,
                        format!("ingress await for process `{process_id}` failed: {err}"),
                    )))
                }
            })
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
        process_id: &str,
    ) -> Result<ProcessTerminalWait, PluginError> {
        lash_core::facade_support::release_process_execution_permit_while(
            self.await_terminal_wait(process_id),
        )
        .await
    }
}

/// Bundled Restate process deployment wiring for a Lash core.
///
/// Construct this once per deployment, pass [`process_work`](Self::process_work)
/// into `LashCoreBuilder::process_work`, and bind
/// [`workflow`](Self::workflow) on the Restate endpoint.
pub struct RestateProcessDeployment {
    #[cfg(test)]
    process_work: Arc<RestateProcessIngressRunner>,
    wiring: ProcessWorkWiring,
    registry: Arc<dyn ProcessRegistry>,
    ingress: RestateIngressClient,
    continuations: Arc<dyn lash_core::ProcessContinuationStore>,
}

impl RestateProcessDeployment {
    pub fn new(
        connection: impl Into<RestateConnection>,
        registry: Arc<dyn ProcessRegistry>,
        continuations: Arc<dyn lash_core::ProcessContinuationStore>,
    ) -> Self {
        Self::new_with_sink(connection, registry, continuations, None)
    }

    /// Like [`new`](Self::new), but installs a host-facing
    /// [`ProcessEventSink`] on the registry decorator this deployment wraps.
    ///
    /// The wrap happens inside the constructor, so the sink must be supplied
    /// here; each appended event is pushed best-effort after its durable write.
    /// See [`ProcessEventSink`] for the freshness-not-truth contract.
    pub fn new_with_sink(
        connection: impl Into<RestateConnection>,
        registry: Arc<dyn ProcessRegistry>,
        continuations: Arc<dyn lash_core::ProcessContinuationStore>,
        sink: Option<Arc<dyn ProcessEventSink>>,
    ) -> Self {
        let connection = connection.into();
        let fault_sink = sink.clone();
        let watched = watch_process_registry_with_sink(registry, sink);
        let registry = Arc::clone(watched.registry());
        let ingress_runner = Arc::new(
            RestateProcessIngressRunner::new(
                connection.clone(),
                Arc::clone(&registry),
                Arc::clone(&continuations),
            )
            .with_event_sink(fault_sink),
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
        }
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

    pub fn workflow(
        &self,
        worker: DurableProcessWorker,
    ) -> LashProcessWorkflowImpl<RestateCoreProcessRunner> {
        let trace_sink = worker.config().runtime_host.tracing.trace_sink.clone();
        let trace_context = worker.config().runtime_host.tracing.trace_context.clone();
        let workflow = LashProcessWorkflowImpl::new(
            Arc::new(RestateCoreProcessRunner::new(worker)),
            Arc::clone(&self.registry),
            Arc::clone(&self.continuations),
            self.ingress.clone(),
        );
        if let Some(sink) = trace_sink {
            workflow.with_trace_sink(sink, trace_context)
        } else {
            workflow
        }
    }
}
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateProcessWorkflowInput {
    pub registration: ProcessRegistration,
    #[serde(default, skip_serializing_if = "ProcessExecutionContext::is_empty")]
    pub execution_context: ProcessExecutionContext,
    #[serde(default)]
    pub segment_ordinal: u64,
    /// Root Restate invocation id for this execution attempt. Segment
    /// successors carry it forward so a process chain remains one attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
}

pub(crate) fn segment_execution_authority(
    process_id: &str,
    segment_ordinal: u64,
    carried_execution_id: Option<&str>,
    invocation_id: &str,
    retained_start: Option<&lash_core::ProcessStarted>,
) -> Result<(String, lash_core::ProcessExecutionWriteAuthority), TerminalError> {
    if segment_ordinal == 0 {
        let execution_id = invocation_id.to_string();
        return Ok((
            execution_id.clone(),
            lash_core::ProcessExecutionWriteAuthority::invocation(process_id, execution_id),
        ));
    }

    let retained_start = retained_start.ok_or_else(|| {
        TerminalError::new(format!(
            "process `{process_id}` segment {segment_ordinal} has a handover without a retained execution start"
        ))
    })?;
    if let Some(carried_execution_id) = carried_execution_id {
        let retained_execution_id = retained_start
            .owner
            .restate_process_execution_id(process_id)
            .ok_or_else(|| {
                TerminalError::new(format!(
                    "process `{process_id}` segment {segment_ordinal} retained a non-Restate execution owner"
                ))
            })?;
        if carried_execution_id != retained_execution_id {
            return Err(TerminalError::new(format!(
                "process `{process_id}` segment {segment_ordinal} carried execution `{carried_execution_id}` but retained execution is `{retained_execution_id}`"
            )));
        }
        let execution_id = carried_execution_id.to_string();
        return Ok((
            execution_id.clone(),
            lash_core::ProcessExecutionWriteAuthority::invocation(process_id, execution_id),
        ));
    }

    let execution_id = invocation_id.to_string();
    Ok((
        execution_id.clone(),
        lash_core::ProcessExecutionWriteAuthority::invocation_resume(
            process_id,
            execution_id,
            retained_start.clone(),
        ),
    ))
}

#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RestateProcessWorkflowOutput {
    Terminal { output: Box<ProcessAwaitOutput> },
    SegmentChained { next_segment_ordinal: u64 },
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateProcessCompleteRequest {
    pub process_id: String,
    pub output: ProcessAwaitOutput,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateProcessAwaitRequest {
    pub process_id: String,
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
