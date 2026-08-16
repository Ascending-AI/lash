use crate::*;

async fn retained_fork_config_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    node_id: &str,
) -> Result<lash_core::PersistedSessionConfig, StoreError> {
    let frame_node_id = crate::runtime_persistence::nearest_frame_node_id_tx(tx, node_id)
        .await?
        .ok_or_else(|| StoreError::MissingFrameOpenAncestor {
            leaf_node_id: node_id.to_string(),
        })?;
    let row = sqlx::query(
        "SELECT parent_node_id, node_json FROM lash_graph_nodes
         WHERE node_id = $1 AND tombstoned = FALSE",
    )
    .bind(&frame_node_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?
    .ok_or_else(|| {
        StoreError::Backend(format!("retained frame node `{frame_node_id}` is missing"))
    })?;
    let parent_node_id = row.get(0);
    let node_json: String = row.get(1);
    lash_core::SessionNodeRecord::decode_storage_body(
        frame_node_id.clone(),
        parent_node_id,
        &node_json,
    )
    .map_err(|error| {
        StoreError::Backend(format!(
            "failed to decode retained frame node `{frame_node_id}`: {error}"
        ))
    })?
    .frame_config()
    .ok_or_else(|| {
        StoreError::Backend(format!(
            "retained frame node `{frame_node_id}` has no frame assignment"
        ))
    })
}

#[async_trait::async_trait]
impl SessionStoreFactory for PostgresSessionStoreFactory {
    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn RuntimePersistence>, StoreError> {
        lash_core::store::validate_session_id(&request.session_id)?;
        let store = PostgresSessionStore {
            pool: self.pool.clone(),
            clock: Arc::clone(&self.clock),
            session_id: request.session_id.clone(),
            #[cfg(test)]
            checkpoint_probe_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            checkpoint_write_transaction_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let meta = SessionMeta {
            session_id: request.session_id.clone(),
            relation: request.relation.clone(),
        };
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        crate::runtime_persistence::lock_session_history_mutation_tx(&mut tx, &request.session_id)
            .await?;
        let deleted = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(
                SELECT 1 FROM lash_deleted_sessions WHERE session_id = $1
             )",
        )
        .bind(&request.session_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        if deleted {
            return Err(StoreError::SessionDeleted {
                session_id: request.session_id.clone(),
            });
        }
        crate::session_meta::write_session_meta_tx(
            &mut tx,
            &meta,
            crate::session_meta::SessionMetaWrite::Insert,
        )
        .await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(Arc::new(store))
    }

    async fn open_existing_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>, String> {
        let store = PostgresSessionStore {
            pool: self.pool.clone(),
            clock: Arc::clone(&self.clock),
            session_id: request.session_id.clone(),
            #[cfg(test)]
            checkpoint_probe_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            checkpoint_write_transaction_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        if store
            .load_session_meta()
            .await
            .map_err(|err| err.to_string())?
            .is_some()
        {
            Ok(Some(Arc::new(store)))
        } else {
            Ok(None)
        }
    }

    async fn has_claimable_queued_work(
        &self,
        request: &SessionStoreCreateRequest,
        now_epoch_ms: u64,
    ) -> Result<Option<bool>, StoreError> {
        sqlx::query_scalar(
            "SELECT EXISTS(
                SELECT 1
                FROM lash_queued_work_batches qwb
                WHERE qwb.session_id = $1
                  AND qwb.available_at_ms <= $2
            ) OR EXISTS(
                SELECT 1
                FROM lash_pending_turn_inputs pti
                WHERE pti.session_id = $1
                  AND pti.state = $3
            )",
        )
        .bind(&request.session_id)
        .bind(now_epoch_ms as i64)
        .bind(lash_core::TurnInputState::DeferredNextTurn.as_str())
        .fetch_one(&self.pool)
        .await
        .map(Some)
        .map_err(store_sqlx_error)
    }

    async fn session_was_deleted(&self, session_id: &str) -> Result<bool, String> {
        sqlx::query_scalar(
            "SELECT EXISTS(
                SELECT 1 FROM lash_deleted_sessions WHERE session_id = $1
             )",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|err| err.to_string())
    }

    async fn delete_session(&self, session_id: &str) -> Result<(), String> {
        let mut tx = self.pool.begin().await.map_err(|err| err.to_string())?;
        delete_session_tx(&mut tx, session_id)
            .await
            .map_err(|err| err.to_string())?;
        tx.commit().await.map_err(|err| err.to_string())
    }

    async fn pin(&self, node_id: &str) -> Result<lash_core::ForkPoint, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        let live_node = sqlx::query_scalar::<_, bool>(
            "SELECT TRUE FROM lash_graph_nodes
             WHERE node_id = $1 AND tombstoned = FALSE
             FOR UPDATE",
        )
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        if live_node.is_none() {
            return Err(StoreError::ForkPointNotRetained {
                node_id: node_id.to_string(),
            });
        }
        if let Some((checkpoint_ref, source_session_id)) = sqlx::query_as::<_, (String, String)>(
            "SELECT checkpoint_ref, source_session_id
             FROM lash_node_anchors WHERE node_id = $1",
        )
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        {
            let config = retained_fork_config_tx(&mut tx, node_id).await?;
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(lash_core::ForkPoint {
                node_id: node_id.to_string(),
                checkpoint_ref: checkpoint_ref.into(),
                source_session_id,
                config,
                pinned: true,
            });
        }
        let retained = sqlx::query_as::<_, (String, String)>(
            "SELECT session_id, checkpoint_ref FROM lash_sessions
             WHERE leaf_node_id = $1 AND checkpoint_ref IS NOT NULL
             ORDER BY session_id LIMIT 1
             FOR SHARE",
        )
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let (source_session_id, checkpoint_ref) =
            retained.ok_or_else(|| StoreError::ForkPointNotRetained {
                node_id: node_id.to_string(),
            })?;
        sqlx::query(
            "INSERT INTO lash_node_anchors (node_id, checkpoint_ref, source_session_id)
             VALUES ($1, $2, $3)",
        )
        .bind(node_id)
        .bind(&checkpoint_ref)
        .bind(&source_session_id)
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let config = retained_fork_config_tx(&mut tx, node_id).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(lash_core::ForkPoint {
            node_id: node_id.to_string(),
            checkpoint_ref: checkpoint_ref.into(),
            source_session_id,
            config,
            pinned: true,
        })
    }

    async fn unpin(&self, node_id: &str) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        sqlx::query(
            "SELECT node_id FROM lash_graph_nodes
             WHERE node_id = $1 AND tombstoned = FALSE
             FOR UPDATE",
        )
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let removed = sqlx::query("DELETE FROM lash_node_anchors WHERE node_id = $1")
            .bind(node_id)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
        if removed == 1 {
            crate::runtime_persistence::retire_unreachable_ancestry_tx(&mut tx, node_id).await?;
        }
        tx.commit().await.map_err(store_sqlx_error)
    }

    async fn fork_points(&self) -> Result<Vec<lash_core::ForkPoint>, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let rows = sqlx::query(
            "SELECT node_id, checkpoint_ref, source_session_id, pinned
             FROM (
                 SELECT DISTINCT ON (node_id)
                        node_id, checkpoint_ref, source_session_id, pinned
                 FROM (
                     SELECT node_id, checkpoint_ref, source_session_id,
                            TRUE AS pinned, 0 AS priority
                     FROM lash_node_anchors
                     UNION ALL
                     SELECT leaf_node_id, checkpoint_ref, session_id,
                            FALSE AS pinned, 1 AS priority
                     FROM lash_sessions
                     WHERE leaf_node_id IS NOT NULL AND checkpoint_ref IS NOT NULL
                 ) candidates
                 ORDER BY node_id, priority, source_session_id
             ) retained
             ORDER BY node_id",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let mut points = Vec::with_capacity(rows.len());
        for row in rows {
            let node_id: String = row.get(0);
            points.push(lash_core::ForkPoint {
                config: retained_fork_config_tx(&mut tx, &node_id).await?,
                node_id,
                checkpoint_ref: BlobRef(row.get(1)),
                source_session_id: row.get(2),
                pinned: row.get(3),
            });
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(points)
    }

    async fn fork_at(
        &self,
        request: &lash_core::ForkSessionRequest,
    ) -> Result<lash_core::ForkSessionResult, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        crate::runtime_persistence::lock_session_history_mutation_tx(&mut tx, &request.session_id)
            .await?;
        // Keep the fork fences in the shared order: exists -> deleted ->
        // retained -> live -> frame.
        let exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(
                 SELECT 1 FROM lash_sessions WHERE session_id = $1
                 UNION ALL
                 SELECT 1 FROM lash_session_meta WHERE session_id = $1
             )",
        )
        .bind(&request.session_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        if exists {
            return Err(StoreError::ForkSessionAlreadyExists {
                session_id: request.session_id.clone(),
            });
        }
        let deleted = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(
                SELECT 1 FROM lash_deleted_sessions WHERE session_id = $1
             )",
        )
        .bind(&request.session_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        if deleted {
            return Err(StoreError::SessionDeleted {
                session_id: request.session_id.clone(),
            });
        }
        let retained = sqlx::query_as::<_, (String, String)>(
            "SELECT source_session_id, checkpoint_ref FROM (
                 SELECT source_session_id, checkpoint_ref, 0 AS priority
                 FROM lash_node_anchors WHERE node_id = $1
                 UNION ALL
                 SELECT session_id, checkpoint_ref, 1 AS priority FROM lash_sessions
                 WHERE leaf_node_id = $1 AND checkpoint_ref IS NOT NULL
             ) retained
             ORDER BY priority, source_session_id LIMIT 1",
        )
        .bind(&request.node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let (source_session_id, checkpoint_ref) =
            retained.ok_or_else(|| StoreError::ForkPointNotRetained {
                node_id: request.node_id.clone(),
            })?;
        let node_facts = sqlx::query_as::<_, (String, i64)>(
            "SELECT session_id, generation FROM lash_graph_nodes
             WHERE node_id = $1 AND tombstoned = FALSE
             FOR UPDATE",
        )
        .bind(&request.node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let (_owning_session_id, fork_generation) =
            node_facts.ok_or_else(|| StoreError::ForkPointNotRetained {
                node_id: request.node_id.clone(),
            })?;
        let current_frame_node_id =
            crate::runtime_persistence::nearest_frame_node_id_tx(&mut tx, &request.node_id)
                .await?
                .ok_or_else(|| StoreError::MissingFrameOpenAncestor {
                    leaf_node_id: request.node_id.clone(),
                })?;
        // Relation and retention-source identities are metadata, not ancestry.
        // Reconstruct every inherited ceiling from the retained parent edges so
        // deleted owners need no surviving head or descendant carrier row.
        let fork_generation = u64_from_sql("SessionGraph node", "generation", fork_generation)?;
        let mut edge_path = Vec::new();
        let mut current_node_id = request.node_id.clone();
        let mut expected_generation = fork_generation;
        loop {
            let facts = sqlx::query_as::<_, (String, Option<String>, String, i64)>(
                "SELECT node_id, parent_node_id, session_id, generation
                 FROM lash_graph_nodes
                 WHERE node_id = $1 AND tombstoned = FALSE
                 FOR SHARE",
            )
            .bind(&current_node_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?
            .ok_or_else(|| StoreError::StoredDataCorrupt {
                record_kind: "SessionGraph",
                message: format!(
                    "retained fork path is missing or tombstoned at `{current_node_id}`"
                ),
            })?;
            let generation = u64_from_sql("SessionGraph node", "generation", facts.3)?;
            if generation != expected_generation {
                return Err(StoreError::StoredDataCorrupt {
                    record_kind: "SessionGraph",
                    message: format!(
                        "parent generation {generation} does not match expected {expected_generation}"
                    ),
                });
            }
            let parent_node_id = facts.1.clone();
            edge_path.push(lash_core::store::ForkNodeFacts {
                node_id: facts.0,
                parent_node_id: facts.1,
                owning_session_id: facts.2,
                generation,
            });
            if expected_generation == 0 {
                break;
            }
            current_node_id = parent_node_id.ok_or_else(|| StoreError::StoredDataCorrupt {
                record_kind: "SessionGraph",
                message: "retained fork path ended before generation zero".to_string(),
            })?;
            expected_generation -= 1;
        }
        edge_path.reverse();
        let fork_plan = lash_core::store::ForkPlan::derive(&request.session_id, edge_path)?;
        let config = lash_core::PersistedSessionConfig::from(&request.policy);
        let head = lash_core::store::SessionHeadMeta::assemble(
            lash_core::store::SessionHeadPayload {
                schema_version: lash_core::store::SESSION_HEAD_META_SCHEMA_VERSION,
                session_id: request.session_id.clone(),
                config,
                current_frame_node_id: Some(current_frame_node_id),
            },
            0,
            Some(checkpoint_ref.clone().into()),
            Some(request.node_id.clone()),
        );
        sqlx::query(
            "INSERT INTO lash_sessions
             (session_id, head_revision, head_json, checkpoint_ref, leaf_node_id)
             VALUES ($1, 0, $2, $3, $4)",
        )
        .bind(&request.session_id)
        .bind(encode_json(&head.payload())?)
        .bind(&checkpoint_ref)
        .bind(&request.node_id)
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        for ancestor in fork_plan.ancestors() {
            sqlx::query(
                "INSERT INTO lash_fork_lineage
                 (session_id, ancestor_session_id, fork_node_id, fork_generation)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(fork_plan.session_id())
            .bind(&ancestor.ancestor_session_id)
            .bind(&ancestor.fork_node_id)
            .bind(i64::try_from(ancestor.fork_generation).map_err(|_| {
                StoreError::Backend("fork generation does not fit PostgreSQL BIGINT".to_string())
            })?)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        }
        let meta = SessionMeta {
            session_id: request.session_id.clone(),
            relation: request.relation.clone(),
        };
        crate::session_meta::write_session_meta_tx(
            &mut tx,
            &meta,
            crate::session_meta::SessionMetaWrite::Insert,
        )
        .await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(lash_core::ForkSessionResult {
            session_id: request.session_id.clone(),
            node_id: request.node_id.clone(),
            source_session_id,
        })
    }
}

impl PostgresSessionStoreFactory {
    /// The read-only delete-time root predicate for one digest, parameterised
    /// `$1 = attachment_id`, `$2 = intent_grace_cutoff_ms`. A ref is live unless
    /// it is eligible for the same conditional forget reconciliation applies.
    /// The targeted probe and the condemn CAS share it so the fence and the
    /// probe cannot drift apart.
    fn live_attachment_ref_sql(&self) -> String {
        let process_dead = if self.process_registry_shared {
            "OR (
                manifest.owner_kind = 'process'
                AND NOT EXISTS (
                    SELECT 1 FROM lash_processes AS process
                    WHERE process.process_id = manifest.owner_id
                )
            )"
        } else {
            ""
        };
        format!(
            "SELECT 1 FROM lash_attachment_manifest AS manifest
             WHERE manifest.attachment_id = $1
               AND NOT (
                    manifest.committed_at_ms IS NULL
                    AND manifest.intent_at_ms <= $2
                    AND (
                        manifest.owner_kind IS NULL
                        OR (
                            manifest.owner_kind = 'turn'
                            AND EXISTS (
                                SELECT 1 FROM lash_runtime_turn_commits AS turn_commit
                                WHERE turn_commit.session_id = manifest.session_id
                                  AND turn_commit.turn_id <> manifest.owner_id
                                  AND turn_commit.committed_at_ms > manifest.intent_at_ms
                            )
                        )
                        {process_dead}
                    )
               )
             LIMIT 1"
        )
    }
}

#[async_trait::async_trait]
impl lash_core::AttachmentRootSet for PostgresSessionStoreFactory {
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<lash_core::AttachmentId>, lash_core::StoreError> {
        // Age is only a post-terminal retention policy. This single DELETE
        // composes age with durable owner-death proof: a later committed turn
        // supersedes a turn owner, a missing process row proves a process owner
        // was pruned, and only unscoped host puts use age alone.
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        let process_dead = if self.process_registry_shared {
            "OR (
                manifest.owner_kind = 'process'
                AND NOT EXISTS (
                    SELECT 1
                    FROM lash_processes AS process
                    WHERE process.process_id = manifest.owner_id
                )
            )"
        } else {
            ""
        };
        let delete_sql = format!(
            "DELETE FROM lash_attachment_manifest AS manifest
             WHERE manifest.committed_at_ms IS NULL
               AND manifest.intent_at_ms <= $1
               AND (
                    manifest.owner_kind IS NULL
                    OR (
                        manifest.owner_kind = 'turn'
                        AND EXISTS (
                            SELECT 1
                            FROM lash_runtime_turn_commits AS turn_commit
                            WHERE turn_commit.session_id = manifest.session_id
                              AND turn_commit.turn_id <> manifest.owner_id
                              AND turn_commit.committed_at_ms > manifest.intent_at_ms
                        )
                    )
                    {process_dead}
               )"
        );
        sqlx::query(&delete_sql)
            .bind(intent_grace_cutoff_epoch_ms as i64)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let rows = sqlx::query("SELECT DISTINCT attachment_id FROM lash_attachment_manifest")
            .fetch_all(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(rows
            .into_iter()
            .map(|row| lash_core::AttachmentId::new(row.get::<String, _>(0)))
            .collect())
    }

    async fn has_live_attachment_ref(
        &self,
        id: &lash_core::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, lash_core::StoreError> {
        let row = sqlx::query(&self.live_attachment_ref_sql())
            .bind(id.as_str())
            .bind(intent_grace_cutoff_epoch_ms as i64)
            .fetch_optional(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
        Ok(row.is_some())
    }

    fn fence(&self) -> lash_core::AttachmentGcFence {
        lash_core::AttachmentGcFence::Fenced
    }

    async fn condemn_attachment(
        &self,
        id: &lash_core::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<lash_core::AttachmentCondemnation, lash_core::StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        // The same per-digest lock a writer's `begin_attachment_write` takes:
        // the root predicate below and that writer's manifest insert cannot
        // interleave.
        crate::attachments::lock_attachment_fence_tx(&mut tx, id.as_str()).await?;
        let rooted = sqlx::query(&self.live_attachment_ref_sql())
            .bind(id.as_str())
            .bind(intent_grace_cutoff_epoch_ms as i64)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?
            .is_some();
        if rooted {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(lash_core::AttachmentCondemnation::RootPresent);
        }
        let inserted = sqlx::query(
            "INSERT INTO lash_attachment_condemnations (attachment_id, phase)
             VALUES ($1, 'condemned')
             ON CONFLICT (attachment_id) DO NOTHING",
        )
        .bind(id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(if inserted == 1 {
            lash_core::AttachmentCondemnation::Condemned
        } else {
            // A peer sweeper owns this digest. Skip on contention.
            lash_core::AttachmentCondemnation::AlreadyCondemned
        })
    }

    async fn arm_attachment_delete(
        &self,
        id: &lash_core::AttachmentId,
    ) -> Result<lash_core::AttachmentDeleteArming, lash_core::StoreError> {
        let armed = sqlx::query(
            "UPDATE lash_attachment_condemnations SET phase = 'deleting'
             WHERE attachment_id = $1 AND phase = 'condemned'",
        )
        .bind(id.as_str())
        .execute(&self.pool)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
        Ok(if armed == 1 {
            lash_core::AttachmentDeleteArming::Armed
        } else {
            // A writer revoked the condemnation: the delete is never issued.
            lash_core::AttachmentDeleteArming::Revoked
        })
    }

    async fn release_attachment_condemnation(
        &self,
        id: &lash_core::AttachmentId,
    ) -> Result<(), lash_core::StoreError> {
        sqlx::query("DELETE FROM lash_attachment_condemnations WHERE attachment_id = $1")
            .bind(id.as_str())
            .execute(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
        Ok(())
    }
}

pub(crate) async fn delete_session_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
) -> Result<(), StoreError> {
    crate::runtime_persistence::lock_session_history_mutation_tx(tx, session_id).await?;
    let materialized = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
             SELECT 1 FROM lash_session_meta WHERE session_id = $1
             UNION ALL
             SELECT 1 FROM lash_sessions WHERE session_id = $1
         )",
    )
    .bind(session_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    if materialized {
        // Permanent identity evidence for host-facing session ids.
        sqlx::query(
            "INSERT INTO lash_deleted_sessions (session_id)
             VALUES ($1)
             ON CONFLICT (session_id) DO NOTHING",
        )
        .bind(session_id)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    }
    // Attachment intents are released before the rest of the session store so
    // a failed transaction cannot leave live-looking state without its owner.
    let leaf_node_id = sqlx::query_scalar::<_, String>(
        "SELECT leaf_node_id FROM lash_sessions
         WHERE session_id = $1 AND leaf_node_id IS NOT NULL
         FOR UPDATE",
    )
    .bind(session_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    sqlx::query("DELETE FROM lash_sessions WHERE session_id = $1")
        .bind(session_id)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    if let Some(leaf_node_id) = leaf_node_id {
        crate::runtime_persistence::retire_unreachable_ancestry_tx(tx, &leaf_node_id).await?;
    }
    let unreachable_candidates = sqlx::query_scalar::<_, String>(
        "SELECT g.node_id FROM lash_graph_nodes AS g
         WHERE g.session_id = $1 AND g.tombstoned = FALSE
           AND NOT EXISTS (
               SELECT 1 FROM lash_graph_nodes AS child
               WHERE child.parent_node_id = g.node_id
                 AND child.tombstoned = FALSE
           )
           AND NOT EXISTS (
               SELECT 1 FROM lash_sessions AS head
               WHERE head.leaf_node_id = g.node_id
           )
           AND NOT EXISTS (
               SELECT 1 FROM lash_node_anchors AS anchor
               WHERE anchor.node_id = g.node_id
           )
         ORDER BY g.seq DESC",
    )
    .bind(session_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    for node_id in unreachable_candidates {
        crate::runtime_persistence::retire_unreachable_ancestry_tx(tx, &node_id).await?;
    }
    sqlx::query("DELETE FROM lash_graph_nodes WHERE tombstoned = TRUE")
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    for sql in [
        "DELETE FROM lash_attachment_manifest WHERE session_id = $1",
        "DELETE FROM lash_queued_work_items WHERE batch_id IN (SELECT batch_id FROM lash_queued_work_batches WHERE session_id = $1)",
        "DELETE FROM lash_queued_work_batches WHERE session_id = $1",
        "DELETE FROM lash_wake_redelivery_fences WHERE session_id = $1",
        "DELETE FROM lash_wake_allocation_floors WHERE target_session_id = $1",
        "DELETE FROM lash_pending_turn_inputs WHERE session_id = $1",
        "DELETE FROM lash_session_execution_leases WHERE session_id = $1",
        "DELETE FROM lash_usage_deltas WHERE session_id = $1",
        "DELETE FROM lash_runtime_turn_commits WHERE session_id = $1",
        "DELETE FROM lash_fork_lineage WHERE session_id = $1",
        "DELETE FROM lash_session_meta WHERE session_id = $1",
    ] {
        sqlx::query(sql)
            .bind(session_id)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    }
    // Trigger manifests are the one artifact-ref namespace with an exact
    // session owner. Module, raw-artifact, and process-environment refs are
    // factory-wide services with no safe session attribution.
    sqlx::query(
        "DELETE FROM lash_lashlang_artifacts
         WHERE namespace = $1 AND artifact_ref = $2",
    )
    .bind(crate::artifact_store::CURRENT_TRIGGER_MANIFEST_NAMESPACE)
    .bind(lash_core::TriggerOwnerScope::session(session_id).namespace())
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(())
}

/// Deletes process-owned runtime sessions as one batch inside the process
/// prune transaction. Process runtime session ids are internal, so they never
/// receive host-facing deletion tombstones.
pub(crate) async fn delete_process_sessions_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_ids: &[String],
) -> Result<(), StoreError> {
    if session_ids.is_empty() {
        return Ok(());
    }

    // Take every session-history mutation fence in a standalone statement
    // before deleting heads or deciding whether graph cleanup is required.
    // The ordered subquery gives overlapping batches one lock-request order.
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended(ordered.session_id, 1::BIGINT))
         FROM (
             SELECT session_id
             FROM unnest($1::TEXT[]) AS target(session_id)
             ORDER BY session_id
         ) AS ordered",
    )
    .bind(session_ids)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;

    let (deleted_leaf_node_ids, has_graph_candidates) = sqlx::query_as::<_, (Vec<String>, bool)>(
        "WITH deleted_sessions AS (
                 DELETE FROM lash_sessions AS session
                 WHERE session.session_id = ANY($1)
                 RETURNING session.session_id, session.leaf_node_id
             )
             SELECT COALESCE(
                        array_agg(leaf_node_id ORDER BY session_id)
                            FILTER (WHERE leaf_node_id IS NOT NULL),
                        ARRAY[]::TEXT[]
                    ),
                    EXISTS (
                        SELECT 1
                        FROM lash_graph_nodes AS graph
                        WHERE graph.tombstoned = FALSE
                          AND (
                              graph.session_id = ANY($1)
                              OR graph.node_id IN (
                                  SELECT leaf_node_id FROM deleted_sessions
                                  WHERE leaf_node_id IS NOT NULL
                              )
                          )
                    )
             FROM deleted_sessions",
    )
    .bind(session_ids)
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;

    if has_graph_candidates {
        for leaf_node_id in deleted_leaf_node_ids {
            crate::runtime_persistence::retire_unreachable_ancestry_tx(tx, &leaf_node_id).await?;
        }
        let unreachable_candidates = sqlx::query_scalar::<_, String>(
            "SELECT graph.node_id FROM lash_graph_nodes AS graph
             WHERE graph.session_id = ANY($1) AND graph.tombstoned = FALSE
               AND NOT EXISTS (
                   SELECT 1 FROM lash_graph_nodes AS child
                   WHERE child.parent_node_id = graph.node_id
                     AND child.tombstoned = FALSE
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lash_sessions AS head
                   WHERE head.leaf_node_id = graph.node_id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lash_node_anchors AS anchor
                   WHERE anchor.node_id = graph.node_id
               )
             ORDER BY graph.seq DESC",
        )
        .bind(session_ids)
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        for node_id in unreachable_candidates {
            crate::runtime_persistence::retire_unreachable_ancestry_tx(tx, &node_id).await?;
        }
    }

    let trigger_owner_namespaces = session_ids
        .iter()
        .map(|session_id| lash_core::TriggerOwnerScope::session(session_id).namespace())
        .collect::<Vec<_>>();
    sqlx::query(
        "WITH deleted_graph_nodes AS (
             DELETE FROM lash_graph_nodes WHERE tombstoned = TRUE
             RETURNING node_id
         ),
         deleted_attachment_manifest AS (
             DELETE FROM lash_attachment_manifest
             WHERE session_id = ANY($1)
             RETURNING attachment_id
         ),
         deleted_queued_work_items AS (
             DELETE FROM lash_queued_work_items AS item
             WHERE EXISTS (
                 SELECT 1 FROM lash_queued_work_batches AS batch
                 WHERE batch.batch_id = item.batch_id
                   AND batch.session_id = ANY($1)
             )
             RETURNING item.batch_id
         ),
         deleted_queued_work_batches AS (
             DELETE FROM lash_queued_work_batches
             WHERE session_id = ANY($1)
               AND (SELECT count(*) FROM deleted_queued_work_items) >= 0
             RETURNING batch_id
         ),
         deleted_wake_redelivery_fences AS (
             DELETE FROM lash_wake_redelivery_fences
             WHERE session_id = ANY($1)
             RETURNING session_id
         ),
         deleted_wake_allocation_floors AS (
             DELETE FROM lash_wake_allocation_floors
             WHERE target_session_id = ANY($1)
             RETURNING target_session_id
         ),
         deleted_pending_turn_inputs AS (
             DELETE FROM lash_pending_turn_inputs
             WHERE session_id = ANY($1)
             RETURNING session_id
         ),
         deleted_session_execution_leases AS (
             DELETE FROM lash_session_execution_leases
             WHERE session_id = ANY($1)
             RETURNING session_id
         ),
         deleted_usage_deltas AS (
             DELETE FROM lash_usage_deltas
             WHERE session_id = ANY($1)
             RETURNING session_id
         ),
         deleted_runtime_turn_commits AS (
             DELETE FROM lash_runtime_turn_commits
             WHERE session_id = ANY($1)
             RETURNING session_id
         ),
         deleted_fork_lineage AS (
             DELETE FROM lash_fork_lineage
             WHERE session_id = ANY($1)
             RETURNING session_id
         ),
         deleted_session_meta AS (
             DELETE FROM lash_session_meta
             WHERE session_id = ANY($1)
             RETURNING session_id
         ),
         deleted_trigger_manifests AS (
             DELETE FROM lash_lashlang_artifacts
             WHERE namespace = $2
               AND artifact_ref = ANY($3)
             RETURNING artifact_ref
         )
         SELECT (SELECT count(*) FROM deleted_graph_nodes)
              + (SELECT count(*) FROM deleted_attachment_manifest)
              + (SELECT count(*) FROM deleted_queued_work_batches)
              + (SELECT count(*) FROM deleted_wake_redelivery_fences)
              + (SELECT count(*) FROM deleted_wake_allocation_floors)
              + (SELECT count(*) FROM deleted_pending_turn_inputs)
              + (SELECT count(*) FROM deleted_session_execution_leases)
              + (SELECT count(*) FROM deleted_usage_deltas)
              + (SELECT count(*) FROM deleted_runtime_turn_commits)
              + (SELECT count(*) FROM deleted_fork_lineage)
              + (SELECT count(*) FROM deleted_session_meta)
              + (SELECT count(*) FROM deleted_trigger_manifests)",
    )
    .bind(session_ids)
    .bind(crate::artifact_store::CURRENT_TRIGGER_MANIFEST_NAMESPACE)
    .bind(&trigger_owner_namespaces)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;

    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct QueuedBatchRow {
    pub(crate) enqueue_seq: u64,
    pub(crate) batch_id: String,
    session_id: String,
    source_key: Option<String>,
    pub(crate) delivery_policy: DeliveryPolicy,
    pub(crate) kind: QueuedWorkKind,
    pub(crate) authority: QueuedWorkAuthority,
    pub(crate) merge_key: Option<String>,
    available_at_ms: u64,
    enqueued_at_ms: u64,
    pub(crate) claim_fencing_token: u64,
    pub(crate) claim_id: Option<String>,
    pub(crate) claim_token: Option<String>,
    pub(crate) claim_session_lease_generation: u64,
}

pub(crate) fn claim_candidate_from_row(
    row: &QueuedBatchRow,
    batch: &QueuedWorkBatch,
) -> Result<ClaimCandidate, StoreError> {
    batch.work_class().ok_or_else(|| {
        StoreError::Backend(format!(
            "queued-work batch `{}` has mixed or empty payload classes",
            batch.batch_id
        ))
    })?;
    Ok(ClaimCandidate::from_batch(
        batch,
        row.claim_fencing_token,
        row.claim_id.clone(),
    ))
}

pub(crate) fn queued_batch_row(row: PgRow) -> Result<QueuedBatchRow, StoreError> {
    let delivery_policy =
        DeliveryPolicy::from_wire_str(row.get::<String, _>("delivery_policy").as_str())
            .ok_or_else(|| {
                StoreError::Backend("invalid queued work delivery policy".to_string())
            })?;
    let kind = QueuedWorkKind::from_wire_str(row.get::<String, _>("work_kind").as_str())
        .ok_or_else(|| StoreError::Backend("invalid queued work kind".to_string()))?;
    let authority_json: String = row.get("authority_json");
    Ok(QueuedBatchRow {
        enqueue_seq: u64_from_sql("QueuedWorkBatch", "enqueue_seq", row.get("enqueue_seq"))?,
        batch_id: row.get("batch_id"),
        session_id: row.get("session_id"),
        source_key: row.get("source_key"),
        delivery_policy,
        kind,
        authority: store_decode_json(&authority_json, "queued work authority")?,
        merge_key: row.get("merge_key"),
        available_at_ms: u64_from_sql(
            "QueuedWorkBatch",
            "available_at_ms",
            row.get("available_at_ms"),
        )?,
        enqueued_at_ms: u64_from_sql(
            "QueuedWorkBatch",
            "enqueued_at_ms",
            row.get("enqueued_at_ms"),
        )?,
        claim_fencing_token: u64_from_sql(
            "QueuedWorkBatch",
            "claim_fencing_token",
            row.get("claim_fencing_token"),
        )?,
        claim_id: row.get("claim_id"),
        claim_token: row.get("claim_token"),
        claim_session_lease_generation: u64_from_sql(
            "QueuedWorkBatch",
            "claim_session_lease_generation",
            row.get("claim_session_lease_generation"),
        )?,
    })
}

pub(crate) async fn load_queued_batch(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    batch_id: &str,
) -> Result<Option<QueuedWorkBatch>, StoreError> {
    let row = sqlx::query(
        "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                work_kind, authority_json, merge_key, available_at_ms, enqueued_at_ms,
                claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                claim_owner_liveness_json, claim_token, claim_session_lease_generation, claim_id
         FROM lash_queued_work_batches
         WHERE batch_id = $1",
    )
    .bind(batch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let row = queued_batch_row(row)?;
    queued_work_batch_from_row(tx, row).await.map(Some)
}

pub(crate) async fn queued_work_batch_from_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    row: QueuedBatchRow,
) -> Result<QueuedWorkBatch, StoreError> {
    let item_rows = sqlx::query(
        "SELECT item_id, payload_json
         FROM lash_queued_work_items
         WHERE batch_id = $1
         ORDER BY item_index ASC",
    )
    .bind(&row.batch_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let mut items = Vec::new();
    for item in item_rows {
        let payload_json: String = item.get(1);
        items.push(QueuedWorkItem {
            item_id: item.get(0),
            payload: store_decode_json(&payload_json, "queued work payload")?,
        });
    }
    Ok(QueuedWorkBatch {
        batch_id: row.batch_id,
        session_id: row.session_id,
        enqueue_seq: row.enqueue_seq,
        source_key: row.source_key,
        delivery_policy: row.delivery_policy,
        kind: row.kind,
        authority: row.authority,
        merge_key: row.merge_key,
        available_at_ms: row.available_at_ms,
        enqueued_at_ms: row.enqueued_at_ms,
        items,
    })
}

pub(crate) async fn ensure_queued_work_completion_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    completed: &QueuedWorkCompletion,
) -> Result<(), StoreError> {
    for batch_id in &completed.batch_ids {
        let authority: Option<(Option<String>, Option<String>, i64)> = sqlx::query_as(
            "SELECT claim_id, claim_token, claim_session_lease_generation
             FROM lash_queued_work_batches
             WHERE session_id = $1
               AND batch_id = $2
             LIMIT 1
             FOR UPDATE",
        )
        .bind(&completed.session_id)
        .bind(batch_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        let authority = authority
            .map(|(claim_id, claim_token, generation)| {
                Ok((
                    claim_id,
                    claim_token,
                    u64_from_sql(
                        "QueuedWorkBatch",
                        "claim_session_lease_generation",
                        generation,
                    )?,
                ))
            })
            .transpose()?;
        let owns_row = authority
            .as_ref()
            .is_some_and(|(claim_id, claim_token, _)| {
                claim_id.as_deref() == Some(completed.claim_id.as_str())
                    && claim_token.as_deref() == Some(completed.lease_token.as_str())
            });
        if !owns_row {
            return Err(StoreError::QueuedWorkClaimSuperseded {
                session_id: completed.session_id.clone(),
                claim_id: completed.claim_id.clone(),
                row_id: Some(batch_id.clone().into_boxed_str()),
                superseding_claim_id: authority
                    .as_ref()
                    .and_then(|(claim_id, _, _)| claim_id.clone())
                    .map(String::into_boxed_str),
                superseding_session_lease_generation: authority.as_ref().and_then(
                    |(claim_id, _, generation)| claim_id.as_ref().map(|_| Box::new(*generation)),
                ),
            });
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct PendingTurnInputRow {
    pub(crate) enqueue_seq: u64,
    pub(crate) input_id: String,
    session_id: String,
    source_key: Option<String>,
    ingress_json: String,
    state: lash_core::TurnInputState,
    input_json: String,
    enqueued_at_ms: u64,
    claim_id: Option<String>,
    pub(crate) claim_fencing_token: u64,
    claim_owner: Option<LeaseOwnerIdentity>,
    claim_token: Option<String>,
    claim_session_lease_generation: u64,
}

pub(crate) fn pending_turn_input_row(row: PgRow) -> Result<PendingTurnInputRow, StoreError> {
    let state = lash_core::TurnInputState::from_wire_str(row.get::<String, _>("state").as_str())
        .ok_or_else(|| StoreError::Backend("invalid pending turn-input state".to_string()))?;
    Ok(PendingTurnInputRow {
        enqueue_seq: u64_from_sql("PendingTurnInput", "enqueue_seq", row.get("enqueue_seq"))?,
        input_id: row.get("input_id"),
        session_id: row.get("session_id"),
        source_key: row.get("source_key"),
        ingress_json: row.get("ingress_json"),
        state,
        input_json: row.get("input_json"),
        enqueued_at_ms: u64_from_sql(
            "PendingTurnInput",
            "enqueued_at_ms",
            row.get("enqueued_at_ms"),
        )?,
        claim_id: row.get("claim_id"),
        claim_fencing_token: u64_from_sql(
            "PendingTurnInput",
            "claim_fencing_token",
            row.get("claim_fencing_token"),
        )?,
        claim_owner: lease_owner_from_columns(
            row.get("claim_owner_id"),
            row.get("claim_owner_incarnation_id"),
            row.get("claim_owner_liveness_json"),
        ),
        claim_token: row.get("claim_token"),
        claim_session_lease_generation: u64_from_sql(
            "PendingTurnInput",
            "claim_session_lease_generation",
            row.get("claim_session_lease_generation"),
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
        ingress: store_decode_json(&row.ingress_json, "turn-input ingress")?,
        state: row.state,
        enqueued_at_ms: row.enqueued_at_ms,
        input: store_decode_json(&row.input_json, "turn input")?,
    })
}

pub(crate) async fn load_pending_turn_input(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
    input_id: &str,
) -> Result<Option<lash_core::PendingTurnInput>, StoreError> {
    let row = sqlx::query(
        "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                claim_owner_id, claim_owner_incarnation_id,
                claim_owner_liveness_json, claim_token, claim_session_lease_generation
         FROM lash_pending_turn_inputs
         WHERE session_id = $1 AND input_id = $2",
    )
    .bind(session_id)
    .bind(input_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    row.map(pending_turn_input_row)
        .transpose()?
        .map(pending_turn_input_from_row)
        .transpose()
}

pub(crate) async fn load_pending_turn_input_row_by_target_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
    target: &lash_core::PendingTurnInputCancelTarget,
    for_update: bool,
) -> Result<Option<PendingTurnInputRow>, StoreError> {
    let for_update = if for_update { " FOR UPDATE" } else { "" };
    let row = match target {
        lash_core::PendingTurnInputCancelTarget::InputId(input_id) => sqlx::query(&format!(
            "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                        state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                        claim_owner_id, claim_owner_incarnation_id,
                        claim_owner_liveness_json, claim_token, claim_session_lease_generation
                 FROM lash_pending_turn_inputs
                 WHERE session_id = $1 AND input_id = $2{for_update}"
        ))
        .bind(session_id)
        .bind(input_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?,
        lash_core::PendingTurnInputCancelTarget::SourceKey(source_key) => sqlx::query(&format!(
            "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                        state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                        claim_owner_id, claim_owner_incarnation_id,
                        claim_owner_liveness_json, claim_token, claim_session_lease_generation
                 FROM lash_pending_turn_inputs
                 WHERE session_id = $1 AND source_key = $2{for_update}"
        ))
        .bind(session_id)
        .bind(source_key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?,
    };
    row.map(pending_turn_input_row).transpose()
}

fn pending_turn_input_claim_diagnostics_from_row(
    row: &PendingTurnInputRow,
) -> Option<lash_core::PendingTurnInputClaimDiagnostics> {
    (row.claim_token.is_some() || matches!(row.state, lash_core::TurnInputState::Accepted)).then(
        || lash_core::PendingTurnInputClaimDiagnostics {
            state: row.state,
            claim_id: row.claim_id.clone(),
            claim_owner: row.claim_owner.clone(),
            claim_session_lease_generation: row
                .claim_token
                .as_ref()
                .map(|_| row.claim_session_lease_generation),
            claim_fencing_token: row.claim_fencing_token,
        },
    )
}

pub(crate) async fn cancel_pending_turn_input_row_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    row: PendingTurnInputRow,
    now_epoch_ms: u64,
) -> Result<lash_core::PendingTurnInputCancelOutcome, StoreError> {
    let mut input = pending_turn_input_from_row(row.clone())?;
    match input.state {
        lash_core::TurnInputState::Cancelled => Ok(
            lash_core::PendingTurnInputCancelOutcome::AlreadyCancelled(input),
        ),
        lash_core::TurnInputState::Completed => Ok(
            lash_core::PendingTurnInputCancelOutcome::AlreadyCompleted(input),
        ),
        lash_core::TurnInputState::Accepted => {
            Ok(lash_core::PendingTurnInputCancelOutcome::AlreadyClaimed {
                input,
                claim: pending_turn_input_claim_diagnostics_from_row(&row),
            })
        }
        lash_core::TurnInputState::PendingActive | lash_core::TurnInputState::DeferredNextTurn => {
            // A claim is live only while the session-execution-lease generation it
            // pins still holds the session lease (ADR 0029).
            let live_claim = row.claim_token.is_some()
                && load_session_execution_lease_tx(tx, &row.session_id)
                    .await?
                    .is_some_and(|lease| {
                        lease.lease_token.is_some()
                            && lease.expires_at_ms > now_epoch_ms
                            && lease.fencing_token == row.claim_session_lease_generation
                    });
            if live_claim {
                return Ok(lash_core::PendingTurnInputCancelOutcome::AlreadyClaimed {
                    input,
                    claim: pending_turn_input_claim_diagnostics_from_row(&row),
                });
            }
            sqlx::query(
                "UPDATE lash_pending_turn_inputs
                 SET state = $3,
                     claim_id = NULL,
                     claim_owner_id = NULL,
                     claim_owner_incarnation_id = NULL,
                     claim_owner_liveness_json = NULL,
                     claim_token = NULL,
                     claim_session_lease_generation = 0
                 WHERE session_id = $1 AND input_id = $2",
            )
            .bind(&row.session_id)
            .bind(&row.input_id)
            .bind(lash_core::TurnInputState::Cancelled.as_str())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
            input.state = lash_core::TurnInputState::Cancelled;
            Ok(lash_core::PendingTurnInputCancelOutcome::Cancelled(input))
        }
    }
}

pub(crate) async fn ensure_turn_input_completion_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    completed: &lash_core::TurnInputCompletion,
) -> Result<(), StoreError> {
    for input_id in &completed.input_ids {
        let authority: Option<(Option<String>, Option<String>, i64)> = sqlx::query_as(
            "SELECT claim_id, claim_token, claim_session_lease_generation
             FROM lash_pending_turn_inputs
             WHERE session_id = $1
               AND input_id = $2
             LIMIT 1
             FOR UPDATE",
        )
        .bind(&completed.session_id)
        .bind(input_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        let authority = authority
            .map(|(claim_id, claim_token, generation)| {
                Ok((
                    claim_id,
                    claim_token,
                    u64_from_sql(
                        "PendingTurnInput",
                        "claim_session_lease_generation",
                        generation,
                    )?,
                ))
            })
            .transpose()?;
        let owns_row = authority
            .as_ref()
            .is_some_and(|(claim_id, claim_token, _)| {
                claim_id.as_deref() == Some(completed.claim_id.as_str())
                    && claim_token.as_deref() == Some(completed.lease_token.as_str())
            });
        if !owns_row {
            return Err(StoreError::TurnInputClaimSuperseded {
                session_id: completed.session_id.clone(),
                claim_id: completed.claim_id.clone(),
                row_id: Some(input_id.clone().into_boxed_str()),
                superseding_claim_id: authority
                    .as_ref()
                    .and_then(|(claim_id, _, _)| claim_id.clone())
                    .map(String::into_boxed_str),
                superseding_session_lease_generation: authority.as_ref().and_then(
                    |(claim_id, _, generation)| claim_id.as_ref().map(|_| Box::new(*generation)),
                ),
            });
        }
    }
    Ok(())
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
