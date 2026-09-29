//! Scheduling boundary for a Restate-backed process start.
//!
//! Registration commits the durable row before the workflow is submitted, so
//! the submit failure path owns a compensation: these two functions keep that
//! pairing in one place.

use super::process_command::{process_command_journal_error, process_command_journal_name};
use super::*;
use restate_sdk::serde::Json;

/// Why a process workflow submission did not return an invocation id.
///
/// The distinction decides whether the scheduling boundary may compensate. A
/// `StartFailed` cancellation is terminal on the spot for a row with no
/// execution and no external reference, so writing one for a submission that
/// did reach the runtime would terminalise a row whose workflow is running.
/// The workflow would then do the child's work and fail its own terminal write.
/// Only a failure that proves the run was never accepted may compensate.
#[derive(Debug)]
pub enum ProcessWorkflowStartFailure {
    /// A reply or journaled terminal failure proves no invocation was accepted.
    Rejected(TerminalError),
    /// No proof of non-acceptance exists, so an invocation may be running.
    Ambiguous(TerminalError),
}

impl ProcessWorkflowStartFailure {
    /// The failure the journaled send's await answered with. A terminal
    /// failure there is proof of non-acceptance: the runtime records the send
    /// as an entry, and a transient condition (no connection, no reply yet)
    /// suspends and retries the handler instead of completing the entry. The
    /// one exception is the engine's cancellation of the sending invocation
    /// (the call's group decided its cancel): it surfaces at whatever the
    /// handler awaits, and the send command it interrupted was journaled
    /// before that await, so the run may well be accepted (FIG-4128).
    pub fn of_send(error: TerminalError) -> Self {
        if context::is_engine_cancellation(&error) {
            Self::Ambiguous(error)
        } else {
            Self::Rejected(error)
        }
    }

    /// The underlying failure, whichever class it is.
    pub fn error(&self) -> &TerminalError {
        match self {
            Self::Rejected(error) | Self::Ambiguous(error) => error,
        }
    }

