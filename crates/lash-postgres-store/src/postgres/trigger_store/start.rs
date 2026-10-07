//! A trigger occurrence's start on PostgreSQL (ADR 0132 §12): the
//! occurrence, every delivery's process and every delivery bound to it, in
//! the one `trigger.start` mailbox transaction.

use super::*;
use crate::guarded_tx::GuardedTx;
use lash_durable::domain::{DomainRefusal, TriggerStart, TriggerStartAnswer};
use lash_durable::{DurableError, StoreFailure, StoreFailureKind};

/// Apply `start` on `tx` at the commit's instant `now`: refuse when its occurrence is already recorded or
/// reclaimed, or when the subscriptions it matches moved since its plan;
/// otherwise record the occurrence, register each delivery's process with its
/// actor ready, and record each delivery bound to it. Answers the processes in
/// delivery order.
pub(crate) async fn start_within(
    tx: &mut GuardedTx<'_>,
    start: &TriggerStart,
    now: lash_durable::DurableInstant,
    fleet: lash_core_execution::FleetFormat,
) -> Result<TriggerStartAnswer, DurableError> {
    let rows = lash_core_execution::facade_support::TriggerStartRows::decode(&start.start_json)
        .map_err(|error| store_failure(&error))?;
    // Boxed: a registration's apply is a large future.
    Box::pin(apply(
        tx,
        &start.occurrence_id,
        rows,
        u64::try_from(now.0).unwrap_or_default(),
        fleet,
    ))
    .await
    .map_err(|error| match error {
        Applied::Refused(refusal) => DurableError::Domain(refusal),
        Applied::Failed(error) => store_failure(&error),
    })
}

/// Why a start wrote nothing.
enum Applied {
    Refused(DomainRefusal),
    Failed(PluginError),
}

impl From<PluginError> for Applied {
    fn from(error: PluginError) -> Self {
        Self::Failed(error)
    }
}

async fn apply(
    tx: &mut GuardedTx<'_>,
    occurrence_id: &str,
    rows: lash_core_execution::facade_support::TriggerStartRows,
    now_ms: u64,
    fleet: lash_core_execution::FleetFormat,
) -> Result<TriggerStartAnswer, Applied> {
    let sql = trigger_sql();
    let occurrence = &rows.occurrence;
    // Two starts of one occurrence serialize on its idempotency key; a
    // reclaim deletes and tombstones in one statement, so a row this
    // transaction finds absent shows its tombstone here.
    sqlx::query(
        crate::connection_sql::connection_sql()
            .lock_xact_by_text
            .sql(),
    )
    .bind(&occurrence.idempotency_key)
    .execute(crate::observed_sql::executor(&mut ***tx))
    .await
    .map_err(plugin_sqlx_error)?;
    let held: Option<String> = sqlx::query_scalar(
        sql.occurrence_postgres
            .select_record_by_idempotency_key
            .sql(),
    )
    .bind(&occurrence.idempotency_key)
    .fetch_optional(crate::observed_sql::executor(&mut ***tx))
    .await
    .map_err(plugin_sqlx_error)?;
    if held.is_some() {
        return Err(Applied::Refused(DomainRefusal::TriggerOccurrenceHeld {
            occurrence: occurrence_id.to_owned(),
        }));
    }
    if postgres_occurrence_reclaimed(tx, occurrence_id).await? {
        return Err(Applied::Refused(
            DomainRefusal::TriggerOccurrenceReclaimed {
                occurrence: occurrence_id.to_owned(),
            },
        ));
    }
    if !rows.plan_holds(&postgres_matched_subscriptions(tx, occurrence).await?) {
        return Err(Applied::Refused(DomainRefusal::TriggerSubscriptionsMoved {
            occurrence: occurrence_id.to_owned(),
        }));
    }
    sqlx::query(sql.occurrence.insert.sql())
        .bind(&occurrence.occurrence_id)
        .bind(&occurrence.idempotency_key)
        .bind(&occurrence.source_type)
        .bind(&occurrence.source_key)
        .bind(occurrence.occurred_at_ms as i64)
        .bind(occurrence.outcome.kind())
        .bind(lash_core_execution::facade_support::encode_trigger_row(
            occurrence,
        )?)
        .execute(crate::observed_sql::executor(&mut ***tx))
        .await
        .map_err(plugin_sqlx_error)?;
    let fired = occurrence.outcome == lash_core_execution::TriggerOccurrenceOutcome::Fired;
    if fired
        && rows.deliveries.iter().all(|delivery| {
            matches!(
                delivery,
                lash_core_execution::facade_support::TriggerDeliveryStartRows::Refused { .. }
            )
        })
    {
        sqlx::query(sql.occurrence.arm_reclaimable.sql())
            .bind(&occurrence.occurrence_id)
            .bind(i64::try_from(occurrence.occurred_at_ms).unwrap_or(i64::MAX))
            .execute(crate::observed_sql::executor(&mut ***tx))
            .await
            .map_err(plugin_sqlx_error)?;
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
                let refused = |reason: String| {
                    Applied::Refused(DomainRefusal::TriggerStartRefused {
                        occurrence: occurrence_id.to_owned(),
                        reason,
                    })
                };
                let record = match crate::process_registry::registration::apply_registration_tx(
                    tx,
                    *registration,
                    observers,
                    process_id,
                    false,
                    now_ms,
                    fleet,
                )
                .await
                {
                    Ok(crate::process_registry::registration::AppliedRegistration::Created(
                        record,
                    )) => record,
                    Ok(
                        crate::process_registry::registration::AppliedRegistration::Retained {
                            record,
                            ..
                        }
                        | crate::process_registry::registration::AppliedRegistration::LostRace {
                            winner: record,
                            ..
                        },
                    ) => {
                        return Err(refused(format!(
                            "process `{}` already holds the start key of the delivery to `{}`",
                            record.id, subscription.subscription_id
                        )));
                    }
                    Err(PluginError::StoreUnavailable { fault }) => {
                        return Err(Applied::Failed(PluginError::StoreUnavailable { fault }));
                    }
                    Err(error) => return Err(refused(error.to_string())),
                };
                (subscription, Some(record.id), "started")
            }
        };
        let sql_revision =
            plugin_sql_counter_value("trigger_subscription_revision", subscription.revision)?;
        sqlx::query(sql.delivery.insert.sql())
            .bind(&occurrence.occurrence_id)
            .bind(&subscription.subscription_id)
            .bind(&subscription.incarnation)
            .bind(sql_revision)
            .bind(lash_core_execution::facade_support::encode_trigger_row(
                &subscription,
            )?)
            .bind(occurrence.occurred_at_ms as i64)
            .bind(
                process_id
                    .as_ref()
                    .map(lash_core_execution::ProcessId::as_str),
            )
            .bind(status)
            .bind(refusal_json)
            .execute(crate::observed_sql::executor(&mut ***tx))
            .await
            .map_err(plugin_sqlx_error)?;
        if let Some(process_id) = process_id {
            processes.push(process_id);
        }
    }
    Ok(TriggerStartAnswer { processes })
}

/// A start that could not be read or written: its rows did not decode, or
/// a statement failed.
fn store_failure(error: &PluginError) -> DurableError {
    let kind = match error {
        PluginError::StoredDataCorrupt { .. } => StoreFailureKind::Corrupt,
        _ => StoreFailureKind::Unavailable,
    };
    DurableError::Store(StoreFailure {
        kind,
        message: error.to_string(),
    })
}
