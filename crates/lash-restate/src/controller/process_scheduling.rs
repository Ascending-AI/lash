//! Scheduling boundary for a Restate-backed process start.
//!
//! Registration commits the durable row before the workflow is submitted, so
//! the submit failure path owns a compensation: these two functions keep that
//! pairing in one place.

use super::*;

pub(super) async fn schedule_restate_process<'ctx, C>(
    registry: Arc<dyn ProcessRegistry>,
    registration: lash_core::ProcessRegistration,
    observers: Vec<SessionId>,
    execution_context: lash_core::ProcessExecutionContext,
    context: &C,
) -> Result<(ProcessRecord, lash_core::StoreRealization), PluginError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
{
    let process_id = registration.id.clone();
    // A differing registration fingerprint is a refusal, and an exact repeat is
    // idempotent: it returns the row an earlier call created rather than
    // failing. Compensation below may only touch a row *this* call created, so
    // the disposition -- not the mere success of the registration -- is what
    // licenses it.
    let registered = registry
        .register_process_reporting_disposition(registration.clone(), &observers)
        .await?;
    let created_here = registered.is_created();
    // The registry's own verdict, carried out to the caller so a coalesced
    // redelivery is reported as a replay rather than a fresh start (FIG-3070).
    let realization = lash_core::StoreRealization::from_wrote(created_here);
    let record = registered.record;
    let invocation_id = match context
        .start_process_workflow(registration, execution_context)
        .await
    {
        Ok(invocation_id) => invocation_id,
        Err(failure) => {
            let submit_error = PluginError::Runtime(RuntimeError::new(
                RuntimeErrorCode::RestateProcessIngressSubmit,
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
            // write fail. And a row this call did not create belongs to the
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
            return match compensate_failed_process_submission(
                registry.as_ref(),
                &record,
                &submit_error,
            )
            .await
            {
                Ok(()) => Err(submit_error),
                // The compensation write itself failed. The row is now exactly
                // the shape the recovery sweep resubmits — nonterminal, no
                // external reference, no cancel request — so the honest answer
                // is the record: the start stands and recovery owns the run.
                Err(compensation_error) => {
                    tracing::error!(
                        process_id = %process_id,
                        submit_error = %submit_error,
                        error = %compensation_error,
                        "Restate process submission failed and its start-failed compensation could not be written; recovery owns the row"
                    );
                    Ok((record, realization))
                }
            };
        }
    };
    registry
        .set_external_ref(
            &process_id,
            ProcessExternalRef {
                backend: "restate".to_string(),
                id: format!(
                    "LashProcessWorkflow/{}",
                    crate::process::process_segment_workflow_key(&process_id, 0)
                ),
                metadata: Some(serde_json::json!({ "invocation_id": invocation_id })),
                // A live start always schedules the first segment; a later
                // segment's reference is written by the handover path or the
                // recovery sweep and supersedes this one.
                segment_ordinal: Some(0),
            },
        )
        .await
        .map(|record| (record, realization))
}

/// Record the StartFailed cancellation for a row whose workflow submission
/// failed after registration.
async fn compensate_failed_process_submission(
    registry: &dyn ProcessRegistry,
    record: &ProcessRecord,
    submit_error: &PluginError,
) -> Result<(), PluginError> {
    registry
        .request_process_cancel(
            &lash_core::ProcessRef::from_record(record),
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
