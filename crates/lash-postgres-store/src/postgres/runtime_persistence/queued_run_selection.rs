use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn claim_selected_queued_work_postgres_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    session_execution_lease: &SessionExecutionLeaseAuthority,
    owner: &LeaseOwnerIdentity,
    boundary: QueuedWorkClaimBoundary,
    batch_ids: &[lash_core_execution::BatchId],
    policy: QueuedWorkClaimPolicy,
) -> Result<lash_core_execution::SelectedQueuedWorkClaimOutcome, StoreError> {
    ensure_session_execution_lease_tx(tx, session_id, session_execution_lease).await?;
    let generation = session_execution_lease.fencing_token;
    let now = postgres_transaction_epoch_ms(tx).await?;
    let requested_ids = batch_ids
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let sql_batch_ids = batch_ids
        .iter()
        .map(|id| id.as_str().to_string())
        .collect::<Vec<_>>();
    let sql = crate::turn_ingress::turn_ingress_sql();
    let present_ids =
        sqlx::query_scalar::<_, String>(sql.queued_batches_postgres.select_present_ids.sql())
            .bind(session_id.as_str())
            .bind(&sql_batch_ids)
            .fetch_all(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
    let already_satisfied_batch_ids = batch_ids
        .iter()
        .filter(|batch_id| !present_ids.contains(batch_id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if present_ids.is_empty() {
        return Ok(lash_core_execution::SelectedQueuedWorkClaimOutcome::new(
            None,
            already_satisfied_batch_ids,
        ));
    }
    let requested_rows = sqlx::query(sql.queued_batches_postgres.select_by_ids.sql())
        .bind(session_id.as_str())
        .bind(now as i64)
        .bind(sql_session_lease_generation(generation)?)
        .bind(&sql_batch_ids)
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .into_iter()
        .map(queued_batch_row)
        .collect::<Result<Vec<_>, _>>()?;
    if requested_rows.len() != present_ids.len() {
        return Ok(lash_core_execution::SelectedQueuedWorkClaimOutcome::new(
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
        validation_rows.extend(
            sqlx::query(sql.queued_batches_postgres.select_by_claim_ids.sql())
                .bind(session_id.as_str())
                .bind(now as i64)
                .bind(sql_session_lease_generation(generation)?)
                .bind(&involved_claim_ids)
                .fetch_all(&mut **tx)
                .await
                .map_err(store_sqlx_error)?
                .into_iter()
                .map(queued_batch_row)
                .collect::<Result<Vec<_>, _>>()?,
        );
        validation_rows.sort_by_key(|row| row.enqueue_seq);
        validation_rows.dedup_by(|left, right| left.batch_id == right.batch_id);
    }
    let validation_batch_claims = validation_rows
        .iter()
        .map(|row| {
            (
                lash_core_execution::BatchId::from(row.batch_id.clone()),
                row.claim_id.clone(),
            )
        })
        .collect::<Vec<_>>();
    let interrupted_positions =
        lash_core_execution::store::queued_work::select_interrupted_exact_claim_indices(
            &validation_batch_claims,
            batch_ids,
        )
        .map_err(|required_batch_ids| {
            StoreError::SelectedQueuedWorkRequiresInterruptedComposition {
                required_batch_ids: required_batch_ids
                    .into_iter()
                    .map(lash_core_execution::BatchId::into_inner)
                    .collect(),
            }
        })?;
    let (selected, mut selected_batches) = if let Some(interrupted_positions) =
        interrupted_positions
    {
        let selected = interrupted_positions
            .into_iter()
            .map(|position| validation_rows[position].clone())
            .collect::<Vec<_>>();
        let mut selected_batches = Vec::with_capacity(selected.len());
        for row in &selected {
            selected_batches.push(queued_work_batch_from_row(tx, row.clone()).await?);
        }
        (selected, selected_batches)
    } else {
        let mut requested_batches = std::collections::BTreeMap::new();
        for row in &requested_rows {
            let batch = queued_work_batch_from_row(tx, row.clone()).await?;
            if batch.work_class() != lash_core_execution::store::QueuedWorkClass::TurnWork {
                return Ok(lash_core_execution::SelectedQueuedWorkClaimOutcome::new(
                    None,
                    already_satisfied_batch_ids,
                ));
            }
            requested_batches.insert(row.batch_id.clone(), batch);
        }
        let span_rows = sqlx::query(sql.queued_batches_postgres.select_span.sql())
                .bind(session_id.as_str())
                .bind(now as i64)
                .bind(sql_session_lease_generation(generation)?)
                .bind(requested_rows[0].enqueue_seq as i64)
                .bind({
                    #[expect(
                        clippy::expect_used,
                        reason = "`requested_rows[0]` on the line above already requires a non-empty slice"
                    )]
                    let last = requested_rows
                        .last()
                        .expect("requested rows exist")
                        .enqueue_seq as i64;
                    last
                })
                .fetch_all(&mut **tx)
                .await
                .map_err(store_sqlx_error)?
                .into_iter()
                .map(queued_batch_row)
                .collect::<Result<Vec<_>, _>>()?;
        let Some(first_position) = span_rows
            .iter()
            .position(|row| requested_ids.contains(row.batch_id.as_str()))
        else {
            return Ok(lash_core_execution::SelectedQueuedWorkClaimOutcome::new(
                None,
                already_satisfied_batch_ids,
            ));
        };
        let selected = span_rows[first_position..]
            .iter()
            .take_while(|row| requested_ids.contains(row.batch_id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        #[expect(
            clippy::expect_used,
            reason = "`selected` was filtered to ids in `requested_ids`, which are exactly the keys of `requested_batches`"
        )]
        let selected_batches = selected
            .iter()
            .map(|row| {
                requested_batches
                    .get(&row.batch_id)
                    .expect("contiguous exact row was validated")
                    .clone()
            })
            .collect::<Vec<_>>();
        (selected, selected_batches)
    };
    let candidates = selected
        .iter()
        .zip(selected_batches.iter())
        .map(|(row, batch)| claim_candidate_from_row(row, batch))
        .collect::<Vec<_>>();
    let selected_len =
        match select_exact_turn_work_claim_prefix(&candidates, boundary, &policy, now)? {
            TurnWorkClaimPrefix::Selected { len } => len,
            TurnWorkClaimPrefix::Refused { .. } => {
                return Ok(lash_core_execution::SelectedQueuedWorkClaimOutcome::new(
                    None,
                    already_satisfied_batch_ids,
                ));
            }
        };

    selected_batches.truncate(selected_len);
    match claim_queued_work_rows_postgres(
        tx,
        now,
        session_id,
        owner,
        generation,
        &selected[..selected_len],
        selected_batches,
        &candidates,
    )
    .await?
    {
        ClaimTransactionOutcome::Commit(claim) => {
            Ok(lash_core_execution::SelectedQueuedWorkClaimOutcome::new(
                claim,
                already_satisfied_batch_ids,
            ))
        }
        ClaimTransactionOutcome::Rollback(_) => {
            Ok(lash_core_execution::SelectedQueuedWorkClaimOutcome::new(
                None,
                already_satisfied_batch_ids,
            ))
        }
    }
}
