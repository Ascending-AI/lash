use crate::*;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;

use crate::process_sql::process_sql;

pub(crate) fn process_status_label(record: &ProcessRecord) -> &'static str {
    record.status.label()
}

/// The `cancel_requested_at_ms` column: the first accepted cancellation's
/// timestamp, or `NULL` when no cancel has been requested. One column carries
/// the fact and its age, so "a cancel is pending" and "it was requested at T"
/// cannot disagree.
pub(crate) fn cancel_requested_at_ms(record: &ProcessRecord) -> Option<i64> {
    record
        .cancel_request
        .as_ref()
        .map(|request| request.requested_at_ms as i64)
}

pub(crate) async fn process_change_horizon_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<u64, PluginError> {
    let horizon: i64 = sqlx::query_scalar(
        process_sql()
            .clock_postgres
            .select_compaction_horizon_for_share
            .sql(),
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(plugin_sqlx_error)?;
    plugin_u64_from_sql(
        "ProcessChangeClock",
        "tombstone_compaction_horizon",
        horizon,
    )
}

pub(crate) async fn load_process_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    process_id: &ProcessId,
) -> Result<Option<ProcessRecord>, PluginError> {
    let json: Option<String> = sqlx::query_scalar(
        process_sql()
            .process_postgres
            .select_record_json_for_update
            .sql(),
    )
    .bind(process_id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(plugin_sqlx_error)?;
    json.map(|json| serde_json::from_str(&json).map_err(process_decode_error))
        .transpose()
}

/// The retained process registered under `start_key`, if any, locked for the
/// registration transaction that read it.
pub(crate) async fn load_process_by_start_key_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    start_key: &lash_core_execution::StartKey,
) -> Result<Option<ProcessRecord>, PluginError> {
    let json: Option<String> =
        sqlx::query_scalar(process_sql().process.select_record_json_by_start_key.sql())
            .bind(start_key.as_str())
            .fetch_optional(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?;
    json.map(|json| serde_json::from_str(&json).map_err(process_decode_error))
        .transpose()
}

pub(crate) async fn load_process(
    pool: &PgPool,
    process_id: &ProcessId,
) -> Result<Option<ProcessRecord>, PluginError> {
    let json: Option<String> =
        sqlx::query_scalar(process_sql().process.select_record_json_by_id.sql())
            .bind(process_id.as_str())
            .fetch_optional(pool)
            .await
            .map_err(plugin_sqlx_error)?;
    json.map(|json| serde_json::from_str(&json).map_err(process_decode_error))
        .transpose()
}

pub(crate) async fn require_process_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    process_id: &ProcessId,
) -> Result<ProcessRecord, PluginError> {
    if let Some(record) = load_process_tx(tx, process_id).await? {
        return Ok(record);
    }
    let row = sqlx::query(process_sql().tombstone.select_terminal.sql())
        .bind(process_id.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(plugin_sqlx_error)?;
    let tombstone = row
        .map(|row| {
            Ok::<_, PluginError>(registry_transitions::ProcessTombstoneStamp {
                terminal_label: row.get(0),
                pruned_at_ms: plugin_u64_from_sql("ProcessTombstone", "pruned_at_ms", row.get(1))?,
            })
        })
        .transpose()?;
    Err(registry_transitions::absent_process_error(
        process_id, tombstone,
    ))
}

pub(crate) fn decode_matching_process(
    row: sqlx::postgres::PgRow,
    filter: &lash_core_execution::ProcessListFilter,
) -> Result<Option<ProcessRecord>, PluginError> {
    let json: String = row.get(0);
    let record = serde_json::from_str(&json).map_err(process_decode_error)?;
    // JSONB normalizes numeric representations (`1` equals `1.0`).
    // SQL is the coarse pushdown; this is the exact public Value contract.
    Ok(filter.matches_record(&record).then_some(record))
}

pub(crate) async fn wake_session_id_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    process_id: &ProcessId,
) -> Result<Option<SessionId>, PluginError> {
    sqlx::query_scalar::<_, Option<String>>(process_sql().process.select_wake_session_id.sql())
        .bind(process_id.as_str())
        .fetch_one(&mut **tx)
        .await
        .map(|session_id| session_id.map(SessionId::from))
        .map_err(plugin_sqlx_error)
}

