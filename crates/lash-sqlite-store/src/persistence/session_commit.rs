use super::*;
use crate::session_sql::session_sql;

#[async_trait::async_trait]
impl SessionCommitStore for Store {
    async fn committed_turn_exists(
        &self,
        turn_id: &lash_core_execution::TurnId,
    ) -> Result<bool, StoreError> {
        let Some(session_id) = self.resolve_session_id_for_read().await? else {
            return Ok(false);
        };
        let key = lash_core_execution::store_backend_support::turn_commit_receipt_storage_key(
            &session_id,
            turn_id,
        )?;
        self.conn
            .call(move |conn| {
                let exists: bool = conn.query_row(
                    session_sql().turn_commits.exists_for_turn.sql(),
                    params![session_id.as_str(), key],
                    |row| row.get(0),
                )?;
                Ok(exists)
            })
            .await
            .map_err(sqlite_error)
    }

    async fn drain_end_exists(&self, drain_id: &str) -> Result<bool, StoreError> {
        let Some(session_id) = self.resolve_session_id_for_read().await? else {
            return Ok(false);
        };
        let key = lash_core_execution::store_backend_support::drain_end_receipt_storage_key(
            &session_id,
            drain_id,
        )?;
        self.conn
            .call(move |conn| {
                let exists: bool = conn.query_row(
                    session_sql().turn_commits.exists_for_turn.sql(),
                    params![session_id.as_str(), key],
                    |row| row.get(0),
                )?;
                Ok(exists)
            })
            .await
            .map_err(sqlite_error)
    }

    async fn read_session_state_version(&self) -> Result<u32, StoreError> {
        let Some(session_id) = self.resolve_session_id_for_read().await? else {
            return Ok(lash_core_execution::store::OLDEST_SUPPORTED_SESSION_STATE_VERSION);
        };
        self.conn
            .call(move |conn| Ok(read_session_state_version_conn(conn, &session_id)))
            .await
            .map_err(sqlite_error)?
    }

