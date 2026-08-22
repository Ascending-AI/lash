//! Atomic, lease-fenced terminal process completion.

use super::process_registry::{ProcessEventAppendArm, ProcessEventWriteAuthorization, tx_outcome};
use super::*;

/// Unleased terminal completion, validated and appended as one atomic unit.
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
    process_id: &str,
    await_output: ProcessAwaitOutput,
    authority: lash_core::ProcessCompletionAuthority,
    parent_end_actions: Vec<lash_core::ToolIntentParentEndAction>,
) -> Result<lash_core::ProcessCompletionOutcome, lash_core::PluginError> {
    let process_id = process_id.to_string();
    let now = registry.clock.timestamp_ms();
    let wake_delivery_config = registry.wake_delivery_config;
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome((|| {
                let mut record = SqliteProcessRegistry::require_process_conn(tx, &process_id)?;
                if record.is_terminal() {
                    return Ok(lash_core::ProcessCompletionOutcome::from_stored(
                        record,
                        &await_output,
                    ));
                }
                // Validate the authority against the row's declared disposition
                // *inside* the transaction that appends, so a concurrent
                // complete→prune→re-register with a different disposition cannot
                // slip between the check and the append.
                authority.validate(&process_id, record.disposition, &await_output)?;
                let request = lash_core::facade_support::terminal_append_request(
                    &process_id,
                    &await_output,
                    Some(&authority),
                );
                let (_, arm) = SqliteProcessRegistry::apply_process_event_append_conn(
                    tx,
                    &mut record,
                    request,
                    now,
                    wake_delivery_config,
                    ProcessEventWriteAuthorization::Preauthorized,
                    &parent_end_actions,
                )?;
                Ok(match arm {
                    ProcessEventAppendArm::Replayed { .. } => {
                        lash_core::ProcessCompletionOutcome::AlreadyApplied { stored: record }
                    }
                    ProcessEventAppendArm::Inserted => {
                        lash_core::ProcessCompletionOutcome::Committed(record)
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
    parent_end_actions: Vec<lash_core::ToolIntentParentEndAction>,
) -> Result<lash_core::ProcessCompletionOutcome, lash_core::PluginError> {
    let lease = lease.clone();
    let now = registry.clock.timestamp_ms();
    let wake_delivery_config = registry.wake_delivery_config;
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome((|| {
                let process_id = lease.process_id.as_str();
                let mut record = SqliteProcessRegistry::require_process_conn(tx, process_id)?;
                if record.is_terminal() {
                    return Ok(lash_core::ProcessCompletionOutcome::from_stored(
                        record,
                        &await_output,
                    ));
                }
                let request = lash_core::facade_support::terminal_append_request(
                    process_id,
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
                    &parent_end_actions,
                )?;
                if matches!(arm, ProcessEventAppendArm::Replayed { .. }) {
                    return Ok(lash_core::ProcessCompletionOutcome::AlreadyApplied {
                        stored: record,
                    });
                }
                tx.execute(
                    "UPDATE process_leases
                     SET lease_owner_id = NULL,
                         lease_owner_incarnation_id = NULL,
                         lease_owner_liveness_json = NULL,
                         lease_token = NULL,
                         lease_claimed_at_ms = 0,
                         lease_expires_at_ms = 0
                     WHERE process_id = ?1
                       AND lease_token = ?2
                       AND lease_fencing_token = ?3",
                    params![process_id, lease.lease_token, lease.fencing_token as i64],
                )
                .map_err(process_sqlite_error)?;
                Ok(lash_core::ProcessCompletionOutcome::Committed(record))
            })()))
        })
        .await
        .map_err(process_sqlite_error)?
}
