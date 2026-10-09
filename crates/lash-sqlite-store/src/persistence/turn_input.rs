use super::*;

#[async_trait::async_trait]
impl lash_core_execution::TurnInputStore for SqliteStore {
    async fn turn_input_submission_digest(
        &self,
        session_id: &SessionId,
        source_key: &str,
    ) -> Result<Option<String>, StoreError> {
        let session_id = session_id.clone();
        let source_key = source_key.to_string();
        self.conn
            .call(move |conn| {
                let outcome = conn
                    .query_row(
                        crate::turn_ingress::turn_ingress_sql()
                            .pending_inputs
                            .select_id_by_source_key
                            .sql(),
                        params![session_id.as_str(), source_key],
                        |row| row.get(1),
                    )
                    .optional()
                    .map_err(sqlite_error);
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn enqueue_pending_turn_inputs(
        &self,
        batch: lash_core_execution::PendingTurnInputBatch,
    ) -> Result<Vec<lash_core_execution::PendingTurnInput>, StoreError> {
        let drafts = batch.drafts().len() as u64;
        let first_nonce = self.commit_count.fetch_add(drafts, AtomicOrdering::Relaxed);
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome = enqueue_pending_turn_inputs_conn(tx, &batch, now, first_nonce);
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn admit_pending_turn_inputs(
        &self,
        batch: lash_core_execution::PendingTurnInputBatch,
    ) -> Result<lash_core_execution::TurnInputAdmission, StoreError> {
        let drafts = batch.drafts().len() as u64;
        let first_nonce = self.commit_count.fetch_add(drafts, AtomicOrdering::Relaxed);
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let fleet = tx.fleet();
                let outcome = admit_pending_turn_inputs_conn(tx, &batch, now, first_nonce, fleet);
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn load_run_spec(
        &self,
        session_id: &SessionId,
        hash: &lash_core_execution::RunSpecHash,
    ) -> Result<Option<lash_core_execution::RunSpec>, StoreError> {
        let session_id = session_id.clone();
        let hash = hash.clone();
        self.conn
            .call(move |conn| {
                let outcome = (|| {
                    let stored: Option<String> = conn
                        .query_row(
                            crate::turn_ingress::turn_ingress_sql()
                                .run_specs
                                .select_spec
                                .sql(),
                            params![session_id.as_str(), hash.as_str()],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(sqlite_error)?;
                    stored
                        .map(|json| {
                            lash_core_execution::RunSpec::from_canonical_json(&json).map_err(
                                |error| StoreError::StoredDataCorrupt {
                                    record_kind: "RunSpec",
                                    message: error.to_string(),
                                },
                            )
                        })
                        .transpose()
                })();
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn list_pending_turn_inputs(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core_execution::PendingTurnInputRead>, StoreError> {
        let session_id = SessionId::parse(session_id.to_string())?;
        // Open and admitted rows, and the rows a checkpoint accepted into a
        // running run, read in one snapshot and listed in `enqueue_seq`
        // order (FIG-4044).
        self.conn
            .read(move |tx| {
                let outcome: Result<Vec<lash_core_execution::PendingTurnInputRead>, StoreError> =
                    (|| {
                        let statements = &crate::turn_ingress::turn_ingress_sql().pending_inputs;
                        let mut rows = Vec::new();
                        for sql in [
                            statements.list_undelivered.sql(),
                            statements.list_accepted.sql(),
                        ] {
                            let mut stmt = tx.prepare(sql).map_err(sqlite_error)?;
                            let listed = stmt
                                .query_map(
                                    params![session_id.as_str()],
                                    pending_turn_input_row_from_sql,
                                )
                                .map_err(sqlite_error)?;
                            rows.extend(
                                listed
                                    .collect::<Result<Vec<_>, _>>()
                                    .map_err(sqlite_error)?,
                            );
                        }
                        let mut reads = rows
                            .into_iter()
                            .map(pending_turn_input_read_from_row)
                            .collect::<Result<Vec<_>, StoreError>>()?;
                        reads.sort_by_key(|read| read.input.enqueue_seq);
                        Ok(reads)
                    })();
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn pending_turn_input(
        &self,
        session_id: &SessionId,
        input_id: &lash_core_execution::InputId,
    ) -> Result<Option<lash_core_execution::PendingTurnInputRead>, StoreError> {
        let session_id = session_id.clone();
        let input_id = input_id.clone();
        self.conn
            .call(move |conn| {
                // One point read by primary key; the list's lifecycle filter
                // is applied to the one row here: a row is listed until it is
                // completed or cancelled, open or admitted to its run alike.
                let outcome = (|| {
                    let row = conn
                        .prepare_cached(
                            crate::turn_ingress::turn_ingress_sql()
                                .pending_inputs
                                .select_by_id
                                .sql(),
                        )
                        .and_then(|mut stmt| {
                            stmt.query_row(
                                params![session_id.as_str(), input_id.as_str()],
                                pending_turn_input_row_from_sql,
                            )
                            .optional()
                        })
                        .map_err(sqlite_error)?;
                    Ok(row
                        .map(pending_turn_input_read_from_row)
                        .transpose()?
                        .filter(|read| !read.input.state.is_terminal()))
                })();
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn list_turn_input_applications(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core_execution::TurnInputApplication>, StoreError> {
        let session_id = SessionId::parse(session_id.to_string())?;
        let fleet = self.fleet_format();
        self.conn
            .call(move |conn| {
                let outcome = (|| {
                    let mut stmt = conn
                        .prepare(
                            crate::session_sql::session_sql()
                                .turn_commits
                                .select_all_for_session
                                .sql(),
                        )
                        .map_err(sqlite_error)?;
                    let rows = stmt
                        .query_map(params![session_id.as_str()], |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, Option<String>>(2)?,
                            ))
                        })
                        .map_err(sqlite_error)?;
                    let mut commits = Vec::new();
                    for row in rows {
                        let (turn_id, result_json, outcome_code) = row.map_err(sqlite_error)?;
                        let result =
                            lash_core_execution::store::decode_runtime_commit_receipt_for_fleet(
                                &session_id,
                                &turn_id,
                                &result_json,
                                fleet,
                            )?;
                        lash_core_execution::store::validate_turn_commit_outcome_code(
                            &result,
                            outcome_code.as_deref(),
                        )?;
                        commits.push((
                            result.head_revision,
                            turn_id,
                            result.turn_input_applications,
                        ));
                    }
                    commits.sort_by(|left, right| {
                        (left.0, left.1.as_str()).cmp(&(right.0, right.1.as_str()))
                    });
                    Ok(commits
                        .into_iter()
                        .flat_map(|(_, _, applications)| applications)
                        .collect())
                })();
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn cancel_pending_turn_inputs(
        &self,
        session_id: &SessionId,
        targets: &[lash_core_execution::PendingTurnInputCancelTarget],
    ) -> Result<Vec<lash_core_execution::PendingTurnInputCancelReceipt>, StoreError> {
        let session_id = SessionId::parse(session_id.to_string())?;
        let targets = targets.to_vec();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<
                    Vec<lash_core_execution::PendingTurnInputCancelReceipt>,
                    StoreError,
                > = (|| {
                    let mut results = Vec::with_capacity(targets.len());
                    for target in targets {
                        let outcome = match load_pending_turn_input_row_by_target_conn(
                            tx,
                            &session_id,
                            &target,
                        )? {
                            Some(row) => cancel_pending_turn_input_row_conn(tx, row, now)?,
                            None => lash_core_execution::PendingTurnInputCancelOutcome::NotFound,
                        };
                        results.push(lash_core_execution::PendingTurnInputCancelReceipt {
                            target,
                            outcome,
                        });
                    }
                    Ok(results)
                })();
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn cancel_pending_turn_input_suffix(
        &self,
        session_id: &SessionId,
        anchor: &lash_core_execution::PendingTurnInputCancelTarget,
    ) -> Result<lash_core_execution::PendingTurnInputSuffixCancelOutcome, StoreError> {
        let session_id = SessionId::parse(session_id.to_string())?;
        let anchor = anchor.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<lash_core_execution::PendingTurnInputSuffixCancelOutcome, StoreError> =
                    (|| {
                        let Some(anchor_row) =
                            load_pending_turn_input_row_by_target_conn(tx, &session_id, &anchor)?
                        else {
                            return Ok(
                                lash_core_execution::PendingTurnInputSuffixCancelOutcome::AnchorNotFound {
                                    anchor,
                                },
                            );
                        };
                        let rows = {
                            let mut stmt = tx
                                .prepare(
                                    crate::turn_ingress::turn_ingress_sql()
                                        .pending_inputs_sqlite
                                        .select_suffix
                                        .sql(),
                                )
                                .map_err(sqlite_error)?;
                            let rows = stmt
                                .query_map(
                                    params![session_id.as_str(), anchor_row.enqueue_seq as i64],
                                    pending_turn_input_row_from_sql,
                                )
                                .map_err(sqlite_error)?;
                            rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
                        };
                        let mut outcomes = Vec::with_capacity(rows.len());
                        for row in rows {
                            outcomes.push(cancel_pending_turn_input_row_conn(tx, row, now)?);
                        }
                        Ok(lash_core_execution::PendingTurnInputSuffixCancelOutcome::Outcomes {
                            anchor,
                            outcomes,
                        })
                    })();
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }
}

#[async_trait::async_trait]
impl lash_core_execution::QueuedWorkStore for SqliteStore {
    async fn enqueue_queued_work(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkBatch, StoreError> {
        self.enqueue_queued_work_sqlite(batch).await
    }

    async fn enqueue_queued_work_with_outcome(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkEnqueueOutcome, StoreError> {
        self.enqueue_queued_work_with_outcome_sqlite(batch).await
    }

    async fn open_session_command_run(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        super::open_session_command_run_sqlite(self, session_id).await
    }

    async fn cancel_queued_work_batch(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<Option<QueuedWorkBatch>, StoreError> {
        self.cancel_queued_work_batch_sqlite(session_id, batch_id)
            .await
    }

    async fn queued_work_batch_completion(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<Option<lash_core_execution::store::RuntimeCommitReceipt>, StoreError> {
        self.queued_work_batch_completion_sqlite(session_id, batch_id)
            .await
    }

    async fn pending_session_work_ordering(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_core_execution::store::PendingSessionWorkOrdering, StoreError> {
        self.pending_session_work_ordering_sqlite(session_id).await
    }

    async fn list_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        self.list_queued_work_sqlite(session_id).await
    }

    async fn list_open_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        self.list_open_queued_work_sqlite(session_id).await
    }

    async fn has_admissible_queued_work(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        lash_core_execution::store::validate_session_id(session_id)?;
        let session_id = session_id.clone();
        self.read_connection()
            .call(move |conn| {
                conn.query_row(
                    crate::turn_ingress::turn_ingress_sql()
                        .family
                        .has_admissible_work
                        .sql(),
                    params![session_id.as_str()],
                    |row| row.get(0),
                )
            })
            .await
            .map_err(sqlite_error)
    }
}

/// Admit every draft of `batch` under the database write lock the caller's
/// transaction holds (FIG-3842): a draft a stored row already answers returns
/// that row, and every other draft is inserted in request order at the next
/// positions of the session's ingress sequence. Any refusal rolls the whole
/// batch back.
pub(crate) fn enqueue_pending_turn_inputs_conn(
    tx: &Connection,
    batch: &lash_core_execution::PendingTurnInputBatch,
    now: u64,
    first_nonce: u64,
) -> Result<Vec<lash_core_execution::PendingTurnInput>, StoreError> {
    use lash_core_execution::store_backend_support as support;
    let session_id = batch.session_id();
    ensure_session_not_deleted_conn(tx, session_id)?;
    for draft in batch.drafts() {
        support::validate_turn_input_source_key(draft)?;
    }
    let sql = crate::turn_ingress::turn_ingress_sql();
    let mut interned = std::collections::BTreeSet::new();
    let mut admitted = Vec::with_capacity(batch.drafts().len());
    for (nonce, draft) in (first_nonce..).zip(batch.drafts()) {
        let submission_digest = support::turn_input_submission_digest(draft)?;
        let by_source_key = match draft.source_key.as_deref() {
            Some(source_key) => tx
                .query_row(
                    sql.pending_inputs.select_id_by_source_key.sql(),
                    params![session_id.as_str(), source_key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(sqlite_error)?,
            None => None,
        };
        let by_input_id = match (&by_source_key, draft.input_id.as_deref()) {
            (None, Some(input_id)) => tx
                .query_row(
                    sql.pending_inputs.select_session_by_input_id.sql(),
                    params![input_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(sqlite_error)?,
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
                ensure_session_not_closing_conn(tx, session_id)?;
                let ids = draft.input.stored_attachment_ids();
                let claim = lash_core_execution::ReferrerClaim::unguarded(
                    lash_core_execution::ArtifactReferrer::Session(session_id.clone()),
                )
                .map_err(|error| error.into_store_error("pending input attachment referrer"))?;
                crate::attachments::acquire_attachment_refs_conn(tx, &claim, &ids, now)?;
                if let Some(turn_id) = draft.ingress.active_turn_id() {
                    support::require_known_turn_address(
                        session_id,
                        turn_id,
                        turn_address_evidence_conn(tx, session_id, turn_id)?,
                    )?;
                }
                let input_id = draft.input_id.clone().unwrap_or_else(|| {
                    support::derive_pending_turn_input_id(
                        session_id,
                        draft.source_key.as_deref(),
                        now,
                        nonce,
                    )
                });
                let state = lash_core_execution::TurnInputState::open(draft.ingress.clone());
                let run_spec = admit_run_spec_conn(tx, draft, &mut interned)?;
                crate::conn::cached_execute(
                    tx,
                    sql.pending_inputs.insert_new.sql(),
                    params![
                        crate::session_ingress::allocate_sequence(tx, session_id)?,
                        input_id.as_str(),
                        session_id.as_str(),
                        draft.source_key.as_deref(),
                        encode_json(&draft.ingress)?,
                        state.as_str(),
                        encode_json(&draft.input)?,
                        submission_digest.as_str(),
                        now as i64,
                        run_spec.column(),
                        lash_core_execution::store_backend_support::encode_trace_cause(
                            &draft.trace_cause,
                        )?,
                    ],
                )
                .map_err(|err| {
                    crate::sqlite_pending_turn_input_insert_error(err, session_id, &input_id)
                })?;
                input_id
            }
        };
        if draft.pin {
            // The pin is written with the acceptance, new or replayed, so
            // the input is pinned before its run can start.
            crate::revisions::pin_conn(
                tx,
                session_id,
                &lash_core_execution::Target::Input(input_id.clone()),
            )?;
        }
        admitted.push(
            load_pending_turn_input_by_id_conn(tx, session_id, &input_id)?.ok_or_else(|| {
                StoreError::Backend("admitted pending turn input disappeared".to_string())
            })?,
        );
    }
    // The rows and the session's wake commit together (ADR 0132 §12): no
    // crash between them can leave input that nothing will admit.
    crate::durable::wake_session_tx(tx, session_id, false, now)?;
    Ok(admitted)
}

/// What session `session_id` records about turn `turn_id`, read under the
/// write lock the admitting transaction holds (ADR 0101 §5.1).
fn turn_address_evidence_conn(
    tx: &Connection,
    session_id: &SessionId,
    turn_id: &lash_core_execution::TurnId,
) -> Result<lash_core_execution::store_backend_support::TurnAddressEvidence, StoreError> {
    let run = lash_core_execution::store::PhysicalTurn::split_turn_id(turn_id).0;
    let ended: bool = tx
        .query_row(
            crate::turn_ingress::turn_ingress_sql()
                .family
                .turn_address_ended
                .sql(),
            params![session_id.as_str(), turn_id.as_str(), run.as_str()],
            |row| row.get(0),
        )
        .map_err(sqlite_error)?;
    let running = crate::session_runs::unfinished_run_turns_conn(tx, session_id)?;
    Ok(
        lash_core_execution::store_backend_support::turn_address_evidence(
            turn_id,
            running.as_ref(),
            ended,
        ),
    )
}

/// The fused admission (FIG-3975): inside the one enqueue transaction, answer
/// the caller's session state-version probe and read the head the queue
/// event publishes against. A failure anywhere rolls the whole admission
/// back, as the separate calls would have.
fn admit_pending_turn_inputs_conn(
    tx: &Connection,
    batch: &lash_core_execution::PendingTurnInputBatch,
    now: u64,
    first_nonce: u64,
    fleet: lash_core_execution::FleetFormat,
) -> Result<lash_core_execution::TurnInputAdmission, StoreError> {
    let session_id = batch.session_id();
    read_session_state_version_conn(tx, session_id, fleet)?;
    let rows = enqueue_pending_turn_inputs_conn(tx, batch, now, first_nonce)?;
    let committed_head = try_load_session_head_meta_from_conn(tx, session_id, fleet)?;
    Ok(lash_core_execution::TurnInputAdmission::Fused {
        rows,
        committed_head,
    })
}

/// Admit `draft`'s run spec inside its enqueue transaction (FIG-3838): refuse
/// a steering spec that differs from its running turn's, then intern a
/// non-default spec once per hash and refuse different bytes under an
/// interned hash. Both refusals roll the whole admission back. A hash this
/// batch's transaction already interned (`interned`) is not interned again.
fn admit_run_spec_conn(
    tx: &Connection,
    draft: &lash_core_execution::PendingTurnInputDraft,
    interned: &mut std::collections::BTreeSet<String>,
) -> Result<lash_core_execution::store_backend_support::RunSpecAdmission, StoreError> {
    use lash_core_execution::store_backend_support as support;
    let spec = support::RunSpecAdmission::of(draft)?;
    let sql = crate::turn_ingress::turn_ingress_sql();
    if let Some(turn_id) = support::steering_run_spec_target(draft) {
        let addressed: Option<(String, Option<String>)> = tx
            .query_row(
                sql.pending_inputs.select_run_spec_by_source_key.sql(),
                params![draft.session_id.as_str(), turn_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sqlite_error)?;
        match addressed {
            Some((state, hash)) => support::check_steering_run_spec(
                &draft.session_id,
                turn_id,
                &spec,
                Some((state.as_str(), hash.as_deref())),
            )?,
            // No input started `turn_id` under a source key: the addressed
            // turn may still be a running run of another kind (FIG-3877).
            None => check_unsourced_steering_run_spec_conn(tx, draft, turn_id, &spec)?,
        }
    }
    if let Some((hash, canonical)) = spec.interned()
        && !interned.contains(hash)
    {
        crate::conn::cached_execute(
            tx,
            sql.run_specs.intern.sql(),
            params![draft.session_id.as_str(), hash, canonical],
        )
        .map_err(sqlite_error)?;
        let stored: String = tx
            .query_row(
                sql.run_specs.select_spec.sql(),
                params![draft.session_id.as_str(), hash],
                |row| row.get(0),
            )
            .map_err(sqlite_error)?;
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
fn check_unsourced_steering_run_spec_conn(
    tx: &Connection,
    draft: &lash_core_execution::PendingTurnInputDraft,
    turn_id: &lash_core_execution::TurnId,
    spec: &lash_core_execution::store_backend_support::RunSpecAdmission,
) -> Result<(), StoreError> {
    if let Some(unfinished) = crate::session_runs::unfinished_run_conn(tx, &draft.session_id)?
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