pub(crate) async fn save_process_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    record: &ProcessRecord,
) -> Result<(), PluginError> {
    let change_seq = next_process_change_seq_tx(tx).await?;
    sqlx::query(process_sql().process.update_mutable_columns.sql())
        .bind(record.id.as_str())
        .bind(record.updated_at_ms as i64)
        .bind(change_seq as i64)
        .bind(process_status_label(record))
        .bind(record.last_event_sequence as i64)
        .bind(cancel_requested_at_ms(record))
        .bind(serde_json::to_string(record).map_err(process_decode_error)?)
        .bind(
            record
                .park
                .as_deref()
                .map(|park| clamp_epoch_ms(park.since_ms)),
        )
        .bind(
            record
                .park
                .as_deref()
                .map(|park| park.reason.code().as_str()),
        )
        .bind(
            record
                .park
                .as_deref()
                .and_then(|park| park.reason.retired_executable_generation_key()),
        )
        .bind(
            record
                .park
                .as_deref()
                .and_then(|park| park.build_generation.as_ref().map(|g| g.as_str())),
        )
        .execute(&mut **tx)
        .await
        .map_err(plugin_sqlx_error)?;
    Ok(())
}

pub(crate) async fn next_process_change_seq_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<u64, PluginError> {
    let seq: i64 = sqlx::query_scalar(process_sql().clock_postgres.bump_returning.sql())
        .fetch_one(&mut **tx)
        .await
        .map_err(plugin_sqlx_error)?;
    plugin_u64_from_sql("ProcessChangeClock", "current_seq", seq)
}

