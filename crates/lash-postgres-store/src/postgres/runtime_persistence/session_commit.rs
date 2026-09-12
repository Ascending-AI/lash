use super::*;

#[async_trait::async_trait]
impl SessionCommitStore for PostgresSessionStore {
    async fn read_session_state_version(&self) -> Result<u32, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        let version = read_session_state_version_tx(&mut tx, &self.session_id, false).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(version)
    }

    async fn admit_session_state(
        &self,
        lease: &SessionExecutionLeaseAuthority,
    ) -> Result<lash_core::store::SessionStateAdmission, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        ensure_session_execution_lease_tx(&mut tx, &lease.session_id, lease).await?;
        let version = read_session_state_version_tx(&mut tx, &lease.session_id, true).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(lash_core::store::SessionStateAdmission {
            session_id: lease.session_id.clone(),
            version,
            lease_fencing_token: lease.fencing_token,
        })
    }

    async fn load_session(&self) -> Result<Option<PersistedSessionRead>, StoreError> {
        let session_id = &self.session_id;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        read_session_state_version_tx(&mut tx, session_id, false).await?;
        let Some(meta) = load_session_head_meta_tx(&mut tx, session_id, false).await? else {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(None);
        };
        let leaf_node_id = meta.leaf_node_id.clone();
        let graph = load_graph_tx(&mut tx, session_id, leaf_node_id.clone()).await?;
        let checkpoint = match meta.checkpoint_ref.as_ref() {
            Some(blob_ref) => get_checkpoint_tx(&mut tx, blob_ref).await?,
            None => None,
        };
        let token_ledger = lash_core::store::merge_token_ledger_entries_checked(
            load_usage_deltas_tx(&mut tx, session_id).await?,
        )?;
        let turn_failure_rows = sqlx::query(LOAD_TURN_FAILURE_SETTLEMENTS_SQL)
            .bind(session_id.as_str())
            .fetch_all(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let mut turn_failure_settlements = Vec::new();
        for row in turn_failure_rows {
            let turn_id = row.get::<String, _>("turn_id");
            let result_json = row.get::<String, _>("result_json");
            let receipt: RuntimeCommitReceipt = match serde_json::from_str(&result_json) {
                Ok(receipt) => receipt,
                Err(error) => {
                    tracing::warn!(
                        target: "lash_postgres_store::runtime_persistence",
                        session_id = session_id.as_str(),
                        turn_id = turn_id.as_str(),
                        error = %error,
                        "skipping corrupt runtime turn receipt while loading failure evidence"
                    );
                    continue;
                }
            };
            if !receipt.failure_evidence.is_empty() {
                turn_failure_settlements.push(lash_core::TurnFailureSettlement {
                    turn_id,
                    evidence: receipt.failure_evidence,
                });
            }
        }
        let read = PersistedSessionRead {
            session_id: meta.session_id,
            head_revision: meta.head_revision,
            config: meta.config,
            current_frame_node_id: meta.current_frame_node_id,
            graph,
            checkpoint_ref: meta.checkpoint_ref,
            checkpoint,
            token_ledger,
            turn_failure_settlements,
        };
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(Some(read))
    }

    async fn load_session_head_meta(&self) -> Result<Option<SessionHeadMeta>, StoreError> {
        self.read_session_state_version().await?;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        let meta = load_session_head_meta_tx(&mut tx, &self.session_id, false).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(meta)
    }

    /// FIG-653: fork-lineage visibility is graph membership, not authorization.
    async fn load_node(&self, node_id: &str) -> Result<Option<SessionNodeRecord>, StoreError> {
        let session_id = &self.session_id;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let row = sqlx::query(
            "SELECT node.node_id, node.parent_node_id, node.node_json,
                    node.session_id, node.generation
             FROM lash_graph_nodes AS node
             WHERE node.node_id = $1 AND node.tombstoned = FALSE
               AND (
                   node.session_id = $2
                   OR EXISTS (
                       SELECT 1 FROM lash_fork_lineage AS lineage
                       WHERE lineage.session_id = $2
                         AND lineage.ancestor_session_id = node.session_id
                         AND node.generation <= lineage.fork_generation
                   )
               )",
        )
        .bind(node_id)
        .bind(session_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let Some(row) = row else {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(None);
        };
        let candidate_id: String = row.get(0);
        let parent_node_id: Option<String> = row.get(1);
        let json: String = row.get(2);
        let owner: String = row.get(3);
        let candidate_generation: i64 = row.get(4);
        if owner != *session_id {
            let rows = sqlx::query(
                "WITH readable_sessions(session_id, generation_ceiling) AS (
                     SELECT $1::TEXT, NULL::BIGINT
                     UNION ALL
                     SELECT lineage.ancestor_session_id, lineage.fork_generation
                     FROM lash_fork_lineage AS lineage
                     WHERE lineage.session_id = $1
                 )
                 SELECT session.leaf_node_id, head.generation, head.tombstoned,
                        node.node_id, node.parent_node_id,
                        node.generation, node.tombstoned
                 FROM lash_sessions AS session
                 LEFT JOIN lash_graph_nodes AS head
                   ON head.node_id = session.leaf_node_id
                 LEFT JOIN readable_sessions AS readable ON TRUE
                 LEFT JOIN lash_graph_nodes AS node
                   ON node.session_id = readable.session_id
                  AND node.generation BETWEEN $2 AND head.generation
                  AND (
                      readable.generation_ceiling IS NULL
                      OR node.generation <= readable.generation_ceiling
                  )
                 WHERE session.session_id = $1",
            )
            .bind(session_id.as_str())
            .bind(candidate_generation)
            .fetch_all(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            let Some(first) = rows.first() else {
                tx.commit().await.map_err(store_sqlx_error)?;
                return Ok(None);
            };
            let head_id: Option<String> = first.get(0);
            let Some(head_id) = head_id else {
                tx.commit().await.map_err(store_sqlx_error)?;
                return Ok(None);
            };
            let head_generation: Option<i64> = first.get(1);
            let head_tombstoned: Option<bool> = first.get(2);
            let (Some(head_generation), Some(head_tombstoned)) = (head_generation, head_tombstoned)
            else {
                return Err(StoreError::StoredDataCorrupt {
                    record_kind: "SessionGraph",
                    message: "head leaf is missing".to_string(),
                });
            };
            if head_tombstoned {
                return Err(StoreError::StoredDataCorrupt {
                    record_kind: "SessionGraph",
                    message: "head leaf is tombstoned".to_string(),
                });
            }
            if candidate_generation < 0 || candidate_generation > head_generation {
                tx.commit().await.map_err(store_sqlx_error)?;
                return Ok(None);
            }
            let mut range = std::collections::HashMap::new();
            for row in rows {
                let node_id: Option<String> = row.get(3);
                let generation: Option<i64> = row.get(5);
                let tombstoned: Option<bool> = row.get(6);
                if let (Some(node_id), Some(generation), Some(tombstoned)) =
                    (node_id, generation, tombstoned)
                {
                    range.insert(
                        node_id,
                        (row.get::<Option<String>, _>(4), generation, tombstoned),
                    );
                }
            }
            let mut current_id = head_id;
            let mut current_generation = head_generation;
            loop {
                let Some((parent_id, generation, tombstoned)) = range.get(&current_id) else {
                    return Err(StoreError::StoredDataCorrupt {
                        record_kind: "SessionGraph",
                        message: "readable generation range omits an edge-path node".to_string(),
                    });
                };
                if *tombstoned || *generation != current_generation {
                    return Err(StoreError::StoredDataCorrupt {
                        record_kind: "SessionGraph",
                        message: "parent edge crosses a tombstone or generation gap".to_string(),
                    });
                }
                if current_generation == candidate_generation {
                    if current_id != candidate_id {
                        tx.commit().await.map_err(store_sqlx_error)?;
                        return Ok(None);
                    }
                    break;
                }
                current_id = parent_id
                    .clone()
                    .ok_or_else(|| StoreError::StoredDataCorrupt {
                        record_kind: "SessionGraph",
                        message: "parent edge ended before the candidate generation".to_string(),
                    })?;
                current_generation -= 1;
            }
        }
        let node = SessionNodeRecord::decode_storage_body(candidate_id, parent_node_id, &json)
            .map_err(|err| StoreError::StoredDataCorrupt {
                record_kind: "SessionGraph node",
                message: err.to_string(),
            })?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(Some(node))
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError> {
        let planner = lash_core::store::RuntimeCommitPlanner::prepare(commit)?;
        let commit = planner.commit();
        self.bind_session_id(&commit.session_id)?;
        let now = self.clock.timestamp_ms();
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        // A head row does not exist during the first commit, so row locking
        // alone cannot serialize create-versus-delete. This session-keyed lock
        // is the common authority for every history commit and deletion.
        ensure_session_not_deleted_tx(&mut tx, &commit.session_id).await?;
        if commit.turn_cancel_closure_settlement.is_none()
            && let Some(fence) = commit.session_execution_lease_fence.as_ref()
        {
            ensure_session_execution_lease_tx(&mut tx, &commit.session_id, fence).await?;
        }
        // Read without a lock for early validation and receipt replay. Before
        // mutating graph reachability, existing sessions lock and recheck this
        // revision so commit, maintenance, and deletion share one authority.
        let existing = load_session_head_meta_tx(&mut tx, &commit.session_id, false).await?;
        planner.validate_session_binding(existing.as_ref().map(|meta| &meta.session_id))?;
        let direct_meta = SessionMeta {
            session_id: commit.session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            pending_observer_intents: Vec::new(),
        };
        planner.validate_node_derivation()?;
        {
            let prior = sqlx::query(
                "SELECT turn_commit_hash, result_json,
                        request_identity_hash, identity_encoding_version,
                        requested_node_count
                 FROM lash_runtime_turn_commits
                 WHERE session_id = $1 AND turn_id = $2",
            )
            .bind(commit.session_id.as_str())
            .bind(planner.operation_key())
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            if let Some(row) = prior {
                let hash: String = row.get(0);
                let result_json: String = row.get(1);
                let stored_identity: Option<String> = row.get(2);
                let stored_version: Option<i32> = row.get(3);
                let stored_requested_node_count: Option<i64> = row.get(4);
                // The shared codec owns both unit-shape and integer-range validation.
                // In particular, a negative PostgreSQL INTEGER cannot become legacy replay.
                // The ancestor column intentionally stays outside this receipt SELECT.
                // Its semantic value is already bound by the stored request hash.
                // Fresh-append ancestor fencing continues below, after receipt adjudication.
                let append_request_identity =
                    lash_core::store_backend_support::decode_append_request_identity(
                        &commit.turn_commit.operation.key,
                        stored_identity,
                        stored_version.map(i64::from),
                        stored_requested_node_count,
                    )?;
                let result = store_decode_json(&result_json, "runtime turn commit result")?;
                let prior = lash_core::store::RuntimeCommitReceiptRecord {
                    turn_commit_hash: hash,
                    result,
                    append_request_identity,
                };
                if let Some(replay) = planner.decide_receipt(Some(prior))? {
                    crate::session_meta::write_session_meta_tx(
                        &mut tx,
                        &direct_meta,
                        crate::session_meta::SessionMetaWrite::Insert,
                        now,
                    )
                    .await?;
                    if let Some(completion) = replay.release_session_execution_lease() {
                        let _release_was_current =
                            release_session_execution_lease_tx(&mut tx, completion).await?;
                        // FIG-884: ancillary stale release must never veto a
                        // replayed commit or clear a successor claim.
                    }
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
            let current_fence = commit
                .session_execution_lease_fence
                .as_ref()
                .or(commit.release_session_execution_lease.as_ref())
                .ok_or_else(|| StoreError::TurnCancelClosureAuthorizationMismatch {
                    session_id: commit.session_id.clone(),
                    turn_id: closure.turn_id().clone(),
                })?;
            ensure_session_execution_lease_tx(&mut tx, &commit.session_id, current_fence).await?;
            if closure.session_id() != &commit.session_id
                || commit.interrupted_turn_input_turn_id.as_ref() != Some(closure.turn_id())
            {
                return Err(StoreError::TurnCancelClosureAuthorizationMismatch {
                    session_id: commit.session_id.clone(),
                    turn_id: closure.turn_id().clone(),
                });
            }
            let stored: Option<String> = sqlx::query_scalar(
                "SELECT authorization_json FROM lash_turn_cancel_closure_authorizations WHERE session_id = $1 AND turn_id = $2 FOR UPDATE",
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
        let (checkpoint_ref, manifest) = put_checkpoint_tx(&mut tx, &commit.checkpoint).await?;
        crate::session_meta::write_session_meta_tx(
            &mut tx,
            &direct_meta,
            crate::session_meta::SessionMetaWrite::Insert,
            now,
        )
        .await?;
        let actual_revision = existing.as_ref().map_or(0, |meta| meta.head_revision);
        if existing.is_none() {
            let placeholder = SessionHeadMeta::assemble(
                SessionHeadPayload {
                    schema_version: lash_core::store::SESSION_HEAD_META_SCHEMA_VERSION,
                    session_id: commit.session_id.clone(),
                    config: commit.config.clone(),
                    current_frame_node_id: None,
                },
                0,
                None,
                None,
            );
            sqlx::query(
                "INSERT INTO lash_sessions
                 (session_id, head_revision, head_json, checkpoint_ref, leaf_node_id)
                 VALUES ($1, 0, $2, NULL, NULL)
                 ON CONFLICT (session_id) DO NOTHING",
            )
            .bind(commit.session_id.as_str())
            .bind(encode_json(&placeholder.payload())?)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        }
        let locked_revision = sqlx::query_scalar::<_, i64>(
            "SELECT head_revision
             FROM lash_sessions
             WHERE session_id = $1
             FOR UPDATE",
        )
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
                "SELECT generation, frame_node_id FROM lash_graph_nodes
                 WHERE node_id = $1 AND tombstoned = FALSE
                 FOR UPDATE",
            )
            .bind(leaf_node_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?
            .map(|(generation, frame_node_id)| {
                Ok(lash_core::store::ParentNodeFacts {
                    node_id: leaf_node_id.to_string(),
                    generation: u64_from_sql("SessionGraph node", "generation", generation)?,
                    frame_node_id,
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
                "SELECT EXISTS(
                     SELECT 1 FROM lash_graph_nodes AS node
                     WHERE node.node_id = $1
                       AND node.tombstoned = FALSE
                       AND node.generation <= $3
                       AND (
                           node.session_id = $2
                           OR EXISTS (
                               SELECT 1 FROM lash_fork_lineage AS lineage
                               WHERE lineage.session_id = $2
                                 AND lineage.ancestor_session_id = node.session_id
                                 AND node.generation <= lineage.fork_generation
                           )
                       )
                 )",
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
        let authoritative_revision = locked_revision.max(actual_revision);
        let node_ids = commit
            .graph
            .nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<Vec<_>>();
        let occupied_node_ids = sqlx::query_scalar::<_, String>(
            "SELECT node_id
             FROM lash_graph_nodes
             WHERE node_id = ANY($1)",
        )
        .bind(&node_ids)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
        let selected_leaf_is_live = match commit.graph.leaf_node_id() {
            Some(leaf_node_id) => sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(
                    SELECT 1 FROM lash_graph_nodes
                    WHERE node_id = $1 AND tombstoned = FALSE
                )",
            )
            .bind(leaf_node_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(store_sqlx_error)?,
            None => false,
        };
        let has_live_nodes = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(
                SELECT 1 FROM lash_graph_nodes
                WHERE session_id = $1 AND tombstoned = FALSE
            )",
        )
        .bind(commit.session_id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let published_leaf = match old_leaf_node_id {
            None => lash_core::store::PublishedLeafFacts::Absent,
            Some(node_id) => match parent_node_facts {
                Some(parent) => lash_core::store::PublishedLeafFacts::Live(parent),
                None => lash_core::store::PublishedLeafFacts::Retired { node_id },
            },
        };
        let plan = planner.plan(lash_core::store::FreshRuntimeCommitFacts {
            actual_head_revision: authoritative_revision,
            published_leaf,
            requested_ancestor_is_active,
            occupied_node_ids,
            selected_leaf_is_live,
            has_live_nodes,
        })?;
        let sql_head_revision = sql_monotonic_counter_value(
            "session_head_revision",
            plan.actual_head_revision(),
            plan.next_head_revision(),
        )?;
        for completed in &commit.completed_queue_claims {
            ensure_queued_work_completion_tx(&mut tx, completed).await?;
        }
        for completed in &commit.completed_turn_input_claims {
            ensure_turn_input_completion_tx(&mut tx, completed).await?;
        }
        for entry in &commit.usage_deltas {
            let entry_ordinal = i64::try_from(entry.identity.entry_ordinal).map_err(|_| {
                StoreError::Backend(
                    "usage delta ordinal does not fit PostgreSQL BIGINT".to_string(),
                )
            })?;
            sqlx::query(
                "INSERT INTO lash_usage_deltas (
                    session_id, operation_storage_key, entry_ordinal, payload_encoding_version, payload_hash, source, model, input_tokens, output_tokens, cache_read_input_tokens, cache_write_input_tokens, reasoning_output_tokens, usage_disposition_json
                 ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
                 ON CONFLICT (session_id, operation_storage_key, entry_ordinal, payload_encoding_version, payload_hash)
                 DO NOTHING",
            )
            .bind(commit.session_id.as_str())
            .bind(&entry.identity.operation_storage_key)
            .bind(entry_ordinal)
            .bind(i32::try_from(entry.identity.payload_encoding_version).map_err(|_| {
                StoreError::Backend(
                    "usage payload encoding version does not fit PostgreSQL INTEGER".to_string(),
                )
            })?)
            .bind(&entry.identity.payload_hash)
            .bind(&entry.entry.source)
            .bind(&entry.entry.model)
            .bind(entry.entry.usage.input_tokens)
            .bind(entry.entry.usage.output_tokens)
            .bind(entry.entry.usage.cache_read_input_tokens)
            .bind(entry.entry.usage.cache_write_input_tokens)
            .bind(entry.entry.usage.reasoning_output_tokens)
            .bind(encode_usage_disposition(&entry.entry.usage_disposition)?)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        }
        for (node, facts) in commit.graph.nodes.iter().zip(plan.planned_node_facts()) {
            let node_json = node.encode_storage_body().map_err(|err| {
                StoreError::Backend(format!("failed to encode graph node body: {err}"))
            })?;
            sqlx::query(
                "INSERT INTO lash_graph_nodes
                     (session_id, node_id, parent_node_id, generation, frame_node_id, node_json)
                     VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(commit.session_id.as_str())
            .bind(&node.node_id)
            .bind(&node.parent_node_id)
            .bind(i64::try_from(facts.generation).map_err(|_| {
                StoreError::Backend("node generation does not fit PostgreSQL BIGINT".to_string())
            })?)
            .bind(&facts.frame_node_id)
            .bind(node_json)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                graph_node_insert_error(error, &commit.session_id, facts.generation, &node.node_id)
            })?;
        }
        let meta = plan.head_meta(checkpoint_ref.clone());
        // Conditional publication is still required for concurrent first
        // commits, where no head row existed to lock. Existing sessions already
        // hold the row lock above; the revision predicate is defense in depth.
        let head_write = sqlx::query(
            "INSERT INTO lash_sessions
             (session_id, head_revision, head_json, checkpoint_ref, leaf_node_id)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (session_id) DO UPDATE SET
                head_revision = EXCLUDED.head_revision,
                head_json = EXCLUDED.head_json,
                checkpoint_ref = EXCLUDED.checkpoint_ref,
                leaf_node_id = EXCLUDED.leaf_node_id
             WHERE lash_sessions.head_revision = $6",
        )
        .bind(commit.session_id.as_str())
        .bind(sql_head_revision)
        .bind(encode_json(&meta.payload())?)
        .bind(checkpoint_ref.as_str())
        .bind(meta.leaf_node_id.as_deref())
        .bind(plan.actual_head_revision() as i64)
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
        if head_write.rows_affected() == 0 {
            // A concurrent commit won the race: the head no longer matches the
            // revision we read. Re-read the now-current revision for an accurate
            // report, then drop `tx` (auto-rollback), discarding this attempt's
            // node/usage writes; the caller reloads and retries.
            let actual_now = sqlx::query_scalar::<_, i64>(
                "SELECT head_revision FROM lash_sessions WHERE session_id = $1",
            )
            .bind(commit.session_id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?
            .map(|revision| u64_from_sql("SessionHeadMeta", "head_revision", revision))
            .transpose()?
            .unwrap_or(plan.actual_head_revision());
            return Err(plan.head_publication_conflict(actual_now));
        }
        sqlx::query("UPDATE lash_session_meta SET last_commit_at_ms = $2 WHERE session_id = $1")
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
        complete_queued_work_claims_tx(&mut tx, &commit.completed_queue_claims).await?;
        complete_turn_input_claims_tx(&mut tx, &commit.completed_turn_input_claims).await?;
        let mut turn_cancel_input_outcome = lash_core::TurnCancelInputOutcome::default();
        if let Some(turn_id) = commit.interrupted_turn_input_turn_id.as_ref() {
            let cancellation = commit.interrupted_turn_input_cancellation.as_ref();
            let disposition = cancellation
                .map_or(lash_core::TurnCancelDisposition::Defer, |evidence| {
                    evidence.undelivered
                });
            if let Some(evidence) = commit
                .turn_cancel_closure_settlement
                .as_ref()
                .and_then(lash_core::TurnCancelClosureSettlement::base_cancellation)
            {
                let observed = commit
                    .interrupted_turn_cancel_intent
                    .as_ref()
                    .ok_or_else(|| {
                        StoreError::Backend(
                            "interrupted turn commit omitted cancellation intent predicate"
                                .to_string(),
                        )
                    })?;
                if !reconcile_turn_cancel_winner_tx(
                    &mut tx,
                    &commit.session_id,
                    turn_id,
                    observed,
                    evidence,
                )
                .await?
                {
                    return Err(StoreError::TurnCancelIntentChanged {
                        session_id: commit.session_id.clone(),
                        turn_id: turn_id.clone(),
                    });
                }
            }
            let rows = sqlx::query(&format!(
                "SELECT {PENDING_TURN_INPUT_COLUMNS}
                 FROM lash_pending_turn_inputs
                 WHERE session_id = $1 AND state = $2
                 ORDER BY enqueue_seq ASC
                 FOR UPDATE"
            ))
            .bind(commit.session_id.as_str())
            .bind(lash_core::TurnInputState::PendingActive.as_str())
            .fetch_all(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            let mut inputs = Vec::new();
            for row in rows {
                let input = pending_turn_input_from_row(pending_turn_input_row(row)?)?;
                if input
                    .ingress
                    .active_turn_id()
                    .is_some_and(|active| active == turn_id)
                {
                    inputs.push((input.input_id, input.input));
                }
            }
            for (input_id, payload) in inputs {
                sqlx::query(
                    "UPDATE lash_pending_turn_inputs
                     SET state = $3,
                         ingress_json = COALESCE($4, ingress_json),
                         claim_id = NULL,
                         claim_owner_id = NULL,
                         claim_owner_incarnation_id = NULL,
                         claim_token = NULL,
                         claim_session_lease_generation = 0
                     WHERE session_id = $1 AND input_id = $2",
                )
                .bind(commit.session_id.as_str())
                .bind(&input_id)
                .bind(match disposition {
                    lash_core::TurnCancelDisposition::Defer => {
                        lash_core::TurnInputState::DeferredNextTurn.as_str()
                    }
                    lash_core::TurnCancelDisposition::Drop => {
                        lash_core::TurnInputState::Cancelled.as_str()
                    }
                })
                .bind(match disposition {
                    lash_core::TurnCancelDisposition::Defer => {
                        Some(encode_json(&lash_core::TurnInputIngress::NextTurn)?)
                    }
                    lash_core::TurnCancelDisposition::Drop => None,
                })
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
                let affected = lash_core::TurnCancelAffectedInput {
                    input_id,
                    payload,
                    disposition,
                };
                if cancellation.is_some() {
                    append_turn_cancel_outcome_tx(
                        &mut tx,
                        &commit.session_id,
                        turn_id,
                        affected.clone(),
                    )
                    .await?;
                    turn_cancel_input_outcome.affected_inputs.push(affected);
                }
            }
        }
        commit_attachment_refs_tx(
            &mut tx,
            &commit.session_id,
            &commit.committed_attachment_ids,
            now,
        )
        .await?;
        if let Some(turn_id) = commit.turn_commit.operation.turn_id() {
            sqlx::query(
                "UPDATE lash_attachment_manifest
                     SET committed_at_ms = COALESCE(committed_at_ms, $1)
                     WHERE session_id = $2
                       AND owner_kind = $4
                       AND owner_id = $3
                       AND committed_at_ms IS NULL",
            )
            .bind(now as i64)
            .bind(commit.session_id.as_str())
            .bind(turn_id.as_str())
            .bind(AttachmentOwnerKind::Turn.as_str())
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        }
        let mut enqueued_queue_batches = Vec::new();
        for batch in &commit.enqueued_queue_batches {
            enqueued_queue_batches.push(enqueue_queued_work_tx(&mut tx, batch, now).await?);
        }
        let mut result = plan.result(checkpoint_ref, manifest, enqueued_queue_batches);
        result.turn_cancel_input_outcome = turn_cancel_input_outcome;
        {
            let receipt = plan.receipt_write(&result);
            let columns = append_identity_columns(receipt.append_request_identity)?;
            let result_json = encode_json(receipt.result)?;
            sqlx::query(
                "INSERT INTO lash_runtime_turn_commits (
                    session_id, turn_id, turn_commit_hash, result_json, committed_at_ms,
                    request_identity_hash, requested_node_count, identity_encoding_version
                 )
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(receipt.session_id.as_str())
            .bind(receipt.operation_key)
            .bind(receipt.turn_commit_hash)
            .bind(&result_json)
            .bind(now as i64)
            .bind(columns.0)
            .bind(columns.1)
            .bind(columns.2)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            if commit.turn_commit.operation.key == "session-command" {
                for batch_id in commit
                    .completed_queue_claims
                    .iter()
                    .flat_map(|completion| &completion.batch_ids)
                {
                    let marker =
                        lash_core::store_backend_support::session_command_batch_completion_key(
                            &commit.session_id,
                            batch_id,
                        )?;
                    sqlx::query(
                        "INSERT INTO lash_runtime_turn_commits (
                            session_id, turn_id, turn_commit_hash, result_json, committed_at_ms,
                            request_identity_hash, requested_node_count, identity_encoding_version
                         ) VALUES ($1, $2, $3, $4, $5, NULL, NULL, NULL)",
                    )
                    .bind(commit.session_id.as_str())
                    .bind(marker)
                    .bind(receipt.turn_commit_hash)
                    .bind(&result_json)
                    .bind(now as i64)
                    .execute(&mut *tx)
                    .await
                    .map_err(store_sqlx_error)?;
                }
            }
        }
        if let Some(settlement) = commit.turn_cancel_closure_settlement.as_ref() {
            let closure = settlement.authorization();
            sqlx::query(
                "DELETE FROM lash_turn_cancel_closure_authorizations WHERE session_id = $1 AND turn_id = $2",
            )
            .bind(closure.session_id().as_str())
            .bind(closure.turn_id().as_str())
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        }
        // A plain-commit receipt writes three NULL append-identity columns.
        if let Some(completion) = commit.release_session_execution_lease.as_ref() {
            let _release_was_current =
                release_session_execution_lease_tx(&mut tx, completion).await?;
            // FIG-884: head CAS is commit authority; release is ancillary.
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(result)
    }

    async fn admit_and_bind_session(
        &self,
        binding: &lash_core::SessionBinding,
    ) -> Result<lash_core::SessionAdmission, StoreError> {
        binding.validate()?;
        let session_id = &binding.session_id;
        self.bind_session_id(session_id)?;
        let meta = SessionMeta {
            session_id: SessionId::from(session_id.to_string()),
            relation: binding.relation.clone(),
            pending_observer_intents: Vec::new(),
        };
        let created_at_ms = self.clock.timestamp_ms();
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        ensure_session_not_deleted_tx(&mut tx, session_id).await?;
        let inserted = crate::session_meta::write_session_meta_tx(
            &mut tx,
            &meta,
            crate::session_meta::SessionMetaWrite::Insert,
            created_at_ms,
        )
        .await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(if inserted {
            lash_core::SessionAdmission::Created
        } else {
            lash_core::SessionAdmission::Rebound
        })
    }

    async fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError> {
        self.bind_session_id(&meta.session_id)?;
        let created_at_ms = self.clock.timestamp_ms();
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        ensure_session_not_deleted_tx(&mut tx, &meta.session_id).await?;
        crate::session_meta::write_session_meta_tx(
            &mut tx,
            &meta,
            crate::session_meta::SessionMetaWrite::Replace,
            created_at_ms,
        )
        .await?;
        tx.commit().await.map_err(store_sqlx_error)
    }

    async fn load_session_meta(&self) -> Result<Option<SessionMeta>, StoreError> {
        crate::session_meta::load_session_meta(&self.pool, Some(&self.session_id)).await
    }
}
