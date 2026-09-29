mod graph_nodes;

use super::*;
use crate::session_sql::session_sql;
use graph_nodes::{insert_graph_nodes_conn, occupied_node_ids_conn};
use lash_core_execution::FleetFormatStore;

/// Apply a commit's frame transition: carry out of `ended` into the
/// successor, then end every frame in `left` (the frames the commit leaves,
/// `ended` among them) gated on the transition's execution.
fn commit_frame_transition_tx(
    tx: &rusqlite::Connection,
    transition: &lash_core_execution::store::FrameTransition,
    left: &[lash_core_execution::FrameNodeId],
    now_ms: u64,
) -> Result<(), StoreError> {
    use lash_core_execution::{ArtifactReferrer, ArtifactStoreId};
    let source = ArtifactReferrer::FrameEnvironment(transition.ended.clone());
    let successor = ArtifactReferrer::FrameEnvironment(transition.successor.clone());
    if crate::artifact_store::artifact_fenced_tx(tx, &successor).map_err(sqlite_error)? {
        return Err(StoreError::ArtifactReferrerEnded {
            referrer: successor,
        });
    }
    let sql = crate::artifact_store::artifact_sql();
    for carry in &transition.carries {
        let namespace = match &carry.store {
            ArtifactStoreId::LashlangModule => crate::artifact_store::MODULE_ARTIFACT_NAMESPACE,
            ArtifactStoreId::ProcessEnv => crate::artifact_store::PROCESS_ENV_NAMESPACE,
            ArtifactStoreId::Engine(_) => {
                return Err(StoreError::Backend(
                    "a frame transition cannot carry an engine artifact".into(),
                ));
            }
        };
        let exists: bool = tx
            .query_row(
                sql.edges.select_edge_exists.sql(),
                params![
                    namespace,
                    carry.artifact_ref,
                    source.kind().as_str(),
                    source.canonical_id()
                ],
                |row| row.get(0),
            )
            .map_err(sqlite_error)?;
        if !exists {
            return Err(StoreError::ArtifactCarryMissing {
                artifact_ref: carry.artifact_ref.clone(),
                to: successor,
            });
        }
        crate::conn::cached_execute(
            tx,
            sql.edges.insert_edge.sql(),
            params![
                namespace,
                carry.artifact_ref,
                successor.kind().as_str(),
                successor.canonical_id()
            ],
        )
        .map_err(sqlite_error)?;
    }
    end_frames_tx(
        tx,
        transition.ended.session_id(),
        left,
        Some(&transition.gate),
        now_ms,
    )
}

/// End each frame in `left`: fence it and upsert its `Ended` cleanup with no
/// carries, gated on `gate` (ADR 0113 §3.1, Lane G amendment).
fn end_frames_tx(
    tx: &rusqlite::Connection,
    session_id: &SessionId,
    left: &[lash_core_execution::FrameNodeId],
    gate: Option<&lash_sansio::EffectJournalIdentity>,
    now_ms: u64,
) -> Result<(), StoreError> {
    for frame in left {
        let referrer = lash_core_execution::ArtifactReferrer::FrameEnvironment(
            lash_core_execution::FrameEnvironmentId::new(session_id.clone(), frame.clone()),
        );
        crate::artifact_store::fence_artifact_referrer_tx(tx, &referrer, now_ms)
            .map_err(sqlite_error)?;
        crate::obligation_ledger::arm_cleanup_tx(
            tx,
            &lash_core_execution::ArtifactCleanup::ended(referrer, Vec::new(), gate.cloned()),
            now_ms,
            "core",
        )?;
    }
    Ok(())
}