pub(crate) async fn load_event_by_key_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    process_id: &ProcessId,
    replay_key: &str,
) -> Result<Option<ProcessEvent>, PluginError> {
    let row = sqlx::query(process_sql().event.select_by_replay_key.sql())
        .bind(process_id.as_str())
        .bind(replay_key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(plugin_sqlx_error)?;
    row.map(|row| {
        let json: String = row.get(0);
        serde_json::from_str(&json).map_err(process_decode_error)
    })
    .transpose()
}

pub(crate) async fn next_process_event_sequence_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    process_id: &ProcessId,
    target_session_id: Option<&SessionId>,
) -> Result<(Option<u64>, u64), PluginError> {
    let last_sequence: Option<i64> =
        sqlx::query_scalar(process_sql().event.select_max_sequence.sql())
            .bind(process_id.as_str())
            .fetch_one(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?;
    let last_sequence = last_sequence
        .map(|sequence| plugin_u64_from_sql("ProcessEvent", "sequence", sequence))
        .transpose()?;
    let sender_floor = if let Some(target_session_id) = target_session_id {
        sqlx::query_scalar::<_, i64>(process_sql().floor.select_floor.sql())
            .bind(target_session_id.as_str())
            .bind(process_id.as_str())
            .fetch_optional(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?
            .map(|floor| plugin_u64_from_sql("WakeAllocationFloor", "allocation_floor", floor))
            .transpose()?
    } else {
        None
    };
    let sequence =
        lash_core_execution::runtime::allocate_process_event_sequence(last_sequence, sender_floor)?;
    Ok((last_sequence, sequence))
}

/// Which arm of the prepared append plan the store actually applied.
///
/// Entry points map this onto their own outcome type; the shared append
/// sequence never decides what a caller returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessEventAppendArm {
    /// The replay arm. No event row was written, so the wake allocation floor
    /// stays where the original insert left it.
    Replayed,
    /// The insert arm. Exactly one event row was written and the wake
    /// allocation floor advanced to its sequence.
    Inserted,
}

/// A batch of process-event appends staged against one in-memory projection
/// inside one transaction (FIG-3571), saved once by [`Self::commit`].
pub(crate) struct ProcessEventBatch {
    fleet_format: lash_core_execution::FleetFormat,
    record_changed: bool,
}

impl ProcessEventBatch {
    /// Start an empty batch whose appends stamp `fleet_format`'s versions.
    pub(crate) fn for_fleet(fleet_format: lash_core_execution::FleetFormat) -> Self {
        Self {
            fleet_format,
            record_changed: false,
        }
    }

    /// Stage one preauthorized append of the batch.
    pub(crate) async fn stage(
        &mut self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        record: &mut ProcessRecord,
        request: ProcessEventAppendRequest,
        occurred_at_ms: u64,
        wake_delivery_config: lash_core_execution::WakeDeliveryConfig,
    ) -> Result<ProcessEventAppendReceipt, PluginError> {
        self.stage_arm(
            tx,
            record,
            request,
            occurred_at_ms,
            wake_delivery_config,
            ProcessEventWriteAuthorization::Preauthorized,
        )
        .await
        .map(|(receipt, _)| receipt)
    }

    /// Stage one append of the batch under `authorization`, answering its
    /// arm.
    pub(crate) async fn stage_arm(
        &mut self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        record: &mut ProcessRecord,
        request: ProcessEventAppendRequest,
        occurred_at_ms: u64,
        wake_delivery_config: lash_core_execution::WakeDeliveryConfig,
        authorization: ProcessEventWriteAuthorization<'_>,
    ) -> Result<(ProcessEventAppendReceipt, ProcessEventAppendArm), PluginError> {
        let (receipt, arm, record_changed) = stage_process_event_append_tx(
            tx,
            record,
            request,
            occurred_at_ms,
            wake_delivery_config,
            authorization,
            self.fleet_format,
        )
        .await?;
        self.record_changed |= record_changed;
        Ok((receipt, arm))
    }

    /// Save the process once if any staged append moved its projection.
    pub(crate) async fn commit(
        self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        record: &ProcessRecord,
    ) -> Result<(), PluginError> {
        if self.record_changed {
            save_process_tx(tx, record).await?;
        }
        Ok(())
    }
}

/// Stage `requests` in order as one batch (FIG-3571): each goes through the
/// append sequence against the in-memory projection, and the process is saved
/// once, advancing the change clock once, when any of them moved it. The
/// caller owns the transaction, so a refusal of any request commits none.
pub(crate) async fn append_process_event_batch_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    record: &mut ProcessRecord,
    requests: Vec<ProcessEventAppendRequest>,
    occurred_at_ms: u64,
    wake_delivery_config: lash_core_execution::WakeDeliveryConfig,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<Vec<ProcessEventAppendReceipt>, PluginError> {
    let mut batch = ProcessEventBatch::for_fleet(fleet_format);
    let mut receipts = Vec::with_capacity(requests.len());
    for request in requests {
        receipts.push(
            batch
                .stage(tx, record, request, occurred_at_ms, wake_delivery_config)
                .await?,
        );
    }
    batch.commit(tx, record).await?;
    Ok(receipts)
}

/// Where the write authority for one process-event append is settled.
pub(crate) enum ProcessEventWriteAuthorization<'a> {
    /// The entry point authorized the write before the append sequence began.
    Preauthorized,
    /// Re-read the persisted lease and authorize against it after the
    /// replay-or-insert decision and before the first row is written.
    Lease(&'a ProcessLease),
}

/// One process-event append for the PostgreSQL store: the append sequence
/// ([`stage_process_event_append_tx`]) followed by the process save when the
/// append moved the projection.
pub(crate) async fn apply_process_event_append_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    record: &mut ProcessRecord,
    request: ProcessEventAppendRequest,
    occurred_at_ms: u64,
    wake_delivery_config: lash_core_execution::WakeDeliveryConfig,
    authorization: ProcessEventWriteAuthorization<'_>,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<(ProcessEventAppendReceipt, ProcessEventAppendArm), PluginError> {
    let (receipt, arm, record_changed) = stage_process_event_append_tx(
        tx,
        record,
        request,
        occurred_at_ms,
        wake_delivery_config,
        authorization,
        fleet_format,
    )
    .await?;
    if record_changed {
        save_process_tx(tx, record).await?;
    }
    Ok((receipt, arm))
}

