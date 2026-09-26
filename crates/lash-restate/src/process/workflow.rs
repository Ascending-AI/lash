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
    ProcessRecord, ProcessRegistration, ProcessRegistry, ScopedEffectController,
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
    RestateProcessWorkflowPayload, SegmentAdmission, SegmentStarted, admit_segment,
    boundary_must_be_declined, handler_error_from_plugin, handover_digest, is_replay_mismatch,
    process_segment_workflow_key, resolve_process_cancel_signal, resolve_process_terminal_promise,
    restate_now_ms, restate_process_terminal_await_key, restate_process_terminal_output,
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
/// The handover a later segment resumes from, read once and journaled, so a
/// redrive replays the runner from the recorded handover even after the
/// segment retired it (FIG-3809).
const RESUME_STEP: &str = "lash.segment.resume";

/// The terminal a segment proposes; the completion step turns it into the
/// stored outcome.
#[derive(Debug)]
pub(crate) enum TerminalProposal {
    /// The runner's terminal and its terminal batch (FIG-3571), which the
    /// completion step commits ahead of it in one transaction.
    Output {
        output: Box<ProcessAwaitOutput>,
        prelude: Vec<lash_core::ProcessEventAppendRequest>,
    },
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

/// A segment failure no retry can fix, by class; each ends the process
/// Failed under its own code.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "class", content = "message", rename_all = "snake_case")]
pub(crate) enum SegmentFailure {
    /// The handover this segment resumes from is not retained.
    HandoverMissing(String),
    /// The retained handover is not the one admission recorded.
    HandoverMismatch(String),
    /// The segment's effect controller could not be minted.
    Controller(String),
    /// The recorded boundary read failed.
    Boundary(String),
    /// The handover to the successor could not be written.
    HandoverWrite(String),
    /// The process's records break an admission invariant (FIG-3819).
    AdmissionInvariant(String),
}

impl SegmentFailure {
    fn code(&self) -> &'static str {
        match self {
            Self::HandoverMissing(_) => "process_segment_handover_missing",
            Self::HandoverMismatch(_) => "process_segment_handover_mismatch",
            Self::Controller(_) => "process_segment_controller",
            Self::Boundary(_) => "process_segment_boundary",
            Self::HandoverWrite(_) => "process_segment_handover_write",
            Self::AdmissionInvariant(_) => "process_segment_admission_invariant",
        }
    }

    fn into_output(self) -> ProcessAwaitOutput {
        let code = self.code();
        let (Self::HandoverMissing(message)
        | Self::HandoverMismatch(message)
        | Self::Controller(message)
        | Self::Boundary(message)
        | Self::HandoverWrite(message)
        | Self::AdmissionInvariant(message)) = self;
        ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::failure(
            lash_core::ToolFailure::runtime(lash_core::ToolFailureClass::Execution, code, message),
        ))
    }
}

/// Whether a failing segment already resolved its cancel promise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SegmentSignal {
    Unresolved,
    Resolved,
}

/// What a SubstrateLost recovery's completion step recorded.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "recovery", content = "value", rename_all = "snake_case")]
enum SubstrateLostRecovery {
    /// The process's stored terminal: the recovery's, or one stored first.
    Ended(Box<ProcessAwaitOutput>),
    /// A later segment carries the process; nothing was stored.
    HandedOver { segment_ordinal: u64 },
}

fn cancelled_output(process_id: &ProcessId) -> ProcessAwaitOutput {
    ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::cancelled(
        lash_core::ToolCancellation::runtime(format!("process `{process_id}` was cancelled")),
    ))
}

