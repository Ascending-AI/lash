use crate::*;

pub(crate) const QUEUED_WORK_COLUMNS: [&str; 14] = [
    "enqueue_seq",
    "batch_id",
    "session_id",
    "source_key",
    "delivery_policy",
    "work_kind",
    "authority_json",
    "merge_key",
    "available_at_ms",
    "enqueued_at_ms",
    "claim_fencing_token",
    "claim_token",
    "claim_session_lease_generation",
    "claim_id",
];

impl PostgresSessionStoreFactory {
    fn store_for(&self, session_id: SessionId) -> PostgresSessionStore {
        PostgresSessionStore {
            pool: self.pool.clone(),
            await_event_signing_secret: Arc::clone(&self.await_event_signing_secret),
            clock: Arc::clone(&self.clock),
            session_id,
            #[cfg(any(test, feature = "testing"))]
            lease_clock_for_testing: self.lease_clock_for_testing.clone(),
            #[cfg(test)]
            checkpoint_probe_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            checkpoint_write_transaction_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

impl PostgresSessionStoreFactory {
    /// Concrete constructor behind [`SessionStoreFactory::create_store`]; the
    /// gated conformance factory shares it.
    pub(crate) async fn create_session_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<PostgresSessionStore>, StoreError> {
        lash_core::store::validate_session_id(&request.session_id)?;
        let store = self.store_for(request.session_id.clone());
        let meta = SessionMeta {
            session_id: request.session_id.clone(),
            relation: request.relation.clone(),
            pending_observer_intents: request.pending_observer_intents.clone(),
        };
        let created_at_ms = self.clock.timestamp_ms();
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        crate::runtime_persistence::lock_session_history_mutation_tx(&mut tx, &request.session_id)
            .await?;
        let deleted = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(
                SELECT 1 FROM lash_deleted_sessions WHERE session_id = $1
             )",
        )
        .bind(request.session_id.as_str())
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
            created_at_ms,
        )
        .await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(Arc::new(store))
    }

    /// Concrete reopen behind [`SessionStoreFactory::open_existing_store`];
    /// the gated conformance factory shares it.
    pub(crate) async fn open_existing_session_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Option<Arc<PostgresSessionStore>>, String> {
        let store = self.store_for(request.session_id.clone());
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
}

#[async_trait::async_trait]
impl SessionStoreFactory for PostgresSessionStoreFactory {
    async fn reclaim_retained_evidence(
        &self,
        bound: lash_core::store::RetentionBound,
    ) -> lash_core::MaintenanceResult<lash_core::store::RetentionReport> {
        crate::evidence_retention::reclaim(self, bound)
            .await
            .map_err(|failure| *failure)
    }

    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn RuntimePersistence>, StoreError> {
        Ok(self.create_session_store(request).await? as Arc<dyn RuntimePersistence>)
    }

    async fn open_existing_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>, String> {
        lash_core::store::validate_session_id(&request.session_id)
            .map_err(|error| error.to_string())?;
        Ok(self
            .open_existing_session_store(request)
            .await?
            .map(|store| store as Arc<dyn RuntimePersistence>))
    }

    async fn open_existing_store_by_id(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>, String> {
        lash_core::store::validate_session_id(session_id).map_err(|error| error.to_string())?;
        let store = self.store_for(session_id.clone());
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

    async fn pending_turn_cancel_closure_pins(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core::TurnCancelClosureAuthorization>, StoreError> {
        self.store_for(session_id.clone())
            .pending_turn_cancel_closure_pins()
            .await
    }
    async fn retire_turn_cancel_closure_scope(
        &self,
        scope: &lash_core::ExecutionScope,
    ) -> Result<(), StoreError> {
        crate::turn_cancel_closure::retire_scope(&self.pool, scope).await
    }
    async fn has_claimable_queued_work(
        &self,
        request: &SessionStoreCreateRequest,
        now_epoch_ms: u64,
    ) -> Result<Option<bool>, StoreError> {
        lash_core::store::validate_session_id(&request.session_id)?;
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
        .bind(request.session_id.as_str())
        .bind(now_epoch_ms as i64)
        .bind(lash_core::TurnInputState::DeferredNextTurn.as_str())
        .fetch_one(&self.pool)
        .await
        .map(Some)
        .map_err(store_sqlx_error)
    }

    async fn session_was_deleted(&self, session_id: &SessionId) -> Result<bool, String> {
        lash_core::store::validate_session_id(session_id).map_err(|error| error.to_string())?;
        sqlx::query_scalar(
            "SELECT EXISTS(
                SELECT 1 FROM lash_deleted_sessions WHERE session_id = $1
             )",
        )
        .bind(session_id.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(|err| err.to_string())
    }

    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> lash_core::MaintenanceResult<lash_core::SessionBlobReclaimReport> {
        lash_core::store::validate_session_id(session_id)
            .map_err(lash_core::MaintenanceFailure::failed_before_any_work)?;
        let mut tx = self.pool.begin().await.map_err(|err| {
            lash_core::MaintenanceFailure::failed_before_any_work(store_sqlx_error(err))
        })?;
        let mut report = lash_core::SessionBlobReclaimReport::default();
        if let Err(error) = delete_session_tx(&mut tx, session_id, &mut report).await {
            report.deleted_blob_count = 0;
            return Err(lash_core::MaintenanceFailure::failed(error, report));
        }
        if let Err(error) = tx.commit().await {
            report.deleted_blob_count = 0;
            return Err(lash_core::MaintenanceFailure::failed(
                store_sqlx_error(error),
                report,
            ));
        }
        Ok(report)
    }

    async fn pin(&self, node_id: &str) -> Result<lash_core::ForkPoint, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        let (source_session_id, checkpoint_ref) =
            crate::support::retained_checkpoint_tx(&mut tx, node_id)
                .await?
                .ok_or_else(|| StoreError::ForkPointNotRetained {
                    node_id: node_id.to_string(),
                })?;
        crate::runtime_persistence::lock_session_history_mutation_tx(&mut tx, &source_session_id)
            .await?;
        crate::support::lock_checkpoint_blob_tx(&mut tx, &checkpoint_ref, None).await?;
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
            let config = crate::support::retained_fork_config_tx(&mut tx, node_id).await?;
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(lash_core::ForkPoint {
                node_id: node_id.to_string(),
                checkpoint_ref: checkpoint_ref.into(),
                source_session_id: SessionId::from(source_session_id),
                config,
                pinned: true,
            });
        }
        if !crate::support::retention_source_holds_checkpoint_tx(
            &mut tx,
            node_id,
            &source_session_id,
            &checkpoint_ref,
        )
        .await?
        {
            return Err(StoreError::ForkPointNotRetained {
                node_id: node_id.to_string(),
            });
        }
        sqlx::query(
            "INSERT INTO lash_node_anchors (node_id, checkpoint_ref, source_session_id)
             VALUES ($1, $2, $3)",
        )
        .bind(node_id)
        .bind(&checkpoint_ref)
        .bind(source_session_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let config = crate::support::retained_fork_config_tx(&mut tx, node_id).await?;
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
                config: crate::support::retained_fork_config_tx(&mut tx, &node_id).await?,
                node_id,
                checkpoint_ref: BlobRef(row.get(1)),
                source_session_id: SessionId::from(row.get::<String, _>(2)),
                pinned: row.get(3),
            });
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(points)
    }

    async fn fork_at(
        &self,
        request: &lash_core::ForkSessionRequest,
    ) -> Result<lash_core::ForkSessionReceipt, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        // Target identity fences precede source-retention fences. This unlocked
        // fast path only decides already-materialized targets and permanent
        // tombstones; keep the post-lock checks below for concurrent changes.
        let (exists, deleted) = sqlx::query_as::<_, (bool, bool)>(
            "SELECT
                EXISTS(
                    SELECT 1 FROM lash_sessions WHERE session_id = $1
                    UNION ALL
                    SELECT 1 FROM lash_session_meta WHERE session_id = $1
                ),
                EXISTS(
                    SELECT 1 FROM lash_deleted_sessions WHERE session_id = $1
                )",
        )
        .bind(request.session_id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        if exists {
            return Err(StoreError::ForkSessionAlreadyExists {
                session_id: request.session_id.clone(),
            });
        }
        if deleted {
            return Err(StoreError::SessionDeleted {
                session_id: request.session_id.clone(),
            });
        }
        let (source_session_id, checkpoint_ref) =
            crate::support::retained_checkpoint_tx(&mut tx, &request.node_id)
                .await?
                .ok_or_else(|| StoreError::ForkPointNotRetained {
                    node_id: request.node_id.clone(),
                })?;
        let session_ids = vec![request.session_id.clone(), source_session_id.clone()];
        crate::runtime_persistence::lock_session_history_mutations_tx(&mut tx, &session_ids)
            .await?;
        // Keep the fork fences in the global order: every session advisory
        // fence first, then the retained checkpoint root, then graph and head.
        let exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(
                 SELECT 1 FROM lash_sessions WHERE session_id = $1
                 UNION ALL
                 SELECT 1 FROM lash_session_meta WHERE session_id = $1
             )",
        )
        .bind(request.session_id.as_str())
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
        .bind(request.session_id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        if deleted {
            return Err(StoreError::SessionDeleted {
                session_id: request.session_id.clone(),
            });
        }
        crate::support::lock_checkpoint_blob_tx(&mut tx, &checkpoint_ref, None).await?;
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
        if !crate::support::retention_source_holds_checkpoint_tx(
            &mut tx,
            &request.node_id,
            &source_session_id,
            &checkpoint_ref,
        )
        .await?
        {
            return Err(StoreError::ForkPointNotRetained {
                node_id: request.node_id.clone(),
            });
        }
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
                owning_session_id: SessionId::from(facts.2),
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
                current_frame_node_id: Some(
                    serde_json::from_value(serde_json::Value::String(current_frame_node_id))
                        .expect("a persisted frame node id is a transparent string"),
                ),
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
        .bind(request.session_id.as_str())
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
            .bind(ancestor.ancestor_session_id.as_str())
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
            pending_observer_intents: request.pending_observer_intents.clone(),
        };
        let created_at_ms = self.clock.timestamp_ms();
        crate::session_meta::write_session_meta_tx(
            &mut tx,
            &meta,
            crate::session_meta::SessionMetaWrite::Insert,
            created_at_ms,
        )
        .await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(lash_core::ForkSessionReceipt {
            session_id: request.session_id.clone(),
            node_id: request.node_id.clone(),
            source_session_id,
            observed_processes: Vec::new(),
        })
    }

    async fn list_sessions(
        &self,
        filter: &SessionListFilter,
    ) -> Result<Vec<SessionSummary>, StoreError> {
        crate::session_catalog::list_sessions(&self.pool, filter).await
    }

    async fn read_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<lash_core::SessionReadView>, StoreError> {
        lash_core::store::validate_session_id(session_id)?;
        let store = self.store_for(session_id.clone());
        lash_core::store::load_persisted_session_read_view(&store).await
    }
}

impl PostgresSessionStoreFactory {
    /// The read-only delete-time root predicate for one digest, parameterised
    /// `$1 = attachment_id`, `$2 = intent_grace_cutoff_ms`. A ref is live unless
    /// it is eligible for the same conditional forget reconciliation applies.
    /// The targeted probe and the condemn CAS share it so the fence and the
    /// probe cannot drift apart.
    fn live_attachment_ref_sql(&self) -> String {
        crate::attachments::live_attachment_ref_sql(self.process_registry_shared)
    }
}

#[async_trait::async_trait]
impl lash_core::AttachmentRootSet for PostgresSessionStoreFactory {
    fn can_prove_process_owner_death(&self) -> bool {
        self.process_registry_shared
    }

    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<lash_core::AttachmentId>, lash_core::StoreError> {
        // Age is only a post-terminal retention policy. This single DELETE
        // composes age with durable owner-death proof: a later committed turn
        // supersedes a turn owner, a missing process row proves a process owner
        // was pruned, and only unscoped host puts use age alone.
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        sqlx::query(crate::attachments::RECLAIM_DELETED_ATTACHMENT_ROOTS)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let delete_sql = crate::attachments::forget_aged_uncommitted_attachment_intents_sql(
            self.process_registry_shared,
        );
        let cutoff = clamp_epoch_ms(intent_grace_cutoff_epoch_ms);
        sqlx::query(&delete_sql)
            .bind(cutoff)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let rows = sqlx::query("SELECT DISTINCT attachment_id FROM lash_attachment_manifest")
            .fetch_all(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        rows.into_iter()
            .map(|row| attachment_id_from_sql("AttachmentManifest", "attachment_id", row.get(0)))
            .collect()
    }

    async fn has_live_attachment_ref(
        &self,
        id: &lash_core::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, lash_core::StoreError> {
        let cutoff = clamp_epoch_ms(intent_grace_cutoff_epoch_ms);
        let row = sqlx::query(&self.live_attachment_ref_sql())
            .bind(id.as_str())
            .bind(cutoff)
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
        let cutoff = clamp_epoch_ms(intent_grace_cutoff_epoch_ms);
        let rooted = sqlx::query(&self.live_attachment_ref_sql())
            .bind(id.as_str())
            .bind(cutoff)
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
        // Under the same per-digest advisory key the writer half takes, and in a
        // transaction: a bare pooled UPDATE could commit *inside* a writer's
        // open `begin_attachment_write` — after it read `condemned` and before
        // it deleted the row — leaving the writer to erase a `deleting` row and
        // put bytes into an in-flight delete.
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        crate::attachments::lock_attachment_fence_tx(&mut tx, id.as_str()).await?;
        let armed = sqlx::query(
            "UPDATE lash_attachment_condemnations SET phase = 'deleting'
             WHERE attachment_id = $1 AND phase = 'condemned' AND write_token IS NULL",
        )
        .bind(id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
        tx.commit().await.map_err(store_sqlx_error)?;
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
        crate::attachments::release_attachment_condemnation(&self.pool, id.as_str()).await
    }

    async fn recover_abandoned_attachment_write(
        &self,
        id: &lash_core::AttachmentId,
    ) -> Result<(), lash_core::StoreError> {
        crate::attachments::recover_abandoned_attachment_write(&self.pool, id.as_str()).await
    }

    async fn reclaim_attachment_condemnation(
        &self,
        id: &lash_core::AttachmentId,
    ) -> Result<(), lash_core::StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        crate::attachments::lock_attachment_fence_tx(&mut tx, id.as_str()).await?;
        sqlx::query(
            "UPDATE lash_attachment_condemnations SET phase = 'reclaimed'
             WHERE attachment_id = $1 AND phase = 'deleting'",
        )
        .bind(id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(())
    }
}

pub(crate) async fn delete_session_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    report: &mut lash_core::SessionBlobReclaimReport,
) -> Result<(), StoreError> {
    crate::runtime_persistence::lock_session_history_mutation_tx(tx, session_id).await?;
    crate::turn_cancel_closure::ensure_session_not_pinned_tx(tx, session_id).await?;
    let materialized = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
             SELECT 1 FROM lash_session_meta WHERE session_id = $1
             UNION ALL
             SELECT 1 FROM lash_sessions WHERE session_id = $1
         )",
    )
    .bind(session_id.as_str())
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    if materialized {
        // Permanent identity evidence for host-facing session ids.
        sqlx::query(
            "INSERT INTO lash_deleted_sessions
             (session_id, created_at_ms, last_commit_at_ms, head_revision,
              relation_kind, parent_session_id)
             SELECT meta.session_id, meta.created_at_ms, meta.last_commit_at_ms,
                    COALESCE(session.head_revision, 0), meta.relation_kind,
                    meta.parent_session_id
             FROM lash_session_meta AS meta
             LEFT JOIN lash_sessions AS session ON session.session_id = meta.session_id
             WHERE meta.session_id = $1
             ON CONFLICT (session_id) DO NOTHING",
        )
        .bind(session_id.as_str())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        sqlx::query(
            "INSERT INTO lash_deleted_sessions
             (session_id, created_at_ms, last_commit_at_ms, head_revision,
              relation_kind, parent_session_id)
             VALUES ($1, 0, NULL, 0, 'root', NULL)
             ON CONFLICT (session_id) DO NOTHING",
        )
        .bind(session_id.as_str())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    }
    // Attachment intents are released before the rest of the session store so
    // a failed transaction cannot leave live-looking state without its owner.
    // The session advisory fence stabilizes this head read. Do not take its
    // row lock before the complete hash-sorted blob candidate set below.
    let head = sqlx::query_as::<_, (Option<String>, Option<String>)>(
        "SELECT leaf_node_id, checkpoint_ref FROM lash_sessions
         WHERE session_id = $1",
    )
    .bind(session_id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let (leaf_node_id, checkpoint_ref) = head.unwrap_or((None, None));
    let mut checkpoint_refs = std::collections::BTreeSet::new();
    if let Some(checkpoint_ref) = checkpoint_ref.as_deref() {
        checkpoint_refs.insert(checkpoint_ref.to_string());
    }
    let candidates =
        crate::session_blob_reclaim::enumerate_checkpoint_blob_candidates_tx(tx, &checkpoint_refs)
            .await?;
    crate::session_blob_reclaim::lock_session_blob_candidates_tx(
        tx,
        &candidates,
        &format!("session `{session_id}`"),
    )
    .await?;
    report.enumerated_blob_count = candidates.len();
    sqlx::query("DELETE FROM lash_sessions WHERE session_id = $1")
        .bind(session_id.as_str())
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
         ORDER BY g.generation DESC",
    )
    .bind(session_id.as_str())
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    for node_id in unreachable_candidates {
        crate::runtime_persistence::retire_unreachable_ancestry_tx(tx, &node_id).await?;
    }
    // Delete-time reclaim covers this session's tombstoned rows plus any
    // tombstoned row owned by an already-deleted session. A node can be
    // tombstoned *after* its owner is gone (unpin of a pinned leaf whose session
    // was deleted, or ancestry retired at a fork child's delete), and no
    // session-scoped vacuum could ever reach it: the owning id is permanently
    // unbindable. Live sessions' rows stay resident for their own vacuum, so
    // this is not a catalog-wide sweep.
    sqlx::query(
        "DELETE FROM lash_graph_nodes
         WHERE tombstoned = TRUE
           AND (session_id = $1
                OR session_id IN (SELECT session_id FROM lash_deleted_sessions))",
    )
    .bind(session_id.as_str())
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    for sql in [
        "DELETE FROM lash_queued_work_items WHERE batch_id IN (SELECT batch_id FROM lash_queued_work_batches WHERE session_id = $1)",
        "DELETE FROM lash_queued_work_batches WHERE session_id = $1",
        "DELETE FROM lash_wake_redelivery_fences WHERE session_id = $1",
        "DELETE FROM lash_wake_allocation_floors WHERE target_session_id = $1",
        "DELETE FROM lash_pending_turn_inputs WHERE session_id = $1",
        "DELETE FROM lash_turn_cancel_requests WHERE session_id = $1",
        // Administration revokes the session's effect authority before store
        // deletion, after which the pinned closure obligation may be retired.
        "DELETE FROM lash_turn_cancel_closure_authorizations WHERE session_id = $1",
        "DELETE FROM lash_turn_cancellation_bindings WHERE session_id = $1",
        "DELETE FROM lash_session_execution_leases WHERE session_id = $1",
        "DELETE FROM lash_fork_lineage WHERE session_id = $1",
        "DELETE FROM lash_session_meta WHERE session_id = $1",
    ] {
        sqlx::query(sql)
            .bind(session_id.as_str())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    }
    sqlx::query(crate::attachments::RECLAIM_DELETED_ATTACHMENT_ROOTS)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    crate::session_blob_reclaim::reclaim_session_checkpoint_blobs_tx(
        tx,
        candidates,
        &checkpoint_refs,
        report,
    )
    .await
}

/// Deletes process-owned runtime sessions as one batch inside the process
/// prune transaction.
///
/// The batch obeys the same two laws as a single-session delete: every
/// materialized id it removes joins the permanent deleted set, and the reclaim
/// arm drops tombstoned rows owned by any already-deleted session, not only by
/// the batch. Process runtime session ids are lash-minted, but they are just as
/// unbindable as host-facing ids once deleted, so a row left tombstoned under
/// one of them could never be reached by a session-scoped vacuum again.
pub(crate) async fn delete_process_sessions_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_ids: &[SessionId],
) -> lash_core::MaintenanceResult<lash_core::SessionBlobReclaimReport> {
    if session_ids.is_empty() {
        return Ok(lash_core::SessionBlobReclaimReport::default());
    }
    let session_id_texts: Vec<_> = session_ids.iter().map(SessionId::as_str).collect();
    let mut report = lash_core::SessionBlobReclaimReport::default();
    let outcome: Result<(), StoreError> = async {
        crate::runtime_persistence::lock_session_history_mutations_tx(tx, session_ids).await?;
        crate::turn_cancel_closure::ensure_sessions_not_pinned_tx(tx, session_ids).await?;
        let checkpoint_refs = sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT checkpoint_ref
         FROM lash_sessions
         WHERE session_id = ANY($1) AND checkpoint_ref IS NOT NULL
         ORDER BY checkpoint_ref",
        )
        .bind(&session_id_texts[..])
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
        let candidates = crate::session_blob_reclaim::enumerate_checkpoint_blob_candidates_tx(
            tx,
            &checkpoint_refs,
        )
        .await?;
        crate::session_blob_reclaim::lock_session_blob_candidates_tx(
            tx,
            &candidates,
            "process-prune session batch",
        )
        .await?;
        report.enumerated_blob_count = candidates.len();

        // Record permanent identity before deletion so reclaim can see it.
        sqlx::query(
            "INSERT INTO lash_deleted_sessions
         (session_id, created_at_ms, last_commit_at_ms, head_revision,
          relation_kind, parent_session_id)
         SELECT target.session_id, COALESCE(meta.created_at_ms, 0),
                meta.last_commit_at_ms, COALESCE(session.head_revision, 0),
                COALESCE(meta.relation_kind, 'root'), meta.parent_session_id
         FROM unnest($1::TEXT[]) AS target(session_id)
         LEFT JOIN lash_session_meta AS meta ON meta.session_id = target.session_id
         LEFT JOIN lash_sessions AS session ON session.session_id = target.session_id
         WHERE EXISTS (
                   SELECT 1 FROM lash_session_meta AS meta
                   WHERE meta.session_id = target.session_id
               )
            OR EXISTS (
                   SELECT 1 FROM lash_sessions AS session
                   WHERE session.session_id = target.session_id
               )
         ON CONFLICT (session_id) DO NOTHING",
        )
        .bind(&session_id_texts[..])
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;

        let (deleted_leaf_node_ids, has_graph_candidates) =
            sqlx::query_as::<_, (Vec<String>, bool)>(
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
            .bind(&session_id_texts[..])
            .fetch_one(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;

        if has_graph_candidates {
            for leaf_node_id in deleted_leaf_node_ids {
                crate::runtime_persistence::retire_unreachable_ancestry_tx(tx, &leaf_node_id)
                    .await?;
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
             ORDER BY graph.session_id, graph.generation DESC",
            )
            .bind(&session_id_texts[..])
            .fetch_all(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
            for node_id in unreachable_candidates {
                crate::runtime_persistence::retire_unreachable_ancestry_tx(tx, &node_id).await?;
            }
        }

        sqlx::query(
            // Delete-time reclaim covers the batch's tombstoned rows plus any
            // tombstoned row owned by an already-deleted session. The ancestry
            // retire above tombstones a node regardless of who owns it, so a batch
            // can strand a row belonging to a session outside it; that owner is
            // unbindable, so no session-scoped vacuum could ever reach the row.
            // Live sessions' rows stay resident for their own vacuum, so this is
            // not a catalog-wide sweep.
            "WITH deleted_graph_nodes AS (
             DELETE FROM lash_graph_nodes
             WHERE tombstoned = TRUE
               AND (session_id = ANY($1)
                    OR session_id IN (SELECT session_id FROM lash_deleted_sessions))
             RETURNING node_id
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
         deleted_turn_cancel_requests AS (
             DELETE FROM lash_turn_cancel_requests
             WHERE session_id = ANY($1)
             RETURNING session_id
         ),
         deleted_turn_cancel_closures AS (
             DELETE FROM lash_turn_cancel_closure_authorizations
             WHERE session_id = ANY($1)
             RETURNING session_id
         ),
         deleted_turn_cancellation_bindings AS (
             DELETE FROM lash_turn_cancellation_bindings
             WHERE session_id = ANY($1)
             RETURNING session_id
         ),
         deleted_session_execution_leases AS (
             DELETE FROM lash_session_execution_leases
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
         )
         SELECT (SELECT count(*) FROM deleted_graph_nodes)
              + (SELECT count(*) FROM deleted_queued_work_batches)
              + (SELECT count(*) FROM deleted_wake_redelivery_fences)
              + (SELECT count(*) FROM deleted_wake_allocation_floors)
              + (SELECT count(*) FROM deleted_pending_turn_inputs)
              + (SELECT count(*) FROM deleted_turn_cancel_closures)
              + (SELECT count(*) FROM deleted_turn_cancellation_bindings)
              + (SELECT count(*) FROM deleted_session_execution_leases)
              + (SELECT count(*) FROM deleted_fork_lineage)
              + (SELECT count(*) FROM deleted_session_meta)",
        )
        .bind(&session_id_texts[..])
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;

        sqlx::query(crate::attachments::RECLAIM_DELETED_ATTACHMENT_ROOTS)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        crate::session_blob_reclaim::reclaim_session_checkpoint_blobs_tx(
            tx,
            candidates,
            &checkpoint_refs,
            &mut report,
        )
        .await?;
        Ok(())
    }
    .await;
    match outcome {
        Ok(()) => Ok(report),
        Err(error) => {
            // The caller owns the transaction and rolls it back on this stop;
            // no physical delete in the partial report can survive.
            report.deleted_blob_count = 0;
            Err(lash_core::MaintenanceFailure::failed(error, report))
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct QueuedBatchRow {
    pub(crate) enqueue_seq: u64,
    pub(crate) batch_id: String,
    session_id: SessionId,
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
) -> ClaimCandidate {
    ClaimCandidate::from_batch(
        batch,
        row.claim_fencing_token,
        row.claim_id.clone(),
        row.claim_token.clone(),
    )
}

pub(crate) fn queued_batch_row(row: PgRow) -> Result<QueuedBatchRow, StoreError> {
    let delivery_policy =
        DeliveryPolicy::from_wire_str(row.get::<String, _>(QUEUED_WORK_COLUMNS[4]).as_str())
            .ok_or_else(|| {
                StoreError::Backend("invalid queued work delivery policy".to_string())
            })?;
    let kind = QueuedWorkKind::from_wire_str(row.get::<String, _>(QUEUED_WORK_COLUMNS[5]).as_str())
        .ok_or_else(|| StoreError::Backend("invalid queued work kind".to_string()))?;
    let authority_json: String = row.get(QUEUED_WORK_COLUMNS[6]);
    Ok(QueuedBatchRow {
        enqueue_seq: u64_from_sql(
            "QueuedWorkBatch",
            "enqueue_seq",
            row.get(QUEUED_WORK_COLUMNS[0]),
        )?,
        batch_id: row.get(QUEUED_WORK_COLUMNS[1]),
        session_id: SessionId::from(row.get::<String, _>(QUEUED_WORK_COLUMNS[2])),
        source_key: row.get(QUEUED_WORK_COLUMNS[3]),
        delivery_policy,
        kind,
        authority: store_decode_json(&authority_json, "queued work authority")?,
        merge_key: row.get(QUEUED_WORK_COLUMNS[7]),
        available_at_ms: u64_from_sql(
            "QueuedWorkBatch",
            "available_at_ms",
            row.get(QUEUED_WORK_COLUMNS[8]),
        )?,
        enqueued_at_ms: u64_from_sql(
            "QueuedWorkBatch",
            "enqueued_at_ms",
            row.get(QUEUED_WORK_COLUMNS[9]),
        )?,
        claim_fencing_token: u64_from_sql(
            "QueuedWorkBatch",
            "claim_fencing_token",
            row.get(QUEUED_WORK_COLUMNS[10]),
        )?,
        claim_id: row.get(QUEUED_WORK_COLUMNS[13]),
        claim_token: row.get(QUEUED_WORK_COLUMNS[11]),
        claim_session_lease_generation: u64_from_sql(
            "QueuedWorkBatch",
            "claim_session_lease_generation",
            row.get(QUEUED_WORK_COLUMNS[12]),
        )?,
    })
}

pub(crate) async fn load_queued_batch(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    batch_id: &str,
) -> Result<Option<QueuedWorkBatch>, StoreError> {
    let row = sqlx::query(&format!(
        "SELECT {QUEUED_WORK_COLUMNS}
         FROM lash_queued_work_batches
         WHERE batch_id = $1",
        QUEUED_WORK_COLUMNS = QUEUED_WORK_COLUMNS.join(", ")
    ))
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
    let batch = QueuedWorkBatch {
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
    };
    batch.validate_payload_family()?;
    Ok(batch)
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
        .bind(completed.session_id.as_str())
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
    session_id: SessionId,
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
        session_id: SessionId::from(row.get::<String, _>("session_id")),
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
        )?,
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
    session_id: &SessionId,
    input_id: &str,
) -> Result<Option<lash_core::PendingTurnInput>, StoreError> {
    let row = sqlx::query(
        "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                claim_owner_id, claim_owner_incarnation_id,
                claim_token, claim_session_lease_generation
         FROM lash_pending_turn_inputs
         WHERE session_id = $1 AND input_id = $2",
    )
    .bind(session_id.as_str())
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
    session_id: &SessionId,
    target: &lash_core::PendingTurnInputCancelTarget,
    for_update: bool,
) -> Result<Option<PendingTurnInputRow>, StoreError> {
    let for_update = if for_update { " FOR UPDATE" } else { "" };
    let row = match target {
        lash_core::PendingTurnInputCancelTarget::InputId(input_id) => sqlx::query(&format!(
            "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                        state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                        claim_owner_id, claim_owner_incarnation_id,
                        claim_token, claim_session_lease_generation
                 FROM lash_pending_turn_inputs
                 WHERE session_id = $1 AND input_id = $2{for_update}"
        ))
        .bind(session_id.as_str())
        .bind(input_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?,
        lash_core::PendingTurnInputCancelTarget::SourceKey(source_key) => sqlx::query(&format!(
            "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                        state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                        claim_owner_id, claim_owner_incarnation_id,
                        claim_token, claim_session_lease_generation
                 FROM lash_pending_turn_inputs
                 WHERE session_id = $1 AND source_key = $2{for_update}"
        ))
        .bind(session_id.as_str())
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
                     claim_token = NULL,
                     claim_session_lease_generation = 0
                 WHERE session_id = $1 AND input_id = $2",
            )
            .bind(row.session_id.as_str())
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
        session_id: &SessionId,
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