/// The one process-event append sequence for the PostgreSQL store, short of
/// the process save.
///
/// Every entry point runs these steps, in this order: replay-key lookup, wake
/// session id, next sequence number, prepare, the replay-or-insert decision,
/// the five-bind event insert, the projection update, the parent-end
/// retention, the wake-delivery insert, and the wake allocation floor. The
/// third value says whether the projection moved; the caller saves the
/// process once it has: after this one append, or after the batch it belongs
/// to. Entry points keep their own prologue, transaction lifetime and outcome
/// mapping.
///
/// `occurred_at_ms` is the caller's clock and the only clock this function
/// sees: each entry point keeps its own source (the injected store clock, or
/// the sanctioned PostgreSQL lease clock), and this function never reads one.
/// The `Lease` authorization compares that same value against the stored lease,
/// exactly as the leased entry point did inline.
async fn stage_process_event_append_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    record: &mut ProcessRecord,
    request: ProcessEventAppendRequest,
    occurred_at_ms: u64,
    wake_delivery_config: lash_core_execution::WakeDeliveryConfig,
    authorization: ProcessEventWriteAuthorization<'_>,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<(ProcessEventAppendReceipt, ProcessEventAppendArm, bool), PluginError> {
    let process_id = record.id.clone();
    let replay_lookup =
        if let Some(replay_key) = request.replay.as_ref().map(|replay| replay.key.as_str()) {
            load_event_by_key_tx(tx, &process_id, replay_key).await?
        } else {
            None
        };
    let wake_session_id = wake_session_id_tx(tx, &process_id).await?;
    let (last_sequence, sequence) =
        next_process_event_sequence_tx(tx, &process_id, wake_session_id.as_ref()).await?;
    let prepared = lash_core_execution::runtime::prepare_process_event_append(
        record,
        request,
        sequence,
        last_sequence,
        replay_lookup,
        occurred_at_ms,
        wake_session_id.as_ref(),
        fleet_format,
    )?;
    match prepared {
        lash_core_execution::facade_support::ProcessEventAppendPlan::Replay {
            event,
            repair_record,
            wake_delivery,
            ..
        } => {
            insert_wake_delivery_tx(tx, wake_delivery.as_ref(), wake_delivery_config).await?;
            let repaired = repair_record.is_some();
            if let Some(repaired) = repair_record {
                *record = repaired;
            }
            Ok((
                ProcessEventAppendReceipt {
                    last_event_sequence: record.last_event_sequence,
                    realization: lash_core_execution::StoreRealization::Coalesced,
                    event,
                    wake_delivery,
                },
                ProcessEventAppendArm::Replayed,
                repaired,
            ))
        }
        lash_core_execution::facade_support::ProcessEventAppendPlan::Insert {
            event,
            projected_record,
            wake_delivery,
        } => {
            match authorization {
                ProcessEventWriteAuthorization::Preauthorized => {}
                ProcessEventWriteAuthorization::Lease(lease) => {
                    // The shared process-lease verdict is the decision here
                    // (FIG-3388): the row is locked by `load_process_lease_row_tx`
                    // and the release write's predicate backstops this call.
                    let current = load_process_lease_row_tx(tx, &process_id).await?;
                    let verdict = lash_core_execution::store_backend_support::process_lease_verdict(
                        current
                            .as_ref()
                            .map(registry_transitions::ProcessLeaseRow::facts),
                        lash_core_execution::store_backend_support::ProcessLeaseAuthority {
                            lease_token: &lease.lease_token,
                            fencing_token: lease.fencing_token,
                        },
                        occurred_at_ms,
                    );
                    if !verdict.is_current() {
                        return Err(PluginError::ProcessLeaseSuperseded { process_id });
                    }
                }
            }
            sqlx::query(process_sql().event.insert.sql())
                .bind(process_id.as_str())
                .bind(sequence as i64)
                .bind(event.event_type.as_str())
                .bind(event.invocation.replay_key())
                .bind(serde_json::to_string(&event).map_err(process_decode_error)?)
                .execute(&mut **tx)
                .await
                .map_err(plugin_sqlx_error)?;
            let park_transitions = lash_core_execution::runtime::process_park_transitions(
                record.park.as_deref(),
                &projected_record,
            );
            *record = projected_record;
            // The park feed rides the event's own transaction (FIG-3659
            // NOW-B): a park that opened or closed here is durable in the feed
            // exactly when the fact that moved it is.
            crate::process_registry::park_feed::log_process_park_transitions_tx(
                tx,
                &record.park_key(),
                &park_transitions,
                occurred_at_ms,
                record
                    .park
                    .as_deref()
                    .and_then(|park| park.build_generation.as_ref().map(|g| g.as_str())),
            )
            .await?;
            // A process that just reached a terminal status is an ended parent
            // scope: its ledger row rides the same transaction as the terminal
            // append, so no child can be stranded by a crash between the two.
            if record.is_terminal() {
                crate::process_registry::parent_end::record_tx(
                    tx,
                    &lash_core_execution::ScopeId::process(process_id.clone()),
                    occurred_at_ms,
                    fleet_format,
                )
                .await?;
            }
            insert_wake_delivery_tx(tx, wake_delivery.as_ref(), wake_delivery_config).await?;
            advance_wake_allocation_floor_tx(tx, wake_session_id.as_ref(), &process_id, sequence)
                .await?;
            Ok((
                ProcessEventAppendReceipt {
                    last_event_sequence: event.sequence,
                    realization: lash_core_execution::StoreRealization::Realized,
                    event,
                    wake_delivery,
                },
                ProcessEventAppendArm::Inserted,
                true,
            ))
        }
    }
}

