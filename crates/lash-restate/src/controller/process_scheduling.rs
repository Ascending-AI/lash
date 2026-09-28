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
    let created_here = started.disposition == lash_core::ProcessRegistrationDisposition::Created;
    let record = started.record;
    let process_id = record.id.clone();
    // Registration armed the row's `ProcessStart` obligation; the journaled
    // send below is that obligation's delivery, so claim the row first
    // (ADR 0109 §1.5): the reconcile relay does not deliver it under us, and
    // the settle after the send is the delivery's commit. A `None` relay or
    // a `None` claim — the row was already taken — leaves the reconcile pass
    // its row; the send below coalesces on the workflow key either way.
    let claim_token = match &starts {
        Some(starts) => {
            let starts = Arc::clone(starts);
            let claimed_id = process_id.clone();
            let Json(token) = context
                .run_json_or_retry_send(
                    process_command_journal_name(invocation, "process-start-claim"),
                    async move {
                        starts
                            .claim_start(&claimed_id)
                            .await
                            .map(|token| token.map(|token| token.as_str().to_owned()))
                            .map_err(|error| error.to_string())
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
                        "process-start-settle",
                    )
                    .await?;
                }
                return Ok((record, realization));
            }
            let compensation_registry = Arc::clone(&registry);
            let compensation_record = record.clone();
            let compensation_error = submit_error.to_string();
            let Json(compensated) = context
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
                                Ok(()) => true,
                                Err(error) => {
                                    tracing::error!(
                                        process_id = %compensation_record.id,
                                        submit_error = %compensation_error,
                                        %error,
                                        "Restate process submission failed and its start-failed compensation could not be written; recovery owns the row"
                                    );
                                    false
                                }
                            },
                        )
                    },
                )
                .await
                .map_err(|error| process_command_journal_error("start compensation", error))?;
            // The claim settles with the verdict the row ended at: the
            // compensated row is terminal, so nothing remains to deliver;
            // one whose compensation could not be written goes back due
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
                settle_start_claim(
                    starts,
                    context,
                    invocation,
                    &process_id,
                    token,
                    settlement,
                    "process-start-settle",
                )
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
    let Json(record) = context
        .run_json_or_retry_send(
            process_command_journal_name(invocation, "process-start-external-ref"),
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
            "process-start-settle",
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
    step: &'static str,
) -> Result<(), RuntimeEffectControllerError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
{
    let starts = Arc::clone(starts);
    let process_id = process_id.clone();
    let Json(()) = context
        .run_json_or_retry_send(process_command_journal_name(invocation, step), async move {
            starts
                .settle_start(
                    &process_id,
                    lash_core::store::ClaimToken::new(token),
                    settlement,
                )
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| process_command_journal_error("start settle", error))?;
    Ok(())
}

async fn compensate_failed_process_submission(
    registry: &dyn ProcessRegistry,
    record: &ProcessRecord,
    submit_error: &str,
) -> Result<(), PluginError> {
    registry
        .request_process_cancel(
            &record.id,
            lash_core::CancelOrigin::StartFailed,
            format!("restate:start-failed:{}", record.id),
            None,
        )
        .await
        .map(|_| ())
        .inspect_err(|_| {
            tracing::warn!(
                process_id = %record.id,
                error = %submit_error,
                "Restate process workflow submission failed"
            );
        })
}
