use super::*;
use crate::session_sql::session_sql;

/// End every frame in `left` (the frames the commit leaves) in the commit's
/// transaction: fence it and upsert its `Ended` cleanup with no carries
/// (ADR 0113 §3.1, Lane G amendment). A transition first carries its
/// artifacts out of its `ended` frame, one of `left`, into the successor,
/// and gates every cleanup on its execution; without one the cleanups are
/// ungated. The referrer locks of every ended frame and the successor are
/// taken in key order (§2.3).
async fn end_frames_left_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    transition: Option<&lash_core_execution::store::FrameTransition>,
    left: &[lash_core_execution::FrameNodeId],
    now_ms: u64,
) -> Result<(), StoreError> {
    use lash_core_execution::{ArtifactReferrer, FrameEnvironmentId};
    let ended_frames = left
        .iter()
        .map(|frame| {
            ArtifactReferrer::FrameEnvironment(FrameEnvironmentId::new(
                session_id.clone(),
                frame.clone(),
            ))
        })
        .collect::<Vec<_>>();
    let mut referrers = ended_frames.clone();
    referrers.extend(
        transition
            .map(|transition| ArtifactReferrer::FrameEnvironment(transition.successor.clone())),
    );
    let mut keys = referrers
        .iter()
        .map(|referrer| {
            (
                format!(
                    "lash-artifact-referrer:{}:{}",
                    referrer.kind().as_str(),
                    referrer.canonical_id()
                ),
                referrer,
            )
        })
        .collect::<Vec<_>>();
    keys.sort_by(|left, right| left.0.cmp(&right.0));
    keys.dedup_by(|left, right| left.0 == right.0);
    for (_, referrer) in keys {
        crate::artifact_store::lock_referrer_tx(tx, referrer)
            .await
            .map_err(store_sqlx_error)?;
    }
    if let Some(transition) = transition {
        carry_into_successor_tx(tx, transition).await?;
    }
    let sql = crate::artifact_store::artifact_sql();
    let gate = transition.map(|transition| transition.gate.clone());
    for ended in ended_frames {
        sqlx::query(sql.fences.insert_fence.sql())
            .bind(ended.kind().as_str())
            .bind(ended.canonical_id())
            .bind(crate::support::clamp_epoch_ms(now_ms))
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        crate::obligation_ledger::arm_cleanup_tx(
            tx,
            &lash_core_execution::ArtifactCleanup::ended(ended, Vec::new(), gate.clone()),
            now_ms,
        )
        .await?;
    }
    Ok(())
}