    async fn admit_session_state(
        &self,
        lease: &SessionExecutionLeaseAuthority,
    ) -> Result<lash_core_execution::store::SessionStateAdmission, StoreError> {
        let lease = lease.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome = (|| {
                    ensure_session_execution_lease_conn(tx, &lease.session_id, &lease, now)?;
                    let version = read_session_state_version_conn(tx, &lease.session_id)?;
                    Ok(lash_core_execution::store::SessionStateAdmission {
                        session_id: lease.session_id.clone(),
                        version,
                        lease_fencing_token: lease.fencing_token,
                    })
                })();
                Ok(match outcome {
                    Ok(admission) => TxOutcome::Commit(Ok(admission)),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn load_session(&self) -> Result<Option<PersistedSessionRead>, StoreError> {
        self.load_session_read(None).await
    }

    async fn load_session_at(
        &self,
        base: &lash_core_execution::store::SessionHeadRef,
    ) -> Result<PersistedSessionRead, StoreError> {
        self.load_session_read(Some(base.clone()))
            .await?
            .ok_or(StoreError::TurnBaseNotRetained {
                revision: base.revision,
            })
    }

    async fn retain_admission_base(
        &self,
        lease: &SessionExecutionLeaseAuthority,
        base: &lash_core_execution::store::SessionHeadRef,
    ) -> Result<(), StoreError> {
        let lease = lease.clone();
        let checkpoint_ref = base.checkpoint.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome = (|| {
                    ensure_session_execution_lease_conn(tx, &lease.session_id, &lease, now)?;
                    crate::session_meta::retain_admission_base_conn(
                        tx,
                        &lease.session_id,
                        checkpoint_ref.as_ref(),
                    )
                })();
                Ok(match outcome {
                    Ok(()) => TxOutcome::Commit(Ok(())),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn raise_pending_follow_on_attempts(
        &self,
        lease: &SessionExecutionLeaseAuthority,
        follow_on_turn_id: &lash_core_execution::TurnId,
    ) -> Result<lash_core_execution::store::PendingFollowOn, StoreError> {
        let lease = lease.clone();
        let follow_on_turn_id = follow_on_turn_id.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome = super::claim_support::raise_pending_follow_on_conn(
                    tx,
                    &lease,
                    &follow_on_turn_id,
                    now,
                );
                Ok(match outcome {
                    Ok(raised) => TxOutcome::Commit(Ok(raised)),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn load_session_head_meta(&self) -> Result<Option<SessionHeadMeta>, StoreError> {
        self.read_session_state_version().await?;
        Store::load_session_head_meta(self).await
    }

    /// FIG-653: session-relative history reads enforce graph membership, not authorization.
    async fn load_node(
        &self,
        node_id: &str,
    ) -> Result<Option<lash_core_execution::SessionNodeRecord>, StoreError> {
        let session_id = self.selected_session_id()?;
        let node_id = node_id.to_string();
        let row: Option<(String, Option<String>, String)> = self
            .conn
            .call(move |conn| {
                let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
                let outcome = (|| {
                    let candidate = tx
                        .query_row(
                            session_sql().graph_sqlite.select_lookup.sql(),
                            params![node_id, session_id.as_str()],
                            |row| {
                                Ok((
                                    row.get::<_, String>(0)?,
                                    row.get::<_, Option<String>>(1)?,
                                    row.get::<_, String>(2)?,
                                    row.get::<_, String>(3)?,
                                    row.get::<_, i64>(4)?,
                                ))
                            },
                        )
                        .optional()?;
                    let Some((
                        candidate_id,
                        parent_node_id,
                        node_json,
                        owner,
                        candidate_generation,
                    )) = candidate
                    else {
                        return Ok(None);
                    };
                    if owner != session_id {
                        let mut stmt =
                            tx.prepare(session_sql().head.select_readable_range.sql())?;
                        let rows = stmt
                            .query_map(params![session_id.as_str(), candidate_generation], |row| {
                                Ok((
                                    row.get::<_, Option<String>>(0)?,
                                    row.get::<_, Option<i64>>(1)?,
                                    row.get::<_, Option<i64>>(2)?,
                                    row.get::<_, Option<String>>(3)?,
                                    row.get::<_, Option<String>>(4)?,
                                    row.get::<_, Option<i64>>(5)?,
                                    row.get::<_, Option<i64>>(6)?,
                                ))
                            })?
                            .collect::<Result<Vec<_>, _>>()?;
                        let Some(first) = rows.first() else {
                            return Ok(None);
                        };
                        let Some(head_id) = first.0.clone() else {
                            return Ok(None);
                        };
                        let (Some(head_generation), Some(head_tombstoned)) = (first.1, first.2)
                        else {
                            return Err(sqlite_conversion_error(stored_data_corrupt(
                                "SessionGraph",
                                "head leaf is missing",
                            )));
                        };
                        if head_tombstoned != 0 {
                            return Err(sqlite_conversion_error(stored_data_corrupt(
                                "SessionGraph",
                                "head leaf is tombstoned",
                            )));
                        }
                        if candidate_generation < 0 || candidate_generation > head_generation {
                            return Ok(None);
                        }
                        let mut range = std::collections::HashMap::new();
                        for row in rows {
                            if let (
                                Some(node_id),
                                parent_node_id,
                                Some(generation),
                                Some(tombstoned),
                            ) = (row.3, row.4, row.5, row.6)
                            {
                                range.insert(node_id, (parent_node_id, generation, tombstoned));
                            }
                        }
                        let mut current_id = head_id;
                        let mut current_generation = head_generation;
                        loop {
                            let Some((parent_id, generation, tombstoned)) = range.get(&current_id)
                            else {
                                return Err(sqlite_conversion_error(stored_data_corrupt(
                                    "SessionGraph",
                                    "readable generation range omits an edge-path node",
                                )));
                            };
                            if *tombstoned != 0 || *generation != current_generation {
                                return Err(sqlite_conversion_error(stored_data_corrupt(
                                    "SessionGraph",
                                    "parent edge crosses a tombstone or generation gap",
                                )));
                            }
                            if current_generation == candidate_generation {
                                if current_id != candidate_id {
                                    return Ok(None);
                                }
                                break;
                            }
                            current_id = parent_id.clone().ok_or_else(|| {
                                sqlite_conversion_error(stored_data_corrupt(
                                    "SessionGraph",
                                    "parent edge ended before the candidate generation",
                                ))
                            })?;
                            current_generation -= 1;
                        }
                    }
                    Ok(Some((candidate_id, parent_node_id, node_json)))
                })()?;
                tx.commit()?;
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?;
        row.map(|(node_id, parent_node_id, node_json)| {
            lash_core_execution::SessionNodeRecord::decode_storage_body(
                node_id,
                parent_node_id,
                &node_json,
            )
            .map_err(|error| stored_data_corrupt("SessionGraph node", error))
        })
        .transpose()
    }

    async fn record_turn_park(
        &self,
        park: &lash_core_execution::store::TurnParkWrite,
    ) -> Result<lash_core_execution::store::TurnPark, StoreError> {
        self.bind_session(&park.session_id)?;
        let session_id = park.session_id.clone();
        let turn_id = park.turn_id.as_str().to_string();
        let reason_code = park.reason.code().as_str().to_string();
        let reason_json = serde_json::to_string(&park.reason).map_err(|error| {
            StoreError::RecordEncodingFailed {
                record_kind: "TurnPark".to_string(),
                message: error.to_string(),
            }
        })?;
        let reason = park.reason.clone();
        let park_executable_generation = reason
            .retired_executable_generation_key()
            .map(str::to_string);
        let at_ms = i64::try_from(park.at_ms).map_err(|_| {
            StoreError::Backend(format!(
                "turn park instant {} exceeds the stored range",
                park.at_ms
            ))
        })?;
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<lash_core_execution::store::TurnPark, StoreError> = (|| {
                    ensure_session_not_deleted_conn(tx, &session_id)?;
                    let turn_parks = &crate::turn_ingress::turn_ingress_sql().turn_parks;
                    let existing: Option<(String, i64, i64, i64)> = tx
                        .query_row(
                            turn_parks.select_by_session.sql(),
                            params![session_id.as_str()],
                            |row| {
                                Ok((
                                    row.get::<_, String>(1)?,
                                    row.get::<_, i64>(2)?,
                                    row.get::<_, i64>(5)?,
                                    row.get::<_, i64>(7)?,
                                ))
                            },
                        )
                        .optional()
                        .map_err(sqlite_error)?;
                    match existing {
                        Some((stored_turn_id, park_id, since_ms, attempts))
                            if stored_turn_id == turn_id =>
                        {
                            // A same-turn re-park keeps `park_id` and
                            // `since_ms`, refreshes the reason and
                            // `last_refused_ms`, and counts the refusal — no
                            // feed event.
                            tx.execute(
                                turn_parks.update_same_turn.sql(),
                                params![
                                    session_id.as_str(),
                                    turn_id,
                                    reason_code,
                                    reason_json,
                                    at_ms,
                                    park_executable_generation
                                ],
                            )
                            .map_err(sqlite_error)?;
                            return Ok(lash_core_execution::store::TurnPark {
                                session_id: session_id.clone(),
                                turn_id: turn_id.clone().into(),
                                reason,
                                park_id: lash_core_execution::store::ParkId::from_feed_sequence(
                                    u64::try_from(park_id).unwrap_or_default(),
                                ),
                                since_ms: u64::try_from(since_ms).unwrap_or_default(),
                                last_refused_ms: u64::try_from(at_ms).unwrap_or_default(),
                                attempts: u32::try_from(attempts.saturating_add(1))
                                    .unwrap_or(u32::MAX),
                            });
                        }
                        Some((superseded_turn_id, superseded_park_id, ..)) => {
                            // A different turn's park supersedes the stored
                            // one: close it on the feed, then open the new
                            // park.
                            let deleted: Option<(String, i64)> = tx
                                .query_row(
                                    turn_parks.delete_for_supersede_returning.sql(),
                                    params![session_id.as_str(), turn_id],
                                    |row| Ok((row.get(0)?, row.get(1)?)),
                                )
                                .optional()
                                .map_err(sqlite_error)?;
                            if deleted.is_none() {
                                return Err(StoreError::Backend(format!(
                                    "turn park supersede of `{superseded_turn_id}` in session \
                                     `{session_id}` deleted no row"
                                )));
                            }
                            crate::persistence::turn_park_feed::log_turn_park_closed_conn(
                                tx,
                                &session_id,
                                &superseded_turn_id,
                                superseded_park_id,
                                &lash_core_execution::store::ParkEventKind::Unparked {
                                    cause: lash_core_execution::store::UnparkCause::Superseded,
                                },
                                at_ms,
                            )?;
                        }
                        None => {}
                    }
                    let park_id = crate::persistence::turn_park_feed::log_turn_parked_conn(
                        tx,
                        &session_id,
                        &turn_id,
                        &reason,
                        at_ms,
                    )?;
                    tx.execute(
                        turn_parks.insert.sql(),
                        params![
                            session_id.as_str(),
                            turn_id,
                            park_id,
                            reason_code,
                            reason_json,
                            at_ms,
                            at_ms,
                            1,
                            park_executable_generation
                        ],
                    )
                    .map_err(sqlite_error)?;
                    Ok(lash_core_execution::store::TurnPark {
                        session_id: session_id.clone(),
                        turn_id: turn_id.clone().into(),
                        reason,
                        park_id: lash_core_execution::store::ParkId::from_feed_sequence(
                            u64::try_from(park_id).unwrap_or_default(),
                        ),
                        since_ms: u64::try_from(at_ms).unwrap_or_default(),
                        last_refused_ms: u64::try_from(at_ms).unwrap_or_default(),
                        attempts: 1,
                    })
                })(
                );
                Ok(match outcome {
                    Ok(park) => TxOutcome::Commit(Ok(park)),
                    Err(err) => TxOutcome::Rollback(Err(err)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn load_turn_park(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<lash_core_execution::store::TurnPark>, StoreError> {
        let session = session_id.as_str().to_string();
        let row: Option<(String, i64, String, String, i64, i64, i64)> = self
            .conn
            .call(move |conn| {
                conn.query_row(
                    crate::turn_ingress::turn_ingress_sql()
                        .turn_parks
                        .select_by_session
                        .sql(),
                    params![session],
                    |row| {
                        Ok((
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                        ))
                    },
                )
                .optional()
            })
            .await
            .map_err(sqlite_error)?;
        row.map(
            |(turn_id, park_id, reason_code, reason_json, since_ms, last_refused_ms, attempts)| {
                lash_core_execution::store::TurnPark::decode(
                    session_id.clone(),
                    turn_id.into(),
                    lash_core_execution::store::ParkId::from_feed_sequence(
                        u64::try_from(park_id).unwrap_or_default(),
                    ),
                    &reason_code,
                    &reason_json,
                    u64::try_from(since_ms).unwrap_or_default(),
                    u64::try_from(last_refused_ms).unwrap_or_default(),
                    u32::try_from(attempts).unwrap_or(u32::MAX),
                )
            },
        )
        .transpose()
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError> {
        let planner =
            lash_core_execution::store::RuntimeCommitPlanner::prepare(commit, self.fleet_format())?;
        self.bind_session(&planner.commit().session_id)?;
        let blob_profile = self.options.blob_profile;
        let now = self.clock.timestamp_ms();
        let planner_fleet_format = self.fleet_format();
        let result = self
            .conn
            .write_flow(move |tx| {
                let outcome: Result<RuntimeCommitReceipt, StoreError> = (|| {
                    let commit = planner.commit();
                    ensure_session_not_deleted_conn(tx, &commit.session_id)?;
                    super::session_ingress::require_commit_fences_conn(tx, commit, now)?;
                    let existing =
                        try_load_session_head_meta_from_conn(tx, &commit.session_id)?;
                    planner.validate_session_binding(
                        existing.as_ref().map(|meta| &meta.session_id),
                    )?;
                    crate::session_meta::write_session_meta(
                        tx,
                        &SessionMeta {
                            session_id: commit.session_id.clone(),
                            relation: lash_core_execution::SessionRelation::Root,
                            pending_observer_intents: Vec::new(),
                        },
                        crate::session_meta::SessionMetaWrite::Insert,
                        now,
                        planner_fleet_format,
                    )?;
                    planner.validate_node_derivation()?;
                    // A turn's commit settles its park (FIG-3586), in the
                    // commit's transaction; another turn's commit leaves it.
                    if let Some(turn_id) = commit.turn_commit.operation.turn_id() {
                        let released: Option<(String, i64)> = tx
                            .query_row(
                                crate::turn_ingress::turn_ingress_sql()
                                    .turn_parks
                                    .delete_for_turn_returning
                                    .sql(),
                                params![commit.session_id.as_str(), turn_id.as_str()],
                                |row| Ok((row.get(0)?, row.get(1)?)),
                            )
                            .optional()
                            .map_err(sqlite_error)?;
                        if let Some((released_turn_id, released_park_id)) = released {
                            crate::persistence::turn_park_feed::log_turn_park_closed_conn(
                                tx,
                                &commit.session_id,
                                &released_turn_id,
                                released_park_id,
                                &lash_core_execution::store::ParkEventKind::Unparked {
                                    cause: lash_core_execution::store::UnparkCause::TurnCommitted,
                                },
                                crate::clamp_epoch_ms(now),
                            )?;
                        }
                    }
                    {
                        let prior: Option<(
                            String,
                            String,
                            Option<String>,
                            Option<i64>,
                            Option<i64>,
                        )> = tx
                            .query_row(
                                session_sql().turn_commits.select_receipt.sql(),
                                params![commit.session_id.as_str(), planner.operation_key()],
                                |row| {
                                    Ok((
                                        row.get(0)?,
                                        row.get(1)?,
                                        row.get(2)?,
                                        row.get(3)?,
                                        row.get(4)?,
                                    ))
                                },
                            )
                            .optional()
                            .map_err(sqlite_error)?;
                        if let Some((
                            stored_hash,
                            result_json,
                            stored_identity,
                            stored_version,
                            stored_requested_node_count,
                        )) = prior
                        {
                            // One codec owns the unit-shape and integer-range checks.
                            // The ancestor stays write-only because the request hash binds it.
                            let append_request_identity =
                                lash_core_execution::store_backend_support::decode_append_request_identity(
                                    &commit.turn_commit.operation.key,
                                    stored_identity,
                                    stored_version,
                                    stored_requested_node_count,
                                )?;
                            let result = lash_core_execution::store::decode_runtime_commit_receipt(
                                &commit.session_id,
                                planner.operation_key(),
                                &result_json,
                            )?;
                            let prior = lash_core_execution::store::RuntimeCommitReceiptRecord {
                                turn_commit_hash: stored_hash,
                                result,
                                append_request_identity,
                            };
                            if let Some(replay) = planner.decide_receipt(Some(prior))? {
                                if let Some(completion) =
                                    replay.release_session_execution_lease()
                                {
                                    let _release_was_current =
                                        release_session_execution_lease_conn(tx, completion)?;
                                    // FIG-884: ancillary stale release must
                                    // never veto a replayed commit.
                                }
                                if let Some(settlement) = commit.turn_cancel_closure_settlement.as_ref()
                        && settlement.authorization().session_id() == commit.session_id
                        && commit.interrupted_turn_input_turn_id.as_ref() == Some(settlement.authorization().turn_id())
                        && commit.interrupted_turn_input_cancellation.as_ref() == settlement.effective_cancellation()
                    {
                                    let closure = settlement.authorization();
                                    tx.execute(crate::turn_ingress::turn_ingress_sql().closures.delete_settled.sql(),
                                        params![closure.session_id().as_str(), closure.turn_id().as_str(), encode_json(closure)?],
                                    ).map_err(sqlite_error)?;
                                }
                                return Ok(replay.into_result());
                            }
                        }
                    }
                    if commit.interrupted_turn_cancel_intent.is_some()
                        && commit.turn_cancel_closure_settlement.is_none()
                    {
                        return Err(StoreError::TurnCancelClosureAuthorizationMismatch {
                            session_id: commit.session_id.clone(),
                            turn_id: commit
                                .interrupted_turn_input_turn_id
                                .clone()
                                .unwrap_or_else(|| lash_core_execution::TurnId::from("missing-turn-id")),
                        });
                    }
                    if let Some(settlement) = commit.turn_cancel_closure_settlement.as_ref() {
                        let closure = settlement.authorization();
                        if commit.interrupted_turn_input_cancellation.as_ref()
                            != settlement.effective_cancellation()
                        {
                            return Err(StoreError::TurnCancelClosureAuthorizationMismatch {
                                session_id: commit.session_id.clone(),
                                turn_id: closure.turn_id().clone(),
                            });
                        }
                        if closure.session_id() != commit.session_id
                            || commit.interrupted_turn_input_turn_id.as_ref()
                                != Some(closure.turn_id())
                        {
                            return Err(StoreError::TurnCancelClosureAuthorizationMismatch {
                                session_id: commit.session_id.clone(),
                                turn_id: closure.turn_id().clone(),
                            });
                        }
                        if closure.admitted_scope().session_id().is_none() {
                            let scope_id = closure.admitted_scope().journal_identity()
                                .map_err(|error| StoreError::Backend(error.to_string()))?.key().to_string();
                            let retired = tx.query_row(
                                crate::turn_ingress::turn_ingress_sql()
                                    .retired_scopes
                                    .exists_for_scope
                                    .sql(),
                                params![scope_id], |row| row.get::<_, bool>(0),
                            ).map_err(sqlite_error)?;
                            if retired { return Err(StoreError::TurnCancelClosureScopeRetired { scope_id }); }
                        }
                        let final_key = lash_core_execution::OperationId::turn(closure.session_id(), closure.turn_id(), "final").storage_key()?;
                        let committed = tx.query_row(
                            session_sql().turn_commits.exists_for_turn.sql(),
                            params![closure.session_id().as_str(), final_key], |row| row.get::<_, bool>(0),
                        ).map_err(sqlite_error)?;
                        if committed {
                            return Err(StoreError::TurnCancelClosureAuthorizationMismatch {
                                session_id: closure.session_id().clone(), turn_id: closure.turn_id().clone(),
                            });
                        }
                        let stored = tx
                            .query_row(
                                crate::turn_ingress::turn_ingress_sql()
                                    .closures_sqlite
                                    .select_by_turn
                                    .sql(),
                                params![closure.session_id().as_str(), closure.turn_id().as_str()],
                                |row| row.get::<_, String>(0),
                            )
                            .optional()
                            .map_err(sqlite_error)?;
                        let expected = encode_json(closure)?;
                        if stored.as_deref() != Some(expected.as_str()) {
                            return Err(StoreError::TurnCancelClosureAuthorizationMismatch {
                                session_id: closure.session_id().clone(),
                                turn_id: closure.turn_id().clone(),
                            });
                        }
                    }
                    if let (Some(turn_id), Some(observed)) = (
                        commit.interrupted_turn_input_turn_id.as_ref(),
                        commit.interrupted_turn_cancel_intent.as_ref(),
                    ) && load_turn_cancel_intent_snapshot_conn(
                        tx,
                        &commit.session_id,
                        turn_id,
                    )? != *observed
                    {
                        return Err(StoreError::TurnCancelIntentChanged {
                            session_id: commit.session_id.clone(),
                            turn_id: turn_id.clone(),
                        });
                    }
                    let actual_revision = existing.as_ref().map_or(0, |meta| meta.head_revision);
                    let old_leaf_node_id = existing
                        .as_ref()
                        .and_then(|head| head.leaf_node_id.clone());
                    let parent_node_facts = old_leaf_node_id
                        .as_deref()
                        .map(|leaf_node_id| {
                            tx.query_row(
                                session_sql().graph_sqlite.select_parent_facts.sql(),
                                params![leaf_node_id],
                                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                            )
                            .optional()
                            .map_err(sqlite_error)?
                            .map(|(generation, frame_node_id)| {
                                Ok(lash_core_execution::store::ParentNodeFacts {
                                    node_id: leaf_node_id.to_string().into(),
                                    generation: u64::try_from(generation).map_err(|_| {
                                        stored_data_corrupt(
                                            "SessionGraph node",
                                            format!("negative generation {generation}"),
                                        )
                                    })?,
                                    frame_node_id: frame_node_id.into(),
                                })
                            })
                            .transpose()
                        })
                        .transpose()?
                        .flatten();
                    let requested_ancestor_is_active = match (
                        requested_append_ancestor(&commit.turn_commit),
                        parent_node_facts.as_ref(),
                    ) {
                        (None, _) => true,
                        (Some(_), None) => false,
                        (Some(required), Some(parent)) => tx
                            .query_row(
                                session_sql().graph_sqlite.exists_readable_ancestor.sql(),
                                params![required, commit.session_id.as_str(), i64::try_from(parent.generation).map_err(|_| {
                                    StoreError::Backend("parent generation does not fit SQLite INTEGER".to_string())
                                })?],
                                |_| Ok(()),
                            )
                            .optional()
                            .map_err(sqlite_error)?
                            .is_some(),
                    };
                    let occupied_node_ids = occupied_node_ids_conn(tx, commit.graph.nodes())?;
                    let published_leaf = match old_leaf_node_id {
                        None => lash_core_execution::store::PublishedLeafFacts::Absent,
                        Some(node_id) => match parent_node_facts {
                            Some(parent) => lash_core_execution::store::PublishedLeafFacts::Live(parent),
                            None => lash_core_execution::store::PublishedLeafFacts::Retired { node_id },
                        },
                    };
                    if let Some(pending) = load_run_conn(tx, &commit.session_id, None)? {
                        let own_initial_command = pending.members.is_none()
                            && commit.session_execution_lease_fence.is_some()
                            && commit.turn_commit.operation.key == "session-command";
                        if commit.queued_run.as_ref().is_none_or(|progress| progress.scope != pending.scope) && !own_initial_command {
                            return Err(StoreError::QueuedRunConflict { session_id: commit.session_id.clone() });
                        }
                    }
                                        let queued_admission = if let Some(progress) = &commit.queued_run {
                        let admission = load_run_conn(tx, &commit.session_id, Some(&progress.scope))?.ok_or_else(|| StoreError::QueuedRunConflict { session_id: commit.session_id.clone() })?;
                        admission.advance(progress)?;
                        let fence = commit.session_execution_lease_fence.as_ref().ok_or_else(|| StoreError::SessionExecutionLeaseExpired { session_id: commit.session_id.clone() })?;
                        validate_run_members_conn(tx, fence, progress)?;
                        Some(admission)
                    } else { None };
                    let plan = planner.plan(lash_core_execution::store::FreshRuntimeCommitFacts {
                        actual_head_revision: actual_revision,
                        published_leaf,
                        requested_ancestor_is_active,
                        occupied_node_ids,
                        existing_pending_follow_on: existing
                            .as_ref()
                            .and_then(|meta| meta.pending_follow_on.clone()),
                    })?;
                    let sql_head_revision = sql_monotonic_counter_value(
                        "session_head_revision",
                        plan.actual_head_revision(),
                        plan.next_head_revision(),
                    )?;
                    // Settlement authority is decided here, inside the
                    // commit's `BEGIN IMMEDIATE` write transaction, and
                    // returns as shared plans; the plans' ordered writes
                    // execute below, at the same point in the commit the
                    // hand-written bodies ran (FIG-1065).
                    let mut queued_work_plans =
                        Vec::with_capacity(commit.completed_queue_claims.len());
                    for completed in &commit.completed_queue_claims {
                        queued_work_plans.push(plan_queued_work_settlement_conn(tx, completed)?);
                    }
                    let mut turn_input_plans =
                        Vec::with_capacity(commit.completed_turn_input_claims.len());
                    for completed in &commit.completed_turn_input_claims {
                        let mut rows = Vec::with_capacity(completed.input_ids.len());
                        for input_id in &completed.input_ids {
                            let observed = tx
                                .query_row(
                                    crate::turn_ingress::turn_ingress_sql()
                                        .pending_inputs_sqlite
                                        .settlement_facts
                                        .sql(),
                                    params![completed.session_id.as_str(), input_id.as_str()],
                                    |row| {
                                        Ok((
                                            row.get::<_, Option<String>>(0)?,
                                            row.get::<_, Option<String>>(1)?,
                                            row.get::<_, i64>(2)?,
                                            row.get::<_, String>(3)?,
                                        ))
                                    },
                                )
                                .optional()
                                .map_err(sqlite_error)?;
                            let facts = observed
                                .map(|(claim_id, claim_token, generation, state)| {
                                    Ok(
                                        lash_core_execution::store::claim_plan::TurnInputSettlementRowFacts {
                                            claim_id,
                                            claim_token,
                                            claim_session_lease_generation: u64::try_from(
                                                generation,
                                            )
                                            .map_err(|_| {
                                                stored_data_corrupt(
                                                    "PendingTurnInput",
                                                    format!(
                                                        "claim_session_lease_generation must be non-negative, got {generation}"
                                                    ),
                                                )
                                            })?,
                                            state,
                                        },
                                    )
                                })
                                .transpose()?;
                            rows.push(lash_core_execution::store::claim_plan::TurnInputSettlementRow {
                                input_id: input_id.clone(),
                                facts,
                            });
                        }
                        // The shared planner takes the verdict. One
                        // predicate, two regimes: the claim fields only
                        // strengthen it (ADR 0069 section 5).
                        turn_input_plans.push(
                            lash_core_execution::store::claim_plan::plan_turn_input_settlement(
                                completed, rows,
                            )
                            .into_result()?,
                        );
                    }

                    let stored_checkpoint =
                        Self::put_checkpoint_conn(tx, &commit.checkpoint, blob_profile)?;

                    if !commit.usage_deltas.is_empty() {
                        let mut stmt = tx
                            .prepare(session_sql().usage_sqlite.insert.sql())
                            .map_err(sqlite_error)?;
                        for entry in &commit.usage_deltas {
                            let entry_ordinal = i64::try_from(entry.identity.entry_ordinal)
                                .map_err(|_| {
                                    StoreError::Backend(
                                        "usage delta ordinal does not fit SQLite INTEGER"
                                            .to_string(),
                                    )
                                })?;
                            stmt.execute(params![
                                commit.session_id.as_str(),
                                entry.identity.operation_storage_key,
                                entry_ordinal,
                                i64::from(entry.identity.payload_encoding_version),
                                entry.identity.payload_hash,
                                entry.entry.source,
                                entry.entry.model,
                                entry.entry.usage.input_tokens,
                                entry.entry.usage.output_tokens,
                                entry.entry.usage.cache_read_input_tokens,
                                entry.entry.usage.cache_write_input_tokens,
                                entry.entry.usage.reasoning_output_tokens,
                                crate::blobs::encode_usage_disposition(
                                    &entry.entry.usage_disposition,
                                )?,
                            ])
                            .map_err(sqlite_error)?;
                        }
                    }

                    insert_graph_nodes_conn(
                        tx,
                        &commit.session_id,
                        commit.graph.nodes(),
                        plan.planned_node_facts(),
                    )?;
                    let meta = plan.head_meta(stored_checkpoint.checkpoint_ref.clone());
                    // Divergence ruling (FIG-3381): SQLite carries no CAS
                    // predicate on its head upsert and needs none. `existing`
                    // was read inside this `BEGIN IMMEDIATE` transaction,
                    // which is SQLite's database-wide single-writer lock, so
                    // no revision can move between that read and this write.
                    // The invariant is asserted rather than assumed: if the
                    // head read is ever moved out of the write transaction,
                    // this refuses the publication instead of publishing over
                    // a revision nobody held.
                    lash_core_execution::store_backend_support::require_single_writer_head_publication(
                        &commit.session_id,
                        crate::SQLITE_BACKEND,
                        !tx.is_autocommit(),
                    )?;
                    // Read the published revision again, inside the same
                    // write transaction, and let the shared verdict decide.
                    // This is SQLite's equivalent of PostgreSQL's
                    // `SELECT head_revision … FOR UPDATE`: under
                    // `BEGIN IMMEDIATE` it must still be the revision the plan
                    // was built on, and if the earlier read is ever moved out
                    // of this transaction it will not be.
                    let published_revision = tx
                        .query_row(
                            session_sql().head.select_revision.sql(),
                            params![commit.session_id.as_str()],
                            |row| row.get::<_, i64>(0),
                        )
                        .optional()
                        .map_err(sqlite_error)?
                        .map(|revision| {
                            u64_from_sql("SessionHeadMeta", "head_revision", revision)
                        })
                        .transpose()
                        .map_err(sqlite_error)?
                        .unwrap_or(0);
                    match lash_core_execution::store_backend_support::head_publication_verdict(
                        plan.actual_head_revision(),
                        published_revision,
                    ) {
                        lash_core_execution::store_backend_support::HeadPublicationVerdict::Publish => {}
                        lash_core_execution::store_backend_support::HeadPublicationVerdict::HeadMoved {
                            observed_head_revision,
                            ..
                        } => {
                            return Err(StoreError::HeadRevisionConflict {
                                expected: plan.actual_head_revision(),
                                actual: observed_head_revision,
                            });
                        }
                    }
                    tx.execute(
                        session_sql().head.upsert.sql(),
                        params![
                            meta.session_id.as_str(),
                            encode_json(&meta.payload())?,
                            sql_head_revision,
                            meta.leaf_node_id.as_deref(),
                            meta.checkpoint_ref.as_ref().map(BlobRef::as_str),
                            lash_core_execution::store::pending_follow_on::encode_pending_follow_on(
                                meta.pending_follow_on.as_ref(),
                            )?,
                        ],
                    )
                    .map_err(sqlite_error)?;
                    tx.execute(
                        session_sql().meta.touch_last_commit.sql(),
                        params![commit.session_id.as_str(), crate::clamp_epoch_ms(now)],
                    )
                    .map_err(sqlite_error)?;
                    if plan.head_changed()
                        && let Some(old_leaf_node_id) = plan.old_leaf_node_id()
                    {
                        retire_unreachable_ancestry_conn(tx, old_leaf_node_id)?;
                    }
                    {
                        let turn_ingress = crate::turn_ingress::turn_ingress_sql();
                        for settlement_plan in &queued_work_plans {
                            for write in settlement_plan.writes() {
                                match write {
                                    // The fence lands before the queue row
                                    // leaves: a crash between the two would
                                    // replay a wake the session already
                                    // consumed (FIG-1065).
                                    lash_core_execution::store::claim_plan::QueuedWorkSettlementWrite::FenceWakeRedelivery { wake, .. } => {
                                        crate::queued_work::raise_wake_redelivery_fence_conn(
                                            tx,
                                            settlement_plan.session_id(),
                                            wake,
                                        )?;
                                    }
                                    lash_core_execution::store::claim_plan::QueuedWorkSettlementWrite::SettleClaimedBatch { batch_id } => {
                                        let settled = tx
                                            .execute(
                                                turn_ingress.queued_batches.settle_claimed.sql(),
                                                params![
                                                    settlement_plan.session_id().as_str(),
                                                    batch_id.as_str(),
                                                    settlement_plan.claim_id(),
                                                    settlement_plan.lease_token()
                                                ],
                                            )
                                            .map_err(sqlite_error)?;
                                        // Backstop: `plan_queued_work_settlement_conn`
                                        // already took the verdict over this row earlier in
                                        // the same write transaction, so the predicate
                                        // cannot legitimately miss. A miss is recorded as
                                        // evidence and then fails closed with the same
                                        // supersession this site has always returned.
                                        lash_core_execution::store_backend_support::require_fenced_write_applied(
                                            lash_core_execution::store_backend_support::FencedWrite::QueuedWorkClaimSettlement,
                                            crate::SQLITE_BACKEND,
                                            batch_id.as_str(),
                                            u64::try_from(settled).unwrap_or(u64::MAX),
                                            || settlement_plan.superseded_error(batch_id),
                                        )?;
                                    }
                                }
                            }
                        }
                    }
                    {
                        let pending_inputs =
                            &crate::turn_ingress::turn_ingress_sql().pending_inputs;
                        for settlement_plan in &turn_input_plans {
                            for step in settlement_plan.steps() {
                                // One conditional write for both settlement
                                // regimes: the claim fields are an optional
                                // predicate strengthener, and either way
                                // exactly one row must change (ADR 0069
                                // section 5).
                                let settled = match (step.regime, settlement_plan.claim()) {
                                    (
                                        lash_core_execution::store::claim_plan::TurnInputSettlementRegime::Claimed,
                                        Some(claim),
                                    ) => tx.execute(
                                        pending_inputs.settle_claimed.sql(),
                                        params![
                                            settlement_plan.session_id().as_str(),
                                            step.input_id.as_str(),
                                            step.settle_state.as_str(),
                                            claim.claim_id,
                                            claim.lease_token,
                                        ],
                                    ),
                                    (
                                        lash_core_execution::store::claim_plan::TurnInputSettlementRegime::Claimed,
                                        None,
                                    ) => {
                                        return Err(StoreError::Backend(
                                            "claimed turn-input settlement step without a claim"
                                                .to_string(),
                                        ));
                                    }
                                    (
                                        lash_core_execution::store::claim_plan::TurnInputSettlementRegime::Unclaimed,
                                        _,
                                    ) => tx.execute(
                                        pending_inputs.settle_unclaimed.sql(),
                                        params![
                                            settlement_plan.session_id().as_str(),
                                            step.input_id.as_str(),
                                            step.settle_state.as_str(),
                                        ],
                                    ),
                                }
                                .map_err(sqlite_error)?;
                                // Backstop: the verdict was already taken over
                                // this row earlier in the same write transaction,
                                // so the predicate cannot legitimately miss. A
                                // miss is recorded as evidence and then fails
                                // closed with the same supersession this site has
                                // always returned.
                                lash_core_execution::store_backend_support::require_fenced_write_applied(
                                    lash_core_execution::store::claim_plan::TurnInputSettlementPlan::fenced_write(step),
                                    crate::SQLITE_BACKEND,
                                    step.input_id.as_str(),
                                    u64::try_from(settled).unwrap_or(u64::MAX),
                                    || settlement_plan.superseded_error(step),
                                )?;
                            }
                        }
                    }
                    let mut turn_cancel_input_outcome = lash_core_execution::TurnCancelInputOutcome::default();
                    if let Some(turn_id) = commit.interrupted_turn_input_turn_id.as_ref() {
                        let cancellation = commit.interrupted_turn_input_cancellation.as_ref();
                        let disposition = cancellation.map_or(
                            lash_core_execution::TurnCancelDisposition::Defer,
                            |evidence| evidence.undelivered,
                        );
                        if let Some(evidence) = commit
                            .turn_cancel_closure_settlement
                            .as_ref()
                            .and_then(lash_core_execution::TurnCancelClosureSettlement::base_cancellation)
                        {
                            let observed = commit.interrupted_turn_cancel_intent.as_ref().ok_or_else(|| {
                                StoreError::Backend("interrupted turn commit omitted cancellation intent predicate".to_string())
                            })?;
                            if !reconcile_turn_cancel_winner_conn(
                                tx,
                                &commit.session_id,
                                turn_id,
                                observed,
                                evidence,
                            )? {
                                return Err(StoreError::TurnCancelIntentChanged {
                                    session_id: commit.session_id.clone(),
                                    turn_id: turn_id.clone(),
                                });
                            }
                        }
                        // Withheld claims are released first, under their
                        // own fence, so the disposition below settles them
                        // exactly as it settles an unclaimed row (FIG-3531).
                        release_undelivered_turn_input_claims_conn(
                            tx,
                            &commit.undelivered_turn_input_claims,
                        )?;
                        let input_ids = {
                            let mut stmt = tx
                                .prepare(
                                    crate::turn_ingress::turn_ingress_sql()
                                        .pending_inputs_sqlite
                                        .select_pending_active
                                        .sql(),
                                )
                                .map_err(sqlite_error)?;
                            let rows = stmt
                                .query_map(
                                    params![commit.session_id.as_str()],
                                    pending_turn_input_row_from_sql,
                                )
                                .map_err(sqlite_error)?;
                            let mut input_ids = Vec::new();
                            for row in rows {
                                let row = row.map_err(sqlite_error)?;
                                let ingress = decode_turn_input_ingress(row.ingress_json)?;
                                if ingress
                                    .active_turn_id()
                                    .is_some_and(|active| active == turn_id)
                                {
                                    input_ids.push((row.input_id, decode_stored_json(&row.input_json, "turn input")?));
                                }
                            }
                            input_ids
                        };
                        let deferred = lash_core_execution::TurnInputState::DeferredNextTurn;
                        let deferred_ingress = encode_json(&deferred.ingress())?;
                        let pending_inputs =
                            &crate::turn_ingress::turn_ingress_sql().pending_inputs;
                        for (input_id, payload) in input_ids {
                            // Two dispositions, two named statements: deferring
                            // rewrites the ingress so the row stops naming a
                            // turn that is over, dropping is the cancel this
                            // table already has.
                            match disposition {
                                lash_core_execution::TurnCancelDisposition::Defer => tx.execute(
                                    pending_inputs.defer_to_next_turn.sql(),
                                    params![
                                        commit.session_id.as_str(),
                                        input_id,
                                        deferred.as_str(),
                                        deferred_ingress.as_str(),
                                    ],
                                ),
                                lash_core_execution::TurnCancelDisposition::Drop => tx.execute(
                                    pending_inputs.cancel.sql(),
                                    params![
                                        commit.session_id.as_str(),
                                        input_id,
                                        lash_core_execution::runtime::TurnInputStateKind::Cancelled.as_str(),
                                    ],
                                ),
                            }
                            .map_err(sqlite_error)?;
                            let affected = lash_core_execution::TurnCancelAffectedInput { input_id: input_id.into(), payload, disposition };
                            if cancellation.is_some() {
                                append_turn_cancel_outcome_conn(tx, &commit.session_id, turn_id, affected.clone())?;
                                turn_cancel_input_outcome.affected_inputs.push(affected);
                            }
                        }
                    }
                    crate::attachments::commit_attachment_refs_conn(
                        tx, &commit.session_id, &commit.committed_attachment_ids, now as i64,
                    )?;
                    if let Some(turn_id) = commit.turn_commit.operation.turn_id() {
                        tx.execute(
                            crate::attachments::attachment_sql()
                                .manifest
                                .commit_owned
                                .sql(),
                            params![
                                now as i64,
                                commit.session_id.as_str(),
                                turn_id.as_str(),
                                AttachmentOwnerKind::Turn.as_str()
                            ],
                        )
                        .map_err(sqlite_error)?;
                    }
                    if let (Some(admission), Some(progress)) = (&queued_admission, &commit.queued_run) {
                        if matches!(progress.progress, lash_core_execution::store::QueuedRunProgress::Settle { .. }) {
                    let fence = commit.session_execution_lease_fence.as_ref().ok_or_else(|| StoreError::SessionExecutionLeaseExpired { session_id: commit.session_id.clone() })?;
                    settle_run_members_conn(tx, fence, &progress.scope)?;
                }
                write_run_conn(tx, &admission.advance(progress)?, false)?;
                    }
                    crate::session_roots::write_commit_root_terminal_conn(tx, commit, plan.next_head_revision(), now)?;
                    let mut result = plan.result(
                        stored_checkpoint.checkpoint_ref,
                        stored_checkpoint.manifest,
                    );
                    result.turn_cancel_input_outcome = turn_cancel_input_outcome;
                    {
                        let receipt = plan.receipt_write(&result);
                        let result_json = encode_json(receipt.result)?;
                        let identity = append_identity_columns(receipt.append_request_identity);
                        tx.execute(
                            session_sql().turn_commits.insert.sql(),
                            params![
                                receipt.session_id.as_str(),
                                receipt.operation_key,
                                receipt.turn_commit_hash,
                                result_json,
                                now as i64,
                                identity.0,
                                identity.1,
                                identity.2,
                            ],
                        )
                        .map_err(sqlite_error)?;
                        if commit.turn_commit.operation.key == "session-command" {
                            for batch_id in commit
                                .completed_queue_claims
                                .iter()
                                .flat_map(|completion| &completion.batch_ids)
                            {
                                let marker = lash_core_execution::store_backend_support::session_command_batch_completion_key(
                                    &commit.session_id,
                                    batch_id,
                                )?;
                                tx.execute(
                                    session_sql().turn_commits.insert_marker.sql(),
                                    params![
                                        commit.session_id.as_str(),
                                        marker,
                                        receipt.turn_commit_hash,
                                        result_json,
                                        now as i64,
                                    ],
                                )
                                .map_err(sqlite_error)?;
                            }
                        }
                    }
                    if let Some(settlement) = commit.turn_cancel_closure_settlement.as_ref() {
                        let closure = settlement.authorization();
                        tx.execute(
                            crate::turn_ingress::turn_ingress_sql()
                                .closures
                                .delete_by_turn
                                .sql(),
                            params![closure.session_id().as_str(), closure.turn_id().as_str()],
                        )
                        .map_err(sqlite_error)?;
                    }
                    if let Some(completion) = commit.release_session_execution_lease.as_ref() {
                        let _release_was_current =
                            release_session_execution_lease_conn(tx, completion)?;
                        // FIG-884: head CAS is commit authority; release is ancillary.
                    }
                    Ok(result)
                })();
                // Roll back on a `StoreError` so a failure after the first
                // write (e.g. a head-revision conflict surfaced mid-commit, or a
                // backend write error) does not leave the partial transaction
                // committed, while still carrying the typed error to the caller.
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)??;
        Ok(result)
    }

    async fn admit_and_bind_session(
        &self,
        binding: &lash_core_execution::SessionBinding,
    ) -> Result<lash_core_execution::SessionAdmission, StoreError> {
        binding.validate()?;
        let session_id = binding.session_id.clone();
        // The tombstone outranks the handle's own binding: a bound handle
        // asked to admit a deleted session answers SessionDeleted, not
        // SessionBindingMismatch (FIG-1282). The binding decision is made
        // inside the write transaction, between the tombstone check and the
        // metadata write: the connection thread serializes these closures,
        // and the `OnceLock` covers binders outside a transaction, so a
        // competing admission that loses the bind rolls its creation back
        // rather than committing a session it is then refused for. The lock
        // crosses the closure's 'static bound as a shared `Arc`.
        let bound = Arc::clone(&self.session_id);
        let created_at_ms = self.clock.timestamp_ms();
        let fleet_format = self.fleet_format();
        let meta = SessionMeta {
            session_id: session_id.clone(),
            relation: binding.relation.clone(),
            pending_observer_intents: Vec::new(),
        };
        let admission = self
            .conn
            .write_flow(move |tx| {
                let outcome: Result<lash_core_execution::SessionAdmission, StoreError> = (|| {
                    ensure_session_not_deleted_conn(tx, &session_id)?;
                    crate::bind_session_lock(&bound, &session_id)?;
                    let inserted = crate::session_meta::write_session_meta(
                        tx,
                        &meta,
                        crate::session_meta::SessionMetaWrite::Insert,
                        created_at_ms,
                        fleet_format,
                    )?;
                    if inserted {
                        return Ok(lash_core_execution::SessionAdmission::Created);
                    }
                    let recorded = crate::session_meta::load_recorded_lineage(tx, &session_id)?
                        .ok_or_else(|| StoreError::SessionBindingNotMaterialized {
                            session_id: session_id.clone(),
                        })?;
                    lash_core_execution::store_backend_support::guard_rebind_lineage(
                        &session_id,
                        &recorded,
                        &meta.relation,
                    )?;
                    Ok(lash_core_execution::SessionAdmission::Rebound)
                })(
                );
                Ok(match outcome {
                    Ok(admission) => TxOutcome::Commit(Ok(admission)),
                    Err(err) => TxOutcome::Rollback(Err(err)),
                })
            })
            .await
            .map_err(sqlite_error)??;
        Ok(admission)
    }

    async fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError> {
        Store::save_session_meta(self, meta).await
    }

    async fn load_session_meta(&self) -> Result<Option<SessionMeta>, StoreError> {
        Store::load_session_meta(self).await
    }
}

/// Release the turn-input claims a cancelled turn withheld from its terminal
/// checkpoint (FIG-3531), each under its own fence, inside the commit
/// transaction.
///
/// Each row returns to the open spelling its ingress carries —
/// `pending_active` for the active-turn rows a terminal checkpoint claims — so
/// the cancellation's disposition, which runs next, settles and records it
/// exactly as it does an unclaimed row. A claim this turn no longer holds
/// matches no row and is left to its new holder.
fn release_undelivered_turn_input_claims_conn(
    tx: &rusqlite::Connection,
    claims: &[lash_core_execution::TurnInputClaim],
) -> Result<(), StoreError> {
    let sql = crate::turn_ingress::turn_ingress_sql();
    for claim in claims {
        tx.execute(
            sql.pending_inputs_sqlite.abandon_claim.sql(),
            params![
                claim.session_id.as_str(),
                claim.claim_id.as_str(),
                claim.lease_token,
                lash_core_execution::runtime::TurnInputStateKind::PendingActive.as_str(),
                lash_core_execution::runtime::TurnInputStateKind::DeferredNextTurn.as_str(),
            ],
        )
        .map_err(sqlite_error)?;
    }
    Ok(())
}

/// The subset of `nodes` whose ids already occupy a `graph_nodes` row.
///
/// Asked as one statement per commit rather than one per node: the planner
/// needs the whole occupied set before it decides anything, so walking the
/// nodes one query at a time bought nothing and cost a round trip per node.
/// The id list is bound as a single JSON array, the same idiom the checkpoint
/// ref batches use, so the scalar-parameter ceiling is never in play.
fn occupied_node_ids_conn(
    tx: &rusqlite::Connection,
    nodes: &[lash_core_execution::SessionNodeRecord],
) -> Result<std::collections::HashSet<lash_core_execution::NodeId>, StoreError> {
    let mut occupied = std::collections::HashSet::new();
    if nodes.is_empty() {
        return Ok(occupied);
    }
    let node_ids = nodes
        .iter()
        .map(|node| node.node_id.as_str())
        .collect::<Vec<_>>();
    for chunk in node_ids.chunks(OCCUPIED_NODE_ID_CHUNK_SIZE) {
        let encoded = serde_json::to_string(chunk).map_err(|error| {
            StoreError::Backend(format!("failed to encode commit node id batch: {error}"))
        })?;
        let mut statement = tx
            .prepare(session_sql().graph_sqlite.select_occupied.sql())
            .map_err(sqlite_error)?;
        let rows = statement
            .query_map(params![encoded], |row| row.get::<_, String>(0))
            .map_err(sqlite_error)?;
        for node_id in rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)? {
            occupied.insert(lash_core_execution::NodeId::from(node_id));
        }
    }
    Ok(occupied)
}

/// One JSON-array bind per commit keeps the encoded id list around a MiB while
/// staying far above any realistic per-commit node count.
const OCCUPIED_NODE_ID_CHUNK_SIZE: usize = 16_384;

/// Asked as one multi-row `INSERT` rather than one statement per node: the rows
/// are already known in full before any of them is written, and they all land or
/// none of them do regardless, so a statement per node bought no atomicity — it
/// bought a round trip per node.
///
/// A constraint violation is where the batch would lose something real. The
/// per-node errors name the colliding generation or node id, and SQLite reports
/// only that the batch failed, not which row failed it. So a failed batch is
/// replayed one node at a time to find the offender and raise exactly the error
/// the loop used to raise. That replay runs only on the failing path, where a
/// commit is being refused anyway.
fn insert_graph_nodes_conn(
    tx: &rusqlite::Connection,
    session_id: &SessionId,
    nodes: &[lash_core_execution::SessionNodeRecord],
    facts: &[lash_core_execution::store::PlannedNodeFacts],
) -> Result<(), StoreError> {
    for (nodes, facts) in nodes
        .chunks(GRAPH_NODE_INSERT_CHUNK_SIZE)
        .zip(facts.chunks(GRAPH_NODE_INSERT_CHUNK_SIZE))
    {
        let mut rows = Vec::with_capacity(nodes.len());
        for (node, facts) in nodes.iter().zip(facts) {
            let node_json = node.encode_storage_body().map_err(|err| {
                StoreError::Backend(format!("failed to encode graph node body: {err}"))
            })?;
            let generation = i64::try_from(facts.generation).map_err(|_| {
                StoreError::Backend("node generation does not fit SQLite INTEGER".to_string())
            })?;
            rows.push(serde_json::json!([
                session_id.as_str(),
                node.node_id.as_str(),
                node.parent_node_id.as_deref(),
                generation,
                facts.frame_node_id.as_str(),
                node_json,
            ]));
        }
        let encoded = serde_json::to_string(&rows).map_err(|error| {
            StoreError::Backend(format!("failed to encode commit node batch: {error}"))
        })?;
        if tx
            .execute(
                session_sql().graph_sqlite.insert_batch.sql(),
                params![encoded],
            )
            .is_err()
        {
            insert_graph_nodes_one_at_a_time(tx, session_id, nodes, facts)?;
        }
    }
    Ok(())
}

/// The batch rides as one JSON array bound to a single parameter, so SQLite's
/// 32,766-parameter ceiling is not in play at all; the chunk bounds the encoded
/// array's size instead, and sits far above any per-commit node count, so the
/// chunking never runs in practice.
const GRAPH_NODE_INSERT_CHUNK_SIZE: usize = 512;

/// Replay a failed node batch row by row so the refusal names the offending row.
///
/// Reached only after the batch has already failed and the transaction is headed
/// for a rollback, so the extra statements cost nothing a successful commit pays.
fn insert_graph_nodes_one_at_a_time(
    tx: &rusqlite::Connection,
    session_id: &SessionId,
    nodes: &[lash_core_execution::SessionNodeRecord],
    facts: &[lash_core_execution::store::PlannedNodeFacts],
) -> Result<(), StoreError> {
    for (node, facts) in nodes.iter().zip(facts) {
        let node_json = node.encode_storage_body().map_err(|err| {
            StoreError::Backend(format!("failed to encode graph node body: {err}"))
        })?;
        tx.execute(
            session_sql().graph.insert.sql(),
            params![
                session_id.as_str(),
                node.node_id.as_str(),
                node.parent_node_id.as_deref(),
                i64::try_from(facts.generation).map_err(|_| StoreError::Backend(
                    "node generation does not fit SQLite INTEGER".to_string()
                ))?,
                facts.frame_node_id.as_str(),
                node_json
            ],
        )
        .map_err(|error| {
            sqlite_graph_node_insert_error(error, session_id, facts.generation, &node.node_id)
        })?;
    }
    Ok(())
}

impl Store {
    /// The live session (`base: None`), or the session as it stood at the head
    /// one of its turns was admitted on (FIG-3682).
    ///
    /// A base read takes the graph along the base leaf and the base
    /// checkpoint, and the frame nearest the base leaf with that frame's
    /// configuration when the live head has since moved to another frame. A
    /// base checkpoint the store no longer holds is
    /// [`StoreError::TurnBaseNotRetained`], never another head.
    async fn load_session_read(
        &self,
        base: Option<lash_core_execution::store::SessionHeadRef>,
    ) -> Result<Option<PersistedSessionRead>, StoreError> {
        let Some(session_id) = self.resolve_session_id_for_read().await? else {
            return Ok(None);
        };
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                let outcome: Result<Option<PersistedSessionRead>, StoreError> = (|| {
                    read_session_state_version_conn(&tx, &session_id)?;
                    let Some(meta) = try_load_session_head_meta_from_conn(&tx, &session_id)? else {
                        return Ok(None);
                    };
                    let (head_revision, leaf_node_id, checkpoint_ref) = match base.as_ref() {
                        None => (
                            meta.head_revision,
                            meta.leaf_node_id.clone(),
                            meta.checkpoint_ref.clone(),
                        ),
                        Some(base) => (base.revision, base.leaf.clone(), base.checkpoint.clone()),
                    };
                    let graph = Self::load_active_path_session_graph_from_conn(
                        &tx,
                        &session_id,
                        leaf_node_id
                            .clone()
                            .map(lash_core_execution::NodeId::into_inner),
                    )?;
                    let checkpoint = match checkpoint_ref.as_ref() {
                        Some(blob_ref) => Some(match Self::get_checkpoint_conn(&tx, blob_ref)? {
                            Some(checkpoint) => checkpoint,
                            None if base.is_some() => {
                                return Err(StoreError::TurnBaseNotRetained {
                                    revision: head_revision,
                                });
                            }
                            None => {
                                return Err(StoreError::CheckpointComponentMissing {
                                    key: "manifest".to_string(),
                                    blob_ref: blob_ref.clone(),
                                });
                            }
                        }),
                        None => None,
                    };
                    // A turn is admitted only while the head owes no follow-on
                    // (ADR 0101 §3), so an admitted base never carries one.
                    let pending_follow_on = if base.is_some() {
                        None
                    } else {
                        meta.pending_follow_on.clone()
                    };
                    let (current_frame_node_id, config) = match leaf_node_id.as_ref() {
                        Some(leaf)
                            if base.is_some() && meta.leaf_node_id.as_ref() != Some(leaf) =>
                        {
                            let frame = super::nearest_frame_node_id_conn(&tx, leaf.as_str())?
                                .and_then(|frame| {
                                    lash_core_execution::FrameNodeId::new(frame).ok()
                                });
                            let frame_config = frame
                                .as_ref()
                                .filter(|frame| meta.current_frame_node_id.as_ref() != Some(*frame))
                                .and_then(|frame| graph.find_node(frame.as_str()))
                                .and_then(lash_core_execution::SessionNodeRecord::frame_config);
                            (frame, frame_config.unwrap_or(meta.config))
                        }
                        _ => (meta.current_frame_node_id, meta.config),
                    };
                    Ok(Some(PersistedSessionRead {
                        session_id: meta.session_id,
                        head_revision,
                        config,
                        current_frame_node_id,
                        pending_follow_on,
                        graph,
                        checkpoint_ref,
                        checkpoint,
                        token_ledger:
                            lash_core_execution::store::merge_token_ledger_entries_checked(
                                Self::load_usage_deltas_conn(&tx, &session_id)?,
                            )?,
                        turn_failure_settlements: load_turn_failure_settlements_conn(
                            &tx,
                            &session_id,
                        )?,
                    }))
                })(
                );
                tx.commit()?;
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?
    }
}