#[restate_sdk::workflow]
pub trait LashProcessWorkflow {
    async fn run(
        input: Json<RestateProcessWorkflowPayload>,
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
    retry_max_attempts: u64,
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
            retry_max_attempts: super::PROCESS_HANDLER_MAX_ATTEMPTS,
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

    /// Stop retrying a segment after `max_attempts` attempts and pause its
    /// invocation (FIG-3675): the process parks through the park reconcile
    /// instead of burning retries forever.
    pub(crate) fn with_retry_max_attempts(mut self, max_attempts: u64) -> Self {
        self.retry_max_attempts = max_attempts;
        self
    }

    /// The attempts a segment's `run` invocation makes before it pauses.
    pub(crate) fn retry_max_attempts(&self) -> u64 {
        self.retry_max_attempts
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
    ///
    /// An input whose process id this build cannot read (a retired
    /// generation's host-chosen name) names no process this store holds, so
    /// only the invocation is refused.
    async fn refuse_retired_journal(
        &self,
        process_id: Option<&ProcessId>,
        journal_version: u32,
    ) -> HandlerResult<Json<RestateProcessWorkflowOutput>> {
        let found = format!("restate-process-journal-v{journal_version}");
        let named = process_id.map_or("an unreadable process id", ProcessId::as_str);
        tracing::warn!(
            process_id = named,
            found = found.as_str(),
            expected = RESTATE_PROCESS_JOURNAL_VERSION,
            "refusing a process segment built for a retired journal generation"
        );
        let Some(process_id) = process_id else {
            return Err(TerminalError::new(format!(
                "process segment input for {named} carries {found}; this handler journals generation {RESTATE_PROCESS_JOURNAL_VERSION}"
            ))
            .into());
        };
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
            Vec::new(),
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

    /// End the process Failed, typed by `failure`'s code, from a segment
    /// error no retry can fix, and publish the terminal: a segment never
    /// ends while its process stays Running with its awaiters parked
    /// (FIG-3789). `signal` says whether the segment already resolved its
    /// cancel promise.
    async fn fail_segment(
        &self,
        context: &WorkflowContext<'_>,
        process_id: &ProcessId,
        segment_ordinal: u64,
        failure: SegmentFailure,
        signal: SegmentSignal,
    ) -> HandlerResult<Json<RestateProcessWorkflowOutput>> {
        tracing::warn!(
            process_id = process_id.as_str(),
            segment_ordinal,
            code = failure.code(),
            "a process segment failed; ending the process"
        );
        let output = self
            .complete_terminal_step(
                context,
                process_id,
                TerminalProposal::Output {
                    output: Box::new(failure.into_output()),
                    prelude: Vec::new(),
                },
            )
            .await?;
        if signal == SegmentSignal::Unresolved {
            resolve_process_cancel_signal(context, RestateProcessCancelSignal::SegmentFinished)?;
        }
        self.deliver_segment_terminal(context, process_id, segment_ordinal, output)
            .await
    }

    /// End a process whose segment's journal is lost, as the completion step
    /// (`lash.process.complete`) does, unless a later segment already carries
    /// it: the recovery authority's check runs in the store's terminal append
    /// transaction, and the step journals which way it went, so a replay
    /// reads the recorded answer and never re-queries (FIG-3820).
    async fn recover_substrate_lost_step(
        &self,
        journal: &WorkflowContext<'_>,
        process_id: &ProcessId,
        segment_ordinal: u64,
        owner: lash_core::LeaseOwnerIdentity,
    ) -> Result<SubstrateLostRecovery, HandlerError> {
        let registry = &self.registry;
        let Json(recovered) = journal
            .run_json_or_retry_send::<Result<SubstrateLostRecovery, String>, _>(
                COMPLETE_STEP.to_string(),
                async move {
                    let proposed = ProcessAwaitOutput::Abandoned {
                        evidence: Box::new(AbandonEvidence {
                            writer: AbandonWriter::ResumeRefused {
                                reason: lash_core::ProcessResumeRefusal::SubstrateLost,
                            },
                            owner: Some(owner),
                            epoch_ms: restate_now_ms(),
                        }),
                        control: None,
                    };
                    let authority = lash_core::ProcessCompletionAuthority::WorkflowKeyRecovery {
                        workflow_key: process_id.to_string(),
                        segment_ordinal,
                    };
                    match registry
                        .complete_process(process_id, proposed, authority)
                        .await
                    {
                        Ok(completion) => match completion.stored().outcome.clone() {
                            Some(stored) => Ok(Ok(SubstrateLostRecovery::Ended(Box::new(stored)))),
                            None => Ok(Err(format!(
                                "process `{process_id}` completion returned a non-terminal record"
                            ))),
                        },
                        Err(PluginError::ProcessHandedOver {
                            segment_ordinal, ..
                        }) => Ok(Ok(SubstrateLostRecovery::HandedOver { segment_ordinal })),
                        Err(error) => step_fault(error),
                    }
                },
            )
            .await
            .map_err(HandlerError::from)?;
        recovered.map_err(|refusal| TerminalError::new(refusal).into())
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
                    let (proposed, prelude) = match proposal {
                        TerminalProposal::Output { output, prelude } => (*output, prelude),
                        TerminalProposal::Abandoned { writer, owner } => (
                            ProcessAwaitOutput::Abandoned {
                                evidence: Box::new(AbandonEvidence {
                                    writer,
                                    owner,
                                    epoch_ms: restate_now_ms(),
                                }),
                                control: None,
                            },
                            Vec::new(),
                        ),
                    };
                    match complete_process_outcome(registry, process_id, proposed, prelude).await {
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
        complete_process_outcome(&self.registry, process_id, proposed, Vec::new()).await
    }

    /// [`run_registration`](Self::run_registration) for a test that drives a
    /// segment without the handler's admission: the segment proof is minted
    /// from the scope and the execution authority the test supplies, and the
    /// proposed terminal is stored directly rather than through the
    /// completion step.
    #[cfg(test)]
    pub(crate) async fn run_registration_for_test(
        &self,
        process_id: ProcessId,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        scoped_effect_controller: ScopedEffectController<'_>,
        segment_ordinal: u64,
        handover: Option<lash_core::SegmentHandover>,
    ) -> Result<lash_core::ProcessRunOutcome, HandlerError> {
        let started = SegmentStarted::for_test(
            scoped_effect_controller.admitted_scope().clone(),
            segment_ordinal,
            execution_context.execution_write_authority.clone(),
        );
        match self
            .run_registration(
                process_id.clone(),
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
                let (proposed, prelude) = match proposal {
                    TerminalProposal::Output { output, prelude } => (*output, prelude),
                    TerminalProposal::Abandoned { writer, owner } => (
                        ProcessAwaitOutput::Abandoned {
                            evidence: Box::new(AbandonEvidence {
                                writer,
                                owner,
                                epoch_ms: restate_now_ms(),
                            }),
                            control: None,
                        },
                        Vec::new(),
                    ),
                };
                let stored =
                    complete_process_outcome(&self.registry, &process_id, proposed, prelude)
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
        process_id: ProcessId,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        scoped_effect_controller: ScopedEffectController<'_>,
        started: &SegmentStarted,
        handover: Option<lash_core::SegmentHandover>,
    ) -> Result<SegmentRunEnd, HandlerError> {
        let segment_ordinal = started.segment_ordinal();
        let execution_context =
            execution_context.with_execution_write_authority(started.write_authority().clone());
        if segment_ordinal > 0 && handover.is_none() {
            return Err(HandlerError::from(TerminalError::new(format!(
                "process `{process_id}` segment {segment_ordinal} omitted its validated handover"
            ))));
        }
        // A parked process's retry re-runs its body to find out whether this
        // build can replay the journal (FIG-3659 NOW-B): the park stays
        // listed, and stops refusing until the body refuses again. The read
        // and the write sit outside the journal — they issue no command, so
        // they cannot move the replay.
        if let Some(record) = self
            .registry
            .get_process(&process_id)
            .await
            .map_err(HandlerError::from)?
            .filter(ProcessRecord::is_refusing_park)
        {
            self.registry
                .begin_parked_rerun_with_authority(&process_id, &park_authority(&record, started))
                .await
                .map_err(HandlerError::from)?;
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
                process_id.clone(),
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
            Ok(lash_core::ProcessRunOutcome::Terminal { output, prelude })
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
                        prelude,
                    })
                } else {
                    Ok(lash_core::ProcessRunOutcome::Terminal { output, prelude })
                }
            }
            outcome => outcome,
        };
        match outcome {
            Ok(lash_core::ProcessRunOutcome::Terminal { output, prelude }) => {
                // The terminal append writes the ended parent scope's ledger
                // row in the same store transaction; the sweep, not this
                // handler, cancels the children.
                Ok(SegmentRunEnd::Terminal(TerminalProposal::Output {
                    output,
                    prelude,
                }))
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
            // A segment whose journal diverged parks the process (FIG-3659
            // NOW-B, FIG-3674): the park is written through the registry —
            // non-terminal, no terminal evidence, its claims held — and the
            // attempt ends the one way a park ends, retryably, because it
            // cannot complete mid-replay (FIG-3697). A retry re-parks the same
            // park until a build that can replay the journal runs it.
            Err(err) if is_replay_mismatch(&err) => {
                self.park_diverged_process(&process_id, &err, started).await;
                Err(crate::parked_turn_failure(err))
            }
            Err(err) if err.is_retryable() => Err(HandlerError::from(err)),
            // Another owner carries the process, or no process is left to
            // end: this invocation stops without writing a terminal.
            Err(
                err @ (PluginError::ProcessLeaseSuperseded { .. }
                | PluginError::SessionExecutionLeaseLost { .. }
                | PluginError::ProcessUnknown { .. }
                | PluginError::ProcessNotVisible { .. }),
            ) => Err(handler_error_from_plugin(err)),
            // A failure retrying cannot fix without changing durable state
            // ends the process Failed, typed by its code.
            Err(err) if err.is_terminal() => {
                Ok(SegmentRunEnd::Terminal(TerminalProposal::Output {
                    output: Box::new(terminal_process_output(err)),
                    prelude: Vec::new(),
                }))
            }
            // Any other runner failure is the host's infrastructure (an
            // unavailable store, a plugin the deployment could not wire),
            // never a producer outcome: the attempt fails retryably, so the
            // process stays recoverable on its retry policy rather than
            // ending with neither a terminal nor a live invocation
            // (FIG-3789).
            Err(err) => Err(HandlerError::from(err)),
        }
    }

    /// Park `process_id` on the replay refusal `refusal` (FIG-3659 NOW-B).
    ///
    /// Best effort, like a turn's park: the attempt fails retryably either
    /// way, and a retry that refuses again writes the park again, so a failed
    /// write is logged rather than allowed to turn a park into a failure.
    async fn park_diverged_process(
        &self,
        process_id: &ProcessId,
        refusal: &PluginError,
        started: &SegmentStarted,
    ) {
        let Some(reason) = refusal.park_reason() else {
            return;
        };
        let code = reason.code();
        let parked = match self.registry.get_process(process_id).await {
            Ok(Some(record)) => {
                self.registry
                    .park_process_with_authority(
                        process_id,
                        reason.into(),
                        &park_authority(&record, started),
                    )
                    .await
            }
            Ok(None) => Err(lash_core::runtime::registry_transitions::unknown_process(
                process_id,
            )),
            Err(error) => Err(error),
        };
        match parked {
            Ok(parked) => {
                lash_core::operational_metrics::record_work_parked("process", code.as_str());
                let park = parked.park.as_deref();
                tracing::warn!(
                    event = "process.parked",
                    process_id = process_id.as_str(),
                    reason_code = code.as_str(),
                    effect_kind = park
                        .and_then(|park| park.reason.effect_kind())
                        .unwrap_or_default(),
                    attempts = park.map_or(0, |park| park.attempts),
                    park_id = park.map_or(0, |park| park.park_id.feed_sequence()),
                    "process parked on a replay divergence"
                );
            }
            Err(error) => tracing::error!(
                event = "process.park_record_failed",
                process_id = process_id.as_str(),
                reason_code = code.as_str(),
                error = %error,
                "a diverged process could not record its park"
            ),
        }
    }
}

/// Store `proposed` as the process's terminal, answering the outcome the
/// registry kept: this one, or the one an earlier writer committed.
pub(crate) async fn complete_process_outcome(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &ProcessId,
    proposed: ProcessAwaitOutput,
    prelude: Vec<lash_core::ProcessEventAppendRequest>,
) -> Result<ProcessAwaitOutput, PluginError> {
    let completion = registry
        .complete_process_with_prelude(
            process_id,
            proposed,
            prelude,
            workflow_key_authority(process_id),
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

/// Record `request`'s cancellation in the registry.
async fn record_cancel_requested(
    registry: &Arc<dyn ProcessRegistry>,
    request: &RestateProcessCancelRequest,
) -> Result<(), PluginError> {
    registry
        .append_event(
            &request.process_id,
            lash_core::ProcessEventAppendRequest::cancel_requested(
                &request.process_id,
                &request.request,
            ),
        )
        .await
        .map(|_| ())
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
        Json(payload): Json<RestateProcessWorkflowPayload>,
    ) -> HandlerResult<Json<RestateProcessWorkflowOutput>> {
        // The journal-generation gate runs before the handler journals
        // anything, and before the input is decoded against this generation's
        // shape: an input built for another command prefix is refused typed
        // rather than replayed against commands it never recorded (FIG-3588).
        let input = match payload {
            RestateProcessWorkflowPayload::Current(input) => *input,
            RestateProcessWorkflowPayload::Retired {
                journal_version,
                process_id,
            } => {
                return self
                    .refuse_retired_journal(process_id.as_ref(), journal_version)
                    .await;
            }
        };
        let process_id = input.process_id.clone();
        // Admission is the handler's first journaled work: the verdict, then
        // the start marker, and only the proof the start returns can mint the
        // segment's effect controller or drive its runner (FIG-3588). The
        // verdict records what decides the rest of the journal's shape: whether
        // the segment is superseded or its handover missing, the digest of the
        // handover it resumes from, and its boundary policy (FIG-3673). FIG-788:
        // nothing branches around the runner on a non-journaled read.
        let selector = Arc::clone(&self.segment_effect_budget);
        let registration = input.registration.clone();
        let current_generation = self.runner.executable_generation(&input.registration);
        let (started, handover_digest_recorded, policy, writer) = match admit_segment(
            &ctx,
            &self.registry,
            &self.continuations,
            &process_id,
            input.segment_ordinal,
            current_generation.clone(),
            move || selector(&registration),
        )
        .await?
        {
            SegmentAdmission::Started(admitted) => {
                let super::admission::AdmittedSegment {
                    started,
                    handover,
                    policy,
                    writer,
                } = *admitted;
                (started, handover, policy, writer)
            }
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
            SegmentAdmission::Ended { output } => {
                // The process ended before this segment could carry it on: run
                // nothing, republish the stored terminal (FIG-3820).
                resolve_process_cancel_signal(&ctx, RestateProcessCancelSignal::SegmentFinished)?;
                return self
                    .deliver_segment_terminal(&ctx, &process_id, input.segment_ordinal, *output)
                    .await;
            }
            SegmentAdmission::MissingHandover => {
                return self
                    .fail_segment(
                        &ctx,
                        &process_id,
                        input.segment_ordinal,
                        SegmentFailure::HandoverMissing(format!(
                            "missing persisted handover for process `{process_id}` segment {}",
                            input.segment_ordinal
                        )),
                        SegmentSignal::Unresolved,
                    )
                    .await;
            }
            SegmentAdmission::Invariant { message } => {
                return self
                    .fail_segment(
                        &ctx,
                        &process_id,
                        input.segment_ordinal,
                        SegmentFailure::AdmissionInvariant(message),
                        SegmentSignal::Unresolved,
                    )
                    .await;
            }
            SegmentAdmission::SubstrateLost { lost } => {
                tracing::warn!(
                    process_id = process_id.as_str(),
                    segment_ordinal = input.segment_ordinal,
                    lost_owner_id = lost.owner.owner_id.as_str(),
                    lost_attempt = lost.attempt,
                    "a process segment started under a journal this invocation cannot read; abandoning instead of re-running"
                );
                let output = match self
                    .recover_substrate_lost_step(
                        &ctx,
                        &process_id,
                        input.segment_ordinal,
                        lost.owner,
                    )
                    .await?
                {
                    SubstrateLostRecovery::Ended(output) => *output,
                    // A later segment carries the process: the lost execution
                    // handed over before this recovery could end it, and the
                    // recovery stands down (FIG-3820).
                    SubstrateLostRecovery::HandedOver { segment_ordinal } => {
                        return Ok(Json(RestateProcessWorkflowOutput::SegmentChained {
                            next_segment_ordinal: segment_ordinal,
                        }));
                    }
                };
                resolve_process_cancel_signal(&ctx, RestateProcessCancelSignal::SegmentFinished)?;
                return self
                    .deliver_segment_terminal(&ctx, &process_id, input.segment_ordinal, output)
                    .await;
            }
        };
        // The generation fence (FIG-3571), segment 0 included: the start
        // journaled the stamp the incarnation's record names, and a runner
        // whose engine runs another generation parks the process before the
        // segment's controller or runner exists, exactly as a diverged journal
        // parks it.
        if let Err(refusal) =
            lash_core::ExecutableGenerationRefusal::check(started.generation(), current_generation)
        {
            let refused =
                PluginError::Runtime(lash_core::RuntimeError::retired_process_generation(refusal));
            self.park_diverged_process(&process_id, &refused, &started)
                .await;
            return Err(crate::parked_turn_failure(refused));
        }
        // The handover a later segment resumes from is immutable per ordinal:
        // read it once after admission, hold it to the digest the verdict
        // recorded, and journal it. A redrive replays the runner from the
        // recorded handover, so the segment may retire it once it has handed
        // over (FIG-3809).
        let mut handover = match handover_digest_recorded {
            None => None,
            Some(recorded) => {
                let continuations = &self.continuations;
                let pid = &process_id;
                let segment_ordinal = input.segment_ordinal;
                let Json(resumed) = ctx
                    .run_json_or_retry_send::<Result<lash_core::SegmentHandover, SegmentFailure>, _>(
                        RESUME_STEP.to_string(),
                        async move {
                            let persisted = match continuations
                                .get_segment_handover(pid, segment_ordinal)
                                .await
                            {
                                Ok(persisted) => persisted,
                                Err(error) if error.is_retryable() => {
                                    return Err(error.to_string());
                                }
                                Err(error) => {
                                    return Ok(Err(SegmentFailure::HandoverMissing(
                                        error.to_string(),
                                    )));
                                }
                            };
                            let Some(persisted) = persisted else {
                                return Ok(Err(SegmentFailure::HandoverMissing(format!(
                                    "missing persisted handover for process `{pid}` segment \
                                     {segment_ordinal}: its admission recorded one that is no \
                                     longer retained"
                                ))));
                            };
                            Ok(match handover_digest(&persisted.handover) {
                                Ok(digest) if digest == recorded => Ok(persisted.handover),
                                Ok(_) => Err(SegmentFailure::HandoverMismatch(format!(
                                    "process `{pid}` segment {segment_ordinal} handover differs \
                                     from the one its admission recorded"
                                ))),
                                Err(error) => Err(SegmentFailure::HandoverMismatch(format!(
                                    "process `{pid}` segment {segment_ordinal} handover digest: \
                                     {error:?}"
                                ))),
                            })
                        },
                    )
                    .await
                    .map_err(HandlerError::from)?;
                match resumed {
                    Ok(handover) => Some(handover),
                    Err(failure) => {
                        return self
                            .fail_segment(
                                &ctx,
                                &process_id,
                                input.segment_ordinal,
                                failure,
                                SegmentSignal::Unresolved,
                            )
                            .await;
                    }
                }
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
            let scoped_effect_controller = match controller.process_segment_controller(&started) {
                Ok(scoped) => scoped,
                Err(error) => {
                    return self
                        .fail_segment(
                            controller.context(),
                            &process_id,
                            input.segment_ordinal,
                            SegmentFailure::Controller(error.to_string()),
                            SegmentSignal::Unresolved,
                        )
                        .await;
                }
            };
            let end = self
                .run_registration(
                    process_id.clone(),
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
                let pid = &process_id;
                let Json(declined) = controller
                    .context()
                    .run_json_or_retry_send::<Result<bool, String>, _>(
                        BOUNDARY_STEP.to_string(),
                        async move {
                            match registry.get_process(pid).await {
                                Ok(record) => Ok(Ok(boundary_must_be_declined(record.as_ref()))),
                                Err(error) => step_fault(error),
                            }
                        },
                    )
                    .await
                    .map_err(HandlerError::from)?;
                match declined {
                    Ok(true) => {
                        handover = Some(boundary.clone());
                        continue;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        return self
                            .fail_segment(
                                controller.context(),
                                &process_id,
                                input.segment_ordinal,
                                SegmentFailure::Boundary(error),
                                SegmentSignal::Unresolved,
                            )
                            .await;
                    }
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
                                writer,
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
        if let Err(error) = handed_over {
            // Nothing was sent: the process is still this segment's to end.
            return self
                .fail_segment(
                    context,
                    &process_id,
                    input.segment_ordinal,
                    SegmentFailure::HandoverWrite(error),
                    SegmentSignal::Resolved,
                )
                .await;
        }
        // FIG-788: successor emission is unconditional. A cancellation
        // can land between attempts, so the recorded read below may
        // shape only commands after this deployed prefix.
        let request = context
            .workflow_client::<LashProcessWorkflowClient>(successor_key.clone())
            .run(Json(
                RestateProcessWorkflowInput {
                    process_id: process_id.clone(),
                    registration: input.registration,
                    execution_context: input.execution_context,
                    segment_ordinal: next_segment_ordinal,
                    journal_version: RESTATE_PROCESS_JOURNAL_VERSION,
                }
                .into(),
            ));
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
                        // No process is left to cancel.
                        Ok(None) => return Ok(Ok(None)),
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
        // The successor carries the process from the send on, so a failure
        // from here cannot strand it: it ends only this invocation.
        if let Some(cancel) = forward.map_err(TerminalError::new)? {
            let deliver = context
                .workflow_client::<LashProcessWorkflowClient>(successor_key)
                .deliver_cancel(Json(cancel));
            let Json(()) = deliver.call().await?;
        }
        // This segment has handed the process on and recorded the cancel it
        // forwards, so its handover is no longer anyone's to read: retire it,
        // and any older one. A redrive of this segment, before or after this
        // step, replays its runner from the handover its resume step
        // journaled (FIG-3809).
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
            if let Err(error) = retired {
                // Retiring is cleanup: a handover left behind is deleted with
                // the process when it is pruned, and the successor carries
                // the process either way.
                tracing::warn!(
                    process_id = process_id.as_str(),
                    segment_ordinal = input.segment_ordinal,
                    error = error.as_str(),
                    "a retired process segment could not delete its handovers"
                );
            }
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
                    let record = match registry.get_process(&cancel.process_id).await {
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
        let process_id = &request.process_id;
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
                    &request.process_id,
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

/// The execution authority a segment writes its park under: the segment's
/// execution identity bound to the attempt its root start recorded. Restate
/// successors of one attempt share the root execution id, so the record's
/// retained start names the attempt the identity is valid for; a stale
/// identity still fails the registry's same-execution fence.
fn park_authority(
    record: &ProcessRecord,
    started: &SegmentStarted,
) -> lash_core::ProcessExecutionWriteAuthority {
    started.write_authority().bind_attempt(
        record
            .first_started
            .as_deref()
            .map_or(1, |started| started.attempt),
    )
}
