use super::*;
use lash_vocabulary::identity::SessionId;

pub(crate) fn decode_turn_input_ingress(
    value: String,
) -> Result<lash_core_store::turn_input_vocabulary::TurnInputIngress, StoreError> {
    serde_json::from_str(&value).map_err(|err| stored_data_corrupt("TurnInputIngress", err))
}

pub(crate) fn decode_turn_input_state(
    value: String,
    ingress: lash_core_store::turn_input_vocabulary::TurnInputIngress,
    terminal_at_ms: Option<u64>,
) -> Result<lash_core_store::turn_input_vocabulary::TurnInputState, StoreError> {
    lash_core_store::turn_input_vocabulary::TurnInputState::from_persisted(
        &value,
        ingress,
        terminal_at_ms,
    )
}

/// The submitted input, or [`Released`](lash_core_store::turn_input_vocabulary::SubmittedBody::Released)
/// once the session's retention released the settled body (FIG-6607).
pub(crate) fn decode_turn_input(
    value: Option<String>,
) -> Result<
    lash_core_store::turn_input_vocabulary::SubmittedBody<
        lash_core_store::turn_input_vocabulary::TurnInput,
    >,
    StoreError,
> {
    value
        .map(|value| serde_json::from_str(&value))
        .transpose()
        .map(Into::into)
        .map_err(|err| stored_data_corrupt("TurnInput", err))
}

#[derive(Clone, Debug)]
pub(crate) struct PendingTurnInputRow {
    pub(crate) enqueue_seq: u64,
    pub(crate) input_id: String,
    pub(crate) session_id: SessionId,
    pub(crate) source_key: Option<String>,
    pub(crate) origin_json: String,
    pub(crate) ingress_json: String,
    pub(crate) state: String,
    /// `None` once the session's retention released the settled body.
    pub(crate) input_json: Option<String>,
    pub(crate) enqueued_at_ms: u64,
    /// The run that took the input, retained after settlement; absent while unbound.
    pub(crate) admitted_run: Option<String>,
    pub(crate) run_spec_hash: Option<String>,
    /// When the row's tombstone was written; `None` until it is terminal.
    pub(crate) terminal_at_ms: Option<u64>,
    pub(crate) trace_cause_json: Option<String>,
}

pub(crate) fn pending_turn_input_row_from_sql(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<PendingTurnInputRow> {
    Ok(PendingTurnInputRow {
        enqueue_seq: u64_from_sql("PendingTurnInput", "enqueue_seq", row.get(0)?)?,
        input_id: row.get(1)?,
        session_id: crate::codec::sql_identity(row.get::<_, String>(2)?)?,
        source_key: row.get(3)?,
        origin_json: row.get(13)?,
        ingress_json: row.get(4)?,
        state: row.get(5)?,
        input_json: row.get(6)?,
        enqueued_at_ms: u64_from_sql("PendingTurnInput", "enqueued_at_ms", row.get(7)?)?,
        admitted_run: row.get(8)?,
        run_spec_hash: row.get(10)?,
        terminal_at_ms: row
            .get::<_, Option<i64>>(11)?
            .map(|at| u64_from_sql("PendingTurnInput", "terminal_at_ms", at))
            .transpose()?,
        trace_cause_json: row.get(12)?,
    })
}

pub(crate) fn pending_turn_input_from_row(
    row: PendingTurnInputRow,
) -> Result<lash_core_store::turn_input_vocabulary::PendingTurnInput, StoreError> {
    let ingress = decode_turn_input_ingress(row.ingress_json)?;
    Ok(lash_core_store::turn_input_vocabulary::PendingTurnInput {
        input_id: row.input_id.try_into()?,
        session_id: row.session_id,
        enqueue_seq: row.enqueue_seq,
        source_key: row.source_key,
        origin: serde_json::from_str(&row.origin_json)
            .map_err(|error| stored_data_corrupt("TurnInputOrigin", error))?,
        state: decode_turn_input_state(row.state, ingress, row.terminal_at_ms)?,
        enqueued_at_ms: row.enqueued_at_ms,
        input: decode_turn_input(row.input_json)?,
        run_spec: row
            .run_spec_hash
            .map(lash_core_store::turn_input_vocabulary::RunSpecHash::from_stored),
        trace_cause: lash_core_store::store_backend_support::decode_trace_cause(
            "PendingTurnInput",
            row.trace_cause_json.as_deref(),
        )?,
    })
}

pub(crate) fn pending_turn_input_read_from_row(
    row: PendingTurnInputRow,
) -> Result<lash_core_store::turn_input_vocabulary::PendingTurnInputRead, StoreError> {
    let admitted_run = (!matches!(row.state.as_str(), "completed" | "cancelled"))
        .then(|| row.admitted_run.clone())
        .flatten();
    let input = pending_turn_input_from_row(row)?;
    Ok(match admitted_run {
        Some(run) => lash_core_store::turn_input_vocabulary::PendingTurnInputRead::admitted(
            input,
            lash_vocabulary::identity::TurnId::parse(run)?,
        ),
        None => lash_core_store::turn_input_vocabulary::PendingTurnInputRead::open(input),
    })
}

pub(crate) fn load_pending_turn_input_by_id_conn(
    conn: &Connection,
    session_id: &SessionId,
    input_id: &str,
) -> Result<Option<lash_core_store::turn_input_vocabulary::PendingTurnInput>, StoreError> {
    let row = conn
        .prepare_cached(
            crate::turn_ingress::turn_ingress_sql()
                .pending_inputs
                .select_by_id
                .sql(),
        )
        .and_then(|mut statement| {
            statement.query_row(
                params![session_id.as_str(), input_id],
                pending_turn_input_row_from_sql,
            )
        })
        .optional()
        .map_err(sqlite_error)?;
    row.map(pending_turn_input_from_row).transpose()
}

pub(crate) fn load_pending_turn_input_row_by_target_conn(
    conn: &Connection,
    session_id: &SessionId,
    target: &lash_core_store::turn_input_vocabulary::PendingTurnInputCancelTarget,
) -> Result<Option<PendingTurnInputRow>, StoreError> {
    let sql = crate::turn_ingress::turn_ingress_sql();
    match target {
        lash_core_store::turn_input_vocabulary::PendingTurnInputCancelTarget::InputId(input_id) => {
            conn.prepare_cached(sql.pending_inputs.select_by_id.sql())
                .and_then(|mut statement| {
                    statement.query_row(
                        params![session_id.as_str(), input_id.as_str()],
                        pending_turn_input_row_from_sql,
                    )
                })
                .optional()
                .map_err(sqlite_error)
        }
        lash_core_store::turn_input_vocabulary::PendingTurnInputCancelTarget::SourceKey(
            source_key,
        ) => conn
            .prepare_cached(sql.pending_inputs.select_by_source_key.sql())
            .and_then(|mut statement| {
                statement.query_row(
                    params![session_id.as_str(), source_key],
                    pending_turn_input_row_from_sql,
                )
            })
            .optional()
            .map_err(sqlite_error),
    }
}
