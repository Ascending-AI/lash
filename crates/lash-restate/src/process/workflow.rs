#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API while its replacement is staged"
)]

//! The `LashProcessWorkflow` segment executor.
//!
//! One responsibility: run exactly one process segment inside a Restate
//! workflow invocation — admit it, drive its runner, and deliver either a
//! terminal outcome or a segment successor.
//!
//! The drive is deterministic (FIG-3673): every input that shapes the journal
//! is itself a journaled command. The segment's cancellation is its durable
//! cancel promise, which the drive observes only through recorded races on its
//! waits, recorded peeks at its body's cancel checkpoints and one recorded peek
//! after the runner settles. Every registry read or write the handler makes is
//! a named step. The live watch that stops step bodies on a cancel
//! ([`ProcessStopDelivery`](crate::process_stop::ProcessStopDelivery)) is
//! execution-side only: the drive never reads it.

use lash_sansio::ProcessId;
use std::sync::Arc;

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
    PROCESS_CANCEL_PROMISE_KEY, RESTATE_PROCESS_JOURNAL_VERSION, RestateProcessAwaitRequest,
    RestateProcessCancelRequest, RestateProcessCancelSignal, RestateProcessCompleteRequest,
    RestateProcessRunner, RestateProcessWorkflowInput, RestateProcessWorkflowOutput,
    SegmentAdmission, SegmentStarted, admit_segment, boundary_must_be_declined,
    handler_error_from_plugin, handover_digest, is_replay_mismatch, process_segment_workflow_key,
    resolve_process_cancel_signal, resolve_process_terminal_promise, restate_now_ms,
    restate_process_terminal_await_key, restate_process_terminal_output,
    terminal_completion_workflow_key, terminal_process_output, workflow_key_authority,
};
use crate::controller::{
    RestateControllerContext, RestateEffectControllerOptions, RestateRuntimeEffectController,
};
use crate::ingress::RestateIngressClient;
use crate::process_stop::ProcessStopDelivery;

/// The journal name of the terminal completion step.
const COMPLETE_STEP: &str = "lash.process.complete";
/// The journal name of the step that decides whether a boundary is declined.
const BOUNDARY_STEP: &str = "lash.segment.boundary";
/// The journal name of the step that publishes a successor's handover.
const HANDOVER_STEP: &str = "lash.segment.handover";
/// The journal name of the step that reads a cancel to forward to a successor.
const CANCEL_FORWARD_STEP: &str = "lash.segment.cancel-forward";
/// The journal name of the step that records a cancel request.
const CANCEL_RECORD_STEP: &str = "lash.process.cancel.record";
/// The journal name of the step that finds the segment a cancel is routed to.
const CANCEL_ROUTE_STEP: &str = "lash.process.cancel.route";
/// The journal name of the step that asks a `SessionTurn` child turn to stop.
const CANCEL_CHILD_TURN_STEP: &str = "lash.process.cancel.child-turn";
/// The journal name of the step that retires the handovers a segment no
/// longer needs.
const RETIRE_STEP: &str = "lash.segment.retire";

/// The terminal a segment proposes; the completion step turns it into the
/// stored outcome.
#[derive(Debug)]
pub(crate) enum TerminalProposal {
    Output(Box<ProcessAwaitOutput>),
    Abandoned {
        writer: AbandonWriter,
        owner: Option<lash_core::LeaseOwnerIdentity>,
    },
}

/// How a segment's runner ended, before its terminal is stored.
#[derive(Debug)]
pub(crate) enum SegmentRunEnd {
    Terminal(TerminalProposal),
    Boundary(lash_core::SegmentHandover),
}

/// A store failure a step leaves unrecorded so the invocation retries it, or
/// a failure it records as the handler's terminal refusal.
fn step_fault<T>(error: PluginError) -> Result<Result<T, String>, String> {
    if error.is_retryable() {
        Err(error.to_string())
    } else {
        Ok(Err(error.to_string()))
    }
}