pub(crate) async fn append_process_event_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    record: &mut ProcessRecord,
    request: ProcessEventAppendRequest,
    occurred_at_ms: u64,
    wake_delivery_config: lash_core_execution::WakeDeliveryConfig,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<ProcessEventAppendReceipt, PluginError> {
    apply_process_event_append_tx(
        tx,
        record,
        request,
        occurred_at_ms,
        wake_delivery_config,
        ProcessEventWriteAuthorization::Preauthorized,
        fleet_format,
    )
    .await
    .map(|(receipt, _)| receipt)
}

pub(crate) async fn advance_wake_allocation_floor_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    target_session_id: Option<&SessionId>,
    process_id: &ProcessId,
    sequence: u64,
) -> Result<(), PluginError> {
    let Some(target_session_id) = target_session_id else {
        return Ok(());
    };
    sqlx::query(process_sql().floor_postgres.upsert_max.sql())
        .bind(target_session_id.as_str())
        .bind(process_id.as_str())
        .bind(sequence as i64)
        .execute(&mut **tx)
        .await
        .map_err(plugin_sqlx_error)?;
    Ok(())
}

pub(crate) async fn insert_wake_delivery_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    wake: Option<&lash_core_execution::ProcessWakeDelivery>,
    config: lash_core_execution::WakeDeliveryConfig,
) -> Result<(), PluginError> {
    let Some(wake) = wake else {
        return Ok(());
    };
    let delivery = lash_core_execution::WakeDelivery::pending(wake.clone(), config)?;
    sqlx::query(process_sql().wake_postgres.insert_pending.sql())
        .bind(&delivery.delivery_id)
        .bind(delivery.wake.process_id.as_str())
        .bind(delivery.wake.target_session_id.as_str())
        .bind(delivery.wake.sequence as i64)
        .bind(delivery.next_attempt_at_ms as i64)
        .bind(delivery.expires_at_ms as i64)
        .bind(serde_json::to_string(&delivery.wake).map_err(process_decode_error)?)
        .execute(&mut **tx)
        .await
        .map_err(plugin_sqlx_error)?;
    Ok(())
}