/// Carry the transition's artifacts out of its `ended` frame into the
/// successor, under referrer locks the caller already holds.
async fn carry_into_successor_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    transition: &lash_core_execution::store::FrameTransition,
) -> Result<(), StoreError> {
    use lash_core_execution::{ArtifactReferrer, ArtifactStoreId};
    let ended = ArtifactReferrer::FrameEnvironment(transition.ended.clone());
    let successor = ArtifactReferrer::FrameEnvironment(transition.successor.clone());
    let sql = crate::artifact_store::artifact_sql();
    let successor_fenced: bool = sqlx::query_scalar(sql.fences.select_is_fenced.sql())
        .bind(successor.kind().as_str())
        .bind(successor.canonical_id())
        .fetch_one(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    if successor_fenced {
        return Err(StoreError::ArtifactReferrerEnded {
            referrer: successor,
        });
    }
    let mut carries = transition
        .carries
        .iter()
        .map(|artifact| {
            let namespace = match &artifact.store {
                ArtifactStoreId::LashlangModule => {
                    Ok(crate::artifact_store::MODULE_ARTIFACT_NAMESPACE)
                }
                ArtifactStoreId::ProcessEnv => Ok(crate::artifact_store::PROCESS_ENV_NAMESPACE),
                ArtifactStoreId::Engine(_) => Err(StoreError::Backend(
                    "frame carry names an engine artifact".into(),
                )),
            }?;
            Ok((namespace, artifact.artifact_ref.as_str()))
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    carries.sort_unstable();
    carries.dedup();
    for (namespace, artifact_ref) in &carries {
        let key = format!("lash-artifact:{namespace}:{artifact_ref}");
        sqlx::query(
            crate::connection_sql::connection_sql()
                .lock_xact_by_text
                .sql(),
        )
        .bind(key)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    }
    for (namespace, artifact_ref) in carries {
        let source_edge: bool = sqlx::query_scalar(sql.edges.select_edge_exists.sql())
            .bind(namespace)
            .bind(artifact_ref)
            .bind(ended.kind().as_str())
            .bind(ended.canonical_id())
            .fetch_one(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        if !source_edge {
            return Err(StoreError::ArtifactCarryMissing {
                artifact_ref: artifact_ref.to_owned(),
                to: successor,
            });
        }
        sqlx::query(sql.edges.insert_edge.sql())
            .bind(namespace)
            .bind(artifact_ref)
            .bind(successor.kind().as_str())
            .bind(successor.canonical_id())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    }
    Ok(())
}

#[async_trait::async_trait]
impl SessionCommitStore for PostgresStore {
    async fn committed_turn_exists(
        &self,
        session_id: &SessionId,
        turn_id: &lash_core_execution::TurnId,
    ) -> Result<bool, StoreError> {
        let key = lash_core_execution::store_backend_support::turn_commit_receipt_storage_key(
            session_id, turn_id,
        )?;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let exists: bool = sqlx::query_scalar(session_sql().turn_commits.exists_for_turn.sql())
            .bind(session_id.as_str())
            .bind(&key)
            .fetch_one(connection.as_mut())
            .await
            .map_err(store_sqlx_error)?;
        Ok(exists)
    }

    async fn drain_end_exists(
        &self,
        session_id: &SessionId,
        drain_id: &str,
    ) -> Result<bool, StoreError> {
        let key = lash_core_execution::store_backend_support::drain_end_receipt_storage_key(
            session_id, drain_id,
        )?;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let exists: bool = sqlx::query_scalar(session_sql().turn_commits.exists_for_turn.sql())
            .bind(session_id.as_str())
            .bind(&key)
            .fetch_one(connection.as_mut())
            .await
            .map_err(store_sqlx_error)?;
        Ok(exists)
    }

    async fn read_session_state_version(&self, session_id: &SessionId) -> Result<u32, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        let version =
            read_session_state_version_tx(&mut tx, session_id, false, self.fleet_format).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(version)
    }

    async fn admit_session_state(
        &self,
        fence: &lash_core_execution::store::DriveFence,
    ) -> Result<lash_core_execution::store::SessionStateAdmission, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        require_drive_fence_tx(&mut tx, fence).await?;
        let version =
            read_session_state_version_tx(&mut tx, fence.session(), true, self.fleet_format)
                .await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(lash_core_execution::store::SessionStateAdmission {
            session_id: fence.session().clone(),
            version,
            drive_epoch: fence.epoch(),
        })
    }

    async fn retain_admission_base(
        &self,
        fence: &lash_core_execution::store::DriveFence,
        base: &lash_core_execution::store::SessionHeadRef,
    ) -> Result<(), StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        require_drive_fence_tx(&mut tx, fence).await?;
        sqlx::query(session_sql().meta.retain_admission_base.sql())
            .bind(fence.session().as_str())
            .bind(base.checkpoint.as_ref().map(|blob_ref| blob_ref.as_str()))
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(())
    }

    async fn raise_pending_follow_on_attempts(
        &self,
        fence: &lash_core_execution::store::DriveFence,
        follow_on_turn_id: &lash_core_execution::TurnId,
    ) -> Result<lash_core_execution::store::PendingFollowOn, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        require_drive_fence_tx(&mut tx, fence).await?;
        let session_id = fence.session();
        let not_pending = || StoreError::FollowOnNotPending {
            session_id: session_id.clone(),
            follow_on_turn_id: follow_on_turn_id.clone(),
        };
        let pending = pending_follow_on_tx(&mut tx, session_id, true)
            .await?
            .filter(|pending| pending.is_turn(follow_on_turn_id))
            .ok_or_else(not_pending)?;
        let raised = pending.raised()?;
        let updated = sqlx::query(session_sql().head.raise_pending_follow_on.sql())
            .bind(session_id.as_str())
            .bind(
                lash_core_execution::store::pending_follow_on::encode_pending_follow_on(Some(
                    &raised,
                ))?,
            )
            .bind(follow_on_turn_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        if updated.rows_affected() != 1 {
            return Err(not_pending());
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(raised)
    }

    async fn load_session_head_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionHeadMeta>, StoreError> {
        self.read_session_state_version(session_id).await?;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        let meta = load_session_head_meta_tx(&mut tx, session_id, false, self.fleet_format).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(meta)
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError> {
        let planner =
            lash_core_execution::store::RuntimeCommitPlanner::prepare(commit, self.fleet_format)?;
        let commit = planner.commit();
        let now = self.clock.timestamp_ms();
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        // The simulator's backend-fault plan arms this transaction seam; the
        // `testing` feature is off in production, where these expand to nothing.
        #[cfg(feature = "testing")]
        let write_transaction_ordinal = self
            .fault_injector
            .as_ref()
            .map_or(0, crate::testing::PostgresFaultInjector::begin_write);
        pg_sim_fault!(self.fault_injector, AfterBegin, write_transaction_ordinal);
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        // A head row does not exist during the first commit, so row locking
        // alone cannot serialize create-versus-delete. This session-keyed lock
        // is the common authority for every history commit and deletion.
        ensure_session_not_deleted_tx(&mut tx, &commit.session_id).await?;
        // The store is multi-session (ADR 0112): only a session the catalog
        // admitted commits, and admission is what writes its meta row.
        let admitted =
            sqlx::query_scalar::<_, bool>(session_sql().meta_postgres.exists_materialized.sql())
                .bind(commit.session_id.as_str())
                .fetch_one(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
        if !admitted {
            return Err(StoreError::SessionNotFound {
                session_id: commit.session_id.clone(),
            });
        }
        // A root's commit is fenced by the admission its root was sealed
        // under: a successor's seal refuses it before anything is read or
        // written (ADR 0105 §2).
        super::drive_epoch::require_commit_fences_tx(&mut tx, commit).await?;
        // Read without a lock for early validation and receipt replay. Before
        // mutating graph reachability, existing sessions lock and recheck this
        // revision so commit, maintenance, and deletion share one authority.
        let existing =
            load_session_head_meta_tx(&mut tx, &commit.session_id, false, self.fleet_format)
                .await?;
        planner.validate_node_derivation()?;
        {
            // A root's commit settles its park (FIG-3586, FIG-3600 S7) in the
            // same round trip as its receipt read, whichever of its physical
            // turns committed; another root's commit leaves it.
            let prior = sqlx::query(
                session_sql()
                    .turn_commits_postgres
                    .select_receipt_settling_turn_park
                    .sql(),
            )
            .bind(commit.session_id.as_str())
            .bind(planner.operation_key())
            .bind(commit.settled_park_root().map(|root| root.as_str()))
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            if let Some(row) = prior {
                let hash: String = row.get(0);
                let result_json: String = row.get(1);
                let stored_outcome: Option<String> = row.get(2);
                let stored_identity: Option<String> = row.get(3);
                let stored_version: Option<i32> = row.get(4);
                let stored_requested_node_count: Option<i64> = row.get(5);
                // The shared codec owns both unit-shape and integer-range validation.
                // In particular, a negative PostgreSQL INTEGER cannot become legacy replay.
                // The ancestor column intentionally stays outside this receipt SELECT.
                // Its semantic value is already bound by the stored request hash.
                // Fresh-append ancestor fencing continues below, after receipt adjudication.
                let append_request_identity =
                    lash_core_execution::store_backend_support::decode_append_request_identity(
                        &commit.turn_commit.operation.key,
                        stored_identity,
                        stored_version.map(i64::from),
                        stored_requested_node_count,
                    )?;
                let result = lash_core_execution::store::decode_runtime_commit_receipt_for_fleet(
                    &commit.session_id,
                    planner.operation_key(),
                    &result_json,
                    self.fleet_format,
                )?;
                lash_core_execution::store::validate_turn_commit_outcome_code(
                    &result,
                    stored_outcome.as_deref(),
                )?;
                let prior = lash_core_execution::store::RuntimeCommitReceiptRecord {
                    turn_commit_hash: hash,
                    result,
                    append_request_identity,
                };
                if let Some(replay) = planner.decide_receipt(Some(prior))? {
                    if let Some(settlement) = commit.turn_cancel_closure_settlement.as_ref()
                        && settlement.authorization().session_id() == commit.session_id
                        && commit.interrupted_turn_input_turn_id.as_ref()
                            == Some(settlement.authorization().turn_id())
                        && commit.interrupted_turn_input_cancellation.as_ref()
                            == settlement.effective_cancellation()
                    {
                        let closure = settlement.authorization();
                        let encoded = serde_json::to_string(closure).map_err(|error| {
                            StoreError::RecordEncodingFailed {
                                record_kind: "TurnCancelClosureAuthorization".to_string(),
                                message: error.to_string(),
                            }
                        })?;
                        sqlx::query(
                            crate::turn_ingress::turn_ingress_sql()
                                .closures
                                .delete_settled
                                .sql(),
                        )
                        .bind(closure.session_id().as_str())
                        .bind(closure.turn_id().as_str())
                        .bind(encoded)
                        .execute(&mut *tx)
                        .await
                        .map_err(store_sqlx_error)?;
                    }
                    pg_sim_fault!(self.fault_injector, BeforeCommit, write_transaction_ordinal);
                    pg_sim_fault!(self.fault_injector, CommitIo, write_transaction_ordinal);
                    tx.commit().await.map_err(store_sqlx_error)?;
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
                    .unwrap_or_else(|| TurnId::from("missing-turn-id")),
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
                || commit.interrupted_turn_input_turn_id.as_ref() != Some(closure.turn_id())
            {
                return Err(StoreError::TurnCancelClosureAuthorizationMismatch {
                    session_id: commit.session_id.clone(),
                    turn_id: closure.turn_id().clone(),
                });
            }
            if closure.admitted_scope().session_id().is_none() {
                let scope_id = closure
                    .admitted_scope()
                    .journal_identity()
                    .map_err(|error| StoreError::Backend(error.to_string()))?
                    .key()
                    .to_string();
                crate::turn_cancel_closure::lock_scope(&mut tx, &scope_id)
                    .await
                    .map_err(store_sqlx_error)?;
                let retired: bool = sqlx::query_scalar(
                    crate::turn_ingress::turn_ingress_sql()
                        .retired_scopes
                        .exists_for_scope
                        .sql(),
                )
                .bind(&scope_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
                if retired {
                    return Err(StoreError::TurnCancelClosureScopeRetired { scope_id });
                }
            }
            let final_key = lash_core_execution::OperationId::turn(
                closure.session_id(),
                closure.turn_id(),
                "final",
            )
            .storage_key()?;
            let committed: bool =
                sqlx::query_scalar(session_sql().turn_commits.exists_for_turn.sql())
                    .bind(closure.session_id().as_str())
                    .bind(final_key)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(store_sqlx_error)?;
            if committed {
                return Err(StoreError::TurnCancelClosureAuthorizationMismatch {
                    session_id: closure.session_id().clone(),
                    turn_id: closure.turn_id().clone(),
                });
            }
            let stored: Option<String> = sqlx::query_scalar(
                crate::turn_ingress::turn_ingress_sql()
                    .closures_postgres
                    .select_by_turn
                    .sql(),
            )
            .bind(closure.session_id().as_str())
            .bind(closure.turn_id().as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            let expected = serde_json::to_string(closure).map_err(|error| {
                StoreError::RecordEncodingFailed {
                    record_kind: "TurnCancelClosureAuthorization".to_string(),
                    message: error.to_string(),
                }
            })?;
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
        ) && load_turn_cancel_intent_snapshot_tx(&mut tx, &commit.session_id, turn_id).await?
            != *observed
        {
            return Err(StoreError::TurnCancelIntentChanged {
                session_id: commit.session_id.clone(),
                turn_id: turn_id.clone(),
            });
        }
        // Publication owns the complete sorted blob-row set before this fresh
        // commit locks or writes any checkpoint owner edge, graph row, or head.
        let (checkpoint_ref, manifest) =
            put_checkpoint_tx(&mut tx, &commit.checkpoint, self.fleet_format).await?;
        let actual_revision = existing.as_ref().map_or(0, |meta| meta.head_revision);
        if existing.is_none() {
            let placeholder = SessionHeadMeta::assemble(
                &commit.session_id,
                SessionHeadPayload {
                    schema_version: self.fleet_format.writer_version(
                        lash_core_execution::surface_format!(
                            lash_core_execution::store::SESSION_HEAD_META_SCHEMA_VERSION
                        ),
                    ),
                    session_id: commit.session_id.clone(),
                    config: commit.config.clone(),
                    current_frame_node_id: None,
                },
                0,
                None,
                None,
            )?;
            sqlx::query(session_sql().head.insert_placeholder.sql())
                .bind(commit.session_id.as_str())
                .bind(encode_json(&placeholder.payload())?)
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
        }
        let locked_revision =
            sqlx::query_scalar::<_, i64>(session_sql().head.select_revision_for_update.sql())
                .bind(commit.session_id.as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(store_sqlx_error)?
                .map(|revision| u64_from_sql("SessionHeadMeta", "head_revision", revision))
                .transpose()?
                .ok_or_else(|| StoreError::StoredDataCorrupt {
                    record_kind: "SessionHeadMeta",
                    message: "head row disappeared while commit authority was held".to_string(),
                })?;
        let old_leaf_node_id = existing.as_ref().and_then(|head| head.leaf_node_id.clone());
        let parent_node_facts = match old_leaf_node_id.as_deref() {
            Some(leaf_node_id) => sqlx::query_as::<_, (i64, String)>(
                session_sql()
                    .graph_postgres
                    .select_parent_facts_for_update
                    .sql(),
            )
            .bind(leaf_node_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?
            .map(|(generation, frame_node_id)| {
                Ok(lash_core_execution::store::ParentNodeFacts {
                    node_id: leaf_node_id.to_string().into(),
                    generation: u64_from_sql("SessionGraph node", "generation", generation)?,
                    frame_node_id: frame_node_id.into(),
                })
            })
            .transpose()?,
            None => None,
        };
        let requested_ancestor_is_active = match (
            requested_append_ancestor(&commit.turn_commit),
            parent_node_facts.as_ref(),
        ) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(required), Some(parent)) => sqlx::query_scalar::<_, bool>(
                session_sql().graph_postgres.exists_readable_ancestor.sql(),
            )
            .bind(required)
            .bind(commit.session_id.as_str())
            .bind(i64::try_from(parent.generation).map_err(|_| {
                StoreError::Backend("parent generation does not fit PostgreSQL BIGINT".to_string())
            })?)
            .fetch_one(&mut *tx)
            .await
            .map_err(store_sqlx_error)?,
        };
        // The head-CAS verdict, in shared code, over the two reads this
        // transaction made: `existing` without a row lock (it serves early
        // validation and receipt replay) and `locked_revision` under
        // `FOR UPDATE`. The locked read is the authority. Both happen after
        // the session-keyed advisory lock, so they agree; a disagreement means
        // the head moved under commit authority and the caller must reload.
        let authoritative_revision =
            match lash_core_execution::store_backend_support::head_publication_verdict(
                actual_revision,
                locked_revision,
            ) {
                lash_core_execution::store_backend_support::HeadPublicationVerdict::Publish => {
                    locked_revision
                }
                lash_core_execution::store_backend_support::HeadPublicationVerdict::HeadMoved {
                    observed_head_revision,
                    ..
                } => {
                    return Err(StoreError::HeadRevisionConflict {
                        expected: commit.expected_head_revision,
                        actual: observed_head_revision,
                    });
                }
            };
        let node_ids = commit
            .graph
            .nodes()
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<Vec<_>>();
        let occupied_node_ids =
            sqlx::query_scalar::<_, String>(session_sql().graph_postgres.select_occupied.sql())
                .bind(&node_ids)
                .fetch_all(&mut *tx)
                .await
                .map_err(store_sqlx_error)?
                .into_iter()
                .map(lash_core_execution::NodeId::from)
                .collect::<std::collections::HashSet<_>>();
        let published_leaf = match old_leaf_node_id {
            None => lash_core_execution::store::PublishedLeafFacts::Absent,
            Some(node_id) => match parent_node_facts {
                Some(parent) => lash_core_execution::store::PublishedLeafFacts::Live(parent),
                None => lash_core_execution::store::PublishedLeafFacts::Retired { node_id },
            },
        };
        // The head row is locked above, so the fact read here is the one this
        // commit publishes over (ADR 0101 §3).
        let existing_pending_follow_on =
            pending_follow_on_tx(&mut tx, &commit.session_id, true).await?;
        let plan = planner.plan(lash_core_execution::store::FreshRuntimeCommitFacts {
            actual_head_revision: authoritative_revision,
            published_leaf,
            requested_ancestor_is_active,
            occupied_node_ids,
            existing_pending_follow_on,
        })?;
        let sql_head_revision = sql_monotonic_counter_value(
            "session_head_revision",
            plan.actual_head_revision(),
            plan.next_head_revision(),
        )?;
        for entry in &commit.usage_deltas {
            let entry_ordinal = i64::try_from(entry.identity.entry_ordinal).map_err(|_| {
                StoreError::Backend(
                    "usage delta ordinal does not fit PostgreSQL BIGINT".to_string(),
                )
            })?;
            let (reconciled_call_id, reconciled_attempt_ordinal) =
                match &entry.entry.usage_disposition {
                    lash_core_execution::LedgerUsageDisposition::Reconciled {
                        call_id,
                        attempt_ordinal,
                    } => (Some(call_id.as_str()), Some(i64::from(*attempt_ordinal))),
                    _ => (None, None),
                };
            let inserted_seq: Option<i64> =
                sqlx::query_scalar(session_sql().usage_postgres.insert.sql())
                    .bind(commit.session_id.as_str())
                    .bind(&entry.identity.operation_storage_key)
                    .bind(entry_ordinal)
                    .bind(
                        i32::try_from(entry.identity.payload_encoding_version).map_err(|_| {
                            StoreError::Backend(
                                "usage payload encoding version does not fit PostgreSQL INTEGER"
                                    .to_string(),
                            )
                        })?,
                    )
                    .bind(&entry.identity.payload_hash)
                    .bind(&entry.entry.source)
                    .bind(&entry.entry.model)
                    .bind(entry.entry.usage.input_tokens)
                    .bind(entry.entry.usage.output_tokens)
                    .bind(entry.entry.usage.cache_read_input_tokens)
                    .bind(entry.entry.usage.cache_write_input_tokens)
                    .bind(entry.entry.usage.reasoning_output_tokens)
                    .bind(reconciled_call_id)
                    .bind(reconciled_attempt_ordinal)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(store_sqlx_error)?;
            if let (
                Some(seq),
                lash_core_execution::LedgerUsageDisposition::Unreported { attempts },
            ) = (inserted_seq, &entry.entry.usage_disposition)
            {
                for attempt in attempts {
                    sqlx::query(session_sql().usage_holes.insert.sql())
                        .bind(commit.session_id.as_str())
                        .bind(seq)
                        .bind(&attempt.call_id)
                        .bind(i64::from(attempt.attempt_ordinal))
                        .bind(attempt.generation_id.as_deref())
                        .execute(&mut *tx)
                        .await
                        .map_err(store_sqlx_error)?;
                }
            }
        }
        for (node, facts) in commit.graph.nodes().iter().zip(plan.planned_node_facts()) {
            let node_json = node.encode_storage_body(self.fleet_format).map_err(|err| {
                StoreError::Backend(format!("failed to encode graph node body: {err}"))
            })?;
            sqlx::query(session_sql().graph.insert.sql())
                .bind(commit.session_id.as_str())
                .bind(&*node.node_id)
                .bind(node.parent_node_id.as_deref())
                .bind(i64::try_from(facts.generation).map_err(|_| {
                    StoreError::Backend(
                        "node generation does not fit PostgreSQL BIGINT".to_string(),
                    )
                })?)
                .bind(&*facts.frame_node_id)
                .bind(i64::try_from(node_json.len()).map_err(|_| {
                    StoreError::Backend("graph node body exceeds PostgreSQL BIGINT".to_string())
                })?)
                .bind(node_json)
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    graph_node_insert_error(
                        error,
                        &commit.session_id,
                        facts.generation,
                        &node.node_id,
                    )
                })?;
        }
        let meta = plan.head_meta(checkpoint_ref.clone());
        // The head row is locked by the publication verdict above. The
        // earlier head payload is stable under the session advisory lock.
        let left = lash_core_execution::store::frames_left_by_commit(
            existing
                .as_ref()
                .and_then(|head| head.current_frame_node_id.as_ref()),
            &commit.graph,
            meta.current_frame_node_id.as_ref(),
        );
        if let Some(transition) = &commit.frame_transition
            && (transition.ended.session_id() != commit.session_id
                || transition.successor.session_id() != commit.session_id
                || !left.contains(transition.ended.frame_node_id())
                || meta.current_frame_node_id.as_ref()
                    != Some(transition.successor.frame_node_id()))
        {
            return Err(StoreError::Backend(
                "frame transition does not match the committed head".into(),
            ));
        }
        // The revision predicate stays on the upsert as the backstop, and it
        // is the ONLY statement-level guard for a concurrent *first* commit,
        // where the placeholder row above is created inside this transaction.
        // Existing sessions already hold the row lock and the session-keyed
        // advisory lock, so for them it can no longer disagree with the
        // verdict.
        let head_write = sqlx::query(session_sql().head.upsert_cas.sql())
            .bind(commit.session_id.as_str())
            .bind(sql_head_revision)
            .bind(encode_json(&meta.payload())?)
            .bind(checkpoint_ref.as_str())
            .bind(meta.leaf_node_id.as_deref())
            .bind(plan.actual_head_revision() as i64)
            .bind(
                lash_core_execution::store::pending_follow_on::encode_pending_follow_on(
                    meta.pending_follow_on.as_ref(),
                )?,
            )
            .execute(&mut *tx)
            .await;
        let head_write = match head_write {
            Ok(result) => result,
            Err(err) if is_contention_error(&err) => {
                // PostgreSQL aborted this transaction before the head write
                // published. This is not evidence that the head advanced (the
                // rows_affected == 0 branch below is); the unchanged commit is
                // therefore the only semantically valid retry.
                return Err(StoreError::Contended);
            }
            Err(err) => return Err(store_sqlx_error(err)),
        };
        // Backstop: the verdict above authorized exactly this publication over
        // exactly this locked revision, so any other row count means the
        // locked read and the upsert predicate disagree. Record that as
        // evidence, then fail closed with the same `HeadRevisionConflict` this
        // site has always returned, over a freshly read revision so the report
        // is accurate. `tx` then drops (auto-rollback), discarding this
        // attempt's node and usage writes; the caller reloads and retries.
        if !lash_core_execution::store_backend_support::fenced_write_applied(
            lash_core_execution::store_backend_support::FencedWrite::SessionHeadPublication,
            crate::POSTGRES_BACKEND,
            commit.session_id.as_str(),
            head_write.rows_affected(),
        ) {
            let actual_now = sqlx::query_scalar::<_, i64>(session_sql().head.select_revision.sql())
                .bind(commit.session_id.as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(store_sqlx_error)?
                .map(|revision| u64_from_sql("SessionHeadMeta", "head_revision", revision))
                .transpose()?
                .unwrap_or(plan.actual_head_revision());
            return Err(StoreError::HeadRevisionConflict {
                expected: commit.expected_head_revision,
                actual: actual_now,
            });
        }
        end_frames_left_tx(
            &mut tx,
            &commit.session_id,
            commit.frame_transition.as_ref(),
            &left,
            now,
        )
        .await?;
        sqlx::query(session_sql().meta.touch_last_commit.sql())
            .bind(commit.session_id.as_str())
            .bind(i64::try_from(now).unwrap_or(i64::MAX))
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        if plan.head_changed()
            && let Some(old_leaf_node_id) = plan.old_leaf_node_id()
        {
            retire_unreachable_ancestry_tx(&mut tx, old_leaf_node_id).await?;
        }
        // Every row the commit names is settled under the root that admitted
        // it, each verdict taken under the row's lock (FIG-3927).
        let turn_cancel_input_outcome =
            super::ingress_settlement::settle_commit_ingress_tx(&mut tx, commit).await?;
        commit_attachment_refs_tx(
            &mut tx,
            &commit.session_id,
            &commit.committed_attachment_ids,
            now,
        )
        .await?;
        if let Some(turn_id) = commit.turn_commit.operation.turn_id() {
            sqlx::query(
                crate::attachments::attachment_sql()
                    .manifest
                    .commit_owned
                    .sql(),
            )
            .bind(now as i64)
            .bind(commit.session_id.as_str())
            .bind(turn_id.as_str())
            .bind(AttachmentOwnerKind::Turn.as_str())
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        }
        // The root's final commit writes its terminal evidence in this
        // transaction (FIG-3600 S7).
        if let Some(write) = commit.root_terminal.as_deref().cloned() {
            crate::session_roots::write_root_terminal_conn(
                &mut tx,
                &write.into_terminal(commit.session_id.clone(), plan.next_head_revision(), now),
            )
            .await?;
        }
        crate::capture::commit_capture_tx(&mut tx, commit, now).await?;
        let mut result = plan.result(checkpoint_ref, manifest);
        result.turn_cancel_input_outcome = turn_cancel_input_outcome;
        {
            let receipt = plan.receipt_write(&result);
            let columns = append_identity_columns(receipt.append_request_identity)?;
            let result_json = encode_json(receipt.result)?;
            sqlx::query(session_sql().turn_commits.insert.sql())
                .bind(receipt.session_id.as_str())
                .bind(receipt.operation_key)
                .bind(receipt.turn_commit_hash)
                .bind(&result_json)
                .bind(
                    receipt
                        .result
                        .outcome
                        .as_ref()
                        .map(|outcome| outcome.as_str()),
                )
                .bind(now as i64)
                .bind(columns.0)
                .bind(columns.1)
                .bind(columns.2)
                .bind(!receipt.result.failure_evidence.is_empty())
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
            for batch_id in commit
                .applied_commands
                .iter()
                .flat_map(|completion| &completion.batch_ids)
            {
                let marker =
                    lash_core_execution::store_backend_support::session_command_batch_completion_key(
                        &commit.session_id,
                        batch_id,
                    )?;
                sqlx::query(session_sql().turn_commits.insert_marker.sql())
                    .bind(commit.session_id.as_str())
                    .bind(marker)
                    .bind(receipt.turn_commit_hash)
                    .bind(&result_json)
                    .bind(None::<&str>)
                    .bind(now as i64)
                    .bind(false)
                    .execute(&mut *tx)
                    .await
                    .map_err(store_sqlx_error)?;
            }
        }
        if let Some(settlement) = commit.turn_cancel_closure_settlement.as_ref() {
            let closure = settlement.authorization();
            sqlx::query(
                crate::turn_ingress::turn_ingress_sql()
                    .closures
                    .delete_by_turn
                    .sql(),
            )
            .bind(closure.session_id().as_str())
            .bind(closure.turn_id().as_str())
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        }
        // A plain-commit receipt writes three NULL append-identity columns.

        pg_sim_fault!(self.fault_injector, BeforeCommit, write_transaction_ordinal);
        pg_sim_fault!(self.fault_injector, CommitIo, write_transaction_ordinal);
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(result)
    }

    async fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError> {
        let created_at_ms = self.clock.timestamp_ms();
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        ensure_session_not_deleted_tx(&mut tx, &meta.session_id).await?;
        // FIG-3045: the recorded lineage is write-once, so a metadata replace
        // that moves it is refused here exactly as admission refuses a
        // conflicting rebind.
        if let Some(recorded) =
            crate::session_meta::load_recorded_lineage_tx(&mut tx, &meta.session_id).await?
        {
            lash_core_execution::store_backend_support::guard_session_meta_relation_rewrite(
                &meta.session_id,
                &recorded,
                &meta.relation,
            )?;
        }
        crate::session_meta::write_session_meta_tx(
            &mut tx,
            &meta,
            crate::session_meta::SessionMetaWrite::Replace,
            created_at_ms,
            self.fleet_format,
        )
        .await?;
        tx.commit().await.map_err(store_sqlx_error)
    }

    async fn load_session_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionMeta>, StoreError> {
        crate::session_meta::load_session_meta(&self.pool, Some(session_id)).await
    }

    async fn load_session_meta_for_commit(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionMeta>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        ensure_session_not_deleted_tx(&mut tx, session_id).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        self.load_session_meta(session_id).await
    }

    async fn record_turn_park(
        &self,
        park: &lash_core_execution::store::TurnParkWrite,
    ) -> Result<lash_core_execution::store::TurnPark, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        ensure_session_not_deleted_tx(&mut tx, &park.session_id).await?;
        let recorded = super::turn_park::record_turn_park_tx(&mut tx, park).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(recorded)
    }

    async fn load_turn_park(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<lash_core_execution::store::TurnPark>, StoreError> {
        sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .turn_parks
                .select_by_session
                .sql(),
        )
        .bind(session_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(store_sqlx_error)?
        .as_ref()
        .map(super::turn_park::decode_turn_park_row)
        .transpose()
    }
}