fn cancelled_output(process_id: &ProcessId) -> ProcessAwaitOutput {
    ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::cancelled(
        lash_core::ToolCancellation::runtime(format!("process `{process_id}` was cancelled")),
    ))
}

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
pub(crate) struct LashProcessWorkflowImpl<R> {
    runner: Arc<R>,
    registry: Arc<dyn ProcessRegistry>,
    continuations: Arc<dyn lash_core::ProcessContinuationStore>,
    segment_effect_budget: super::SegmentEffectBudget,
    cancel_ingress: Option<RestateIngressClient>,
    authority_id: crate::RestateAuthorityId,
    trace_sink: Option<Arc<dyn lash_trace::TraceSink>>,
    trace_context: lash_trace::TraceContext,
}

impl<R> LashProcessWorkflowImpl<R> {
    /// Build a Restate process workflow whose segments stop their step bodies
    /// on a cancel through a live watch of their cancel promise over the
    /// ingress (execution-side only; the drive never reads it).
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
            segment_effect_budget: Arc::new(|_| 10_000),
            cancel_ingress,
            authority_id,
            trace_sink: None,
            trace_context: lash_trace::TraceContext::default(),
        }
    }

    /// Attach the host's live trace observer to every process-segment
    /// controller created by this workflow.
    #[cfg(test)]
    pub(crate) fn with_trace_sink(
        mut self,
        sink: Arc<dyn lash_trace::TraceSink>,
        context: lash_trace::TraceContext,
    ) -> Self {
        self.trace_sink = Some(sink);
        self.trace_context = context;
        self
    }

    pub(crate) fn with_segment_effect_budget(
        mut self,
        selector: super::SegmentEffectBudget,
    ) -> Self {
        self.segment_effect_budget = selector;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_segment_effect_budget_selector(
        self,
        selector: impl Fn(&ProcessRegistration) -> u64 + Send + Sync + 'static,
    ) -> Self {
        self.with_segment_effect_budget(Arc::new(selector))
    }

    /// The execution-side watch that fires the stop a segment attempt lends
    /// its step bodies once its cancel promise holds a request.
    fn stop_delivery(
        &self,
        process_id: &ProcessId,
        segment_ordinal: u64,
        stop: tokio_util::sync::CancellationToken,
    ) -> ProcessStopDelivery {
        match self.cancel_ingress.clone() {
            Some(ingress) => ProcessStopDelivery::watch(
                ingress,
                process_id.clone(),
                process_segment_workflow_key(process_id, segment_ordinal),
                stop,
            ),
            None => ProcessStopDelivery::none(),
        }
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
    ///
    /// This is the one registry write and clock read outside a step: a step
    /// here would be a command, and an in-flight journal of the retired
    /// generation would meet it as a mismatch before it could be refused.
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
        let stored = complete_process_outcome(
            &self.registry,
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
        // Awaiters wait on the root workflow's terminal promise. Publish the
        // stored refusal through the root's shared `complete_terminal`, a
        // separate invocation, so it lands even when this invocation's own
        // retired journal can never replay. It is idempotent: a promise
        // already resolved stays as it was, and a failed publish is retried
        // with this attempt.
        if let Some(ingress) = self.cancel_ingress.as_ref() {
            ingress
                .call_workflow_json::<_, ()>(
                    crate::LashService::ProcessWorkflow.name(),
                    process_id.as_str(),
                    "complete_terminal",
                    &RestateProcessCompleteRequest {
                        process_id: process_id.clone(),
                        output: stored,
                    },
                )
                .await
                .map_err(HandlerError::from)?;
        }
        Err(TerminalError::new(format!(
            "process `{process_id}` segment input carries {found}; this handler journals generation {RESTATE_PROCESS_JOURNAL_VERSION}"
        ))
        .into())
    }

    /// Store the segment's terminal as one journaled step
    /// (`lash.process.complete`): the evidence clock and the owner read run
    /// inside it, and the stored outcome it journals is the terminal promise's
    /// payload, so every redrive publishes the terminal the first execution
    /// stored.
    async fn complete_terminal_step(
        &self,
        journal: &WorkflowContext<'_>,
        process_id: &ProcessId,
        proposal: TerminalProposal,
    ) -> Result<ProcessAwaitOutput, HandlerError> {
        let registry = &self.registry;
        let Json(stored) = journal
            .run_json_or_retry_send::<Result<ProcessAwaitOutput, String>, _>(
                COMPLETE_STEP.to_string(),
                async move {
                    let proposed = match proposal {
                        TerminalProposal::Output(output) => *output,
                        TerminalProposal::Abandoned { writer, owner } => {
                            ProcessAwaitOutput::Abandoned {
                                evidence: Box::new(AbandonEvidence {
                                    writer,
                                    owner,
                                    epoch_ms: restate_now_ms(),
                                }),
                                control: None,
                            }
                        }
                    };
                    match complete_process_outcome(registry, process_id, proposed).await {
                        Ok(stored) => Ok(Ok(stored)),
                        Err(error) => step_fault(error),
                    }
                },
            )
            .await
            .map_err(HandlerError::from)?;
        stored.map_err(|refusal| TerminalError::new(refusal).into())
    }

    /// Store `proposed` directly, as the completion step's body does.
    #[cfg(test)]
    pub(crate) async fn complete_with_stored_outcome(
        &self,
        process_id: &ProcessId,
        proposed: ProcessAwaitOutput,
    ) -> Result<ProcessAwaitOutput, PluginError> {
        complete_process_outcome(&self.registry, process_id, proposed).await
    }

    /// [`run_registration`](Self::run_registration) for a test that drives a
    /// segment without the handler's admission: the segment proof is minted
    /// from the scope and the execution authority the test supplies, and the
    /// proposed terminal is stored directly rather than through the
    /// completion step.
    #[cfg(test)]
    pub(crate) async fn run_registration_for_test(
        &self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        scoped_effect_controller: ScopedEffectController<'_>,
        segment_ordinal: u64,
        handover: Option<lash_core::SegmentHandover>,
    ) -> Result<lash_core::ProcessRunOutcome, HandlerError> {
        let process_id = registration.id.clone();
        let started = SegmentStarted::for_test(
            scoped_effect_controller.admitted_scope().clone(),
            segment_ordinal,
            execution_context.execution_write_authority.clone(),
        );
        match self
            .run_registration(
                registration,
                execution_context,
                scoped_effect_controller,
                &started,
                handover,
            )
            .await?
        {
            SegmentRunEnd::Boundary(handover) => {
                Ok(lash_core::ProcessRunOutcome::SegmentBoundary(handover))
            }
            SegmentRunEnd::Terminal(proposal) => {
                let proposed = match proposal {
                    TerminalProposal::Output(output) => *output,
                    TerminalProposal::Abandoned { writer, owner } => {
                        ProcessAwaitOutput::Abandoned {
                            evidence: Box::new(AbandonEvidence {
                                writer,
                                owner,
                                epoch_ms: restate_now_ms(),
                            }),
                            control: None,
                        }
                    }
                };
                let stored = complete_process_outcome(&self.registry, &process_id, proposed)
                    .await
                    .map_err(handler_error_from_plugin)?;
                Ok(stored.into())
            }
        }
    }

    /// Drive one admitted segment's runner and propose how it ended.
    ///
    /// The runner is lent a stop that only the execution-side
    /// [`ProcessStopDelivery`] fires, to stop its step bodies; the drive never
    /// reads it. For a `SessionTurn` process a committed cancellation
    /// outranks a settled runner success (PR #897), read here through one
    /// journaled peek of the segment's cancel promise.
    pub(crate) async fn run_registration(
        &self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        scoped_effect_controller: ScopedEffectController<'_>,
        started: &SegmentStarted,
        handover: Option<lash_core::SegmentHandover>,
    ) -> Result<SegmentRunEnd, HandlerError> {
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
        let drive = scoped_effect_controller.clone();
        let stop = tokio_util::sync::CancellationToken::new();
        let delivery = self.stop_delivery(&process_id, segment_ordinal, stop.clone());
        let outcome = delivery
            .drive(self.runner.run_process_segment(
                started,
                registration,
                execution_context,
                scoped_effect_controller,
                handover,
                stop,
            ))
            .await?;
        // A committed cancellation outranks a settled `SessionTurn` success
        // (PR #897): the child turn is stopped through its own durable gate,
        // so its runner can settle successfully after the cancel committed.
        // The peek is journaled, so a redrive reads the verdict the first
        // execution read. A runner `Err` is an infrastructure failure, not a
        // settled outcome — it propagates so the process stays recoverable
        // rather than masking the failure with `Cancelled` over a possibly
        // unsettled child. The child session and its committed turn stay
        // retained either way — lash never deletes a session because a
        // process was cancelled.
        let outcome = match outcome {
            Ok(lash_core::ProcessRunOutcome::Terminal { output })
                if requires_cancelled_session_turn
                    && output.terminal_status() != Some(lash_core::ProcessStatus::Cancelled) =>
            {
                if drive
                    .controller()
                    .observe_process_cancel(&tokio_util::sync::CancellationToken::new())
                    .await
                    .map_err(|error| handler_error_from_plugin(error.into()))?
                {
                    Ok(lash_core::ProcessRunOutcome::Terminal {
                        output: Box::new(cancelled_output(&process_id)),
                    })
                } else {
                    Ok(lash_core::ProcessRunOutcome::Terminal { output })
                }
            }
            outcome => outcome,
        };
        match outcome {
            Ok(lash_core::ProcessRunOutcome::Terminal { output }) => {
                // The terminal append writes the ended parent scope's ledger
                // row in the same store transaction; the sweep, not this
                // handler, cancels the children.
                Ok(SegmentRunEnd::Terminal(TerminalProposal::Output(output)))
            }
            Ok(lash_core::ProcessRunOutcome::SegmentBoundary(boundary)) => {
                Ok(SegmentRunEnd::Boundary(boundary))
            }
            Err(PluginError::ProcessAlreadyStarted { by, .. }) => {
                Ok(SegmentRunEnd::Terminal(TerminalProposal::Abandoned {
                    writer: AbandonWriter::Sweep,
                    owner: Some(*by),
                }))
            }
            // A segment whose journal diverged cannot complete mid-replay: it
            // ends its attempt the one way a park ends (FIG-3697).
            Err(err) if is_replay_mismatch(&err) => Err(crate::parked_turn_failure(err)),
            Err(err) if err.is_retryable() => Err(HandlerError::from(err)),
            Err(err) if err.is_terminal() => Ok(SegmentRunEnd::Terminal(TerminalProposal::Output(
                Box::new(terminal_process_output(err)),
            ))),
            Err(err) => Err(handler_error_from_plugin(err)),
        }
    }
}

/// Store `proposed` as the process's terminal, answering the outcome the
/// registry kept: this one, or the one an earlier writer committed.
async fn complete_process_outcome(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &ProcessId,
    proposed: ProcessAwaitOutput,
) -> Result<ProcessAwaitOutput, PluginError> {
    let completion = registry
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

/// Record `request`'s cancellation in the registry.
async fn record_cancel_requested(
    registry: &Arc<dyn ProcessRegistry>,
    request: &RestateProcessCancelRequest,
) -> Result<(), PluginError> {
    registry
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

/// Refuse a cancel request built for another generation of the `cancel` and
/// `deliver_cancel` handlers' commands, before journaling anything
/// (FIG-3673): its journal, if it has one, holds commands this build does not
/// issue. The refusal is typed and terminal; an in-flight invocation of a
/// retired generation meets it as a journal mismatch, which the engine's
/// retry policy bounds, so such invocations are drained or killed at deploy.
fn refuse_retired_cancel(
    request: &RestateProcessCancelRequest,
    handler: &str,
) -> Result<(), HandlerError> {
    if request.journal_version == RESTATE_PROCESS_JOURNAL_VERSION {
        return Ok(());
    }
    Err(TerminalError::new(format!(
        "process `{}` {handler} request carries restate-process-journal-v{}; this handler journals generation {RESTATE_PROCESS_JOURNAL_VERSION}",
        request.process_ref.process_id, request.journal_version
    ))
    .into())
}

/// Journal `request`'s registry record as a named step of the calling
/// handler.
async fn record_cancel_step(
    journal: &SharedWorkflowContext<'_>,
    registry: &Arc<dyn ProcessRegistry>,
    request: &RestateProcessCancelRequest,
) -> Result<(), HandlerError> {
    let Json(recorded) = journal
        .run_json_or_retry_send::<Result<(), String>, _>(
            CANCEL_RECORD_STEP.to_string(),
            async move {
                match record_cancel_requested(registry, request).await {
                    Ok(()) => Ok(Ok(())),
                    Err(error) => step_fault(error),
                }
            },
        )
        .await
        .map_err(HandlerError::from)?;
    recorded.map_err(|refusal| TerminalError::new(refusal).into())
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
        // Admission is the handler's first journaled work: the verdict, then
        // the start marker, and only the proof the start returns can mint the
        // segment's effect controller or drive its runner (FIG-3588). The
        // verdict records what decides the rest of the journal's shape: whether
        // the segment is superseded or its handover missing, the digest of the
        // handover it resumes from, and its boundary policy (FIG-3673). FIG-788:
        // nothing branches around the runner on a non-journaled read.
        let selector = Arc::clone(&self.segment_effect_budget);
        let registration = input.registration.clone();
        let (started, handover_digest_recorded, policy) = match admit_segment(
            &ctx,
            &self.registry,
            &self.continuations,
            &process_id,
            input.segment_ordinal,
            self.runner.replay_key_grammar(&input.registration),
            move || selector(&registration),
        )
        .await?
        {
            SegmentAdmission::Started {
                started,
                handover,
                policy,
            } => (started, handover, policy),
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
            SegmentAdmission::MissingHandover => {
                return Err(HandlerError::from(TerminalError::new(format!(
                    "missing persisted handover for process `{process_id}` segment {}",
                    input.segment_ordinal
                ))));
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
                    .complete_terminal_step(
                        &ctx,
                        &process_id,
                        TerminalProposal::Abandoned {
                            writer: AbandonWriter::ResumeRefused {
                                reason: lash_core::ProcessResumeRefusal::SubstrateLost,
                            },
                            owner: Some(lost.owner),
                        },
                    )
                    .await?;
                resolve_process_cancel_signal(&ctx, RestateProcessCancelSignal::SegmentFinished)?;
                return self
                    .deliver_segment_terminal(&ctx, &process_id, input.segment_ordinal, output)
                    .await;
            }
        };
        // The handover a later segment resumes from is immutable per ordinal
        // and retained until terminal (FIG-811): read it after admission and
        // hold it to the digest the verdict recorded.
        let mut handover = match handover_digest_recorded {
            None => None,
            Some(recorded) => {
                let persisted = self
                    .continuations
                    .get_segment_handover(&process_id, input.segment_ordinal)
                    .await
                    .map_err(HandlerError::from)?
                    .ok_or_else(|| {
                        HandlerError::from(TerminalError::new(format!(
                            "missing persisted handover for process `{process_id}` segment {}: \
                             its admission recorded one that is no longer retained",
                            input.segment_ordinal
                        )))
                    })?;
                if handover_digest(&persisted.handover)? != recorded {
                    return Err(TerminalError::new(format!(
                        "process `{process_id}` segment {} handover differs from the one its admission recorded",
                        input.segment_ordinal
                    ))
                    .into());
                }
                Some(persisted.handover)
            }
        };
        let options = RestateEffectControllerOptions::default()
            .segment_effect_budget(policy.effect_budget)
            .process_segment_drive();
        let controller =
            RestateRuntimeEffectController::with_options(ctx, self.authority_id.clone(), options);
        let trace = self
            .trace_sink
            .as_ref()
            .map(|sink| (Arc::clone(sink), self.trace_context.clone()))
            .or_else(|| self.runner.trace());
        let controller = if let Some((sink, context)) = trace {
            controller.with_trace_sink_and_context(sink, context)
        } else {
            controller
        };
        let end = loop {
            let scoped_effect_controller = controller
                .process_segment_controller(&started)
                .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
            let end = self
                .run_registration(
                    input.registration.clone(),
                    input.execution_context.clone(),
                    scoped_effect_controller,
                    &started,
                    handover,
                )
                .await?;
            if let SegmentRunEnd::Boundary(boundary) = &end {
                // Whether the boundary is declined is a recorded fact: a live
                // read here would decide on a redrive whether the runner is
                // entered again in this invocation.
                let registry = &self.registry;
                let process_id = &process_id;
                let Json(declined) = controller
                    .context()
                    .run_json_or_retry_send::<Result<bool, String>, _>(
                        BOUNDARY_STEP.to_string(),
                        async move {
                            match registry.get_process(process_id).await {
                                Ok(record) => Ok(Ok(boundary_must_be_declined(record.as_ref()))),
                                Err(error) => step_fault(error),
                            }
                        },
                    )
                    .await
                    .map_err(HandlerError::from)?;
                if declined.map_err(TerminalError::new)? {
                    handover = Some(boundary.clone());
                    continue;
                }
            }
            break end;
        };
        let context = controller.context();
        let handover = match end {
            SegmentRunEnd::Terminal(proposal) => {
                let output = self
                    .complete_terminal_step(context, &process_id, proposal)
                    .await?;
                resolve_process_cancel_signal(
                    context,
                    RestateProcessCancelSignal::SegmentFinished,
                )?;
                return self
                    .deliver_segment_terminal(context, &process_id, input.segment_ordinal, output)
                    .await;
            }
            SegmentRunEnd::Boundary(handover) => handover,
        };
        resolve_process_cancel_signal(context, RestateProcessCancelSignal::SegmentFinished)?;
        let next_segment_ordinal = input.segment_ordinal.saturating_add(1);
        let successor_key = process_segment_workflow_key(&process_id, next_segment_ordinal);
        // The successor's reference names who owns the process now.
        // It is observational: the recovery sweep keys on the latest
        // handover, and admission, not the reference, decides whether
        // a segment runs. It is still written before the handover and
        // the send, so a visible handover always has its reference.
        // Both writes are one journaled step: a store fault ends the
        // attempt unrecorded and the redrive writes again,
        // compare-and-set on the ordinal. The successor's invocation
        // id is not known until the send, so the reference carries
        // none; the workflow key identifies the owner.
        let registry = &self.registry;
        let continuations = &self.continuations;
        let pid = &process_id;
        let reference_id = format!(
            "{}/{successor_key}",
            crate::LashService::ProcessWorkflow.name()
        );
        let Json(handed_over) = context
            .run_json_or_retry_send::<Result<(), String>, _>(
                HANDOVER_STEP.to_string(),
                async move {
                    if let Err(error) = registry
                        .set_external_ref(
                            pid,
                            lash_core::ProcessExternalRef {
                                backend: "restate".to_string(),
                                id: reference_id,
                                metadata: None,
                                segment_ordinal: Some(next_segment_ordinal),
                            },
                        )
                        .await
                    {
                        return step_fault(error);
                    }
                    match continuations
                        .put_segment_handover(
                            pid,
                            lash_core::PersistedSegmentHandover {
                                segment_ordinal: next_segment_ordinal,
                                handover,
                            },
                        )
                        .await
                    {
                        Ok(()) => Ok(Ok(())),
                        Err(error) => step_fault(error),
                    }
                },
            )
            .await
            .map_err(HandlerError::from)?;
        handed_over.map_err(TerminalError::new)?;
        // FIG-788: successor emission is unconditional. A cancellation
        // can land between attempts, so the recorded read below may
        // shape only commands after this deployed prefix.
        let request = context
            .workflow_client::<LashProcessWorkflowClient>(successor_key.clone())
            .run(Json(RestateProcessWorkflowInput {
                registration: input.registration,
                execution_context: input.execution_context,
                segment_ordinal: next_segment_ordinal,
                journal_version: RESTATE_PROCESS_JOURNAL_VERSION,
            }));
        request.send().await?;
        // Cancellation can race the gap after the current segment
        // retires its promise but before the successor handover is
        // visible to the cancel endpoint. Forward that durable fact
        // after scheduling so the successor owns terminalization; the
        // read is a step, so a redrive forwards exactly what the first
        // execution forwarded.
        let registry = &self.registry;
        let pid = &process_id;
        let Json(forward) = context
            .run_json_or_retry_send::<Result<Option<RestateProcessCancelRequest>, String>, _>(
                CANCEL_FORWARD_STEP.to_string(),
                async move {
                    let record = match registry.get_process(pid).await {
                        Ok(Some(record)) => record,
                        Ok(None) => {
                            return Ok(Err(
                                lash_core::runtime::registry_transitions::unknown_process(pid)
                                    .to_string(),
                            ));
                        }
                        Err(error) => return step_fault(error),
                    };
                    if record.cancel_request.is_none() {
                        return Ok(Ok(None));
                    }
                    Ok(RestateProcessCancelRequest::from_record(&record)
                        .map(Some)
                        .map_err(|error| error.to_string()))
                },
            )
            .await
            .map_err(HandlerError::from)?;
        if let Some(cancel) = forward.map_err(TerminalError::new)? {
            let deliver = context
                .workflow_client::<LashProcessWorkflowClient>(successor_key)
                .deliver_cancel(Json(cancel));
            let Json(()) = deliver.call().await?;
        }
        // This segment has handed the process on and recorded the cancel it
        // forwards, so its handover is no longer anyone's to read: retire it,
        // and any older one. Until here it is retained, so a redrive of this
        // segment in the handover gap can still replay its runner and forward
        // a cancel that landed in the gap (FIG-3673). A redrive after this
        // step finds no handover and ends: its successor was sent and its
        // forward recorded.
        if input.segment_ordinal > 0 {
            let continuations = &self.continuations;
            let pid = &process_id;
            let segment_ordinal = input.segment_ordinal;
            let Json(retired) = context
                .run_json_or_retry_send::<Result<(), String>, _>(
                    RETIRE_STEP.to_string(),
                    async move {
                        match continuations
                            .retire_segment_handovers_through(pid, segment_ordinal)
                            .await
                        {
                            Ok(()) => Ok(Ok(())),
                            Err(error) => step_fault(error),
                        }
                    },
                )
                .await
                .map_err(HandlerError::from)?;
            retired.map_err(TerminalError::new)?;
        }
        Ok(Json(RestateProcessWorkflowOutput::SegmentChained {
            next_segment_ordinal,
        }))
    }
    async fn complete_terminal(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(request): Json<RestateProcessCompleteRequest>,
    ) -> HandlerResult<Json<()>> {
        let key = restate_process_terminal_await_key(&self.authority_id, &request.process_id)
            .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
        if ctx
            .peek_promise::<String>(&key.promise_key())
            .await?
            .is_some()
        {
            // Published already: the first terminal stands.
            return Ok(Json(()));
        }
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
        refuse_retired_cancel(&request, "cancel")?;
        record_cancel_step(&ctx, &self.registry, &request).await?;
        resolve_process_cancel_signal(&ctx, RestateProcessCancelSignal::CancelRequested)?;
        // A `SessionTurn` process's child turn is asked to stop through its
        // own durable gate, whether or not the process runs anywhere now: the
        // turn honours it where it honours any request (FIG-3673).
        let registry = &self.registry;
        let runner = &self.runner;
        let cancel = &request;
        let Json(child_turn) = ctx
            .run_json_or_retry_send::<Result<(), String>, _>(
                CANCEL_CHILD_TURN_STEP.to_string(),
                async move {
                    let record = match registry.get_process_ref(&cancel.process_ref).await {
                        Ok(Some(record)) => record,
                        Ok(None) => return Ok(Ok(())),
                        Err(error) => return step_fault(error),
                    };
                    match runner.stop_child_turn(&record, &cancel.request).await {
                        Ok(()) => Ok(Ok(())),
                        Err(error) => step_fault(error),
                    }
                },
            )
            .await
            .map_err(HandlerError::from)?;
        child_turn.map_err(TerminalError::new)?;
        // The segment that owns the process now is a recorded read: a live
        // one would decide on a redrive whether this handler forwards.
        let continuations = &self.continuations;
        let process_id = &request.process_ref.process_id;
        let Json(route) = ctx
            .run_json_or_retry_send::<Result<Option<u64>, String>, _>(
                CANCEL_ROUTE_STEP.to_string(),
                async move {
                    match continuations.latest_segment_handover(process_id).await {
                        Ok(handover) => Ok(Ok(handover
                            .map(|handover| handover.segment_ordinal)
                            .filter(|ordinal| *ordinal > 0))),
                        Err(error) => step_fault(error),
                    }
                },
            )
            .await
            .map_err(HandlerError::from)?;
        if let Some(segment_ordinal) = route.map_err(TerminalError::new)? {
            let deliver = ctx
                .workflow_client::<LashProcessWorkflowClient>(process_segment_workflow_key(
                    &request.process_ref.process_id,
                    segment_ordinal,
                ))
                .deliver_cancel(Json(request.clone()));
            let Json(()) = deliver.call().await?;
        }
        Ok(Json(()))
    }

    async fn deliver_cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(request): Json<RestateProcessCancelRequest>,
    ) -> HandlerResult<Json<()>> {
        refuse_retired_cancel(&request, "deliver_cancel")?;
        record_cancel_step(&ctx, &self.registry, &request).await?;
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