    /// Whether a compensating `StartFailed` cancellation is sound to write.
    pub fn proves_nothing_is_running(&self) -> bool {
        matches!(self, Self::Rejected(_))
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn schedule_restate_process<'ctx, C>(
    registry: Arc<dyn ProcessRegistry>,
    starts: Option<Arc<lash_core::runtime::process_start::ProcessStartRelay>>,
    started: lash_core::runtime::RegisteredProcessStart,
    registration: lash_core::ProcessRegistration,
    execution_context: lash_core::ProcessExecutionContext,
    sender_generation: Option<lash_core::engine::BuildGeneration>,
    context: &C,
    namespace: &crate::RestateNamespace,
    invocation: &RuntimeEffectInvocation,
) -> Result<(ProcessRecord, lash_core::StoreRealization), RuntimeEffectControllerError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
{
    // The registration is the recorded one: a start under a retained key is
    // idempotent and returns the row an earlier call created, whatever this
    // call's content (ADR 0107). Compensation below may only touch a row
    // *this* start created, so the recorded disposition -- not the mere
    // success of the registration -- is what licenses it. Every registry
    // write after the send is journaled too, so a replay after the process
    // was pruned reads what the live start wrote and never the store.
    let realization = started.realization();
    let created_here = started.disposition == lash_core::ProcessRegistrationOutcome::Created;
    let record = started.record;
    let process_id = record.id.clone();
    // Registration armed the row's `ProcessStart` obligation; the journaled
    // send below is that obligation's delivery, so claim the row first
    // (ADR 0109 §1.5): the reconcile relay does not deliver it under us, and
    // the settle after the send is the delivery's commit. A `None` relay or
    // a `None` claim — the row was already taken — leaves the reconcile pass
    // its row; the send below coalesces on the workflow key either way.
    //
    // The engine's cancellation of this invocation (the call's group decided
    // its cancel) may surface at the claim's await after the claim was taken,
    // and withhold the token it answered. The claim then runs again, and the
    // start goes on to its send either way: the call's abandonment cancels
    // the child, and only a submitted run can honour that cancel (FIG-4127).
    // The claim's token is derived from this claim step's identity, the same
    // on both runs and on every replay, so the rerun takes back the claim the
    // first run took and the settle below delivers the row at once rather
    // than leaving it claimed until the relay retakes it (FIG-4131).
    let claimant = process_command_journal_name(invocation, "process-start-claim");
    let claim_token = match &starts {
        Some(starts) => {
            let Json(token) = run_past_engine_cancel(
                context,
                invocation,
                ["process-start-claim", "process-start-claim-after-cancel"],
                || {
                    let starts = Arc::clone(starts);
                    let claimed_id = process_id.clone();
                    let claimant = claimant.clone();
                    async move {
                        starts
                            .claim_start(&claimed_id, &claimant)
                            .await
                            .map(|token| token.map(|token| token.as_str().to_owned()))
                            .map_err(|error| error.to_string())
                    }
                },
            )
            .await
            .map_err(|error| process_command_journal_error("start claim", error))?;
            token
        }
        None => None,
    };
    let invocation_id = match context
        .start_process_workflow(
            namespace,
            process_id.clone(),
            registration,
            execution_context,
            sender_generation,
        )
        .await
    {
        Ok(invocation_id) => invocation_id,
        Err(failure) => {
            let submit_error = PluginError::Runtime(RuntimeError::new(
                RuntimeErrorCode::EngineProcessIngressSubmit,
                format!("Restate process workflow start failed: {}", failure.error()),
            ));
            // Registration already committed, so returning the error bare would
            // leave a Running, unowned row that nothing in the caller's cancel
            // path can reach. Compensate inside the scheduling boundary, before
            // the error reaches the caller: a StartFailed request against a row
            // with no execution and no external reference is terminal on the
            // spot (see `prepare_process_event_append`), so the child is
            // cancelled and never runs.
            //
            // Two things withhold that write. An ambiguous failure carries no
            // proof the run was refused, so an invocation may be executing and
            // terminalising the row would make the workflow's own terminal
            // write fail. And a row this start did not create belongs to the
            // attempt that did: cancelling it would let a retry kill the work
            // its predecessor started.
            if !failure.proves_nothing_is_running() || !created_here {
                tracing::warn!(
                    process_id = %process_id,
                    submit_error = %submit_error,
                    ambiguous = !failure.proves_nothing_is_running(),
                    created_here,
                    "Restate process submission failed without licensing a start-failed compensation; recovery owns the row"
                );
                // The claim this start took on the armed row goes back due:
                // the failure stays readable on the row as `last_error`, and
                // the reconcile pass retries the delivery — an ambiguous
                // failure may already be running, and a retained row's start
                // belongs to the attempt that created it.
                if let (Some(starts), Some(token)) = (&starts, claim_token.clone()) {
                    settle_start_claim(
                        starts,
                        context,
                        invocation,
                        &process_id,
                        token,
                        lash_core::store::ObligationSettlement::Retry {
                            due_at_ms: 0,
                            error: submit_error.to_string(),
                        },
                    )
                    .await?;
                }
                return Ok((record, realization));
            }
            let compensation_registry = Arc::clone(&registry);
            let compensation_record = record.clone();
            let compensation_error = submit_error.to_string();
            let Json(compensation) = context
                .run_json_or_retry_send(
                    process_command_journal_name(invocation, "process-start-compensate"),
                    async move {
                        // The compensation write failing leaves the row
                        // exactly the shape the `ProcessStart` obligation's
                        // relay retries --
                        // nonterminal, no external reference, no cancel
                        // request -- so that answer is recorded, not retried:
                        // the start stands and the obligation owns it.
                        Ok::<_, String>(
                            match compensate_failed_process_submission(
                                compensation_registry.as_ref(),
                                &compensation_record,
                                &compensation_error,
                            )
                            .await
                            {
                                Ok(compensated) => Some(compensated),
                                Err(error) => {
                                    tracing::error!(
                                        process_id = %compensation_record.id,
                                        submit_error = %compensation_error,
                                        %error,
                                        "Restate process submission failed and its start-failed compensation could not be written; recovery owns the row"
                                    );
                                    None
                                }
                            },
                        )
                    },
                )
                .await
                .map_err(|error| process_command_journal_error("start compensation", error))?;
            let compensated = compensation.is_some();
            // The `StartFailed` request is terminal on the spot only for a row
            // nothing runs. One a run already holds -- another delivery of the
            // obligation started it -- keeps the request standing, and a
            // standing request reaches a live run only through the workflow's
            // `cancel` handler, as every other cancel does (FIG-4128).
            if let Some(standing) = compensation.filter(|record| !record.is_terminal()) {
                context
                    .request_process_workflow_cancel(
                        namespace,
                        RestateProcessCancelRequest::from_record(&standing)?,
                    )
                    .await
                    .map_err(|error| {
                        RuntimeEffectControllerError::new(
                            RuntimeErrorCode::EngineProcessCancel,
                            format!(
                                "delivering process `{process_id}`'s start-failed cancel to its run failed: {error}"
                            ),
                        )
                    })?;
            }
            // The claim settles with the verdict the row ended at: the
            // compensated row is terminal or held by its run, so nothing
            // remains to deliver; one whose compensation could not be
            // written goes back due
            // with the failure recorded for the reconcile pass.
            if let (Some(starts), Some(token)) = (&starts, claim_token.clone()) {
                let settlement = if compensated {
                    lash_core::store::ObligationSettlement::Delivered
                } else {
                    lash_core::store::ObligationSettlement::Retry {
                        due_at_ms: 0,
                        error: submit_error.to_string(),
                    }
                };
                settle_start_claim(starts, context, invocation, &process_id, token, settlement)
                    .await?;
            }
            return if compensated {
                Err(submit_error.into())
            } else {
                Ok((record, realization))
            };
        }
    };
    // A new process starts on the stable lane (FIG-3795): the newest build
    // runs segment 0.
    let route = namespace
        .stable(crate::LashService::ProcessWorkflow)
        .to_string();
    let settle_id = process_id.clone();
    let Json(record) = run_past_engine_cancel(
        context,
        invocation,
        [
            "process-start-external-ref",
            "process-start-external-ref-after-cancel",
        ],
        || {
            let registry = Arc::clone(&registry);
            let process_id = process_id.clone();
            let route = route.clone();
            let invocation_id = invocation_id.clone();
            async move {
                registry
                    .set_external_ref(
                        &process_id,
                        ProcessExternalRef {
                            backend: "restate".to_string(),
                            id: format!(
                                "{}/{}",
                                route,
                                crate::process::process_segment_workflow_key(&process_id, 0)
                            ),
                            metadata: Some(serde_json::json!({ "invocation_id": invocation_id })),
                            // A live start always schedules the first segment;
                            // a later segment's reference is written by the
                            // handover path or the lost-run pass and
                            // supersedes this one.
                            segment_ordinal: Some(0),
                        },
                    )
                    .await
                    .map_or_else(
                        |error| {
                            if error.is_terminal() {
                                Ok(Err(RuntimeEffectControllerError::from(error)))
                            } else {
                                Err(error.to_string())
                            }
                        },
                        |record| Ok(Ok(record)),
                    )
            }
        },
    )
    .await
    .map_err(|error| process_command_journal_error("start external reference", error))?;
    // The journaled send delivered the armed row; the claim settles
    // `Delivered` so the reconcile pass never submits it a second time.
    if let (Some(starts), Some(token)) = (&starts, claim_token) {
        settle_start_claim(
            starts,
            context,
            invocation,
            &settle_id,
            token,
            lash_core::store::ObligationSettlement::Delivered,
        )
        .await?;
    }
    Ok((record?, realization))
}

/// Settle a start obligation claimed at the top of
/// [`schedule_restate_process`]: `Delivered` once the journaled send (or the
/// compensation that replaced it) is the row's terminal answer, `Retry` when
/// the send's verdict left the row for the reconcile pass. Journaled like
/// the claim that minted `token`.
async fn settle_start_claim<'ctx, C>(
    starts: &Arc<lash_core::runtime::process_start::ProcessStartRelay>,
    context: &C,
    invocation: &RuntimeEffectInvocation,
    process_id: &lash_core::ProcessId,
    token: String,
    settlement: lash_core::store::ObligationSettlement,
) -> Result<(), RuntimeEffectControllerError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
{
    // A settle that already applied answers `ClaimLost` when it runs again.
    let Json(()) = run_past_engine_cancel(
        context,
        invocation,
        ["process-start-settle", "process-start-settle-after-cancel"],
        || {
            let starts = Arc::clone(starts);
            let process_id = process_id.clone();
            let token = lash_core::store::ClaimToken::new(token.clone());
            let settlement = settlement.clone();
            async move {
                starts
                    .settle_start(&process_id, token, settlement)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            }
        },
    )
    .await
    .map_err(|error| process_command_journal_error("start settle", error))?;
    Ok(())
}

