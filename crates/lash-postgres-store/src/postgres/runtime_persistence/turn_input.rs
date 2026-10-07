use super::*;

#[async_trait::async_trait]
impl lash_core_execution::TurnInputStore for PostgresStore {
    async fn enqueue_pending_turn_inputs(
        &self,
        batch: lash_core_execution::PendingTurnInputBatch,
    ) -> Result<Vec<lash_core_execution::PendingTurnInput>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let now = self.clock.timestamp_ms();
        let admitted = enqueue_pending_turn_inputs_tx(&mut tx, &batch, now).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(admitted)
    }

    /// This backend does not fold the follow-ups (FIG-3975): the probe, the
    /// enqueue and the head read stay separate round-trips.
    async fn admit_pending_turn_inputs(
        &self,
        batch: lash_core_execution::PendingTurnInputBatch,
    ) -> Result<lash_core_execution::TurnInputAdmission, StoreError> {
        self.read_session_state_version(batch.session_id()).await?;
        self.enqueue_pending_turn_inputs(batch)
            .await
            .map(lash_core_execution::TurnInputAdmission::Enqueued)
    }

    async fn load_run_spec(
        &self,
        session_id: &SessionId,
        hash: &lash_core_execution::RunSpecHash,
    ) -> Result<Option<lash_core_execution::RunSpec>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let stored: Option<String> = sqlx::query_scalar(
            crate::turn_ingress::turn_ingress_sql()
                .run_specs
                .select_spec
                .sql(),
        )
        .bind(session_id.as_str())
        .bind(hash.as_str())
        .fetch_optional(&mut *connection)
        .await
        .map_err(store_sqlx_error)?;
        stored
            .map(|json| {
                lash_core_execution::RunSpec::from_canonical_json(&json).map_err(|error| {
                    StoreError::StoredDataCorrupt {
                        record_kind: "RunSpec",
                        message: error.to_string(),
                    }
                })
            })
            .transpose()
    }

    async fn list_pending_turn_inputs(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core_execution::PendingTurnInputRead>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        // Open and admitted rows, and the rows a checkpoint accepted into a
        // running run, read in one snapshot and listed in `enqueue_seq`
        // order (FIG-4044). The isolation level must precede every other
        // statement in the transaction.
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let statements = &crate::turn_ingress::turn_ingress_sql().pending_inputs;
        let mut inputs = Vec::new();
        for sql in [
            statements.list_undelivered.sql(),
            statements.list_accepted.sql(),
        ] {
            let rows = sqlx::query(sql)
                .bind(session_id.as_str())
                .fetch_all(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
            for row in rows {
                inputs.push(pending_turn_input_read_from_row(row)?);
            }
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        inputs.sort_by_key(|read| read.input.enqueue_seq);
        Ok(inputs)
    }

    async fn pending_turn_input(
        &self,
        session_id: &SessionId,
        input_id: &lash_core_execution::InputId,
    ) -> Result<Option<lash_core_execution::PendingTurnInputRead>, StoreError> {
        // One point read by primary key; the list's lifecycle filter is
        // applied to the one row here: a row is listed until it is completed
        // or cancelled, open or admitted to its run alike.
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let row = sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .pending_inputs
                .select_by_id
                .sql(),
        )
        .bind(session_id.as_str())
        .bind(input_id.as_str())
        .fetch_optional(&mut *connection)
        .await
        .map_err(store_sqlx_error)?;
        Ok(row
            .map(pending_turn_input_read_from_row)
            .transpose()?
            .filter(|read| !read.input.state.is_terminal()))
    }

    async fn list_turn_input_applications(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core_execution::TurnInputApplication>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let rows = sqlx::query(
            crate::session_sql::session_sql()
                .turn_commits
                .select_all_for_session
                .sql(),
        )
        .bind(session_id.as_str())
        .fetch_all(&mut *connection)
        .await
        .map_err(store_sqlx_error)?;
        let mut commits = Vec::with_capacity(rows.len());
        for row in rows {
            let turn_id = row.get::<String, _>(0);
            let result_json: String = row.get(1);
            let result = lash_core_execution::store::decode_runtime_commit_receipt_for_fleet(
                session_id,
                &turn_id,
                &result_json,
                self.fence.fleet(),
            )?;
            commits.push((
                result.head_revision,
                turn_id,
                result.turn_input_applications,
            ));
        }
        commits.sort_by(|left, right| (left.0, left.1.as_str()).cmp(&(right.0, right.1.as_str())));
        Ok(commits
            .into_iter()
            .flat_map(|(_, _, applications)| applications)
            .collect())
    }

    async fn cancel_pending_turn_inputs(
        &self,
        session_id: &SessionId,
        targets: &[lash_core_execution::PendingTurnInputCancelTarget],
    ) -> Result<Vec<lash_core_execution::PendingTurnInputCancelReceipt>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let targets = targets.to_vec();
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let mut covered = std::collections::BTreeSet::new();
        for target in &targets {
            if let Some(row) =
                load_pending_turn_input_row_by_target_tx(&mut tx, session_id, target, false).await?
            {
                covered.insert(lash_core_execution::InputId::parse(row.input_id)?);
            }
        }
        lock_cancel_rows_in_queue_order(&mut tx, session_id, CancelLockScope::Targets(&covered))
            .await?;
        let mut results = Vec::with_capacity(targets.len());
        for target in targets {
            let outcome =
                match load_pending_turn_input_row_by_target_tx(&mut tx, session_id, &target, true)
                    .await?
                {
                    Some(row) => cancel_pending_turn_input_row_tx(&mut tx, row, now).await?,
                    None => lash_core_execution::PendingTurnInputCancelOutcome::NotFound,
                };
            results.push(lash_core_execution::PendingTurnInputCancelReceipt { target, outcome });
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(results)
    }

    async fn cancel_pending_turn_input_suffix(
        &self,
        session_id: &SessionId,
        anchor: &lash_core_execution::PendingTurnInputCancelTarget,
    ) -> Result<lash_core_execution::PendingTurnInputSuffixCancelOutcome, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let anchor = anchor.clone();
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let Some(anchor_row) =
            load_pending_turn_input_row_by_target_tx(&mut tx, session_id, &anchor, false).await?
        else {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(
                lash_core_execution::PendingTurnInputSuffixCancelOutcome::AnchorNotFound { anchor },
            );
        };
        lock_cancel_rows_in_queue_order(
            &mut tx,
            session_id,
            CancelLockScope::Suffix(anchor_row.enqueue_seq),
        )
        .await?;
        let rows = sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .pending_inputs_postgres
                .select_suffix
                .sql(),
        )
        .bind(session_id.as_str())
        .bind(anchor_row.enqueue_seq as i64)
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .into_iter()
        .map(pending_turn_input_row)
        .collect::<Result<Vec<_>, StoreError>>()?;
        let mut outcomes = Vec::with_capacity(rows.len());
        for row in rows {
            outcomes.push(cancel_pending_turn_input_row_tx(&mut tx, row, now).await?);
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(lash_core_execution::PendingTurnInputSuffixCancelOutcome::Outcomes { anchor, outcomes })
    }
}

