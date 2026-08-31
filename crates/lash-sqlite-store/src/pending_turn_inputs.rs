use super::*;

pub(crate) fn decode_turn_input_ingress(
    value: String,
) -> Result<lash_core::TurnInputIngress, StoreError> {
    serde_json::from_str(&value)
        .map_err(|err| StoreError::Backend(format!("failed to decode turn-input ingress: {err}")))
}

pub(crate) fn decode_turn_input_state(
    value: String,
) -> Result<lash_core::TurnInputState, StoreError> {
    lash_core::TurnInputState::from_wire_str(&value)
        .ok_or_else(|| StoreError::Backend(format!("unknown turn-input state `{value}`")))
}

pub(crate) fn decode_turn_input(value: String) -> Result<lash_core::TurnInput, StoreError> {
    serde_json::from_str(&value)
        .map_err(|err| StoreError::Backend(format!("failed to decode turn input: {err}")))
}

#[derive(Clone, Debug)]
pub(crate) struct PendingTurnInputRow {
    pub(crate) enqueue_seq: u64,
    pub(crate) input_id: String,
    pub(crate) session_id: String,
    pub(crate) source_key: Option<String>,
    pub(crate) ingress_json: String,
    pub(crate) state: String,
    pub(crate) input_json: String,
    pub(crate) enqueued_at_ms: u64,
    pub(crate) claim_id: Option<String>,
    pub(crate) claim_fencing_token: u64,
    pub(crate) claim_owner: Option<LeaseOwnerIdentity>,
    pub(crate) claim_token: Option<String>,
    pub(crate) claim_session_lease_generation: u64,
}

pub(crate) fn pending_turn_input_row_from_sql(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<PendingTurnInputRow> {
    Ok(PendingTurnInputRow {
        enqueue_seq: u64_from_sql("PendingTurnInput", "enqueue_seq", row.get(0)?)?,
        input_id: row.get(1)?,
        session_id: row.get(2)?,
        source_key: row.get(3)?,
        ingress_json: row.get(4)?,
        state: row.get(5)?,
        input_json: row.get(6)?,
        enqueued_at_ms: u64_from_sql("PendingTurnInput", "enqueued_at_ms", row.get(7)?)?,
        claim_id: row.get(8)?,
        claim_fencing_token: u64_from_sql("PendingTurnInput", "claim_fencing_token", row.get(9)?)?,
        claim_owner: lease_owner_from_columns(row.get(10)?, row.get(11)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                10,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        claim_token: row.get(13)?,
        claim_session_lease_generation: u64_from_sql(
            "PendingTurnInput",
            "claim_session_lease_generation",
            row.get(14)?,
        )?,
    })
}

pub(crate) fn pending_turn_input_from_row(
    row: PendingTurnInputRow,
) -> Result<lash_core::PendingTurnInput, StoreError> {
    Ok(lash_core::PendingTurnInput {
        input_id: row.input_id,
        session_id: row.session_id,
        enqueue_seq: row.enqueue_seq,
        source_key: row.source_key,
        ingress: decode_turn_input_ingress(row.ingress_json)?,
        state: decode_turn_input_state(row.state)?,
        enqueued_at_ms: row.enqueued_at_ms,
        input: decode_turn_input(row.input_json)?,
    })
}

pub(crate) fn load_pending_turn_input_by_id_conn(
    conn: &Connection,
    session_id: &str,
    input_id: &str,
) -> Result<Option<lash_core::PendingTurnInput>, StoreError> {
    let row = conn
        .query_row(
            "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                    state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                    claim_owner_id, claim_owner_incarnation_id,
                    claim_owner_liveness_json, claim_token, claim_session_lease_generation
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND input_id = ?2",
            params![session_id, input_id],
            pending_turn_input_row_from_sql,
        )
        .optional()
        .map_err(sqlite_error)?;
    row.map(pending_turn_input_from_row).transpose()
}

pub(crate) fn load_pending_turn_input_row_by_target_conn(
    conn: &Connection,
    session_id: &str,
    target: &lash_core::PendingTurnInputCancelTarget,
) -> Result<Option<PendingTurnInputRow>, StoreError> {
    match target {
        lash_core::PendingTurnInputCancelTarget::InputId(input_id) => conn
            .query_row(
                "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                        state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                        claim_owner_id, claim_owner_incarnation_id,
                        claim_owner_liveness_json, claim_token, claim_session_lease_generation
                 FROM pending_turn_inputs
                 WHERE session_id = ?1 AND input_id = ?2",
                params![session_id, input_id],
                pending_turn_input_row_from_sql,
            )
            .optional()
            .map_err(sqlite_error),
        lash_core::PendingTurnInputCancelTarget::SourceKey(source_key) => conn
            .query_row(
                "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                        state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                        claim_owner_id, claim_owner_incarnation_id,
                        claim_owner_liveness_json, claim_token, claim_session_lease_generation
                 FROM pending_turn_inputs
                 WHERE session_id = ?1 AND source_key = ?2",
                params![session_id, source_key],
                pending_turn_input_row_from_sql,
            )
            .optional()
            .map_err(sqlite_error),
    }
}

pub(crate) fn pending_turn_input_claim_diagnostics_from_row(
    row: &PendingTurnInputRow,
    state: lash_core::TurnInputState,
) -> Option<lash_core::PendingTurnInputClaimDiagnostics> {
    (row.claim_token.is_some() || matches!(state, lash_core::TurnInputState::Accepted)).then(|| {
        lash_core::PendingTurnInputClaimDiagnostics {
            state,
            claim_id: row.claim_id.clone(),
            claim_owner: row.claim_owner.clone(),
            claim_session_lease_generation: row
                .claim_token
                .as_ref()
                .map(|_| row.claim_session_lease_generation),
            claim_fencing_token: row.claim_fencing_token,
        }
    })
}

#[derive(Clone, Debug)]
pub(crate) struct TurnInputClaimLease {
    pub(crate) claim_id: String,
    pub(crate) lease_token: String,
    pub(crate) fencing_token: u64,
    pub(crate) session_lease_generation: u64,
}

impl TurnInputClaimLease {
    pub(crate) fn derive(
        head: &PendingTurnInputRow,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        now_epoch_ms: u64,
        session_lease_generation: u64,
    ) -> Result<Self, StoreError> {
        let lease = lash_core::store::queued_work::WorkClaimLease::derive(
            lash_core::store::queued_work::ClaimIdDialect::TurnInput,
            head.enqueue_seq,
            head.claim_fencing_token,
            session_id,
            owner,
            now_epoch_ms,
            session_lease_generation,
        )?;
        Ok(Self {
            claim_id: lease.claim_id,
            lease_token: lease.lease_token,
            fencing_token: lease.fencing_token,
            session_lease_generation: lease.session_lease_generation,
        })
    }
}