/// The lease row under the `FOR UPDATE` lock, unprojected: the release
/// verdict needs the raw holder columns to tell a released row (`Released`)
/// from an absent one (`Absent`) and a held row from its successor.
pub(crate) async fn load_process_lease_row_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    process_id: &ProcessId,
) -> Result<Option<registry_transitions::ProcessLeaseRow>, PluginError> {
    let row = sqlx::query(
        process_sql()
            .lease_postgres
            .select_by_process_for_update
            .sql(),
    )
    .bind(process_id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(plugin_sqlx_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(registry_transitions::ProcessLeaseRow {
        owner_id: row.get(0),
        incarnation_id: row.get(5),
        lease_token: row.get(1),
        fencing_token: row.get(2),
        claimed_at_ms: row.get(3),
        expires_at_ms: row.get(4),
    }))
}

pub(crate) async fn load_process_lease_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    process_id: &ProcessId,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<Option<ProcessLease>, PluginError> {
    let Some(row) = load_process_lease_row_tx(tx, process_id).await? else {
        return Ok(None);
    };
    Ok(row.project(process_id, fleet_format))
}

/// Insert-or-replace the persisted lease row for `process_id` with a fresh
/// lease owned by `owner` at `fencing_token`.
pub(crate) async fn acquire_process_lease_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    process_id: &ProcessId,
    owner: &LeaseOwnerIdentity,
    fencing_token: u64,
    now: u64,
    lease_ttl_ms: u64,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<ProcessLease, PluginError> {
    let lease = registry_transitions::acquired_process_lease(
        process_id,
        owner,
        fencing_token,
        now,
        lease_ttl_ms,
        fleet_format,
    );
    let sql_fencing_token = plugin_sql_monotonic_counter_value(
        "process_lease_fencing_token",
        fencing_token.saturating_sub(1),
        lease.fencing_token,
    )?;
    sqlx::query(process_sql().lease_postgres.upsert_acquired.sql())
        .bind(lease.process_id.as_str())
        .bind(&lease.owner.owner_id)
        .bind(&lease.owner.incarnation_id)
        .bind(&lease.lease_token)
        .bind(sql_fencing_token)
        .bind(lease.claimed_at_epoch_ms as i64)
        .bind(lease.expires_at_epoch_ms as i64)
        .execute(&mut **tx)
        .await
        .map_err(plugin_sqlx_error)?;
    Ok(lease)
}

pub(crate) async fn retained_process_lease_fencing_token(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    process_id: &ProcessId,
) -> Result<u64, PluginError> {
    let existing_fence: Option<i64> = sqlx::query_scalar(
        process_sql()
            .lease_postgres
            .select_fencing_token_for_update
            .sql(),
    )
    .bind(process_id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(plugin_sqlx_error)?;
    existing_fence
        .map(|value| plugin_u64_from_sql("ProcessLease", "lease_fencing_token", value))
        .transpose()
        .map(|value| value.unwrap_or(0))
}

pub(crate) async fn validate_process_execution_authority_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    process_id: &ProcessId,
    record: &ProcessRecord,
    authority: &ProcessExecutionWriteAuthority,
    start: Option<&ProcessStarted>,
    now: u64,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<(), PluginError> {
    match authority {
        ProcessExecutionWriteAuthority::Invocation { .. } => {
            if let Some(started) = start {
                authority.validate_invocation_for_start(
                    process_id,
                    started,
                    record.first_started.as_deref(),
                )
            } else {
                authority.validate_invocation_for_write(process_id, record)
            }
        }
        ProcessExecutionWriteAuthority::Lease { lease, .. } => {
            // The process-id half of the fence is checked first so a lease for
            // another process is refused without reading this process's row.
            if lease.process_id != process_id {
                return Err(PluginError::ProcessLeaseSuperseded {
                    process_id: process_id.clone(),
                });
            }
            let current = load_process_lease_tx(tx, process_id, fleet_format).await?;
            registry_transitions::authorize_process_lease_write(
                process_id,
                lease,
                current.as_ref(),
                now,
            )
        }
    }
}

/// One authoritative wall-clock sample for every process-lease transaction.
/// Using the database clock prevents worker clock skew from stealing or
/// spuriously expiring a lease in multi-host Postgres deployments.
///
/// Deliberately the last item in this file: every lease atom above it is inside
/// `postgres_clock_contract`'s lexical fence, and this is the one function
/// allowed to read a clock at all. It is fenced too — a dedicated end-of-file
/// region bans client clock reads from its body and from anything appended
/// after it, and pins that its query samples `clock_timestamp()`.
pub(crate) async fn process_lease_now_epoch_ms_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<u64, PluginError> {
    let now: i64 = sqlx::query_scalar(
        crate::connection_sql::connection_sql()
            .select_statement_epoch_ms
            .sql(),
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(plugin_sqlx_error)?;
    u64::try_from(now).map_err(|_| PluginError::ClockBeforeUnixEpoch {
        clock: "Postgres database clock".to_string(),
        epoch_ms: now,
    })
}
