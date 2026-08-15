//! The `LashProcessWorkflow` segment executor.
//!
//! One responsibility: run exactly one process segment inside a Restate
//! workflow invocation — validate the durable handover, bind this invocation as
//! the execution authority, drive the runner against a live cancellation
//! observer, and deliver either a terminal outcome or a segment successor.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use lash_core::{
    AbandonEvidence, AbandonWriter, ExecutionScope, PluginError, ProcessAwaitOutput,
    ProcessExecutionContext, ProcessRegistration, ProcessRegistry, ScopedEffectController,
};
use restate_sdk::context::{
    ContextClient, ContextPromises, InvocationHandle, RequestTarget, SharedWorkflowContext,
    WorkflowContext,
};
use restate_sdk::errors::{HandlerError, HandlerResult, TerminalError};
use restate_sdk::serde::Json;

use super::{
    PROCESS_CANCEL_CONFIRM_RETRIES, PROCESS_CANCEL_CONFIRM_RETRY_DELAY, PROCESS_CANCEL_PROMISE_KEY,
    RestateProcessAwaitRequest, RestateProcessCancelRequest, RestateProcessCancelSignal,
    RestateProcessCompleteRequest, RestateProcessRunner, RestateProcessWorkflowInput,
    RestateProcessWorkflowOutput, boundary_must_be_declined, missing_segment_is_superseded,
    process_segment_workflow_key, resolve_process_cancel_signal, resolve_process_terminal_promise,
    restate_now_ms, restate_process_terminal_await_key, restate_process_terminal_output,
    retryable_registry_error, segment_execution_authority, terminal_completion_workflow_key,
    validate_segment_program_hash, workflow_key_authority,
};
use crate::controller::{RestateEffectControllerOptions, RestateRuntimeEffectController};
use crate::ingress::RestateIngressClient;

#[restate_sdk::workflow]
pub trait LashProcessWorkflow {
    async fn run(
        input: Json<RestateProcessWorkflowInput>,
    ) -> HandlerResult<Json<RestateProcessWorkflowOutput>>;

    #[shared]
    async fn complete_terminal(
        request: Json<RestateProcessCompleteRequest>,
    ) -> HandlerResult<Json<()>>;

    #[shared]
    async fn await_terminal(
        request: Json<RestateProcessAwaitRequest>,
    ) -> HandlerResult<Json<ProcessAwaitOutput>>;

    #[shared]
    async fn cancel(request: Json<RestateProcessCancelRequest>) -> HandlerResult<Json<()>>;

    #[shared]
    async fn deliver_cancel(request: Json<RestateProcessCancelRequest>) -> HandlerResult<Json<()>>;

    #[shared]
    async fn await_cancel(
        request: Json<RestateProcessAwaitRequest>,
    ) -> HandlerResult<Json<RestateProcessCancelSignal>>;
}
pub struct LashProcessWorkflowImpl<R> {
    runner: Arc<R>,
    registry: Arc<dyn ProcessRegistry>,
    continuations: Arc<dyn lash_core::ProcessContinuationStore>,
    segment_duration_cap: Option<Duration>,
    segment_effect_budget: Arc<dyn Fn(&ProcessRegistration) -> u64 + Send + Sync>,
    cancel_ingress: Option<RestateIngressClient>,
    trace_sink: Option<Arc<dyn lash_trace::TraceSink>>,
    trace_context: lash_trace::TraceContext,
    #[cfg(test)]
    cancel_read_failures: std::sync::atomic::AtomicUsize,
}

impl<R> LashProcessWorkflowImpl<R> {
    /// Build a Restate process workflow whose live cancellation observer calls
    /// back through the ingress to await its durable cancellation promise.
    pub fn new(
        runner: Arc<R>,
        registry: Arc<dyn ProcessRegistry>,
        continuations: Arc<dyn lash_core::ProcessContinuationStore>,
        cancel_ingress: RestateIngressClient,
    ) -> Self {
        Self::new_inner(runner, registry, continuations, Some(cancel_ingress))
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        runner: Arc<R>,
        registry: Arc<dyn ProcessRegistry>,
        continuations: Arc<dyn lash_core::ProcessContinuationStore>,
    ) -> Self {
        Self::new_inner(runner, registry, continuations, None)
    }

