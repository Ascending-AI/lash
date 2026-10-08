use crate::guarded_tx::GuardedTx;
use crate::*;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;

use crate::process_sql::process_sql;

fn decode_process_record(json: &str) -> Result<ProcessRecord, PluginError> {
    serde_json::from_str(json).map_err(|error| PluginError::StoredDataCorrupt {
        record_kind: "process_registry".to_string(),
        message: error.to_string(),
    })
}

pub(crate) fn process_status_label(record: &ProcessRecord) -> &'static str {
    record.status().label()
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
    tx: &mut sqlx::PgConnection,
) -> Result<u64, PluginError> {
    let horizon: i64 = sqlx::query_scalar(
        process_sql()
            .clock_postgres
            .select_compaction_horizon_for_share
            .sql(),
    )
    .fetch_one(crate::observed_sql::executor(&mut *tx))
    .await
    .map_err(plugin_sqlx_error)?;
    plugin_u64_from_sql(
        "ProcessChangeClock",
        "tombstone_compaction_horizon",
        horizon,
    )
}

pub(crate) async fn load_process_tx(
    tx: &mut sqlx::PgConnection,
    process_id: &ProcessId,
) -> Result<Option<ProcessRecord>, PluginError> {
    let json: Option<String> = sqlx::query_scalar(
        process_sql()
            .process_postgres
            .select_record_json_for_update
            .sql(),
    )
    .bind(process_id.as_str())
    .fetch_optional(crate::observed_sql::executor(&mut *tx))
    .await
    .map_err(plugin_sqlx_error)?;
    json.map(|json| decode_process_record(&json)).transpose()
}

/// The retained process registered under `start_key`, if any, locked for the
/// registration transaction that read it.
pub(crate) async fn load_process_by_start_key_tx(
    tx: &mut sqlx::PgConnection,
    start_key: &lash_core_execution::StartKey,
) -> Result<Option<ProcessRecord>, PluginError> {
    let json: Option<String> =
        sqlx::query_scalar(process_sql().process.select_record_json_by_start_key.sql())
            .bind(start_key.as_str())
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(plugin_sqlx_error)?;
    json.map(|json| decode_process_record(&json)).transpose()
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
    json.map(|json| decode_process_record(&json)).transpose()
}

