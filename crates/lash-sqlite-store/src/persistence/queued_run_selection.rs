use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn claim_selected_queued_work_sqlite_conn(
    tx: &Connection,
    now: u64,
    session_id: &SessionId,
    fence: &SessionExecutionLeaseAuthority,
    owner: &LeaseOwnerIdentity,
    boundary: QueuedWorkClaimBoundary,
    batch_ids: &[lash_core::BatchId],
    policy: QueuedWorkClaimPolicy,
) -> Result<SelectedQueuedWorkClaimOutcome, StoreError> {
    ensure_session_execution_lease_conn(tx, session_id, fence, now)?;
    let generation = fence.fencing_token;
    let requested_ids = batch_ids
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let sql = crate::turn_ingress::turn_ingress_sql();
    // Every list bind in this crate is a JSON array unpacked
    // with `json_each`, so the statement's text is fixed and
    // the arity lives in the bound value.
    let sql_batch_ids = encode_json(
        &batch_ids
            .iter()
            .map(lash_core::BatchId::as_str)
            .collect::<Vec<_>>(),
    )?;
    let present_ids = {
        let mut stmt = tx
            .prepare(sql.queued_batches_sqlite.select_present_ids.sql())
            .map_err(sqlite_error)?;
        stmt.query_map(params![session_id.as_str(), sql_batch_ids], |row| {
            row.get::<_, String>(0)
        })
        .map_err(sqlite_error)?
        .collect::<Result<std::collections::BTreeSet<_>, _>>()
        .map_err(sqlite_error)?
    };
    let already_satisfied_batch_ids = batch_ids
        .iter()
        .filter(|batch_id| !present_ids.contains(batch_id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if present_ids.is_empty() {
        return Ok(SelectedQueuedWorkClaimOutcome::new(
            None,
            already_satisfied_batch_ids,
        ));
    }
    let requested_rows = {
        let mut stmt = tx
            .prepare(sql.queued_batches_sqlite.select_by_ids.sql())
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(
                params![
                    session_id.as_str(),
                    now as i64,
                    sql_session_lease_generation(generation)?,
                    sql_batch_ids,
                ],
                queued_batch_row_from_sql,
            )
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    };
    if requested_rows.len() != present_ids.len() {
        return Ok(SelectedQueuedWorkClaimOutcome::new(
            None,
            already_satisfied_batch_ids,
        ));
    }
    let involved_claim_ids = requested_rows
        .iter()
        .filter_map(|row| row.claim_id.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut validation_rows = requested_rows.clone();
    if !involved_claim_ids.is_empty() {
        let mut stmt = tx
            .prepare(sql.queued_batches_sqlite.select_by_claim_ids.sql())
            .map_err(sqlite_error)?;
        let claim_rows = stmt
            .query_map(
                params![
                    session_id.as_str(),
                    now as i64,
                    sql_session_lease_generation(generation)?,
                    encode_json(&involved_claim_ids)?,
                ],
                queued_batch_row_from_sql,
            )
            .map_err(sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sqlite_error)?;
        validation_rows.extend(claim_rows);
        validation_rows.sort_by_key(|row| row.enqueue_seq);
        validation_rows.dedup_by(|left, right| left.batch_id == right.batch_id);
    }
    let validation_batch_claims = validation_rows
        .iter()
        .map(|row| {
            (
                lash_core::BatchId::from(row.batch_id.clone()),
                row.claim_id.clone(),
            )
        })
        .collect::<Vec<_>>();
    let interrupted_positions =
        lash_core::store::queued_work::select_interrupted_exact_claim_indices(
            &validation_batch_claims,
            batch_ids,
        )
        .map_err(|required_batch_ids| {
            StoreError::SelectedQueuedWorkRequiresInterruptedComposition {
                required_batch_ids: required_batch_ids
                    .into_iter()
                    .map(lash_core::BatchId::into_inner)
                    .collect(),
            }
        })?;
    let (mut rows, mut batches) = if let Some(interrupted_positions) = interrupted_positions {
        let rows = interrupted_positions
            .into_iter()
            .map(|position| validation_rows[position].clone())
            .collect::<Vec<_>>();
        let batches = rows
            .iter()
            .map(|row| queued_work_batch_from_conn(tx, row.clone()))
            .collect::<Result<Vec<_>, _>>()?;
        (rows, batches)
    } else {
        let mut requested_batches = std::collections::BTreeMap::new();
        for row in &requested_rows {
            let batch = queued_work_batch_from_conn(tx, row.clone())?;
            if batch.work_class() != lash_core::store::QueuedWorkClass::TurnWork {
                return Ok(SelectedQueuedWorkClaimOutcome::new(
                    None,
                    already_satisfied_batch_ids,
                ));
            }
            requested_batches.insert(row.batch_id.clone(), batch);
        }
        let span_rows = {
            let mut stmt = tx
                .prepare(sql.queued_batches_sqlite.select_span.sql())
                .map_err(sqlite_error)?;
            #[expect(
                clippy::expect_used,
                reason = "`requested_rows[0]` on the line above already requires a non-empty slice"
            )]
            let last_enqueue_seq = requested_rows
                .last()
                .expect("requested rows exist")
                .enqueue_seq as i64;
            stmt.query_map(
                params![
                    session_id.as_str(),
                    now as i64,
                    sql_session_lease_generation(generation)?,
                    requested_rows[0].enqueue_seq as i64,
                    last_enqueue_seq,
                ],
                queued_batch_row_from_sql,
            )
            .map_err(sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sqlite_error)?
        };
        let Some(first_position) = span_rows
            .iter()
            .position(|row| requested_ids.contains(row.batch_id.as_str()))
        else {
            return Ok(SelectedQueuedWorkClaimOutcome::new(
                None,
                already_satisfied_batch_ids,
            ));
        };
        let rows = span_rows[first_position..]
            .iter()
            .take_while(|row| requested_ids.contains(row.batch_id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        #[expect(
            clippy::expect_used,
            reason = "`rows` was filtered to ids in `requested_ids`, which are exactly the keys of `requested_batches`"
        )]
        let batches = rows
            .iter()
            .map(|row| {
                requested_batches
                    .get(&row.batch_id)
                    .expect("contiguous exact row was validated")
                    .clone()
            })
            .collect::<Vec<_>>();
        (rows, batches)
    };
    let candidates = rows
        .iter()
        .zip(batches.iter())
        .map(|(row, batch)| claim_candidate_from_row(row, batch))
        .collect::<Vec<_>>();
    let selected_len =
        match select_exact_turn_work_claim_prefix(&candidates, boundary, &policy, now)? {
            TurnWorkClaimPrefix::Selected { len } => len,
            TurnWorkClaimPrefix::Refused { .. } => {
                return Ok(SelectedQueuedWorkClaimOutcome::new(
                    None,
                    already_satisfied_batch_ids,
                ));
            }
        };
    rows.truncate(selected_len);
    batches.truncate(selected_len);
    let claim = match claim_queued_work_rows_sqlite(
        tx,
        now,
        session_id,
        owner,
        generation,
        &rows,
        batches,
        &candidates,
    )? {
        TxOutcome::Commit(claim) => claim,
        TxOutcome::Rollback(_) => None,
    };
    Ok(SelectedQueuedWorkClaimOutcome::new(
        claim,
        already_satisfied_batch_ids,
    ))
}