#[async_trait::async_trait]
impl SessionCommitStore for SqliteStore {
    async fn committed_turn_exists(
        &self,
        session_id: &SessionId,
        turn_id: &lash_core_execution::TurnId,
    ) -> Result<bool, StoreError> {
        let session_id = session_id.clone();
        let key = lash_core_execution::store_backend_support::turn_commit_receipt_storage_key(
            &session_id,
            turn_id,
        )?;
        self.read_connection()
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

    async fn drain_end_exists(
        &self,
        session_id: &SessionId,
        drain_id: &str,
    ) -> Result<bool, StoreError> {
        let session_id = session_id.clone();
        let key = lash_core_execution::store_backend_support::drain_end_receipt_storage_key(
            &session_id,
            drain_id,
        )?;
        self.read_connection()
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

    async fn read_session_state_version(&self, session_id: &SessionId) -> Result<u32, StoreError> {
        let session_id = session_id.clone();
        let fleet = self.fleet_format();
        self.read_connection()
            .call(move |conn| Ok(read_session_state_version_conn(conn, &session_id, fleet)))
            .await
            .map_err(sqlite_error)?
    }

    async fn admit_session_state(
        &self,
        fence: &lash_core_execution::store::DriveFence,
    ) -> Result<lash_core_execution::store::SessionStateAdmission, StoreError> {
        let fence = fence.clone();
        let fleet = self.fleet_format();
        self.conn
            .write_flow(move |tx| {
                let outcome = (|| {
                    require_drive_fence_conn(tx, &fence)?;
                    let version = read_session_state_version_conn(tx, fence.session(), fleet)?;
                    Ok(lash_core_execution::store::SessionStateAdmission {
                        session_id: fence.session().clone(),
                        version,
                        drive_epoch: fence.epoch(),
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

    async fn retain_admission_base(
        &self,
        fence: &lash_core_execution::store::DriveFence,
        base: &lash_core_execution::store::SessionHeadRef,
    ) -> Result<(), StoreError> {
        let fence = fence.clone();
        let checkpoint_ref = base.checkpoint.clone();
        self.conn
            .write_flow(move |tx| {
                let outcome = (|| {
                    require_drive_fence_conn(tx, &fence)?;
                    crate::session_meta::retain_admission_base_conn(
                        tx,
                        fence.session(),
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
        fence: &lash_core_execution::store::DriveFence,
        follow_on_turn_id: &lash_core_execution::TurnId,
    ) -> Result<lash_core_execution::store::PendingFollowOn, StoreError> {
        let fence = fence.clone();
        let follow_on_turn_id = follow_on_turn_id.clone();
        self.conn
            .write_flow(move |tx| {
                let outcome = raise_pending_follow_on_conn(tx, &fence, &follow_on_turn_id);
                Ok(match outcome {
                    Ok(raised) => TxOutcome::Commit(Ok(raised)),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn load_session_head_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionHeadMeta>, StoreError> {
        self.read_session_state_version(session_id).await?;
        SqliteStore::load_session_head_meta(self, session_id).await
    }

    async fn record_turn_park(
        &self,
        park: &lash_core_execution::store::TurnParkWrite,
    ) -> Result<lash_core_execution::store::TurnPark, StoreError> {
        let write = park.clone();
        self.conn
            .write_flow(move |tx| {
                let outcome = ensure_session_not_deleted_conn(tx, &write.session_id)
                    .and_then(|()| super::turn_park::record_turn_park_conn(tx, &write));
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
        let session_id = session_id.clone();
        self.conn
            .call(move |conn| Ok(super::turn_park::turn_park_conn(conn, &session_id)))
            .await
            .map_err(sqlite_error)?
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError> {
        lash_core_execution::store::validate_session_id(&commit.session_id)?;
        let planner =
            lash_core_execution::store::RuntimeCommitPlanner::prepare(commit, self.fleet_format())?;
        let blob_profile = self.options.blob_profile;
        let now = self.clock.timestamp_ms();
        let fleet = self.fleet_format();
        let result = self
            .conn
            .write_flow(move |tx| {
                let outcome: Result<RuntimeCommitReceipt, StoreError> = (|| {
                    let commit = planner.commit();
                    ensure_session_not_deleted_conn(tx, &commit.session_id)?;
                    let admitted = tx
                        .query_row(
                            session_sql().meta_sqlite.exists_materialized.sql(),
                            params![commit.session_id.as_str()],
                            |_| Ok(()),
                        )
                        .optional()
                        .map_err(sqlite_error)?
                        .is_some();
                    if !admitted {
                        return Err(StoreError::SessionNotFound {
                            session_id: commit.session_id.clone(),
                        });
                    }
                    super::drive_epoch::require_commit_fences_conn(tx, commit)?;
                    let existing =
                        try_load_session_head_meta_from_conn(tx, &commit.session_id, fleet)?;
                    planner.validate_node_derivation()?;
                    // A root's commit settles its park (FIG-3586, FIG-3600
                    // S7), in the commit's transaction, whichever of its
                    // physical turns committed; another root's commit
                    // leaves it.
                    if let Some(turn_id) = commit.settled_park_root() {
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
                                        row.get(5)?,
                                    ))
                                },
                            )
                            .optional()
                            .map_err(sqlite_error)?;
                        if let Some((
                            stored_hash,
                            result_json,
                            stored_outcome,
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
                            let result =
                                lash_core_execution::store::decode_runtime_commit_receipt_for_fleet(
                                    &commit.session_id,
                                    planner.operation_key(),
                                    &result_json,
                                    fleet,
                                )?;
                            lash_core_execution::store::validate_turn_commit_outcome_code(
                                &result, stored_outcome.as_deref(),
                            )?;
                            let prior = lash_core_execution::store::RuntimeCommitReceiptRecord {
                                turn_commit_hash: stored_hash,
                                result,
                                append_request_identity,
                            };
                            if let Some(replay) = planner.decide_receipt(Some(prior))? {
                                if let Some(settlement) = commit.turn_cancel_closure_settlement.as_ref()
                        && settlement.authorization().session_id() == commit.session_id
                        && commit.interrupted_turn_input_turn_id.as_ref() == Some(settlement.authorization().turn_id())
                        && commit.interrupted_turn_input_cancellation.as_ref() == settlement.effective_cancellation()
                    {
                                    let closure = settlement.authorization();
                                    crate::conn::cached_execute(tx, crate::turn_ingress::turn_ingress_sql().closures.delete_settled.sql(),
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
                    let stored_checkpoint =
                        Self::put_checkpoint_conn(tx, &commit.checkpoint, blob_profile, fleet)?;

                    if !commit.usage_deltas.is_empty() {
                        let mut stmt = tx
                            .prepare(session_sql().usage_sqlite.insert.sql())
                            .map_err(sqlite_error)?;
                        for entry in &commit.usage_deltas {
                            let (reconciled_call_id, reconciled_attempt_ordinal) =
                                match &entry.entry.usage_disposition {
                                    lash_core_execution::LedgerUsageDisposition::Reconciled {
                                        call_id,
                                        attempt_ordinal,
                                    } => (Some(call_id.as_str()), Some(i64::from(*attempt_ordinal))),
                                    lash_core_execution::LedgerUsageDisposition::Reported
                                    | lash_core_execution::LedgerUsageDisposition::Unreported { .. } =>
                                        (None, None),
                                };
                            let entry_ordinal = i64::try_from(entry.identity.entry_ordinal)
                                .map_err(|_| {
                                    StoreError::Backend(
                                        "usage delta ordinal does not fit SQLite INTEGER"
                                            .to_string(),
                                    )
                                })?;
                            let inserted = stmt.execute(params![
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
                                reconciled_call_id,
                                reconciled_attempt_ordinal,
                            ])
                            .map_err(sqlite_error)?;
                            if inserted != 0 {
                                let seq = tx.last_insert_rowid();
                                for hole in entry.entry.usage_disposition.unreported_attempt_descriptors() {
                                    crate::conn::cached_execute(
                                        tx,
                                        session_sql().usage_holes.insert.sql(),
                                        params![
                                            commit.session_id.as_str(),
                                            seq,
                                            hole.call_id,
                                            i64::from(hole.attempt_ordinal),
                                            hole.generation_id,
                                        ],
                                    )
                                    .map_err(sqlite_error)?;
                                }
                            }
                        }
                    }

                    insert_graph_nodes_conn(tx, &commit.session_id, commit.graph.nodes(), &plan)?;
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
                    let left = lash_core_execution::store::frames_left_by_commit(
                        existing.as_ref().and_then(|head| head.current_frame_node_id.as_ref()),
                        &commit.graph,
                        meta.current_frame_node_id.as_ref(),
                    );
                    if let Some(transition) = &commit.frame_transition {
                        if transition.ended.session_id() != commit.session_id
                            || transition.successor.session_id() != commit.session_id
                            || !left.contains(transition.ended.frame_node_id())
                            || meta.current_frame_node_id.as_ref()
                                != Some(transition.successor.frame_node_id())
                        {
                            return Err(StoreError::Backend(
                                "frame transition does not match the committed head".into(),
                            ));
                        }
                        commit_frame_transition_tx(tx, transition, &left, now)?;
                    } else {
                        end_frames_tx(tx, &commit.session_id, &left, None, now)?;
                    }
                    crate::conn::cached_execute(tx,
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
                    crate::conn::cached_execute(tx,
                        session_sql().meta.touch_last_commit.sql(),
                        params![commit.session_id.as_str(), crate::clamp_epoch_ms(now)],
                    )
                    .map_err(sqlite_error)?;
                    if plan.head_changed()
                        && let Some(old_leaf_node_id) = plan.old_leaf_node_id()
                    {
                        retire_unreachable_ancestry_conn(tx, old_leaf_node_id)?;
                    }
                    let turn_cancel_input_outcome =
                        super::ingress_settlement::settle_commit_ingress_conn(tx, commit)?;
                    crate::attachments::commit_attachment_refs_conn(
                        tx, &commit.session_id, &commit.committed_attachment_ids, now as i64,
                    )?;
                    if let Some(turn_id) = commit.turn_commit.operation.turn_id() {
                        crate::conn::cached_execute(tx,
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
                    crate::session_roots::write_commit_root_terminal_conn(tx, commit, plan.next_head_revision(), now)?;
                    crate::capture::commit_capture_conn(tx, commit, now)?;
                    let mut result = plan.result(
                        stored_checkpoint.checkpoint_ref,
                        stored_checkpoint.manifest,
                    );
                    result.turn_cancel_input_outcome = turn_cancel_input_outcome;
                    {
                        let receipt = plan.receipt_write(&result);
                        let result_json = encode_json(receipt.result)?;
                        let identity = append_identity_columns(receipt.append_request_identity);
                        crate::conn::cached_execute(tx,
                            session_sql().turn_commits.insert.sql(),
                            params![
                                receipt.session_id.as_str(),
                                receipt.operation_key,
                                receipt.turn_commit_hash,
                                result_json,
                                receipt.result.outcome.as_ref().map(|outcome| outcome.as_str()),
                                now as i64,
                                identity.0,
                                identity.1,
                                identity.2,
                                !result.failure_evidence.is_empty(),
                            ],
                        )
                        .map_err(sqlite_error)?;
                        if let Some(commands) = commit.applied_commands.as_ref() {
                            for batch_id in &commands.batch_ids {
                                let marker = lash_core_execution::store_backend_support::session_command_batch_completion_key(
                                    &commit.session_id,
                                    batch_id,
                                )?;
                                crate::conn::cached_execute(tx,
                                    session_sql().turn_commits.insert_marker.sql(),
                                    params![
                                        commit.session_id.as_str(),
                                        marker,
                                        receipt.turn_commit_hash,
                                        result_json,
                                        Option::<&str>::None,
                                        now as i64,
                                        !result.failure_evidence.is_empty(),
                                    ],
                                )
                                .map_err(sqlite_error)?;
                            }
                        }
                    }
                    if let Some(settlement) = commit.turn_cancel_closure_settlement.as_ref() {
                        let closure = settlement.authorization();
                        crate::conn::cached_execute(tx,
                            crate::turn_ingress::turn_ingress_sql()
                                .closures
                                .delete_by_turn
                                .sql(),
                            params![closure.session_id().as_str(), closure.turn_id().as_str()],
                        )
                        .map_err(sqlite_error)?;
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

    async fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError> {
        SqliteStore::save_session_meta(self, meta).await
    }

    async fn load_session_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionMeta>, StoreError> {
        SqliteStore::load_session_meta(self, session_id).await
    }

    async fn load_session_meta_for_commit(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionMeta>, StoreError> {
        let session_id = session_id.clone();
        self.conn
            .call(move |conn| {
                Ok((|| {
                    ensure_session_not_deleted_conn(conn, &session_id)?;
                    crate::session_meta::load_session_meta(conn, Some(&session_id))
                })())
            })
            .await
            .map_err(sqlite_error)?
    }
}

#[cfg(test)]
mod artifact_frame_transition_tests;