pub(crate) async fn require_process_tx(
    tx: &mut sqlx::PgConnection,
    process_id: &ProcessId,
) -> Result<ProcessRecord, PluginError> {
    if let Some(record) = load_process_tx(tx, process_id).await? {
        return Ok(record);
    }
    let row = sqlx::query(process_sql().tombstone.select_terminal.sql())
        .bind(process_id.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(plugin_sqlx_error)?;
    let tombstone = row
        .map(|row| {
            registry_transitions::ProcessTombstoneStamp::from_row(
                process_id,
                row.get(0),
                plugin_u64_from_sql("ProcessTombstone", "pruned_at_ms", row.get(1))?,
            )
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
    tx: &mut sqlx::PgConnection,
    process_id: &ProcessId,
) -> Result<Option<SessionId>, PluginError> {
    sqlx::query_scalar::<_, Option<String>>(process_sql().process.select_wake_session_id.sql())
        .bind(process_id.as_str())
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(plugin_sqlx_error)?
        .map(SessionId::parse)
        .transpose()
        .map_err(PluginError::from)
}

/// Save `record`'s mutable columns, staged on the process feed: its change
/// sequence is assigned after the transaction commits (FIG-5276).
pub(crate) async fn save_process_tx(
    tx: &mut GuardedTx<'_>,
    record: &ProcessRecord,
) -> Result<(), PluginError> {
    sqlx::query(
        process_sql()
            .process_postgres
            .update_mutable_columns_staged
            .sql(),
    )
    .bind(record.id.as_str())
    .bind(record.updated_at_ms as i64)
    .bind(process_status_label(record))
    .bind(record.last_event_sequence as i64)
    .bind(cancel_requested_at_ms(record))
    .bind(serde_json::to_string(record).map_err(process_decode_error)?)
    .execute(crate::observed_sql::executor(&mut ***tx))
    .await
    .map_err(plugin_sqlx_error)?;
    Ok(())
}

/// The event `request`'s replay key already recorded, if any. A released
/// event comes back with `request`'s payload when it carries the released
/// digest, and refuses as a conflict when it does not.
pub(crate) async fn load_event_by_key_tx(
    tx: &mut sqlx::PgConnection,
    process_id: &ProcessId,
    replay_key: &str,
    request: &ProcessEventAppendRequest,
) -> Result<Option<ProcessEvent>, PluginError> {
    let Some(row) = sqlx::query(process_sql().event.select_by_replay_key.sql())
        .bind(process_id.as_str())
        .bind(replay_key)
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(plugin_sqlx_error)?
    else {
        return Ok(None);
    };
    let json: String = row.get(0);
    let released_digest: Option<String> = row.get(1);
    let mut event: ProcessEvent = serde_json::from_str(&json).map_err(process_decode_error)?;
    if let Some(digest) = released_digest {
        lash_core_execution::runtime::restore_released_process_event_payload(
            &mut event, &digest, request,
        )?;
    }
    Ok(Some(event))
}

pub(crate) async fn next_process_event_sequence_tx(
    tx: &mut sqlx::PgConnection,
    process_id: &ProcessId,
) -> Result<(Option<u64>, u64), PluginError> {
    let last_sequence: Option<i64> =
        sqlx::query_scalar(process_sql().event.select_max_sequence.sql())
            .bind(process_id.as_str())
            .fetch_one(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(plugin_sqlx_error)?;
    let last_sequence = last_sequence
        .map(|sequence| plugin_u64_from_sql("ProcessEvent", "sequence", sequence))
        .transpose()?;
    let sequence =
        lash_core_execution::runtime::allocate_process_event_sequence(last_sequence, None)?;
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
        tx: &mut sqlx::PgConnection,
        record: &mut ProcessRecord,
        request: ProcessEventAppendRequest,
        occurred_at_ms: u64,
    ) -> Result<ProcessEventAppendReceipt, PluginError> {
        self.stage_arm(tx, record, request, occurred_at_ms)
            .await
            .map(|(receipt, _)| receipt)
    }

    /// Stage one append of the batch under `authorization`, answering its
    /// arm.
    pub(crate) async fn stage_arm(
        &mut self,
        tx: &mut sqlx::PgConnection,
        record: &mut ProcessRecord,
        request: ProcessEventAppendRequest,
        occurred_at_ms: u64,
    ) -> Result<(ProcessEventAppendReceipt, ProcessEventAppendArm), PluginError> {
        let (receipt, arm, record_changed) =
            stage_process_event_append_tx(tx, record, request, occurred_at_ms, self.fleet_format)
                .await?;
        self.record_changed |= record_changed;
        Ok((receipt, arm))
    }

    /// Save the process once if any staged append moved its projection.
    pub(crate) async fn commit(
        self,
        tx: &mut GuardedTx<'_>,
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
/// once, one save on the change feed, when any of them moved it. The
/// caller owns the transaction, so a refusal of any request commits none.
pub(crate) async fn append_process_event_batch_tx(
    tx: &mut GuardedTx<'_>,
    record: &mut ProcessRecord,
    requests: Vec<ProcessEventAppendRequest>,
    occurred_at_ms: u64,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<Vec<ProcessEventAppendReceipt>, PluginError> {
    let mut batch = ProcessEventBatch::for_fleet(fleet_format);
    let mut receipts = Vec::with_capacity(requests.len());
    for request in requests {
        receipts.push(batch.stage(tx, record, request, occurred_at_ms).await?);
    }
    batch.commit(tx, record).await?;
    Ok(receipts)
}

/// One process-event append for the PostgreSQL store: the append sequence
/// ([`stage_process_event_append_tx`]) followed by the process save when the
/// append moved the projection.
pub(crate) async fn apply_process_event_append_tx(
    tx: &mut GuardedTx<'_>,
    record: &mut ProcessRecord,
    request: ProcessEventAppendRequest,
    occurred_at_ms: u64,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<(ProcessEventAppendReceipt, ProcessEventAppendArm), PluginError> {
    let (receipt, arm, record_changed) =
        stage_process_event_append_tx(tx, record, request, occurred_at_ms, fleet_format).await?;
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
/// sees: each entry point keeps its own source (the injected store clock), and
/// this function never reads one.
async fn stage_process_event_append_tx(
    tx: &mut sqlx::PgConnection,
    record: &mut ProcessRecord,
    request: ProcessEventAppendRequest,
    occurred_at_ms: u64,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<(ProcessEventAppendReceipt, ProcessEventAppendArm, bool), PluginError> {
    let process_id = record.id.clone();
    let replay_lookup =
        if let Some(replay_key) = request.replay.as_ref().map(|replay| replay.key.as_str()) {
            load_event_by_key_tx(tx, &process_id, replay_key, &request).await?
        } else {
            None
        };
    // A signal's first append selects the wait it resolves from the signals
    // of its type the log already holds (FIG-4298); a replayed signal carries
    // the wait its first append selected.
    let signal_events_before = if replay_lookup.is_none()
        && lash_core_execution::runtime::process_signal_name_from_event_type(&request.event_type)
            .is_some()
    {
        let count: i64 =
            sqlx::query_scalar(process_sql().event.count_by_type_through_sequence.sql())
                .bind(process_id.as_str())
                .bind(request.event_type.as_str())
                .bind(clamp_sequence_bound(u64::MAX))
                .fetch_one(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(plugin_sqlx_error)?;
        Some(count as u64)
    } else {
        None
    };
    let signal = request.signal();
    let wake_session_id = wake_session_id_tx(tx, &process_id).await?;
    let (last_sequence, sequence) = next_process_event_sequence_tx(tx, &process_id).await?;
    let prepared = lash_core_execution::runtime::prepare_process_event_append(
        record,
        request,
        sequence,
        last_sequence,
        replay_lookup,
        signal_events_before,
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
            sqlx::query(process_sql().event.insert.sql())
                .bind(process_id.as_str())
                .bind(sequence as i64)
                .bind(event.event_type.as_str())
                .bind(event.invocation.effect_replay_key())
                .bind(serde_json::to_string(&event).map_err(process_decode_error)?)
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(plugin_sqlx_error)?;
            // A new signal reaches the engine as mail, in the append's own
            // transaction (ADR 0132 §10).
            if let Some(signal) = &signal {
                crate::durable::processes::signal_mail_within(
                    tx,
                    &process_id,
                    signal,
                    lash_durable::DurableInstant(i64::try_from(occurred_at_ms).unwrap_or(i64::MAX)),
                )
                .await?;
            }
            *record = projected_record;
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
            deliver_process_wake_tx(tx, wake_delivery.as_ref(), occurred_at_ms).await?;
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
    tx: &mut GuardedTx<'_>,
    record: &mut ProcessRecord,
    request: ProcessEventAppendRequest,
    occurred_at_ms: u64,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<ProcessEventAppendReceipt, PluginError> {
    apply_process_event_append_tx(tx, record, request, occurred_at_ms, fleet_format)
        .await
        .map(|(receipt, _)| receipt)
}

/// Hand a process event's wake to its target session as queued work, inside
/// the append's own transaction: the producer admits the batch under its
/// source key (so a repeat is the same batch) and wakes the session actor
/// (ADR 0132 §12). A target session that is deleted, or that never existed,
/// receives nothing. The producer runs in a savepoint of the append.
pub(crate) async fn deliver_process_wake_tx(
    tx: &mut sqlx::PgConnection,
    wake: Option<&lash_core_execution::ProcessWakeDelivery>,
    occurred_at_ms: u64,
) -> Result<(), PluginError> {
    let Some(wake) = wake else {
        return Ok(());
    };
    let to_plugin =
        |error: lash_core_execution::StoreError| PluginError::Session(error.to_string());
    let sql = crate::session_sql::session_sql();
    let deleted: bool = sqlx::query_scalar(sql.deleted_postgres.exists.sql())
        .bind(wake.target_session_id.as_str())
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(plugin_sqlx_error)?;
    let live = !deleted
        && sqlx::query(sql.meta_postgres.select_relation_for_share.sql())
            .bind(wake.target_session_id.as_str())
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(plugin_sqlx_error)?
            .is_some();
    if !live {
        tracing::debug!(
            process_id = %wake.process_id,
            target_session_id = %wake.target_session_id,
            sequence = wake.sequence,
            "process wake target is not a live session; nothing is queued"
        );
        return Ok(());
    }
    let mut savepoint = sqlx::Connection::begin(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
    crate::runtime_persistence::enqueue_queued_work_with_outcome_tx(
        &mut savepoint,
        &lash_core_execution::facade_support::process_wake_batch_draft(wake.clone()),
        occurred_at_ms,
    )
    .await
    .map_err(to_plugin)?;
    savepoint.commit().await.map_err(plugin_sqlx_error)?;
    Ok(())
}

pub(crate) fn validate_process_execution_authority(
    process_id: &ProcessId,
    record: &ProcessRecord,
    authority: &ProcessExecutionWriteAuthority,
    start: Option<&ProcessStarted>,
) -> Result<(), PluginError> {
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

/// What recording a cancel request found (L6, FIG-5175).
pub(crate) enum CancelRecorded {
    /// This request is the first: recorded at `at_ms`.
    Requested { at_ms: u64 },
    /// An earlier request stands, recorded at `at_ms`.
    AlreadyRequested { at_ms: u64 },
    /// The process is terminal.
    Ended,
}

/// Record `origin`'s cancel of `process_id` at `now_ms` unless one is
/// recorded, on a durable commit's connection: the first request wins and
/// keeps its timestamp (L6, FIG-5175).
pub(crate) async fn record_cancel_tx(
    tx: &mut GuardedTx<'_>,
    process_id: &ProcessId,
    origin: lash_core_execution::CancelOrigin,
    requester: &str,
    now_ms: u64,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<CancelRecorded, PluginError> {
    let mut record = require_process_tx(tx, process_id).await?;
    if record.is_terminal() {
        return Ok(CancelRecorded::Ended);
    }
    if let Some(existing) = record.cancel_request.as_deref() {
        return Ok(CancelRecorded::AlreadyRequested {
            at_ms: existing.requested_at_ms,
        });
    }
    let request = lash_core_execution::CancelRequest::new(origin, requester, now_ms);
    if let lash_core_execution::runtime::ProcessTransitionPlan::Append(append) =
        lash_core_execution::runtime::prepare_process_transition(
            &record,
            lash_core_execution::runtime::ProcessTransition::RequestCancel(request),
        )?
    {
        append_process_event_tx(tx, &mut record, *append, now_ms, fleet_format).await?;
    }
    Ok(CancelRecorded::Requested { at_ms: now_ms })
}

/// End `process_id` with `output` under its actor's `epoch`, on a durable
/// commit's connection. A process already terminal keeps its first
/// terminal; answers whether this call ended it (L6, FIG-5175).
pub(crate) async fn record_terminal_tx(
    tx: &mut GuardedTx<'_>,
    process_id: &ProcessId,
    output: &ProcessAwaitOutput,
    epoch: u64,
    now_ms: u64,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<bool, PluginError> {
    let mut record = require_process_tx(tx, process_id).await?;
    if record.is_terminal() {
        return Ok(false);
    }
    let output = output.clone().with_cancel_origin(
        record
            .cancel_request
            .as_deref()
            .map(|request| request.origin),
    );
    let authority = lash_core_execution::ProcessCompletionAuthority::ActorEpoch { epoch };
    let mut batch = ProcessEventBatch::for_fleet(fleet_format);
    let request = lash_core_execution::facade_support::terminal_append_request(
        process_id,
        &output,
        Some(&authority),
    );
    batch.stage(tx, &mut record, request, now_ms).await?;
    batch.commit(tx, &record).await?;
    Ok(true)
}

/// Append `request`, replay-keyed, to `process_id` on a durable commit's
/// connection: a repeat under the same key is a no-op (L6, FIG-5175).
pub(crate) async fn record_event_tx(
    tx: &mut GuardedTx<'_>,
    process_id: &ProcessId,
    request: ProcessEventAppendRequest,
    now_ms: u64,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<(), PluginError> {
    let mut record = require_process_tx(tx, process_id).await?;
    append_process_event_tx(tx, &mut record, request, now_ms, fleet_format).await?;
    Ok(())
}
