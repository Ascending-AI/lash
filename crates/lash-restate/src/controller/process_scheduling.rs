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

pub(super) async fn schedule_restate_process<'ctx, C>(
    registry: Arc<dyn ProcessRegistry>,
    started: lash_core::runtime::RegisteredProcessStart,
    registration: lash_core::ProcessRegistration,
    execution_context: lash_core::ProcessExecutionContext,
    context: &C,
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
    let created_here = started.created;
    let record = started.record;
    let process_id = record.id.clone();
    let invocation_id = match context
        .start_process_workflow(process_id.clone(), registration, execution_context)
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
                        // exactly the shape the recovery sweep resubmits --
                        // nonterminal, no external reference, no cancel
                        // request -- so that answer is recorded, not retried:
                        // the start stands and recovery owns the run.
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
            return if compensated {
                Err(submit_error.into())
            } else {
                Ok((record, realization))
            };
        }
    };
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
                                crate::LashService::ProcessWorkflow.name(),
                                crate::process::process_segment_workflow_key(&process_id, 0)
                            ),
                            metadata: Some(serde_json::json!({ "invocation_id": invocation_id })),
                            // A live start always schedules the first segment;
                            // a later segment's reference is written by the
                            // handover path or the recovery sweep and
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
    Ok((record?, realization))
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
