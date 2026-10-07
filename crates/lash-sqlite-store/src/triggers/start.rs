//! A trigger occurrence's start on SQLite (ADR 0132 §12): the occurrence,
//! every delivery's process and every delivery bound to it, in the one
//! `trigger.start` mailbox transaction.
//!
//! The trigger family and the process registry share the deployment's one
//! database, so the start writes both on the transaction's connection.

use super::*;
use lash_durable::domain::{DomainRefusal, TriggerStart, TriggerStartAnswer};
use lash_durable::{DurableError, StoreFailure, StoreFailureKind};

/// Apply `start` on `tx` at the commit's instant `now`: refuse when its occurrence is already recorded or
/// reclaimed, or when the subscriptions it matches moved since its plan;
/// otherwise record the occurrence, register each delivery's process with its
/// actor ready, and record each delivery bound to it. Answers the processes in
/// delivery order.
pub(crate) fn start_within(
    tx: &rusqlite::Connection,
    start: &TriggerStart,
    now: lash_durable::DurableInstant,
    fleet: lash_core_execution::FleetFormat,
) -> rusqlite::Result<Result<TriggerStartAnswer, DurableError>> {
    let rows =
        match lash_core_execution::facade_support::TriggerStartRows::decode(&start.start_json) {
            Ok(rows) => rows,
            Err(error) => return Ok(Err(store_failure(&error))),
        };
    let now_ms = u64::try_from(now.0).unwrap_or_default();
    match apply(tx, &start.occurrence_id, rows, now_ms, fleet) {
        Ok(answer) => Ok(answer),
        Err(error) => Ok(Err(store_failure(&error))),
    }
}

fn apply(
    tx: &rusqlite::Connection,
    occurrence_id: &str,
    rows: lash_core_execution::facade_support::TriggerStartRows,
    now_ms: u64,
    fleet: lash_core_execution::FleetFormat,
) -> Result<Result<TriggerStartAnswer, DurableError>, lash_core_execution::PluginError> {
    let refused = |refusal: DomainRefusal| Ok(Err(DurableError::Domain(refusal)));
    let sql = trigger_sql();
    let occurrence = &rows.occurrence;
    let held: Option<String> = tx
        .query_row(
            sql.occurrence_sqlite.select_record_by_idempotency_key.sql(),
            params![occurrence.idempotency_key.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(process_sqlite_error)?;
    if held.is_some() {
        return refused(DomainRefusal::TriggerOccurrenceHeld {
            occurrence: occurrence_id.to_owned(),
        });
    }
    if super::sqlite_occurrence_reclaimed(tx, occurrence_id)? {
        return refused(DomainRefusal::TriggerOccurrenceReclaimed {
            occurrence: occurrence_id.to_owned(),
        });
    }
    if !rows.plan_holds(&super::sqlite_matched_subscriptions(tx, occurrence)?) {
        return refused(DomainRefusal::TriggerSubscriptionsMoved {
            occurrence: occurrence_id.to_owned(),
        });
    }
    crate::conn::cached_execute(
        tx,
        sql.occurrence.insert.sql(),
        params![
            occurrence.occurrence_id.as_str(),
            occurrence.idempotency_key.as_str(),
            occurrence.source_type.as_str(),
            occurrence.source_key.as_str(),
            occurrence.occurred_at_ms as i64,
            occurrence.outcome.kind(),
            lash_core_execution::facade_support::encode_trigger_row(occurrence)?,
        ],
    )
    .map_err(process_sqlite_error)?;
    let fired = occurrence.outcome == lash_core_execution::TriggerOccurrenceOutcome::Fired;
    if fired
        && rows.deliveries.iter().all(|delivery| {
            matches!(
                delivery,
                lash_core_execution::facade_support::TriggerDeliveryStartRows::Refused { .. }
            )
        })
    {
        crate::conn::cached_execute(
            tx,
            sql.occurrence.arm_reclaimable.sql(),
            params![
                occurrence.occurrence_id.as_str(),
                occurrence.occurred_at_ms as i64
            ],
        )
        .map_err(process_sqlite_error)?;
    }
    let mut processes = Vec::with_capacity(rows.deliveries.len());
    for delivery in rows.deliveries {
        let refusal_json = match &delivery {
            lash_core_execution::facade_support::TriggerDeliveryStartRows::Refused { .. } => Some(
                lash_core_execution::facade_support::encode_trigger_row(&delivery.outcome())?,
            ),
            _ => None,
        };
        let (subscription, process_id, status) = match delivery {
            lash_core_execution::facade_support::TriggerDeliveryStartRows::Refused {
                subscription,
                ..
            } => (subscription, None, "refused"),
            lash_core_execution::facade_support::TriggerDeliveryStartRows::Started {
                subscription,
                registration,
                observers,
                process_id,
            } => {
                let registered = match SqliteProcessRegistry::apply_registration_conn(
                    tx,
                    *registration,
                    observers,
                    process_id,
                    false,
                    now_ms,
                    fleet,
                ) {
                    Ok(registered) if registered.is_created() => registered.record,
                    Ok(existing) => {
                        return refused(DomainRefusal::TriggerStartRefused {
                            occurrence: occurrence_id.to_owned(),
                            reason: format!(
                                "process `{}` already holds the start key of the delivery to `{}`",
                                existing.record.id, subscription.subscription_id
                            ),
                        });
                    }
                    Err(lash_core_execution::PluginError::StoreUnavailable { fault }) => {
                        return Ok(Err(DurableError::Store(StoreFailure {
                            kind: StoreFailureKind::Unavailable,
                            message: fault.to_string(),
                        })));
                    }
                    Err(error) => {
                        return refused(DomainRefusal::TriggerStartRefused {
                            occurrence: occurrence_id.to_owned(),
                            reason: error.to_string(),
                        });
                    }
                };
                (subscription, Some(registered.id), "started")
            }
        };
        let sql_revision =
            plugin_sql_counter_value("trigger_subscription_revision", subscription.revision)?;
        crate::conn::cached_execute(
            tx,
            sql.delivery.insert.sql(),
            params![
                occurrence.occurrence_id.as_str(),
                subscription.subscription_id.as_str(),
                subscription.incarnation.as_str(),
                sql_revision,
                lash_core_execution::facade_support::encode_trigger_row(&subscription)?,
                occurrence.occurred_at_ms as i64,
                process_id
                    .as_ref()
                    .map(lash_core_execution::ProcessId::as_str),
                status,
                refusal_json,
            ],
        )
        .map_err(process_sqlite_error)?;
        if let Some(process_id) = process_id {
            processes.push(process_id);
        }
    }
    Ok(Ok(TriggerStartAnswer { processes }))
}

/// A start that could not be read or written: its rows did not decode, or
/// a statement failed.
fn store_failure(error: &lash_core_execution::PluginError) -> DurableError {
    let kind = match error {
        lash_core_execution::PluginError::StoredDataCorrupt { .. } => StoreFailureKind::Corrupt,
        _ => StoreFailureKind::Unavailable,
    };
    DurableError::Store(StoreFailure {
        kind,
        message: error.to_string(),
    })
}
