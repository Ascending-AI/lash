use crate::guarded_tx::GuardedTx;
use crate::*;
use lash_protocol_types::ProcessId;

use crate::process_sql::process_sql;

fn decode_process_record(json: &str) -> Result<ProcessRecord, PluginError> {
    lash_execution_contracts::process::registry_transitions::decode_record(json)
}

pub(crate) async fn process_change_horizon_tx(
    tx: &mut sqlx::PgConnection,
) -> Result<u64, PluginError> {
    let horizon: i64 = crate::observed_sql::query_scalar(
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
    let json: Option<String> = crate::observed_sql::query_scalar(
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
    start_key: &lash_core_store::process_identity::StartKey,
) -> Result<Option<ProcessRecord>, PluginError> {
    let json: Option<String> = crate::observed_sql::query_scalar(
        process_sql().process.select_record_json_by_start_key.sql(),
    )
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
        crate::observed_sql::query_scalar(process_sql().process.select_record_json_by_id.sql())
            .bind(process_id.as_str())
            .fetch_optional(crate::observed_sql::executor(pool))
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
    let row = crate::observed_sql::query(process_sql().tombstone.select_terminal.sql())
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

/// Save `record`'s mutable columns, staged on the process feed: its change
/// sequence is assigned after the transaction commits (FIG-5276).
pub(crate) async fn save_process_tx(
    tx: &mut GuardedTx<'_>,
    record: &ProcessRecord,
    execution_changed: bool,
) -> Result<(), PluginError> {
    let statements = &process_sql().process_postgres;
    let sql = if execution_changed {
        statements.update_execution_columns_staged.sql()
    } else {
        statements.update_mutable_columns_staged.sql()
    };
    let query = crate::observed_sql::query(sql)
        .bind(record.id.as_str())
        .bind(record.updated_at_ms as i64)
        .bind(serde_json::to_string(&record.lifecycle_storage()).map_err(process_decode_error)?);
    let query = if execution_changed {
        query
            .bind(serde_json::to_string(&record.execution_storage()).map_err(process_decode_error)?)
    } else {
        query
    };
    query
        .execute(crate::observed_sql::executor(&mut ***tx))
        .await
        .map_err(plugin_sqlx_error)?;
    Ok(())
}

/// The event `append`'s replay key already recorded, if any. A released
/// event comes back with `append`'s fact when it carries the released
/// digest, and refuses as a conflict when it does not.
pub(crate) async fn load_event_by_key_tx(
    tx: &mut sqlx::PgConnection,
    process_id: &ProcessId,
    append: &lash_execution_contracts::process::CanonicalProcessEventAppend,
    fleet_format: lash_core_store::store::FleetFormat,
) -> Result<Option<ProcessEvent>, PluginError> {
    let Some(row) = crate::observed_sql::query(process_sql().event.select_by_replay_key.sql())
        .bind(process_id.as_str())
        .bind(append.replay_key())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(plugin_sqlx_error)?
    else {
        return Ok(None);
    };
    let json: String = row.get(0);
    let released_digest: Option<String> = row.get(1);
    let event = match released_digest {
        Some(digest) => lash_execution_contracts::process::restore_released_process_event(
            serde_json::from_str(&json).map_err(process_decode_error)?,
            &digest,
            append,
        )?,
        None => ProcessEvent::decode(&json, fleet_format)?,
    };
    Ok(Some(event))
}

pub(crate) async fn next_process_event_sequence_tx(
    tx: &mut sqlx::PgConnection,
    process_id: &ProcessId,
) -> Result<(Option<u64>, u64), PluginError> {
    let last_sequence: Option<i64> =
        crate::observed_sql::query_scalar(process_sql().event.select_max_sequence.sql())
            .bind(process_id.as_str())
            .fetch_one(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(plugin_sqlx_error)?;
    let last_sequence = last_sequence
        .map(|sequence| plugin_u64_from_sql("ProcessEvent", "sequence", sequence))
        .transpose()?;
    let sequence =
        lash_execution_contracts::process::allocate_process_event_sequence(last_sequence)?;
    Ok((last_sequence, sequence))
}

/// Which arm of the prepared append plan the store actually applied.
///
/// Entry points map this onto their own outcome type; the shared append
/// sequence never decides what a caller returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessEventAppendArm {
    /// The replay arm. No event row was written.
    Replayed,
    /// The insert arm. Exactly one lifecycle event row was written.
    Inserted,
}

/// A batch of process-event appends staged against one in-memory projection
/// inside one transaction (FIG-3571), saved once by [`Self::commit`].
pub(crate) struct ProcessEventBatch {
    fleet_format: lash_core_store::store::FleetFormat,
    record_changed: bool,
    execution_changed: bool,
}

impl ProcessEventBatch {
    /// Start an empty batch whose appends stamp `fleet_format`'s versions.
    pub(crate) fn for_fleet(fleet_format: lash_core_store::store::FleetFormat) -> Self {
        Self {
            fleet_format,
            record_changed: false,
            execution_changed: false,
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
        self.execution_changed |= record_changed
            && receipt.event.fact.kind()
                == lash_execution_contracts::process::ProcessEventKind::Succeeded;
        Ok((receipt, arm))
    }

    /// Save the process once if any staged append moved its projection.
    pub(crate) async fn commit(
        self,
        tx: &mut GuardedTx<'_>,
        record: &ProcessRecord,
    ) -> Result<(), PluginError> {
        if self.record_changed {
            save_process_tx(tx, record, self.execution_changed).await?;
        }
        Ok(())
    }
}

/// One process-event append for the PostgreSQL store: the append sequence
/// ([`stage_process_event_append_tx`]) followed by the process save when the
/// append moved the projection.
pub(crate) async fn apply_process_event_append_tx(
    tx: &mut GuardedTx<'_>,
    record: &mut ProcessRecord,
    request: ProcessEventAppendRequest,
    occurred_at_ms: u64,
    fleet_format: lash_core_store::store::FleetFormat,
) -> Result<(ProcessEventAppendReceipt, ProcessEventAppendArm), PluginError> {
    let (receipt, arm, record_changed) =
        stage_process_event_append_tx(tx, record, request, occurred_at_ms, fleet_format).await?;
    if record_changed {
        save_process_tx(
            tx,
            record,
            receipt.event.fact.kind()
                == lash_execution_contracts::process::ProcessEventKind::Succeeded,
        )
        .await?;
    }
    Ok((receipt, arm))
}

/// The one process-event append sequence for the PostgreSQL store, short of
/// the process save.
///
/// Every entry point runs these steps, in this order: replay-key lookup,
/// next sequence number, prepare, the replay-or-insert decision, the
/// event insert, the projection update and parent-end retention. The
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
    fleet_format: lash_core_store::store::FleetFormat,
) -> Result<(ProcessEventAppendReceipt, ProcessEventAppendArm, bool), PluginError> {
    let process_id = record.id.clone();
    let append = request.canonical(record, fleet_format)?;
    let replay_lookup = load_event_by_key_tx(tx, &process_id, &append, fleet_format).await?;
    let (last_sequence, sequence) = next_process_event_sequence_tx(tx, &process_id).await?;
    let prepared = lash_execution_contracts::process::prepare_process_event_append(
        record,
        append,
        sequence,
        last_sequence,
        replay_lookup,
        occurred_at_ms,
    )?;
    match prepared {
        lash_execution_contracts::process::ProcessEventAppendPlan::Replay {
            event,
            repair_record,
            ..
        } => {
            let repaired = repair_record.is_some();
            if let Some(repaired) = repair_record {
                *record = repaired;
            }
            Ok((
                ProcessEventAppendReceipt {
                    last_event_sequence: record.last_event_sequence,
                    realization: lash_execution_contracts::process::StoreRealization::Coalesced,
                    event,
                },
                ProcessEventAppendArm::Replayed,
                repaired,
            ))
        }
        lash_execution_contracts::process::ProcessEventAppendPlan::Insert {
            event,
            projected_record,
        } => {
            crate::observed_sql::query(process_sql().event.insert.sql())
                .bind(process_id.as_str())
                .bind(sequence as i64)
                .bind(event.fact.event_type())
                .bind(event.invocation.effect_replay_key())
                .bind(serde_json::to_string(&event).map_err(process_decode_error)?)
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(plugin_sqlx_error)?;
            *record = projected_record;
            // A process that just reached a terminal status is an ended parent
            // scope: its ledger row rides the same transaction as the terminal
            // append, so no child can be stranded by a crash between the two.
            if record.is_terminal() {
                crate::process_registry::parent_end::record_tx(
                    tx,
                    &lash_core_store::scope_identity::ScopeId::process(process_id.clone()),
                    occurred_at_ms,
                    fleet_format,
                )
                .await?;
            }
            Ok((
                ProcessEventAppendReceipt {
                    last_event_sequence: event.sequence,
                    realization: lash_execution_contracts::process::StoreRealization::Realized,
                    event,
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
    fleet_format: lash_core_store::store::FleetFormat,
) -> Result<ProcessEventAppendReceipt, PluginError> {
    apply_process_event_append_tx(tx, record, request, occurred_at_ms, fleet_format)
        .await
        .map(|(receipt, _)| receipt)
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
    origin: lash_protocol_types::tool_output::CancelOrigin,
    requester: &str,
    now_ms: u64,
    fleet_format: lash_core_store::store::FleetFormat,
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
    let request = lash_protocol_types::tool_output::CancelRequest::new(origin, requester, now_ms);
    if let lash_execution_contracts::process::ProcessTransitionPlan::Append(append) =
        lash_execution_contracts::process::prepare_process_transition(
            &record,
            lash_execution_contracts::process::ProcessTransition::RequestCancel(request),
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
    fleet_format: lash_core_store::store::FleetFormat,
) -> Result<bool, PluginError> {
    let mut record = require_process_tx(tx, process_id).await?;
    if record.is_terminal() {
        return Ok(false);
    }
    let authority =
        lash_execution_contracts::process::ProcessCompletionAuthority::ActorEpoch { epoch };
    let mut batch = ProcessEventBatch::for_fleet(fleet_format);
    let request = lash_execution_contracts::process::terminal_append_request(
        process_id,
        output,
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
    fleet_format: lash_core_store::store::FleetFormat,
) -> Result<(), PluginError> {
    let mut record = require_process_tx(tx, process_id).await?;
    append_process_event_tx(tx, &mut record, request, now_ms, fleet_format).await?;
    Ok(())
}
