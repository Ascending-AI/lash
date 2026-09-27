//! Atomic terminal process completion.

use super::process_registry::{ProcessEventAppendArm, ProcessEventBatch, tx_outcome};
use super::*;
use lash_sansio::ProcessId;

/// Authority-bound terminal completion, validated and appended as one atomic unit,
/// with the run's terminal batch (`prelude`) ahead of the terminal event and
/// one process save (FIG-3571).
///
/// The load, the authority-vs-input-class validation, and the terminal append
/// all run inside a single `write_flow` transaction. Splitting validation
/// (reading the row's ownership class) from the append leaves a window in which a
/// paused caller could re-validate against one input class, then append after
/// the row was completed, pruned, and re-registered with a *different*
/// input class. Holding one transaction across load→validate→append closes that
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
    let fleet_format = registry.fleet_format;
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
                // Validate the authority against the row's input class
                // *inside* the transaction that appends, so a concurrent
                // complete→prune→re-register with a different input class cannot
                // slip between the check and the append.
                authority.validate(&record)?;
                let mut batch = ProcessEventBatch::for_fleet(fleet_format);
                for request in prelude {
                    batch.stage(tx, &mut record, request, now, wake_delivery_config)?;
                }
                let request = lash_core_execution::facade_support::terminal_append_request(
                    &process_id,
                    &await_output,
                    Some(&authority),
                );
                let (_, arm) =
                    batch.stage_arm(tx, &mut record, request, now, wake_delivery_config)?;
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