/// One journaled step of the scheduling boundary that the engine's
/// cancellation of this invocation does not end: once registration
/// committed, the start must reach the engine, or be compensated, whatever
/// becomes of the call that issued it (FIG-4127). The cancellation surfaces
/// once, at the step's await, whether or not its closure ran; the step then
/// runs again under its second journal name, which every step here answers
/// correctly after a first run whose answer was lost. A replay meets the
/// cancellation at the same await and takes the same path.
async fn run_past_engine_cancel<'ctx, C, T, Fut>(
    context: &C,
    invocation: &RuntimeEffectInvocation,
    [operation, after_cancel]: [&str; 2],
    step: impl Fn() -> Fut,
) -> Result<Json<T>, TerminalError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
    T: serde::Serialize + serde::de::DeserializeOwned + Send + 'static,
    Fut: std::future::Future<Output = Result<T, String>> + Send + 'static,
{
    match context
        .run_json_or_retry_send(process_command_journal_name(invocation, operation), step())
        .await
    {
        Err(error) if context::is_engine_cancellation(&error) => {
            context
                .run_json_or_retry_send(
                    process_command_journal_name(invocation, after_cancel),
                    step(),
                )
                .await
        }
        journaled => journaled,
    }
}

/// Request the `StartFailed` cancel of `record`'s row, answering the row as
/// the request left it.
async fn compensate_failed_process_submission(
    registry: &dyn ProcessRegistry,
    record: &ProcessRecord,
    submit_error: &str,
) -> Result<ProcessRecord, PluginError> {
    registry
        .request_process_cancel(
            &record.id,
            lash_core::CancelOrigin::StartFailed,
            format!("restate:start-failed:{}", record.id),
            None,
        )
        .await
        .inspect_err(|_| {
            tracing::warn!(
                process_id = %record.id,
                error = %submit_error,
                "Restate process workflow submission failed"
            );
        })
}