#[async_trait::async_trait]
impl lash_core_execution::QueuedWorkStore for PostgresStore {
    async fn enqueue_queued_work(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkBatch, StoreError> {
        self.enqueue_queued_work_pg(batch).await
    }

    async fn enqueue_queued_work_with_outcome(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkEnqueueOutcome, StoreError> {
        self.enqueue_queued_work_with_outcome_pg(batch).await
    }

    async fn open_session_command_run(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        super::open_session_command_run_postgres(self, session_id).await
    }

    async fn cancel_queued_work_batch(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<Option<QueuedWorkBatch>, StoreError> {
        self.cancel_queued_work_batch_pg(session_id, batch_id).await
    }

    async fn queued_work_batch_completion(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<Option<lash_core_execution::store::RuntimeCommitReceipt>, StoreError> {
        self.queued_work_batch_completion_pg(session_id, batch_id)
            .await
    }

    async fn pending_session_work_ordering(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_core_execution::store::PendingSessionWorkOrdering, StoreError> {
        self.pending_session_work_ordering_pg(session_id).await
    }

    async fn list_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        self.list_queued_work_pg(session_id).await
    }

    async fn list_open_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        self.list_open_queued_work_pg(session_id).await
    }
    async fn has_admissible_queued_work(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        lash_core_execution::store::validate_session_id(session_id)?;
        sqlx::query_scalar(
            crate::turn_ingress::turn_ingress_sql()
                .family
                .has_admissible_work
                .sql(),
        )
        .bind(session_id.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(store_sqlx_error)
    }
}

/// Admit every draft of `batch` in the caller's transaction `tx` at `now`
/// (FIG-3842): a draft a stored row already answers returns that row, and
/// every other draft is inserted in request order at the next positions of
/// the session's ingress sequence, with the session's wake. Any refusal
/// fails the whole batch; the caller's transaction rolls it back.
pub(crate) async fn enqueue_pending_turn_inputs_tx(
    tx: &mut crate::guarded_tx::GuardedTx<'_>,
    batch: &lash_core_execution::PendingTurnInputBatch,
    now: u64,
) -> Result<Vec<lash_core_execution::PendingTurnInput>, StoreError> {
    use lash_core_execution::store_backend_support as support;
    let session_id = batch.session_id();
    ensure_session_not_deleted_tx(tx, session_id).await?;
    ensure_session_not_closing_tx(tx, session_id).await?;
    for draft in batch.drafts() {
        support::validate_turn_input_source_key(draft)?;
    }
    // The session's write authority, held to the commit: every ingress
    // producer takes it before it allocates, so the absences read below
    // hold and the block allocated below is contiguous (FIG-3842).
    super::lock_session_history_mutation_tx(tx, session_id).await?;
    let ids = batch
        .drafts()
        .iter()
        .flat_map(|draft| draft.input.stored_attachment_ids())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let claim = lash_core_execution::ReferrerClaim::unguarded(
        lash_core_execution::ArtifactReferrer::Session(session_id.clone()),
    )
    .map_err(|error| error.into_store_error("pending input attachment referrer"))?;
    crate::artifact_store::lock_referrer_tx(tx, &claim.referrer())
        .await
        .map_err(store_sqlx_error)?;
    crate::attachments::acquire_attachment_refs_tx(tx, &claim, &ids, now).await?;
    let sql = crate::turn_ingress::turn_ingress_sql();
    let mut interned = std::collections::BTreeSet::new();
    let mut admitted = Vec::with_capacity(batch.drafts().len());
    for draft in batch.drafts() {
        let submission_digest = support::turn_input_submission_digest(draft)?;
        let by_source_key: Option<(String, String)> = match draft.source_key.as_deref() {
            Some(source_key) => sqlx::query_as(sql.pending_inputs.select_id_by_source_key.sql())
                .bind(session_id.as_str())
                .bind(source_key)
                .fetch_optional(&mut ***tx)
                .await
                .map_err(store_sqlx_error)?,
            None => None,
        };
        let by_input_id: Option<(String, String)> =
            match (&by_source_key, draft.input_id.as_deref()) {
                (None, Some(input_id)) => {
                    sqlx::query_as(sql.pending_inputs.select_session_by_input_id.sql())
                        .bind(input_id)
                        .fetch_optional(&mut ***tx)
                        .await
                        .map_err(store_sqlx_error)?
                }
                _ => None,
            };
        let input_id = match support::decide_turn_input_draft_admission(
            draft,
            &submission_digest,
            by_source_key,
            by_input_id,
        )? {
            support::TurnInputDraftAdmission::Existing { input_id } => input_id,
            support::TurnInputDraftAdmission::New => {
                if let Some(turn_id) = draft.ingress.active_turn_id() {
                    let evidence = turn_address_evidence_tx(tx, session_id, turn_id).await?;
                    support::require_known_turn_address(session_id, turn_id, evidence)?;
                }
                let enqueue_seq = super::allocate_ingress_sequence_tx(tx, session_id).await?;
                let input_id = match draft.input_id.clone() {
                    Some(input_id) => input_id,
                    None => support::derive_pending_turn_input_id(
                        session_id,
                        draft.source_key.as_deref(),
                        now,
                        u64_from_sql("PendingTurnInput", "enqueue_seq", enqueue_seq)?,
                    ),
                };
                let state = lash_core_execution::TurnInputState::open(draft.ingress.clone());
                let run_spec = admit_run_spec_tx(tx, draft, &mut interned).await?;
                sqlx::query(sql.pending_inputs.insert_new.sql())
                    .bind(enqueue_seq)
                    .bind(input_id.as_str())
                    .bind(session_id.as_str())
                    .bind(&draft.source_key)
                    .bind(encode_json(&draft.ingress)?)
                    .bind(state.as_str())
                    .bind(encode_json(&draft.input)?)
                    .bind(&submission_digest)
                    .bind(now as i64)
                    .bind(run_spec.column())
                    .bind(
                        lash_core_execution::store_backend_support::encode_trace_cause(
                            &draft.trace_cause,
                        )?,
                    )
                    .execute(&mut ***tx)
                    .await
                    .map_err(|err| pending_turn_input_insert_error(err, session_id, &input_id))?;
                input_id
            }
        };
        if draft.pin {
            // The pin is written with the acceptance, new or replayed,
            // so the input is pinned before its run can start.
            crate::revisions::pin_tx(
                tx,
                session_id,
                &lash_core_execution::Target::Input(input_id.clone()),
            )
            .await?;
        }
        admitted.push(
            load_pending_turn_input(tx, session_id, &input_id)
                .await?
                .ok_or_else(|| {
                    StoreError::Backend("admitted pending turn input disappeared".to_string())
                })?,
        );
    }
    // The rows and the session's wake commit together (ADR 0132 §12): no
    // crash between them can leave input that nothing will admit.
    crate::durable::wake_session_tx(&mut *tx, session_id, false, now).await?;
    Ok(admitted)
}

/// Admit `draft`'s run spec inside its enqueue transaction (FIG-3838): refuse
/// a steering spec that differs from its running turn's, then intern a
/// non-default spec once per hash and refuse different bytes under an
/// interned hash. Both refusals roll the whole admission back. A hash this
/// batch's transaction already interned (`interned`) is not interned again.
///
/// The session lock the enqueue holds serializes concurrent interns of one
/// session, and a spec row is never rewritten, so the read-back sees the
/// bytes this or an earlier admission interned.
async fn admit_run_spec_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    draft: &lash_core_execution::PendingTurnInputDraft,
    interned: &mut std::collections::BTreeSet<String>,
) -> Result<lash_core_execution::store_backend_support::RunSpecAdmission, StoreError> {
    use lash_core_execution::store_backend_support as support;
    let spec = support::RunSpecAdmission::of(draft)?;
    let sql = crate::turn_ingress::turn_ingress_sql();
    if let Some(turn_id) = support::steering_run_spec_target(draft) {
        let addressed = sqlx::query(sql.pending_inputs.select_run_spec_by_source_key.sql())
            .bind(draft.session_id.as_str())
            .bind(turn_id.as_str())
            .fetch_optional(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .map(|row| {
                (
                    row.get::<String, _>("state"),
                    row.get::<Option<String>, _>("run_spec_hash"),
                )
            });
        match addressed {
            Some((state, hash)) => support::check_steering_run_spec(
                &draft.session_id,
                turn_id,
                &spec,
                Some((state.as_str(), hash.as_deref())),
            )?,
            // No input started `turn_id` under a source key: the addressed
            // turn may still be a running run of another kind (FIG-3877).
            None => check_unsourced_steering_run_spec_tx(tx, draft, turn_id, &spec).await?,
        }
    }
    if let Some((hash, canonical)) = spec.interned()
        && !interned.contains(hash)
    {
        sqlx::query(sql.run_specs.intern.sql())
            .bind(draft.session_id.as_str())
            .bind(hash)
            .bind(canonical)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        let stored: String = sqlx::query_scalar(sql.run_specs.select_spec.sql())
            .bind(draft.session_id.as_str())
            .bind(hash)
            .fetch_one(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        spec.check_interned(&draft.session_id, &stored)?;
        interned.insert(hash.to_string());
    }
    Ok(spec)
}

/// The steering verdict over the run kinds no `source_key`-filed input
/// starts (FIG-3877), read inside the admission transaction:
///
/// * `turn_id` is a physical turn of the unfinished queued-headed run: it
///   started from no input, so it runs the default spec.
/// * Otherwise nothing running names `turn_id`: the steering input is a
///   next-turn run under its own spec.
async fn check_unsourced_steering_run_spec_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    draft: &lash_core_execution::PendingTurnInputDraft,
    turn_id: &lash_core_execution::TurnId,
    spec: &lash_core_execution::store_backend_support::RunSpecAdmission,
) -> Result<(), StoreError> {
    if let Some(unfinished) =
        crate::session_runs::unfinished_run_conn(tx, &draft.session_id).await?
        && matches!(
            unfinished.head,
            lash_core_execution::store::AdmittedHead::Batch(_)
        )
        && lash_core_execution::store::PhysicalTurn::split_turn_id(turn_id).0 == unfinished.run
    {
        // A queued-headed run starts from no input, so it runs the default
        // spec.
        lash_core_execution::store_backend_support::check_running_run_spec(
            &draft.session_id,
            turn_id,
            spec,
            None,
        )?;
    }
    Ok(())
}

/// What session `session_id` records about turn `turn_id`, read under the
/// session's write authority the admitting transaction holds (ADR 0101
/// §5.1).
async fn turn_address_evidence_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    turn_id: &lash_core_execution::TurnId,
) -> Result<lash_core_execution::store_backend_support::TurnAddressEvidence, StoreError> {
    let run = lash_core_execution::store::PhysicalTurn::split_turn_id(turn_id).0;
    let ended: bool = sqlx::query_scalar(
        crate::turn_ingress::turn_ingress_sql()
            .family
            .turn_address_ended
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(turn_id.as_str())
    .bind(run.as_str())
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let running = crate::session_runs::unfinished_run_turns_conn(tx, session_id).await?;
    Ok(
        lash_core_execution::store_backend_support::turn_address_evidence(
            turn_id,
            running.as_ref(),
            ended,
        ),
    )
}