    fn new_inner(
        runner: Arc<R>,
        registry: Arc<dyn ProcessRegistry>,
        continuations: Arc<dyn lash_core::ProcessContinuationStore>,
        cancel_ingress: Option<RestateIngressClient>,
    ) -> Self {
        Self {
            runner,
            registry,
            continuations,
            segment_duration_cap: None,
            segment_effect_budget: Arc::new(|_| 10_000),
            cancel_ingress,
            trace_sink: None,
            trace_context: lash_trace::TraceContext::default(),
            #[cfg(test)]
            cancel_read_failures: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn with_segment_duration_cap(mut self, cap: Duration) -> Self {
        self.segment_duration_cap = Some(cap);
        self
    }

    /// Attach the host's live trace observer to every process-segment
    /// controller created by this workflow.
    pub fn with_trace_sink(
        mut self,
        sink: Arc<dyn lash_trace::TraceSink>,
        context: lash_trace::TraceContext,
    ) -> Self {
        self.trace_sink = Some(sink);
        self.trace_context = context;
        self
    }

    /// Select a deterministic completed-effect budget from immutable process
    /// registration data. This is primarily useful for conformance/e2e pairs
    /// that run the same artifact with and without forced segmentation; the
    /// production default remains 10,000 completed effects per incarnation.
    pub fn with_segment_effect_budget_selector(
        mut self,
        selector: impl Fn(&ProcessRegistration) -> u64 + Send + Sync + 'static,
    ) -> Self {
        self.segment_effect_budget = Arc::new(selector);
        self
    }

    pub(crate) fn cancellation_signal(
        &self,
        process_id: &str,
        segment_ordinal: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), HandlerError>> + Send>> {
        let Some(ingress) = self.cancel_ingress.clone() else {
            return Box::pin(std::future::pending());
        };
        let workflow_key = process_segment_workflow_key(process_id, segment_ordinal);
        let request = RestateProcessAwaitRequest {
            process_id: process_id.to_string(),
        };
        Box::pin(async move {
            loop {
                match ingress
                    .call_workflow_json::<_, RestateProcessCancelSignal>(
                        "LashProcessWorkflow",
                        &workflow_key,
                        "await_cancel",
                        &request,
                    )
                    .await
                {
                    Ok(RestateProcessCancelSignal::CancelRequested) => return Ok(()),
                    Ok(RestateProcessCancelSignal::SegmentFinished) => {
                        return std::future::pending::<Result<(), HandlerError>>().await;
                    }
                    Err(error) if error.is_timeout() => {
                        // The attach ceiling bounds one transport connection, not the
                        // durable watch. Re-attach to heal a dead or aged connection.
                    }
                    Err(error) => return Err(HandlerError::from(error)),
                }
            }
        })
    }

    #[cfg(test)]
    pub(crate) fn fail_next_cancel_reads(&self, count: usize) {
        self.cancel_read_failures
            .store(count, std::sync::atomic::Ordering::SeqCst);
    }
}

impl<R> LashProcessWorkflowImpl<R>
where
    R: RestateProcessRunner,
{
    async fn finish_terminal_with_parent_end(
        &self,
        process_id: &str,
        output: Box<ProcessAwaitOutput>,
        actions: Vec<lash_core::ToolIntentParentEndAction>,
        parent_end_controller: ScopedEffectController<'_>,
    ) -> Result<lash_core::ProcessRunOutcome, HandlerError> {
        let stored = self
            .complete_with_stored_outcome_and_parent_end(
                process_id,
                (*output).clone(),
                actions.clone(),
            )
            .await
            .map_err(retryable_registry_error)?;
        if !actions.is_empty() {
            self.runner
                .finish_process_parent_end(
                    lash_core::ProcessParentEndPlan {
                        process_id: process_id.to_string(),
                        actions,
                    },
                    parent_end_controller,
                )
                .await
                .map_err(retryable_registry_error)?;
        }
        Ok(lash_core::ProcessRunOutcome::Terminal(Box::new(stored)))
    }

    pub(crate) async fn complete_with_stored_outcome(
        &self,
        process_id: &str,
        proposed: ProcessAwaitOutput,
    ) -> Result<ProcessAwaitOutput, PluginError> {
        self.complete_with_stored_outcome_and_parent_end(process_id, proposed, Vec::new())
            .await
    }

    pub(crate) async fn complete_with_stored_outcome_and_parent_end(
        &self,
        process_id: &str,
        proposed: ProcessAwaitOutput,
        actions: Vec<lash_core::ToolIntentParentEndAction>,
    ) -> Result<ProcessAwaitOutput, PluginError> {
        let completion = self
            .registry
            .complete_process_with_parent_end(
                process_id,
                proposed,
                workflow_key_authority(process_id),
                actions,
            )
            .await?;
        let record = match completion {
            lash_core::ProcessCompletionOutcome::Committed(record) => record,
            lash_core::ProcessCompletionOutcome::AlreadyApplied { stored }
            | lash_core::ProcessCompletionOutcome::Superseded { stored } => stored,
        };
        record.outcome.ok_or_else(|| {
            PluginError::Session(format!(
                "process `{process_id}` completion returned a non-terminal record"
            ))
        })
    }

    pub(crate) async fn run_registration<F>(
        &self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        scoped_effect_controller: ScopedEffectController<'_>,
        segment_ordinal: u64,
        handover: Option<lash_core::SegmentHandover>,
        cancellation_signal: F,
    ) -> Result<lash_core::ProcessRunOutcome, HandlerError>
    where
        F: Future<Output = Result<(), HandlerError>>,
    {
        let process_id = registration.id.clone();
        if segment_ordinal > 0 && handover.is_none() {
            return Err(HandlerError::from(TerminalError::new(format!(
                "process `{process_id}` segment {segment_ordinal} omitted its validated handover"
            ))));
        }
        let cancellation = tokio_util::sync::CancellationToken::new();
        let parent_end_controller = scoped_effect_controller.clone();
        let runner = self.runner.run_process_segment(
            registration,
            execution_context,
            scoped_effect_controller,
            handover,
            cancellation.clone(),
        );
        tokio::pin!(runner);
        tokio::pin!(cancellation_signal);
        let outcome = tokio::select! {
            biased;
            signal = &mut cancellation_signal => {
                signal?;
                cancellation.cancel();
                self.confirm_process_cancel_requested(&process_id).await?;
                let _ = runner.await;
                Ok(lash_core::ProcessRunOutcome::Terminal(Box::new(
                    ProcessAwaitOutput::Cancelled {
                        message: format!("process `{process_id}` was cancelled"),
                        raw: None,
                        control: None,
                    },
                )))
            }
            outcome = &mut runner => outcome
        };
        match outcome {
            Ok(lash_core::ProcessRunOutcome::Terminal(output)) => {
                self.finish_terminal_with_parent_end(
                    &process_id,
                    output,
                    Vec::new(),
                    parent_end_controller,
                )
                .await
            }
            Ok(lash_core::ProcessRunOutcome::TerminalWithParentEnd { output, actions }) => {
                self.finish_terminal_with_parent_end(
                    &process_id,
                    output,
                    actions,
                    parent_end_controller,
                )
                .await
            }
            Ok(boundary @ lash_core::ProcessRunOutcome::SegmentBoundary(_)) => Ok(boundary),
            Err(PluginError::ProcessAlreadyStarted { by, .. }) => {
                let output = self
                    .complete_with_stored_outcome(
                        &process_id,
                        ProcessAwaitOutput::Abandoned {
                            evidence: Box::new(AbandonEvidence {
                                writer: AbandonWriter::Sweep,
                                owner: Some(*by),
                                epoch_ms: restate_now_ms(),
                            }),
                            control: None,
                        },
                    )
                    .await
                    .map_err(retryable_registry_error)?;
                Ok(output.into())
            }
            Err(PluginError::ProcessAttemptsExhausted { .. }) => {
                let owner = self
                    .registry
                    .get_process(&process_id)
                    .await
                    .map_err(retryable_registry_error)?
                    .and_then(|record| record.first_started.map(|started| started.owner.clone()));
                let output = self
                    .complete_with_stored_outcome(
                        &process_id,
                        ProcessAwaitOutput::Abandoned {
                            evidence: Box::new(AbandonEvidence {
                                writer: AbandonWriter::EngineGaveUp,
                                owner,
                                epoch_ms: restate_now_ms(),
                            }),
                            control: None,
                        },
                    )
                    .await
                    .map_err(retryable_registry_error)?;
                Ok(output.into())
            }
            Err(err) => Err(retryable_registry_error(err)),
        }
    }

    pub(crate) async fn process_cancel_requested(
        &self,
        process_id: &str,
    ) -> Result<bool, PluginError> {
        #[cfg(test)]
        if self
            .cancel_read_failures
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |remaining| remaining.checked_sub(1),
            )
            .is_ok()
        {
            return Err(PluginError::Session(
                "simulated transient cancel registry read failure".to_string(),
            ));
        }
        Ok(self
            .registry
            .events_after(process_id, 0)
            .await?
            .iter()
            .any(|event| event.event_type == "process.cancel_requested"))
    }

    async fn confirm_process_cancel_requested(&self, process_id: &str) -> Result<(), HandlerError> {
        let mut attempt = 0;
        loop {
            match self.process_cancel_requested(process_id).await {
                Ok(true) => return Ok(()),
                Ok(false) => {
                    // With Restate's single-primary registry topology this is a
                    // deterministic contract violation. On a replica-read topology
                    // the same observation would instead be read lag and retryable.
                    return Err(TerminalError::new(format!(
                        "process `{process_id}` cancellation promise resolved without a durable process.cancel_requested event"
                    ))
                    .into());
                }
                Err(error) => {
                    if attempt < PROCESS_CANCEL_CONFIRM_RETRIES {
                        attempt += 1;
                        tokio::time::sleep(PROCESS_CANCEL_CONFIRM_RETRY_DELAY).await;
                    } else {
                        return Err(retryable_registry_error(PluginError::Session(format!(
                            "process `{process_id}` cancellation confirmation failed after {} attempts: {error}",
                            PROCESS_CANCEL_CONFIRM_RETRIES + 1
                        ))));
                    }
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn confirm_process_cancel_requested_for_test(
        &self,
        process_id: &str,
    ) -> Result<(), HandlerError> {
        self.confirm_process_cancel_requested(process_id).await
    }

    async fn record_cancel_requested(
        &self,
        request: &RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        self.registry
            .append_event(
                &request.process_id,
                lash_core::ProcessEventAppendRequest::cancel_requested(
                    &request.process_id,
                    request.reason.clone(),
                ),
            )
            .await
            .map(|_| ())
    }

    #[cfg(test)]
    pub(crate) async fn cancel_registration(
        &self,
        request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        self.record_cancel_requested(&request).await?;
        self.runner.request_process_cancel(request).await
    }
}

impl<R> LashProcessWorkflow for LashProcessWorkflowImpl<R>
where
    R: RestateProcessRunner,
{
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<RestateProcessWorkflowInput>,
    ) -> HandlerResult<Json<RestateProcessWorkflowOutput>> {
        let process_id = input.registration.id.clone();
        let record = self
            .registry
            .get_process(&process_id)
            .await
            .map_err(HandlerError::from)?
            .ok_or_else(|| {
                HandlerError::from(TerminalError::new(format!(
                    "unknown process `{process_id}`"
                )))
            })?;
        // FIG-788: a terminal outcome can land after an attempt has emitted
        // runner commands but before its handler output is committed. Never
        // branch around the runner from this non-journaled read: redrive must
        // reconstruct the deployed command prefix, and idempotent completion
        // below returns the already-stored terminal outcome. `first_started` is
        // immutable execution authority. PR #170 observer removal cannot
        // terminalize a process. Terminal pruning remains a host-owned exposure:
        // the raw cutoff has no finite workflow-lifetime bound to validate
        // against, so a host must retain this row while the workflow can replay.
        let mut handover = if input.segment_ordinal == 0 {
            None
        } else {
            let persisted = self
                .continuations
                .get_segment_handover(&process_id, input.segment_ordinal)
                .await
                .map_err(HandlerError::from)?;
            let Some(persisted) = persisted else {
                let latest = self
                    .continuations
                    .latest_segment_handover(&process_id)
                    .await
                    .map_err(HandlerError::from)?;
                if let Some(output) = record.outcome.clone() {
                    // FIG-811: compatibility for a terminal segment whose
                    // handover was deleted by a pre-lazy-cleanup deployment.
                    // This suffix is a complete replay only when that attempt
                    // emitted no runner commands before terminal delivery.
                    // Current deployments retain handovers until process
                    // pruning, so effectful attempts replay through the runner.
                    resolve_process_cancel_signal(
                        &ctx,
                        RestateProcessCancelSignal::SegmentFinished,
                    )?;
                    let request: restate_sdk::context::Request<
                        '_,
                        Json<RestateProcessCompleteRequest>,
                        Json<()>,
                    > = ContextClient::request(
                        &ctx,
                        RequestTarget::workflow(
                            "LashProcessWorkflow",
                            process_id.clone(),
                            "complete_terminal",
                        ),
                        Json(RestateProcessCompleteRequest {
                            process_id: process_id.clone(),
                            output: output.clone(),
                        }),
                    );
                    request.call().await?;
                    return Ok(Json(RestateProcessWorkflowOutput::Terminal {
                        output: Box::new(output),
                    }));
                }
                if missing_segment_is_superseded(input.segment_ordinal, latest.as_ref()) {
                    let latest = latest.as_ref().ok_or_else(|| {
                        HandlerError::from(TerminalError::new(format!(
                            "process `{process_id}` segment {} was classified as superseded without a latest handover",
                            input.segment_ordinal
                        )))
                    })?;
                    tracing::debug!(
                        process_id,
                        segment_ordinal = input.segment_ordinal,
                        latest_segment_ordinal = latest.segment_ordinal,
                        "ignoring retried superseded process segment"
                    );
                    return Ok(Json(RestateProcessWorkflowOutput::SegmentChained {
                        next_segment_ordinal: latest.segment_ordinal,
                    }));
                }
                return Err(HandlerError::from(TerminalError::new(format!(
                    "missing persisted handover for process `{process_id}` segment {}",
                    input.segment_ordinal
                ))));
            };
            match validate_segment_program_hash(&process_id, persisted) {
                Ok(handover) => Some(handover),
                Err(err) => {
                    let output = ProcessAwaitOutput::Failure {
                        class: lash_core::ToolFailureClass::Execution,
                        code: err.code.to_string(),
                        message: err.message.clone(),
                        raw: None,
                        control: None,
                    };
                    let output = self
                        .complete_with_stored_outcome(&process_id, output)
                        .await
                        .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
                    let request: restate_sdk::context::Request<
                        '_,
                        Json<RestateProcessCompleteRequest>,
                        Json<()>,
                    > = ContextClient::request(
                        &ctx,
                        RequestTarget::workflow(
                            "LashProcessWorkflow",
                            process_id.clone(),
                            "complete_terminal",
                        ),
                        Json(RestateProcessCompleteRequest {
                            process_id: process_id.clone(),
                            output: output.clone(),
                        }),
                    );
                    request.call().await?;
                    return Ok(Json(RestateProcessWorkflowOutput::Terminal {
                        output: Box::new(output),
                    }));
                }
            }
        };
        if input.segment_ordinal == 0 && input.execution_id.is_some() {
            tracing::warn!(
                process_id,
                presented_execution_id = input.execution_id.as_deref(),
                invocation_id = %ctx.invocation_id(),
                verdict = "ignored",
                "segment-zero execution identity derives exclusively from the Restate invocation"
            );
        }
        let (execution_id, execution_write_authority) = segment_execution_authority(
            &process_id,
            input.segment_ordinal,
            input.execution_id.as_deref(),
            ctx.invocation_id(),
            record.first_started.as_deref(),
        )
        .map_err(HandlerError::from)?;
        let mut options = RestateEffectControllerOptions::default()
            .segment_effect_budget((self.segment_effect_budget)(&input.registration));
        if let Some(cap) = self.segment_duration_cap {
            options = options.segment_duration_cap(cap);
        }
        let controller = RestateRuntimeEffectController::with_options(ctx, options);
        let controller = if let Some(sink) = self.trace_sink.as_ref() {
            controller.with_trace_sink_and_context(Arc::clone(sink), self.trace_context.clone())
        } else {
            controller
        };
        let outcome = loop {
            let scoped_effect_controller = controller
                .scoped_effect_controller(ExecutionScope::process(process_id.clone()))
                .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
            let cancel_signal = self.cancellation_signal(&process_id, input.segment_ordinal);
            let outcome = self
                .run_registration(
                    input.registration.clone(),
                    input
                        .execution_context
                        .clone()
                        .with_execution_write_authority(execution_write_authority.clone()),
                    scoped_effect_controller,
                    input.segment_ordinal,
                    handover,
                    cancel_signal,
                )
                .await?;
            if let lash_core::ProcessRunOutcome::SegmentBoundary(boundary) = &outcome {
                let current = self
                    .registry
                    .get_process(&process_id)
                    .await
                    .map_err(retryable_registry_error)?;
                if boundary_must_be_declined(current.as_ref()) {
                    handover = Some(boundary.clone());
                    continue;
                }
            }
            break outcome;
        };
        resolve_process_cancel_signal(
            controller.context(),
            RestateProcessCancelSignal::SegmentFinished,
        )?;
        match outcome {
            lash_core::ProcessRunOutcome::Terminal(output)
            | lash_core::ProcessRunOutcome::TerminalWithParentEnd { output, .. } => {
                let output = *output;
                if terminal_completion_workflow_key(&process_id, input.segment_ordinal).is_none() {
                    resolve_process_terminal_promise(controller.context(), &process_id, &output)?;
                } else {
                    let request: restate_sdk::context::Request<
                        '_,
                        Json<RestateProcessCompleteRequest>,
                        Json<()>,
                    > = ContextClient::request(
                        controller.context(),
                        RequestTarget::workflow(
                            "LashProcessWorkflow",
                            process_id.clone(),
                            "complete_terminal",
                        ),
                        Json(RestateProcessCompleteRequest {
                            process_id: process_id.clone(),
                            output: output.clone(),
                        }),
                    );
                    request.call().await?;
                }

                // FIG-811: the handover remains replay authority until the
                // terminal process reaches host-owned retention pruning. A
                // redrive after delivery can therefore reproduce every runner
                // command before idempotently repeating this terminal suffix.
                Ok(Json(RestateProcessWorkflowOutput::Terminal {
                    output: Box::new(output),
                }))
            }
            lash_core::ProcessRunOutcome::SegmentBoundary(handover) => {
                let next_segment_ordinal = input.segment_ordinal.saturating_add(1);
                self.continuations
                    .put_segment_handover(
                        &process_id,
                        lash_core::PersistedSegmentHandover {
                            segment_ordinal: next_segment_ordinal,
                            program_hash: handover.program_hash.clone().ok_or_else(|| {
                                HandlerError::from(TerminalError::new(format!(
                                    "process `{process_id}` segment handover omitted its program identity"
                                )))
                            })?,
                            handover,
                        },
                    )
                    .await
                    .map_err(HandlerError::from)?;
                let successor_key = process_segment_workflow_key(&process_id, next_segment_ordinal);
                // FIG-788: successor emission is unconditional. A cancellation
                // event can land between attempts, so the append-only registry
                // read below may shape only commands after this deployed prefix.
                let request: restate_sdk::context::Request<
                    '_,
                    Json<RestateProcessWorkflowInput>,
                    Json<RestateProcessWorkflowOutput>,
                > = ContextClient::request(
                    controller.context(),
                    RequestTarget::workflow("LashProcessWorkflow", successor_key.clone(), "run"),
                    Json(RestateProcessWorkflowInput {
                        registration: input.registration,
                        execution_context: input.execution_context,
                        segment_ordinal: next_segment_ordinal,
                        execution_id: Some(execution_id),
                    }),
                );
                let _ = request.send().invocation_id().await?;
                if self
                    .process_cancel_requested(&process_id)
                    .await
                    .map_err(HandlerError::from)?
                {
                    // Cancellation can race the gap after the current segment
                    // retires its promise but before the successor handover is
                    // visible to the cancel endpoint. Forward that durable fact
                    // after scheduling so the successor owns terminalization.
                    let deliver: restate_sdk::context::Request<
                        '_,
                        Json<RestateProcessCancelRequest>,
                        Json<()>,
                    > = ContextClient::request(
                        controller.context(),
                        RequestTarget::workflow(
                            "LashProcessWorkflow",
                            successor_key,
                            "deliver_cancel",
                        ),
                        Json(RestateProcessCancelRequest {
                            process_id: process_id.clone(),
                            reason: Some("process cancelled between segments".to_string()),
                        }),
                    );
                    let Json(()) = deliver.call().await?;
                }
                Ok(Json(RestateProcessWorkflowOutput::SegmentChained {
                    next_segment_ordinal,
                }))
            }
        }
    }

    async fn complete_terminal(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(request): Json<RestateProcessCompleteRequest>,
    ) -> HandlerResult<Json<()>> {
        resolve_process_terminal_promise(&ctx, &request.process_id, &request.output)?;
        Ok(Json(()))
    }

    async fn cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(request): Json<RestateProcessCancelRequest>,
    ) -> HandlerResult<Json<()>> {
        self.record_cancel_requested(&request)
            .await
            .map_err(retryable_registry_error)?;
        resolve_process_cancel_signal(&ctx, RestateProcessCancelSignal::CancelRequested)?;

        if let Some(handover) = self
            .continuations
            .latest_segment_handover(&request.process_id)
            .await
            .map_err(retryable_registry_error)?
            && handover.segment_ordinal > 0
        {
            let deliver: restate_sdk::context::Request<
                '_,
                Json<RestateProcessCancelRequest>,
                Json<()>,
            > = ContextClient::request(
                &ctx,
                RequestTarget::workflow(
                    "LashProcessWorkflow",
                    process_segment_workflow_key(&request.process_id, handover.segment_ordinal),
                    "deliver_cancel",
                ),
                Json(request.clone()),
            );
            let Json(()) = deliver.call().await?;
        }
        self.runner
            .request_process_cancel(request)
            .await
            .map_err(retryable_registry_error)?;
        Ok(Json(()))
    }

    async fn deliver_cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(_request): Json<RestateProcessCancelRequest>,
    ) -> HandlerResult<Json<()>> {
        resolve_process_cancel_signal(&ctx, RestateProcessCancelSignal::CancelRequested)?;
        Ok(Json(()))
    }

    async fn await_cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(_request): Json<RestateProcessAwaitRequest>,
    ) -> HandlerResult<Json<RestateProcessCancelSignal>> {
        let payload = ctx.promise::<String>(PROCESS_CANCEL_PROMISE_KEY).await?;
        let signal = serde_json::from_str(&payload)
            .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
        Ok(Json(signal))
    }

    async fn await_terminal(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(request): Json<RestateProcessAwaitRequest>,
    ) -> HandlerResult<Json<ProcessAwaitOutput>> {
        let key = restate_process_terminal_await_key(&request.process_id)
            .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
        let promise_key = key.promise_key();
        let payload = ctx.promise::<String>(&promise_key).await?;
        let resolution = serde_json::from_str(&payload)
            .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
        let output = restate_process_terminal_output(&request.process_id, resolution)
            .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
        Ok(Json(output))
    }
}
