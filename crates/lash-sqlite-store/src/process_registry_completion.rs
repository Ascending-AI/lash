//! Atomic, lease-fenced terminal process completion.

use super::process_registry::{process_lease_expired, tx_outcome};
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
) -> Result<lash_core::ProcessCompletionOutcome, lash_core::PluginError> {
    let process_id = process_id.to_string();
    let now = registry.clock.timestamp_ms();
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome((|| {
                let mut record = SqliteProcessRegistry::load_process_conn(tx, &process_id)?
                    .ok_or_else(|| {
                        lash_core::PluginError::Session(format!("unknown process `{process_id}`"))
                    })?;
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
                let request =
                    lash_core::terminal_append_request(&process_id, &await_output, Some(&authority));
                let replay_lookup = request
                    .replay
                    .as_ref()
                    .map(|replay| {
                        SqliteProcessRegistry::load_event_by_key_conn(
                            tx,
                            &process_id,
                            replay.key.as_str(),
                        )
                    })
                    .transpose()?
                    .flatten();
                let sequence = tx
                    .query_row(
                        "SELECT COALESCE(MAX(sequence), 0) + 1 FROM process_events WHERE process_id = ?1",
                        params![process_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .map_err(process_sqlite_error)? as u64;
                let prepared =
                    prepare_process_event_append(&record, request, sequence, replay_lookup, now)?;
                match prepared {
                    lash_core::ProcessEventAppendPlan::Replay {
                        repair_record,
                        ..
                    } => {
                        if let Some(repaired) = repair_record {
                            record = repaired;
                            SqliteProcessRegistry::save_process_conn(tx, &record)?;
                        }
                        Ok(lash_core::ProcessCompletionOutcome::AlreadyApplied {
                            stored: record,
                        })
                    }
                    lash_core::ProcessEventAppendPlan::Insert {
                        event,
                        payload_hash,
                        projected_record,
                        occurred_at_ms,
                        ..
                    } => {
                        tx.execute(
                            "INSERT INTO process_events (
                                process_id, sequence, event_type, payload_hash, idempotency_key,
                                occurred_at_ms, event_json
                             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                            params![
                                process_id,
                                sequence as i64,
                                event.event_type.as_str(),
                                payload_hash.as_str(),
                                event.invocation.replay_key(),
                                occurred_at_ms as i64,
                                process_encode_json(&event)?,
                            ],
                        )
                        .map_err(process_sqlite_error)?;
                        record = projected_record;
                        SqliteProcessRegistry::save_process_conn(tx, &record)?;
                        Ok(lash_core::ProcessCompletionOutcome::Committed(record))
                    }
                }
            })()))
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn complete_process_with_lease(
    registry: &SqliteProcessRegistry,
    lease: &ProcessLease,
    await_output: ProcessAwaitOutput,
) -> Result<lash_core::ProcessCompletionOutcome, lash_core::PluginError> {
    let lease = lease.clone();
    let now = registry.clock.timestamp_ms();
    registry.conn
        .write_flow(move |tx| {
            Ok(tx_outcome((|| {
                let process_id = lease.process_id.as_str();
                let mut record = SqliteProcessRegistry::load_process_conn(tx, process_id)?.ok_or_else(|| {
                    lash_core::PluginError::Session(format!(
                        "unknown process `{process_id}`"
                    ))
                })?;
                if record.is_terminal() {
                    return Ok(lash_core::ProcessCompletionOutcome::from_stored(
                        record,
                        &await_output,
                    ));
                }
                let request = lash_core::terminal_append_request(process_id, &await_output, None);
                let replay_lookup = request
                    .replay
                    .as_ref()
                    .map(|replay| {
                        SqliteProcessRegistry::load_event_by_key_conn(tx, process_id, replay.key.as_str())
                    })
                    .transpose()?
                    .flatten();
                let sequence = tx
                    .query_row(
                        "SELECT COALESCE(MAX(sequence), 0) + 1 FROM process_events WHERE process_id = ?1",
                        params![process_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .map_err(process_sqlite_error)? as u64;
                // A successful prior terminal append is replay-idempotent even
                // though that transaction already cleared the lease.
                let prepared = prepare_process_event_append(
                    &record,
                    request,
                    sequence,
                    replay_lookup,
                    now,
                )?;
                if matches!(prepared, lash_core::ProcessEventAppendPlan::Replay { .. }) {
                    return Ok(lash_core::ProcessCompletionOutcome::AlreadyApplied {
                        stored: record,
                    });
                }

                let current = SqliteProcessRegistry::load_process_lease_conn(tx, process_id)?;
                if !guard_lease(current.as_ref(), &lease.lease_token, now)
                    || !current.as_ref().is_some_and(|current| {
                        current.owner.same_incarnation(&lease.owner)
                            && current.fencing_token == lease.fencing_token
                    })
                {
                    return Err(process_lease_expired(process_id));
                }

                let lash_core::ProcessEventAppendPlan::Insert {
                    event,
                    payload_hash,
                    projected_record,
                    occurred_at_ms,
                    ..
                } = prepared
                else {
                    unreachable!("replay returned above")
                };
                tx.execute(
                    "INSERT INTO process_events (
                        process_id, sequence, event_type, payload_hash, idempotency_key,
                        occurred_at_ms, event_json
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        process_id,
                        sequence as i64,
                        event.event_type.as_str(),
                        payload_hash.as_str(),
                        event.invocation.replay_key(),
                        occurred_at_ms as i64,
                        process_encode_json(&event)?,
                    ],
                )
                .map_err(process_sqlite_error)?;
                record = projected_record;
                SqliteProcessRegistry::save_process_conn(tx, &record)?;
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
