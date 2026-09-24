#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API while its replacement is staged"
)]

//! The `LashProcessWorkflow` segment executor.
//!
//! One responsibility: run exactly one process segment inside a Restate
//! workflow invocation — validate the durable handover, bind this invocation as
//! the execution authority, drive the runner against a live cancellation
//! observer, and deliver either a terminal outcome or a segment successor.

use lash_sansio::ProcessId;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use lash_core::{
    AbandonEvidence, AbandonWriter, PluginError, ProcessAwaitOutput, ProcessExecutionContext,
    ProcessRegistration, ProcessRegistry, ScopedEffectController,
};
use restate_sdk::context::{
    ContextClient, ContextPromises, SharedWorkflowContext, WorkflowContext,
};
use restate_sdk::errors::{HandlerError, HandlerResult, TerminalError};
use restate_sdk::serde::Json;

use super::{
    PROCESS_CANCEL_CONFIRM_RETRIES, PROCESS_CANCEL_CONFIRM_RETRY_DELAY, PROCESS_CANCEL_PROMISE_KEY,
    RESTATE_PROCESS_JOURNAL_VERSION, RestateProcessAwaitRequest, RestateProcessCancelRequest,
    RestateProcessCancelSignal, RestateProcessCompleteRequest, RestateProcessRunner,
    RestateProcessWorkflowInput, RestateProcessWorkflowOutput, SegmentAdmission, SegmentStarted,
    admit_segment, boundary_must_be_declined, handler_error_from_plugin, is_replay_mismatch,
    missing_segment_is_superseded, process_segment_workflow_key, resolve_process_cancel_signal,
    resolve_process_terminal_promise, restate_now_ms, restate_process_terminal_await_key,
    restate_process_terminal_output, terminal_completion_workflow_key, terminal_process_output,
    workflow_key_authority,
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
    authority_id: crate::RestateAuthorityId,
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
        authority_id: crate::RestateAuthorityId,
    ) -> Self {
        Self::new_inner(
            runner,
            registry,
            continuations,
            Some(cancel_ingress),
            authority_id,
        )
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        runner: Arc<R>,
        registry: Arc<dyn ProcessRegistry>,
        continuations: Arc<dyn lash_core::ProcessContinuationStore>,
    ) -> Self {
        Self::new_inner(
            runner,
            registry,
            continuations,
            None,
            crate::RestateAuthorityId::new("lash-restate-tests").expect("valid test authority"),
        )
    }

    fn new_inner(
        runner: Arc<R>,
        registry: Arc<dyn ProcessRegistry>,
        continuations: Arc<dyn lash_core::ProcessContinuationStore>,
        cancel_ingress: Option<RestateIngressClient>,
        authority_id: crate::RestateAuthorityId,
    ) -> Self {
        Self {
            runner,
            registry,
            continuations,
            segment_duration_cap: None,
            segment_effect_budget: Arc::new(|_| 10_000),
            cancel_ingress,
            authority_id,
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
        process_id: &ProcessId,
        segment_ordinal: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), HandlerError>> + Send>> {
        let Some(ingress) = self.cancel_ingress.clone() else {
            return Box::pin(std::future::pending());
        };
        let workflow_key = process_segment_workflow_key(process_id, segment_ordinal);
        let request = RestateProcessAwaitRequest {
            process_id: ProcessId::from(process_id.to_string()),
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
                    Err(error) if error.is_service_unregistered() => {
                        // The one failure in this loop that retrying cannot fix:
                        // nothing binds the workflow this watch addresses. A
                        // plain handler error here is retryable, so the engine
                        // would back this invocation off forever against a
                        // missing `bind` — the indefinite-retry-on-an-
                        // unregistered-handler shape FIG-1579 rules out. It
                        // leaves as the engine's own 404-class terminal instead.
                        return Err(crate::ingress::unregistered_service_terminal(
                            "LashProcessWorkflow",
                            "await_cancel",
                            &error,
                        )
                        .into());
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
    /// Publish a terminal this segment reached: the root segment resolves the
    /// process's terminal promise itself, a later segment completes it on the
    /// root workflow.
    async fn deliver_segment_terminal(
        &self,
        context: &WorkflowContext<'_>,
        process_id: &ProcessId,
        segment_ordinal: u64,
        output: ProcessAwaitOutput,
    ) -> HandlerResult<Json<RestateProcessWorkflowOutput>> {
        if terminal_completion_workflow_key(process_id, segment_ordinal).is_none() {
            resolve_process_terminal_promise(context, &self.authority_id, process_id, &output)?;
        } else {
            let request = context
                .workflow_client::<LashProcessWorkflowClient>(process_id.clone())
                .complete_terminal(Json(RestateProcessCompleteRequest {
                    process_id: process_id.clone(),
                    output: output.clone(),
                }));
            request.call().await?;
        }

        // FIG-811: the handover remains replay authority until the terminal
        // process reaches host-owned retention pruning. A redrive after
        // delivery can therefore reproduce every runner command before
        // idempotently repeating this terminal suffix.
        Ok(Json(RestateProcessWorkflowOutput::Terminal {
            output: Box::new(output),
        }))
    }

    /// Refuse an input built for another generation of the handler's command
    /// prefix, before journaling anything: the process ends Abandoned with
    /// `ResumeRefused { RetiredGeneration }` naming the generation it
    /// carried, and the invocation fails terminally without a journal
    /// command, so a journal recorded under that generation is never replayed
    /// against this one (FIG-3588, current temporary cutover policy).
    async fn refuse_retired_journal(
        &self,
        process_id: &ProcessId,
        journal_version: u32,
    ) -> HandlerResult<Json<RestateProcessWorkflowOutput>> {
        let found = format!("restate-process-journal-v{journal_version}");
        tracing::warn!(
            process_id = process_id.as_str(),
            found = found.as_str(),
            expected = RESTATE_PROCESS_JOURNAL_VERSION,
            "refusing a process segment built for a retired journal generation"
        );
        self.complete_with_stored_outcome(
            process_id,
            ProcessAwaitOutput::Abandoned {
                evidence: Box::new(AbandonEvidence {
                    writer: AbandonWriter::ResumeRefused {
                        reason: lash_core::ProcessResumeRefusal::RetiredGeneration {
                            found: found.clone(),
                        },
                    },
                    owner: None,
                    epoch_ms: restate_now_ms(),
                }),
                control: None,
            },
        )
        .await
        .map_err(handler_error_from_plugin)?;
        Err(TerminalError::new(format!(
            "process `{process_id}` segment input carries {found}; this handler journals generation {RESTATE_PROCESS_JOURNAL_VERSION}"
        ))
        .into())
    }

    pub(crate) async fn complete_with_stored_outcome(
        &self,
        process_id: &ProcessId,
        proposed: ProcessAwaitOutput,
    ) -> Result<ProcessAwaitOutput, PluginError> {
        let completion = self
            .registry
            .complete_process(process_id, proposed, workflow_key_authority(process_id))
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

    /// [`run_registration`](Self::run_registration) for a test that drives a
    /// segment without the handler's admission: the segment proof is minted
    /// from the scope and the execution authority the test supplies.
    #[cfg(test)]
    pub(crate) async fn run_registration_for_test<F>(
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
        let started = SegmentStarted::for_test(
            scoped_effect_controller.admitted_scope().clone(),
            segment_ordinal,
            execution_context.execution_write_authority.clone(),
        );
        self.run_registration(
            registration,
            execution_context,
            scoped_effect_controller,
            &started,
            handover,
            cancellation_signal,
        )
        .await
    }

    pub(crate) async fn run_registration<F>(
        &self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        scoped_effect_controller: ScopedEffectController<'_>,
        started: &SegmentStarted,
        handover: Option<lash_core::SegmentHandover>,
        cancellation_signal: F,
    ) -> Result<lash_core::ProcessRunOutcome, HandlerError>
    where
        F: Future<Output = Result<(), HandlerError>>,
    {
        let process_id = registration.id.clone();
        let segment_ordinal = started.segment_ordinal();
        let execution_context =
            execution_context.with_execution_write_authority(started.write_authority().clone());
        if segment_ordinal > 0 && handover.is_none() {
            return Err(HandlerError::from(TerminalError::new(format!(
                "process `{process_id}` segment {segment_ordinal} omitted its validated handover"
            ))));
        }
        let requires_cancelled_session_turn = matches!(
            registration.input.as_ref(),
            lash_core::ProcessInput::SessionTurn { .. }
        );
        let cancellation = tokio_util::sync::CancellationToken::new();
        let runner = self.runner.run_process_segment(
            started,
            registration,
            execution_context,
            scoped_effect_controller,
            handover,
            cancellation.clone(),
        );
        tokio::pin!(runner);
        tokio::pin!(cancellation_signal);
        // Redelivery must poll a committed cancellation before guest replay. If
        // both are ready, replaying the guest first could consume a completed
        // sleep and diverge against a journaled post-wake suffix before
        // cancellation takes ownership of settlement.
        let outcome = tokio::select! {
            biased;
            signal = &mut cancellation_signal => {
                signal?;
                cancellation.cancel();
                self.confirm_process_cancel_requested(&process_id).await?;
                // The runner still has to settle: a `SessionTurn` runner only
                // returns a settled outcome once the retained child's accepted
                // input is terminal and unclaimed. An `Err` is an
                // infrastructure failure, not a settled outcome — propagate it
                // so the invocation retries instead of terminalizing over an
                // unsettled child.
                match runner.await {
                    Err(error) => Err(error),
                    Ok(_) => Ok(lash_core::ProcessRunOutcome::Terminal {
                        output: Box::new(ProcessAwaitOutput::from_tool_output(
                            lash_core::ToolCallOutput::cancelled(
                                lash_core::ToolCancellation::runtime(format!(
                                    "process `{process_id}` was cancelled"
                                )),
                            ),
                        )),
                    }),
                }
            }
            outcome = &mut runner => outcome
        };
        // A committed cancellation outranks a settled runner success
        // (PR #897): if the runner settled before observing the cancel signal,
        // the recorded terminal is still `Cancelled`. A runner `Err` is an
        // infrastructure failure, not a settled outcome — it propagates so the
        // process stays recoverable rather than masking the failure with
        // `Cancelled` over a possibly unsettled child. The child session and
        // its committed turn stay retained either way — lash never deletes a
        // session because a process was cancelled.
        let outcome = if requires_cancelled_session_turn
            && matches!(
                &outcome,
                Ok(outcome)
                    if outcome
                        .terminal_output()
                        .and_then(ProcessAwaitOutput::terminal_status)
                        != Some(lash_core::ProcessStatus::Cancelled)
            )
            && self
                .process_cancel_requested(&process_id)
                .await
                .map_err(handler_error_from_plugin)?
        {
            Ok(lash_core::ProcessRunOutcome::Terminal {
                output: Box::new(ProcessAwaitOutput::from_tool_output(
                    lash_core::ToolCallOutput::cancelled(lash_core::ToolCancellation::runtime(
                        format!("process `{process_id}` was cancelled"),
                    )),
                )),
            })
        } else {
            outcome
        };
        match outcome {
            Ok(lash_core::ProcessRunOutcome::Terminal { output }) => {
                // The terminal append writes the ended parent scope's ledger
                // row in the same store transaction; the sweep, not this
                // handler, cancels the children.
                let stored = self
                    .complete_with_stored_outcome(&process_id, (*output).clone())
                    .await
                    .map_err(handler_error_from_plugin)?;
                Ok(lash_core::ProcessRunOutcome::Terminal {
                    output: Box::new(stored),
                })
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
                    .map_err(handler_error_from_plugin)?;
                Ok(output.into())
            }
            Err(PluginError::ProcessAttemptsExhausted { .. }) => {
                let owner = self
                    .registry
                    .get_process(&process_id)
                    .await
                    .map_err(handler_error_from_plugin)?
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
                    .map_err(handler_error_from_plugin)?;
                Ok(output.into())
            }
            // A segment whose journal diverged cannot complete mid-replay: it
            // ends its attempt the one way a park ends (FIG-3697).
            Err(err) if is_replay_mismatch(&err) => Err(crate::parked_turn_failure(err)),
            Err(err) if err.is_retryable() => Err(HandlerError::from(err)),
            Err(err) if err.is_terminal() => {
                let output = self
                    .complete_with_stored_outcome(&process_id, terminal_process_output(err))
                    .await
                    .map_err(handler_error_from_plugin)?;
                Ok(output.into())
            }
            Err(err) => Err(handler_error_from_plugin(err)),
        }
    }

    pub(crate) async fn process_cancel_requested(
        &self,
        process_id: &ProcessId,
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
            return Err(PluginError::Runtime(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::RuntimeStore,
                "simulated transient cancel registry read failure",
            )));
        }
        Ok(self
            .registry
            .get_process(process_id)
            .await?
            .ok_or_else(|| lash_core::runtime::registry_transitions::unknown_process(process_id))?
            .cancel_request
            .is_some())
    }

    async fn confirm_process_cancel_requested(
        &self,
        process_id: &ProcessId,
    ) -> Result<(), HandlerError> {
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
                        return Err(handler_error_from_plugin(error));
                    }
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn confirm_process_cancel_requested_for_test(
        &self,
        process_id: &ProcessId,
    ) -> Result<(), HandlerError> {
        self.confirm_process_cancel_requested(process_id).await
    }

    async fn record_cancel_requested(
        &self,
        request: &RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        self.registry
            .append_event_ref(
                &request.process_ref,
                lash_core::ProcessEventAppendRequest::cancel_requested(
                    &request.process_ref,
                    &request.request,
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
        // The journal-generation gate runs before the handler journals
        // anything: an input built for another command prefix is refused typed
        // rather than replayed against commands it never recorded (FIG-3588).
        if input.journal_version != RESTATE_PROCESS_JOURNAL_VERSION {
            return self
                .refuse_retired_journal(&process_id, input.journal_version)
                .await;
        }
        // FIG-788: a terminal outcome can land after an attempt has emitted
        // runner commands but before its handler output is committed. Never
        // branch around the runner on a non-journaled read: redrive must
        // reconstruct the deployed command prefix, and idempotent completion
        // below returns the already-stored terminal outcome. Terminal pruning
        // remains a host-owned exposure: the raw cutoff has no finite
        // workflow-lifetime bound to validate against, so a host must retain
        // this row while the workflow can replay.
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
                if missing_segment_is_superseded(input.segment_ordinal, latest.as_ref()) {
                    let latest = latest.as_ref().ok_or_else(|| {
                        HandlerError::from(TerminalError::new(format!(
                            "process `{process_id}` segment {} was classified as superseded without a latest handover",
                            input.segment_ordinal
                        )))
                    })?;
                    tracing::debug!(
                        process_id = process_id.as_str(),
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
            Some(persisted.handover)
        };
        // Admission is the handler's first journaled work: the verdict, then
        // the start marker, and only the proof the start returns can mint the
        // segment's effect controller or drive its runner (FIG-3588).
        let started = match admit_segment(
            &ctx,
            &self.registry,
            &self.continuations,
            &process_id,
            input.segment_ordinal,
            self.runner.replay_key_grammar(&input.registration),
        )
        .await?
        {
            SegmentAdmission::Started(started) => started,
            SegmentAdmission::Superseded {
                latest_segment_ordinal,
            } => {
                // A completed segment is never refused: its successor already
                // carries the process.
                tracing::debug!(
                    process_id = process_id.as_str(),
                    segment_ordinal = input.segment_ordinal,
                    latest_segment_ordinal,
                    "ignoring a completed process segment"
                );
                return Ok(Json(RestateProcessWorkflowOutput::SegmentChained {
                    next_segment_ordinal: latest_segment_ordinal,
                }));
            }
            SegmentAdmission::SubstrateLost { lost } => {
                tracing::warn!(
                    process_id = process_id.as_str(),
                    segment_ordinal = input.segment_ordinal,
                    lost_owner_id = lost.owner.owner_id.as_str(),
                    lost_attempt = lost.attempt,
                    "a process segment started under a journal this invocation cannot read; abandoning instead of re-running"
                );
                let output = self
                    .complete_with_stored_outcome(
                        &process_id,
                        ProcessAwaitOutput::Abandoned {
                            evidence: Box::new(AbandonEvidence {
                                writer: AbandonWriter::ResumeRefused {
                                    reason: lash_core::ProcessResumeRefusal::SubstrateLost,
                                },
                                owner: Some(lost.owner),
                                epoch_ms: restate_now_ms(),
                            }),
                            control: None,
                        },
                    )
                    .await
                    .map_err(handler_error_from_plugin)?;
                resolve_process_cancel_signal(&ctx, RestateProcessCancelSignal::SegmentFinished)?;
                return self
                    .deliver_segment_terminal(&ctx, &process_id, input.segment_ordinal, output)
                    .await;
            }
        };
        let mut options = RestateEffectControllerOptions::default()
            .segment_effect_budget((self.segment_effect_budget)(&input.registration));
        if let Some(cap) = self.segment_duration_cap {
            options = options.segment_duration_cap(cap);
        }
        let controller =
            RestateRuntimeEffectController::with_options(ctx, self.authority_id.clone(), options);
        let controller = if let Some(sink) = self.trace_sink.as_ref() {
            controller.with_trace_sink_and_context(Arc::clone(sink), self.trace_context.clone())
        } else {
            controller
        };
        let outcome = loop {
            let scoped_effect_controller = controller
                .process_segment_controller(&started)
                .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
            let cancel_signal = self.cancellation_signal(&process_id, input.segment_ordinal);
            let outcome = self
                .run_registration(
                    input.registration.clone(),
                    input.execution_context.clone(),
                    scoped_effect_controller,
                    &started,
                    handover,
                    cancel_signal,
                )
                .await?;
            if let lash_core::ProcessRunOutcome::SegmentBoundary(boundary) = &outcome {
                let current = self
                    .registry
                    .get_process(&process_id)
                    .await
                    .map_err(handler_error_from_plugin)?;
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
            lash_core::ProcessRunOutcome::Terminal { output, .. } => {
                self.deliver_segment_terminal(
                    controller.context(),
                    &process_id,
                    input.segment_ordinal,
                    *output,
                )
                .await
            }
            lash_core::ProcessRunOutcome::SegmentBoundary(handover) => {
                let next_segment_ordinal = input.segment_ordinal.saturating_add(1);
                let successor_key = process_segment_workflow_key(&process_id, next_segment_ordinal);
                // The successor's reference names who owns the process now.
                // It is observational: the recovery sweep keys on the latest
                // handover, and admission, not the reference, decides whether
                // a segment runs. It is still written before the handover and
                // the send, so a visible handover always has its reference,
                // and a failed write is a store fault Restate retries — this
                // invocation replays its journal up to here and writes again,
                // compare-and-set on the ordinal — rather than a terminal
                // failure that would strand the chain. The successor's
                // invocation id is not known until the send, so the reference
                // carries none; the workflow key identifies the owner.
                self.registry
                    .set_external_ref(
                        &process_id,
                        lash_core::ProcessExternalRef {
                            backend: "restate".to_string(),
                            id: format!("LashProcessWorkflow/{successor_key}"),
                            metadata: None,
                            segment_ordinal: Some(next_segment_ordinal),
                        },
                    )
                    .await
                    .map_err(HandlerError::from)?;
                self.continuations
                    .put_segment_handover(
                        &process_id,
                        lash_core::PersistedSegmentHandover {
                            segment_ordinal: next_segment_ordinal,
                            handover,
                        },
                    )
                    .await
                    .map_err(HandlerError::from)?;
                // FIG-788: successor emission is unconditional. A cancellation
                // event can land between attempts, so the append-only registry
                // read below may shape only commands after this deployed prefix.
                let request = controller
                    .context()
                    .workflow_client::<LashProcessWorkflowClient>(successor_key.clone())
                    .run(Json(RestateProcessWorkflowInput {
                        registration: input.registration,
                        execution_context: input.execution_context,
                        segment_ordinal: next_segment_ordinal,
                        journal_version: RESTATE_PROCESS_JOURNAL_VERSION,
                    }));
                request.send().await?;
                let record = self
                    .registry
                    .get_process(&process_id)
                    .await
                    .map_err(handler_error_from_plugin)?
                    .ok_or_else(|| {
                        handler_error_from_plugin(
                            lash_core::runtime::registry_transitions::unknown_process(&process_id),
                        )
                    })?;
                if record.cancel_request.is_some() {
                    // Cancellation can race the gap after the current segment
                    // retires its promise but before the successor handover is
                    // visible to the cancel endpoint. Forward that durable fact
                    // after scheduling so the successor owns terminalization.
                    let deliver = controller
                        .context()
                        .workflow_client::<LashProcessWorkflowClient>(successor_key)
                        .deliver_cancel(Json(
                            RestateProcessCancelRequest::from_record(&record)
                                .map_err(handler_error_from_plugin)?,
                        ));
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
        resolve_process_terminal_promise(
            &ctx,
            &self.authority_id,
            &request.process_id,
            &request.output,
        )?;
        Ok(Json(()))
    }

    async fn cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(request): Json<RestateProcessCancelRequest>,
    ) -> HandlerResult<Json<()>> {
        self.record_cancel_requested(&request)
            .await
            .map_err(handler_error_from_plugin)?;
        resolve_process_cancel_signal(&ctx, RestateProcessCancelSignal::CancelRequested)?;

        if let Some(handover) = self
            .continuations
            .latest_segment_handover(&request.process_ref.process_id)
            .await
            .map_err(handler_error_from_plugin)?
            && handover.segment_ordinal > 0
        {
            let deliver = ctx
                .workflow_client::<LashProcessWorkflowClient>(process_segment_workflow_key(
                    &request.process_ref.process_id,
                    handover.segment_ordinal,
                ))
                .deliver_cancel(Json(request.clone()));
            let Json(()) = deliver.call().await?;
        }
        self.runner
            .request_process_cancel(request)
            .await
            .map_err(handler_error_from_plugin)?;
        Ok(Json(()))
    }

    async fn deliver_cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(request): Json<RestateProcessCancelRequest>,
    ) -> HandlerResult<Json<()>> {
        self.record_cancel_requested(&request)
            .await
            .map_err(handler_error_from_plugin)?;
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
        let key = restate_process_terminal_await_key(&self.authority_id, &request.process_id)
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
