use super::*;
use lash_sansio::SessionId;

pub(crate) fn decode_turn_input_ingress(
    value: String,
) -> Result<lash_core_execution::TurnInputIngress, StoreError> {
    serde_json::from_str(&value).map_err(|err| stored_data_corrupt("TurnInputIngress", err))
}

pub(crate) fn decode_turn_input_state(
    value: String,
    ingress: lash_core_execution::TurnInputIngress,
    terminal_at_ms: Option<u64>,
) -> Result<lash_core_execution::TurnInputState, StoreError> {
    lash_core_execution::TurnInputState::from_persisted(&value, ingress, terminal_at_ms)
}

pub(crate) fn decode_turn_input(
    value: String,
) -> Result<lash_core_execution::TurnInput, StoreError> {
    serde_json::from_str(&value).map_err(|err| stored_data_corrupt("TurnInput", err))
}

#[derive(Clone, Debug)]
pub(crate) struct PendingTurnInputRow {
    pub(crate) enqueue_seq: u64,
    pub(crate) input_id: String,
    pub(crate) session_id: SessionId,
    pub(crate) source_key: Option<String>,
    pub(crate) ingress_json: String,
    pub(crate) state: String,
    pub(crate) input_json: String,
    pub(crate) enqueued_at_ms: u64,
    /// The root whose admission holds the row; `None` while it is open.
    pub(crate) admitted_root: Option<String>,
    pub(crate) run_spec_hash: Option<String>,
    /// When the row's tombstone was written; `None` until it is terminal.
    pub(crate) terminal_at_ms: Option<u64>,
}

pub(crate) fn pending_turn_input_row_from_sql(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<PendingTurnInputRow> {
    Ok(PendingTurnInputRow {
        enqueue_seq: u64_from_sql("PendingTurnInput", "enqueue_seq", row.get(0)?)?,
        input_id: row.get(1)?,
        session_id: crate::codec::sql_identity(row.get::<_, String>(2)?)?,
        source_key: row.get(3)?,
        ingress_json: row.get(4)?,
        state: row.get(5)?,
        input_json: row.get(6)?,
        enqueued_at_ms: u64_from_sql("PendingTurnInput", "enqueued_at_ms", row.get(7)?)?,
        admitted_root: row.get(8)?,
        run_spec_hash: row.get(10)?,
        terminal_at_ms: row
            .get::<_, Option<i64>>(11)?
            .map(|at| u64_from_sql("PendingTurnInput", "terminal_at_ms", at))
            .transpose()?,
    })
}

pub(crate) fn pending_turn_input_from_row(
    row: PendingTurnInputRow,
) -> Result<lash_core_execution::PendingTurnInput, StoreError> {
    let ingress = decode_turn_input_ingress(row.ingress_json)?;
    Ok(lash_core_execution::PendingTurnInput {
        input_id: row.input_id.try_into()?,
        session_id: row.session_id,
        enqueue_seq: row.enqueue_seq,
        source_key: row.source_key,
        state: decode_turn_input_state(row.state, ingress, row.terminal_at_ms)?,
        enqueued_at_ms: row.enqueued_at_ms,
        input: decode_turn_input(row.input_json)?,
        run_spec: row
            .run_spec_hash
            .map(lash_core_execution::RunSpecHash::from_stored),
    })
}

pub(crate) fn pending_turn_input_read_from_row(
    row: PendingTurnInputRow,
) -> Result<lash_core_execution::PendingTurnInputRead, StoreError> {
    let admitted_root = row.admitted_root.clone();
    let input = pending_turn_input_from_row(row)?;
    Ok(match admitted_root {
        Some(root) => lash_core_execution::PendingTurnInputRead::admitted(
            input,
            lash_core_execution::TurnId::parse(root)?,
        ),
        None => lash_core_execution::PendingTurnInputRead::open(input),
    })
}

pub(crate) fn load_pending_turn_input_by_id_conn(
    conn: &Connection,
    session_id: &SessionId,
    input_id: &str,
) -> Result<Option<lash_core_execution::PendingTurnInput>, StoreError> {
    let row = conn
        .query_row(
            crate::turn_ingress::turn_ingress_sql()
                .pending_inputs
                .select_by_id
                .sql(),
            params![session_id.as_str(), input_id],
            pending_turn_input_row_from_sql,
        )
        .optional()
        .map_err(sqlite_error)?;
    row.map(pending_turn_input_from_row).transpose()
}

pub(crate) fn load_pending_turn_input_row_by_target_conn(
    conn: &Connection,
    session_id: &SessionId,
    target: &lash_core_execution::PendingTurnInputCancelTarget,
) -> Result<Option<PendingTurnInputRow>, StoreError> {
    let sql = crate::turn_ingress::turn_ingress_sql();
    match target {
        lash_core_execution::PendingTurnInputCancelTarget::InputId(input_id) => conn
            .query_row(
                sql.pending_inputs.select_by_id.sql(),
                params![session_id.as_str(), input_id.as_str()],
                pending_turn_input_row_from_sql,
            )
            .optional()
            .map_err(sqlite_error),
        lash_core_execution::PendingTurnInputCancelTarget::SourceKey(source_key) => conn
            .query_row(
                sql.pending_inputs.select_by_source_key.sql(),
                params![session_id.as_str(), source_key],
                pending_turn_input_row_from_sql,
            )
            .optional()
            .map_err(sqlite_error),
    }
}
