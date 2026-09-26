//! Atomic, lease-fenced terminal process completion.

use super::process_registry::{
    ProcessEventAppendArm, ProcessEventBatch, ProcessEventWriteAuthorization, tx_outcome,
};
use super::*;
use lash_sansio::ProcessId;

/// Unleased terminal completion, validated and appended as one atomic unit,
/// with the run's terminal batch (`prelude`) ahead of the terminal event and
/// one process save (FIG-3571).
///
/// The load, the authority-vs-disposition validation, and the terminal append
/// all run inside a single `write_flow` transaction. Splitting validation
/// (reading the row's `disposition`) from the append leaves a window in which a
/// paused caller could re-validate against one disposition, then append after
/// the row was completed, pruned, and re-registered with a *different*
/// disposition. Holding one transaction across load→validate→append closes that
/// window: the row we validate is the row we append to.
pub(super) async fn complete_process(
    registry: &SqliteProcessRegistry,
    process_id: &ProcessId,
    await_output: ProcessAwaitOutput,
    prelude: Vec<ProcessEventAppendRequest>,
    authority: lash_core_execution::ProcessCompletionAuthority,
) -> Result<lash_core_execution::ProcessCompletionOutcome, lash_core_execution::PluginError> {
    let process_id = process_id.clone();
    let now = registry.clock.timestamp_ms();
    let wake_delivery_config = registry.wake_delivery_config;
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome((|| {
                let mut record = SqliteProcessRegistry::require_process_conn(tx, &process_id)?;
                let await_output = await_output.with_cancel_origin(
                    record
                        .cancel_request
                        .as_deref()
                        .map(|request| request.origin),
                );
                if record.is_terminal() {
                    return Ok(lash_core_execution::ProcessCompletionOutcome::from_stored(
                        record,
                        &await_output,
                    ));
                }
                // Validate the authority against the row's declared disposition
                // *inside* the transaction that appends, so a concurrent
                // complete→prune→re-register with a different disposition cannot
                // slip between the check and the append.
                authority.validate(&record, &await_output)?;
                let mut batch = ProcessEventBatch::default();
                for request in prelude {
                    batch.stage(tx, &mut record, request, now, wake_delivery_config)?;
                }
                let request = lash_core_execution::facade_support::terminal_append_request(
                    &process_id,
                    &await_output,
                    Some(&authority),
                );
                let (_, arm) = batch.stage_arm(
                    tx,
                    &mut record,
                    request,
                    now,
                    wake_delivery_config,
                    ProcessEventWriteAuthorization::Preauthorized,
                )?;
                batch.commit(tx, &record)?;
                Ok(match arm {
                    ProcessEventAppendArm::Replayed { .. } => {
                        lash_core_execution::ProcessCompletionOutcome::AlreadyApplied {
                            stored: record,
                        }
                    }
                    ProcessEventAppendArm::Inserted => {
                        lash_core_execution::ProcessCompletionOutcome::Committed(record)
                    }
                })
            })()))
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn complete_process_with_lease(
    registry: &SqliteProcessRegistry,
    lease: &ProcessLease,
    await_output: ProcessAwaitOutput,
) -> Result<lash_core_execution::ProcessCompletionOutcome, lash_core_execution::PluginError> {
    let lease = lease.clone();
    let now = registry.clock.timestamp_ms();
    let wake_delivery_config = registry.wake_delivery_config;
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome((|| {
                let process_id = lease.process_id.as_str();
                let mut record =
                    SqliteProcessRegistry::require_process_conn(tx, &lease.process_id)?;
                let await_output = await_output.with_cancel_origin(
                    record
                        .cancel_request
                        .as_deref()
                        .map(|request| request.origin),
                );
                if record.is_terminal() {
                    return Ok(lash_core_execution::ProcessCompletionOutcome::from_stored(
                        record,
                        &await_output,
                    ));
                }
                let request = lash_core_execution::facade_support::terminal_append_request(
                    &lease.process_id,
                    &await_output,
                    None,
                );
                // A successful prior terminal append is replay-idempotent even
                // though that transaction already cleared the lease, so the
                // lease fence is re-checked inside the append sequence on the
                // insert arm only.
                let (_, arm) = SqliteProcessRegistry::apply_process_event_append_conn(
                    tx,
                    &mut record,
                    request,
                    now,
                    wake_delivery_config,
                    ProcessEventWriteAuthorization::Lease(&lease),
                )?;
                if matches!(arm, ProcessEventAppendArm::Replayed { .. }) {
                    return Ok(
                        lash_core_execution::ProcessCompletionOutcome::AlreadyApplied {
                            stored: record,
                        },
                    );
                }
                // The verdict inside the append sequence authorized this
                // release; the statement's predicate is the backstop and
                // `require_fenced_write_applied` returns this site's refusal
                // if it and the locked read ever disagree.
                let released = tx
                    .execute(
                        crate::process_registry::sql::process_sql()
                            .lease
                            .release
                            .sql(),
                        params![process_id, lease.lease_token, lease.fencing_token as i64],
                    )
                    .map_err(process_sqlite_error)? as u64;
                lash_core_execution::store_backend_support::require_fenced_write_applied(
                    lash_core_execution::store_backend_support::FencedWrite::ProcessLeaseRelease,
                    crate::SQLITE_BACKEND,
                    process_id,
                    released,
                    || lash_core_execution::PluginError::ProcessLeaseSuperseded {
                        process_id: lease.process_id.clone(),
                    },
                )?;
                Ok(lash_core_execution::ProcessCompletionOutcome::Committed(
                    record,
                ))
            })()))
        })
        .await
        .map_err(process_sqlite_error)?
}
