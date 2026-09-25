use crate::session_sql::session_sql;
use crate::*;

#[path = "session_factory/artifact_retirement.rs"]
mod artifact_retirement;
#[path = "session_factory/control_intent_ledger.rs"]
mod control_intent_ledger;
#[path = "session_factory/store.rs"]
mod store;

#[async_trait::async_trait]
impl SessionStoreFactory for PostgresSessionStoreFactory {
    fn bind_effect_host(&self, effect_host: &Arc<dyn lash_core_execution::EffectHost>) {
        *self
            .turn_cancel_closure_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(effect_host));
        *self
            .effect_host
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(effect_host));
    }

    fn bind_artifact_stores(
        &self,
        process_env_store: Arc<dyn lash_core_execution::ProcessExecutionEnvStore>,
        process_engines: lash_core_execution::ProcessEngineRegistry,
    ) {
        *self
            .artifact_stores
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some((process_env_store, process_engines));
    }

    async fn reclaim_retained_evidence(
        &self,
        bound: lash_core_execution::store::RetentionBound,
    ) -> lash_core_execution::MaintenanceResult<lash_core_execution::store::RetentionReport> {
        let report = crate::evidence_retention::reclaim(self, bound)
            .await
            .map_err(|failure| *failure)?;
        if let Err(error) = self.resume_artifact_owner_retirements().await {
            return Err(lash_core_execution::MaintenanceFailure::failed(
                error, report,
            ));
        }
        Ok(report)
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
        lash_core_execution::store::validate_session_id(&request.session_id)
            .map_err(|error| error.to_string())?;
        Ok(self
            .open_existing_session_store(request)
            .await?
            .map(|store| store as Arc<dyn RuntimePersistence>))
    }

    async fn open_existing_store_by_id(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>, StoreError> {
        lash_core_execution::store::validate_session_id(session_id)?;
        let store = self.store_for(session_id.clone());
        if store.load_session_meta().await?.is_some() {
            Ok(Some(Arc::new(store)))
        } else {
            Ok(None)
        }
    }

    async fn pending_turn_cancel_closure_pins(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core_execution::TurnCancelClosureAuthorization>, StoreError> {
        self.store_for(session_id.clone())
            .pending_turn_cancel_closure_pins()
            .await
    }
    async fn retire_turn_cancel_closure_scope(
        &self,
        scope: &lash_core_execution::ExecutionScope,
    ) -> Result<(), StoreError> {
        crate::turn_cancel_closure::retire_scope(&self.pool, scope).await?;
        if let Some(owner) = self.turn_cancel_closure_owner_binding() {
            owner
                .release(scope)
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))?;
        }
        Ok(())
    }
    async fn has_claimable_queued_work(
        &self,
        request: &SessionStoreCreateRequest,
        now_epoch_ms: u64,
    ) -> Result<Option<bool>, StoreError> {
        lash_core_execution::store::validate_session_id(&request.session_id)?;
        sqlx::query_scalar(
            crate::turn_ingress::turn_ingress_sql()
                .family
                .has_claimable_work
                .sql(),
        )
        .bind(request.session_id.as_str())
        .bind(now_epoch_ms as i64)
        .fetch_one(&self.pool)
        .await
        .map(Some)
        .map_err(store_sqlx_error)
    }

    async fn session_was_deleted(&self, session_id: &SessionId) -> Result<bool, String> {
        lash_core_execution::store::validate_session_id(session_id)
            .map_err(|error| error.to_string())?;
        sqlx::query_scalar(session_sql().deleted_postgres.exists.sql())
            .bind(session_id.as_str())
            .fetch_one(&self.pool)
            .await
            .map_err(|err| err.to_string())
    }

    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> lash_core_execution::MaintenanceResult<lash_core_execution::SessionBlobReclaimReport> {
        lash_core_execution::store::validate_session_id(session_id)
            .map_err(lash_core_execution::MaintenanceFailure::failed_before_any_work)?;
        let mut tx = self.pool.begin().await.map_err(|err| {
            lash_core_execution::MaintenanceFailure::failed_before_any_work(store_sqlx_error(err))
        })?;
        let mut report = lash_core_execution::SessionBlobReclaimReport::default();
        if let Err(error) = delete_session_tx(&mut tx, session_id, &mut report).await {
            report.deleted_blob_count = 0;
            return Err(lash_core_execution::MaintenanceFailure::failed(
                error, report,
            ));
        }
        if let Err(error) = tx.commit().await {
            report.deleted_blob_count = 0;
            return Err(lash_core_execution::MaintenanceFailure::failed(
                store_sqlx_error(error),
                report,
            ));
        }
        Ok(report)
    }

    async fn pin(&self, node_id: &str) -> Result<lash_core_execution::ForkPoint, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        let (source_session_id, checkpoint_ref) =
            crate::support::retained_checkpoint_tx(&mut tx, node_id)
                .await?
                .ok_or_else(|| StoreError::ForkPointNotRetained {
                    node_id: node_id.to_string().into(),
                })?;
        crate::runtime_persistence::lock_session_history_mutation_tx(&mut tx, &source_session_id)
            .await?;
        crate::support::lock_checkpoint_blob_tx(&mut tx, &checkpoint_ref, None).await?;
        let live_node = sqlx::query_scalar::<_, bool>(session_sql().graph_postgres.lock_live.sql())
            .bind(node_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        if live_node.is_none() {
            return Err(StoreError::ForkPointNotRetained {
                node_id: node_id.to_string().into(),
            });
        }
        if let Some((checkpoint_ref, source_session_id)) =
            sqlx::query_as::<_, (String, String)>(session_sql().anchors.select_by_node.sql())
                .bind(node_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(store_sqlx_error)?
        {
            let config = crate::support::retained_fork_config_tx(&mut tx, node_id).await?;
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(lash_core_execution::ForkPoint {
                node_id: node_id.to_string().into(),
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
                node_id: node_id.to_string().into(),
            });
        }
        sqlx::query(session_sql().anchors.insert.sql())
            .bind(node_id)
            .bind(&checkpoint_ref)
            .bind(source_session_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let config = crate::support::retained_fork_config_tx(&mut tx, node_id).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(lash_core_execution::ForkPoint {
            node_id: node_id.to_string().into(),
            checkpoint_ref: checkpoint_ref.into(),
            source_session_id,
            config,
            pinned: true,
        })
    }

    async fn unpin(&self, node_id: &str) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        sqlx::query(session_sql().graph_postgres.lock_live_id.sql())
            .bind(node_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let removed = sqlx::query(session_sql().anchors.delete_by_node.sql())
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

    async fn fork_points(&self) -> Result<Vec<lash_core_execution::ForkPoint>, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        sqlx::query(
            crate::connection_sql::connection_sql()
                .begin_repeatable_read
                .sql(),
        )
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let rows = sqlx::query(session_sql().head.select_fork_points.sql())
            .fetch_all(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let mut points = Vec::with_capacity(rows.len());
        for row in rows {
            let node_id: String = row.get(0);
            points.push(lash_core_execution::ForkPoint {
                config: crate::support::retained_fork_config_tx(&mut tx, &node_id).await?,
                node_id: node_id.into(),
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
        request: &lash_core_execution::ForkSessionRequest,
    ) -> Result<lash_core_execution::ForkSessionReceipt, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        // Target identity fences precede source-retention fences. This unlocked
        // fast path only decides already-materialized targets and permanent
        // tombstones; keep the post-lock checks below for concurrent changes.
        let (exists, deleted) = sqlx::query_as::<_, (bool, bool)>(
            session_sql()
                .meta_postgres
                .exists_materialized_or_deleted
                .sql(),
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
        let exists =
            sqlx::query_scalar::<_, bool>(session_sql().meta_postgres.exists_materialized.sql())
                .bind(request.session_id.as_str())
                .fetch_one(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
        if exists {
            return Err(StoreError::ForkSessionAlreadyExists {
                session_id: request.session_id.clone(),
            });
        }
        let deleted = sqlx::query_scalar::<_, bool>(session_sql().deleted_postgres.exists.sql())
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
            session_sql()
                .graph_postgres
                .select_owner_generation_for_update
                .sql(),
        )
        .bind(&*request.node_id)
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
                session_sql().graph_postgres.select_edge_for_share.sql(),
            )
            .bind(&*current_node_id)
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
            edge_path.push(lash_core_execution::store::ForkNodeFacts {
                node_id: facts.0.into(),
                parent_node_id: facts.1.map(lash_core_execution::NodeId::from),
                owning_session_id: SessionId::from(facts.2),
                generation,
            });
            if expected_generation == 0 {
                break;
            }
            current_node_id = parent_node_id
                .ok_or_else(|| StoreError::StoredDataCorrupt {
                    record_kind: "SessionGraph",
                    message: "retained fork path ended before generation zero".to_string(),
                })?
                .into();
            expected_generation -= 1;
        }
        edge_path.reverse();
        let fork_plan =
            lash_core_execution::store::ForkPlan::derive(&request.session_id, edge_path)?;
        let config = lash_core_execution::PersistedSessionConfig::from(&request.policy);
        let head = lash_core_execution::store::SessionHeadMeta::assemble(
            &request.session_id,
            lash_core_execution::store::SessionHeadPayload {
                schema_version: lash_core_execution::store::SESSION_HEAD_META_SCHEMA_VERSION,
                session_id: request.session_id.clone(),
                config,
                current_frame_node_id: Some({
                    #[expect(
                        clippy::expect_used,
                        reason = "the target is a transparent newtype over `String`, so decoding a JSON string into it cannot fail"
                    )]
                    let node_id =
                        serde_json::from_value(serde_json::Value::String(current_frame_node_id))
                            .expect("a persisted frame node id is a transparent string");
                    node_id
                }),
            },
            0,
            Some(checkpoint_ref.clone().into()),
            Some(request.node_id.clone()),
        )?;
        sqlx::query(session_sql().head.insert_fork.sql())
            .bind(request.session_id.as_str())
            .bind(encode_json(&head.payload())?)
            .bind(&checkpoint_ref)
            .bind(&*request.node_id)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        for ancestor in fork_plan.ancestors() {
            sqlx::query(session_sql().lineage.insert.sql())
                .bind(fork_plan.session_id())
                .bind(ancestor.ancestor_session_id.as_str())
                .bind(&*ancestor.fork_node_id)
                .bind(i64::try_from(ancestor.fork_generation).map_err(|_| {
                    StoreError::Backend(
                        "fork generation does not fit PostgreSQL BIGINT".to_string(),
                    )
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
        Ok(lash_core_execution::ForkSessionReceipt {
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

    async fn count_unsettled_turns(
        &self,
    ) -> Result<lash_core_execution::store::UnsettledTurnCounts, StoreError> {
        // The headline counts and the per-reason split must come from one
        // snapshot — a park landing between two pool reads would appear in
        // the total but not in the split. `REPEATABLE READ` pins both
        // statements to the transaction's first-snapshot.
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        sqlx::query(
            crate::connection_sql::connection_sql()
                .begin_repeatable_read_read_only
                .sql(),
        )
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let row = sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .family
                .count_unsettled_turns
                .sql(),
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let parked: i64 = row.get(0);
        let oldest_since_ms: Option<i64> = row.get(1);
        let in_flight: i64 = row.get(2);
        let reason_rows = sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .family
                .count_parks_by_reason
                .sql(),
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let generation_rows = sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .family
                .count_retired_parks_by_executable_generation
                .sql(),
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        let retired_by_executable_generation = generation_rows
            .into_iter()
            .map(|row| {
                let generation: String = row.get(0);
                let count: i64 = row.get(1);
                (
                    lash_core_execution::ExecutableGeneration::new(generation),
                    usize::try_from(count).unwrap_or_default(),
                )
            })
            .collect();
        let mut parked_by_reason = std::collections::BTreeMap::new();
        for row in reason_rows {
            let code: String = row.get(0);
            let count: i64 = row.get(1);
            let Some(code) = lash_core_execution::store::ParkReasonCode::from_code(&code) else {
                return Err(StoreError::StoredDataCorrupt {
                    record_kind: "TurnPark",
                    message: format!("stored park reason code `{code}` is unknown"),
                });
            };
            parked_by_reason.insert(code, usize::try_from(count).unwrap_or_default());
        }
        Ok(lash_core_execution::store::UnsettledTurnCounts {
            parked_turns: usize::try_from(parked).unwrap_or_default(),
            in_flight_turns: usize::try_from(in_flight).unwrap_or_default(),
            oldest_parked_since_ms: oldest_since_ms.map(|ms| u64::try_from(ms).unwrap_or_default()),
            parked_by_reason,
            retired_by_executable_generation,
        })
    }

    async fn list_turn_parks(
        &self,
        query: &lash_core_execution::store::TurnParkQuery,
    ) -> Result<Vec<lash_core_execution::store::TurnPark>, StoreError> {
        let limit = i64::try_from(query.limit.get()).unwrap_or(i64::MAX);
        let session = query.session.as_ref().map(|id| id.as_str().to_string());
        let at_or_before = query
            .parked_at_or_before_ms
            .map(|ms| i64::try_from(ms).unwrap_or(i64::MAX));
        let (after_since, after_session) = match query.after.as_ref() {
            Some((since_ms, session_id)) => (
                Some(i64::try_from(*since_ms).unwrap_or(i64::MAX)),
                Some(session_id.as_str().to_string()),
            ),
            None => (None, None),
        };
        let reasons: Option<Vec<&str>> = query
            .reasons
            .as_ref()
            .filter(|reasons| !reasons.is_empty())
            .map(|reasons| reasons.iter().map(|code| code.as_str()).collect());
        let rows = sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .turn_parks_postgres
                .list
                .sql(),
        )
        .bind(limit)
        .bind(session)
        .bind(at_or_before)
        .bind(after_since)
        .bind(after_session)
        .bind(reasons)
        .fetch_all(&self.pool)
        .await
        .map_err(store_sqlx_error)?;
        rows.iter()
            .map(crate::runtime_persistence::turn_park::decode_turn_park_row)
            .collect()
    }

    async fn turn_park_feed(
        &self,
        after: lash_core_execution::store::ParkFeedCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<
        lash_core_execution::store::ParkFeedPage<lash_core_execution::store::TurnParkTarget>,
        StoreError,
    > {
        let mut page = lash_core_execution::store::ParkFeedPage {
            events: Vec::new(),
            next: after,
        };
        let after_seq = i64::try_from(after.store_sequence()).unwrap_or(i64::MAX);
        let limit = i64::try_from(limit.get()).unwrap_or(i64::MAX);
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        // The horizon is read under `FOR SHARE`: a compaction still
        // committing must not let a stale-cursor read pass unrefused while
        // its events are already gone.
        let horizon: i64 = sqlx::query_scalar(
            crate::turn_ingress::turn_ingress_sql()
                .turn_park_clock
                .select_compaction_horizon_for_share
                .sql(),
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        if after_seq < horizon {
            return Err(StoreError::ParkFeedCursorCompacted {
                horizon: lash_core_execution::store::ParkFeedCursor::from_store_sequence(
                    u64::try_from(horizon).unwrap_or_default(),
                ),
            });
        }
        let rows = sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .turn_park_events
                .select_events_after
                .sql(),
        )
        .bind(after_seq)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        for row in rows {
            let seq: i64 = row.get(0);
            let session_id: String = row.get(1);
            let turn_id: String = row.get(2);
            let park_id: i64 = row.get(3);
            let kind: String = row.get(4);
            let cause: Option<String> = row.get(5);
            let reason_json: Option<String> = row.get(6);
            let at_ms: i64 = row.get(7);
            let kind = lash_core_execution::store::ParkEventKind::decode_columns(
                &kind,
                cause.as_deref(),
                reason_json.as_deref(),
            )?;
            page.events.push(lash_core_execution::store::ParkFeedEvent {
                seq: u64::try_from(seq).unwrap_or_default(),
                at_ms: u64::try_from(at_ms).unwrap_or_default(),
                target: lash_core_execution::store::TurnParkTarget {
                    session_id: SessionId::from(session_id),
                    turn_id: lash_sansio::TurnId::from(turn_id),
                },
                park_id: lash_core_execution::store::ParkId::from_feed_sequence(
                    u64::try_from(park_id).unwrap_or_default(),
                ),
                kind,
            });
            page.next = lash_core_execution::store::ParkFeedCursor::from_store_sequence(
                u64::try_from(seq).unwrap_or_default(),
            );
        }
        Ok(page)
    }

    async fn root_terminal(
        &self,
        session_id: &SessionId,
        root: &lash_sansio::TurnId,
    ) -> Result<Option<lash_core_execution::store::RootTerminal>, StoreError> {
        // The root's own evidence, else the session's `close_session`
        // tombstone, which answers every root of a deleted session.
        let mut connection = crate::acquire_runtime_connection(&self.pool).await?;
        if let Some(terminal) =
            crate::session_roots::root_terminal_conn(&mut connection, session_id, root).await?
        {
            return Ok(Some(terminal));
        }
        Ok(
            crate::session_roots::close_session_intent_conn(&mut connection, session_id)
                .await?
                .and_then(|intent| intent.session_deleted_terminal(root)),
        )
    }

    async fn list_open_control_intents(
        &self,
        after: Option<lash_core_execution::store::ControlIntentId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<lash_core_execution::store::ControlIntent>, StoreError> {
        let mut connection = crate::acquire_runtime_connection(&self.pool).await?;
        crate::session_roots::open_control_intents_conn(&mut connection, after, limit.get()).await
    }

    async fn compact_turn_park_feed(
        &self,
        through: lash_core_execution::store::ParkFeedCursor,
    ) -> Result<(), StoreError> {
        let through_seq = i64::try_from(through.store_sequence()).unwrap_or(i64::MAX);
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        let sql = crate::turn_ingress::turn_ingress_sql();
        // Lock the clock row before the delete: a concurrent bump either
        // commits ahead of the lock — its events are then visible to the
        // delete and to the clamp — or waits until this horizon update is
        // durable. `through` is clamped to the allocated sequence so the
        // horizon never rises past events the feed has not yet committed.
        let current_seq: i64 =
            sqlx::query_scalar(sql.turn_park_clock.select_current_for_update.sql())
                .fetch_one(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
        let through_seq = through_seq.min(current_seq);
        sqlx::query(sql.turn_park_events.delete_events_through.sql())
            .bind(through_seq)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        sqlx::query(sql.turn_park_clock.raise_compaction_horizon.sql())
            .bind(through_seq)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)
    }

    async fn read_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<lash_core_execution::SessionReadView>, StoreError> {
        lash_core_execution::store::validate_session_id(session_id)?;
        let store = self.store_for(session_id.clone());
        lash_core_execution::store::load_persisted_session_read_view(&store).await
    }
}

impl PostgresSessionStoreFactory {
    /// The read-only delete-time root predicate for one digest, parameterised
    /// `$1 = attachment_id`, `$2 = intent_grace_cutoff_ms`. A ref is live unless
    /// it is eligible for the same conditional forget reconciliation applies.
    /// The targeted probe and the condemn CAS share it so the fence and the
    /// probe cannot drift apart.
    fn live_attachment_ref_sql(&self) -> &'static str {
        crate::attachments::live_attachment_ref_sql(self.process_registry_shared)
    }
}

#[async_trait::async_trait]
impl lash_core_execution::AttachmentRootSet for PostgresSessionStoreFactory {
    fn can_prove_process_owner_death(&self) -> bool {
        self.process_registry_shared
    }

    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<
        std::collections::BTreeSet<lash_core_execution::AttachmentId>,
        lash_core_execution::StoreError,
    > {
        // Age is only a post-terminal retention policy. This single DELETE
        // composes age with durable owner-death proof: a later committed turn
        // supersedes a turn owner, a missing process row proves a process owner
        // was pruned, and only unscoped host puts use age alone.
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        sqlx::query(
            crate::attachments::attachment_sql()
                .manifest_postgres
                .delete_deleted_session_roots
                .sql(),
        )
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let delete_sql = crate::attachments::forget_aged_uncommitted_attachment_intents_sql(
            self.process_registry_shared,
        );
        let cutoff = clamp_epoch_ms(intent_grace_cutoff_epoch_ms);
        sqlx::query(delete_sql)
            .bind(cutoff)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let rows = sqlx::query(
            crate::attachments::attachment_sql()
                .manifest
                .select_rooted_ids
                .sql(),
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        rows.into_iter()
            .map(|row| attachment_id_from_sql("AttachmentManifest", "attachment_id", row.get(0)))
            .collect()
    }

    async fn list_condemnations(
        &self,
    ) -> Result<
        Vec<lash_core_execution::AttachmentCondemnationRecord>,
        lash_core_execution::StoreError,
    > {
        crate::attachments::list_attachment_condemnations(&self.pool).await
    }

    async fn has_live_attachment_ref(
        &self,
        id: &lash_core_execution::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, lash_core_execution::StoreError> {
        let cutoff = clamp_epoch_ms(intent_grace_cutoff_epoch_ms);
        let row = sqlx::query(self.live_attachment_ref_sql())
            .bind(id.as_str())
            .bind(cutoff)
            .fetch_optional(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
        Ok(row.is_some())
    }

    fn fence(&self) -> lash_core_execution::AttachmentGcFence {
        lash_core_execution::AttachmentGcFence::Fenced
    }

    async fn condemn_attachment(
        &self,
        id: &lash_core_execution::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<lash_core_execution::AttachmentCondemnation, lash_core_execution::StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        // The same per-digest lock a writer's `begin_attachment_write` takes:
        // the root predicate below and that writer's manifest insert cannot
        // interleave.
        crate::attachments::lock_attachment_fence_tx(&mut tx, id.as_str()).await?;
        let cutoff = clamp_epoch_ms(intent_grace_cutoff_epoch_ms);
        let rooted = sqlx::query(self.live_attachment_ref_sql())
            .bind(id.as_str())
            .bind(cutoff)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?
            .is_some();
        if rooted {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(lash_core_execution::AttachmentCondemnation::RootPresent);
        }
        let inserted = sqlx::query(
            crate::attachments::attachment_sql()
                .condemnation_postgres
                .insert_condemned
                .sql(),
        )
        .bind(id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
        if inserted == 1 {
            // The digest is proven unrooted, so every remaining manifest row for
            // it is stale evidence of an upload whose bytes this sweep is about
            // to delete. Clearing them here is what makes a negative byte-absence
            // tombstone unnecessary.
            sqlx::query(
                crate::attachments::attachment_sql()
                    .manifest
                    .delete_by_id
                    .sql(),
            )
            .bind(id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(if inserted == 1 {
            lash_core_execution::AttachmentCondemnation::Condemned
        } else {
            // A peer sweeper owns this digest. Skip on contention.
            lash_core_execution::AttachmentCondemnation::AlreadyCondemned
        })
    }

    async fn arm_attachment_delete(
        &self,
        id: &lash_core_execution::AttachmentId,
    ) -> Result<lash_core_execution::AttachmentDeleteArming, lash_core_execution::StoreError> {
        // Under the same per-digest advisory key the writer half takes, and in a
        // transaction: a bare pooled UPDATE could commit *inside* a writer's
        // open `begin_attachment_write` — after it read `condemned` and before
        // it deleted the row — leaving the writer to erase a `deleting` row and
        // put bytes into an in-flight delete.
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        crate::attachments::lock_attachment_fence_tx(&mut tx, id.as_str()).await?;
        let armed = sqlx::query(
            crate::attachments::attachment_sql()
                .condemnation
                .arm_delete
                .sql(),
        )
        .bind(id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(if armed == 1 {
            lash_core_execution::AttachmentDeleteArming::Armed
        } else {
            // A writer revoked the condemnation: the delete is never issued.
            lash_core_execution::AttachmentDeleteArming::Revoked
        })
    }

    async fn release_attachment_condemnation(
        &self,
        id: &lash_core_execution::AttachmentId,
    ) -> Result<(), lash_core_execution::StoreError> {
        crate::attachments::release_attachment_condemnation(&self.pool, id.as_str()).await
    }

    async fn recover_abandoned_attachment_write(
        &self,
        id: &lash_core_execution::AttachmentId,
    ) -> Result<(), lash_core_execution::StoreError> {
        crate::attachments::recover_abandoned_attachment_write(&self.pool, id.as_str()).await
    }

    async fn retire_attachment_condemnation(
        &self,
        id: &lash_core_execution::AttachmentId,
    ) -> Result<(), lash_core_execution::StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        crate::attachments::lock_attachment_fence_tx(&mut tx, id.as_str()).await?;
        sqlx::query(
            crate::attachments::attachment_sql()
                .condemnation
                .delete_armed
                .sql(),
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
    report: &mut lash_core_execution::SessionBlobReclaimReport,
) -> Result<(), StoreError> {
    crate::runtime_persistence::lock_session_history_mutation_tx(tx, session_id).await?;
    crate::turn_cancel_closure::ensure_session_not_pinned_tx(tx, session_id).await?;
    let materialized =
        sqlx::query_scalar::<_, bool>(session_sql().meta_postgres.exists_materialized.sql())
            .bind(session_id.as_str())
            .fetch_one(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    if materialized {
        // Permanent identity evidence for host-facing session ids.
        sqlx::query(session_sql().deleted_postgres.insert_from_meta.sql())
            .bind(session_id.as_str())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        sqlx::query(session_sql().deleted_postgres.insert_root.sql())
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
        session_sql().head.select_reclaim.sql(),
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
    sqlx::query(session_sql().head.delete_by_session.sql())
        .bind(session_id.as_str())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    if let Some(leaf_node_id) = leaf_node_id {
        crate::runtime_persistence::retire_unreachable_ancestry_tx(tx, &leaf_node_id).await?;
    }
    let unreachable_candidates = sqlx::query_scalar::<_, String>(
        session_sql().graph_postgres.select_unreachable_leaves.sql(),
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
        session_sql()
            .graph_postgres
            .delete_tombstoned_reclaimable
            .sql(),
    )
    .bind(session_id.as_str())
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let turn_ingress = crate::turn_ingress::turn_ingress_sql();
    let queued_runs = &turn_ingress.queued_runs;
    // A deleted session's parked turn is cancelled, and its feed event
    // outlives the session row: the ledger is the only place the park
    // transition stays durable (FIG-3659).
    let released = sqlx::query(turn_ingress.turn_parks.delete_by_session_returning.sql())
        .bind(session_id.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    if let Some(released) = released {
        let released_turn_id: String = released.get(0);
        let released_park_id: i64 = released.get(1);
        let at_ms = postgres_transaction_epoch_ms(tx).await?;
        crate::runtime_persistence::turn_park_feed::log_turn_park_closed_tx(
            tx,
            session_id,
            &released_turn_id,
            released_park_id,
            &lash_core_execution::store::ParkEventKind::Cancelled {
                cause: lash_core_execution::store::ParkCancelCause::SessionDeleted,
            },
            at_ms,
        )
        .await?;
    }
    // The session's logical roots and their input bindings go with it; a
    // `close_session` intent stays as its deletion tombstone.
    crate::session_roots::delete_session_roots_conn(tx, session_id).await?;
    for statement in [
        turn_ingress.queued_items_postgres.delete_by_session.sql(),
        turn_ingress.queued_batches.delete_by_session.sql(),
        crate::process_sql::process_sql()
            .fence
            .delete_by_session
            .sql(),
        crate::process_sql::process_sql()
            .floor
            .delete_by_session
            .sql(),
        queued_runs.delete_members.sql(),
        queued_runs.delete_runs.sql(),
        turn_ingress.pending_inputs.delete_by_session.sql(),
        crate::session_ingress::session_ingress_sql()
            .shared
            .delete_by_session
            .sql(),
        turn_ingress.cancel_requests.delete_by_session.sql(),
        // Administration revokes the session's effect authority before store
        // deletion, after which the pinned closure obligation may be retired.
        turn_ingress.closures.delete_by_session.sql(),
        turn_ingress.bindings.delete_by_session.sql(),
        turn_ingress.leases.delete_by_session.sql(),
    ] {
        sqlx::query(statement)
            .bind(session_id.as_str())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    }
    // The session-core rows the family owns, named rather than spelled.
    for statement in [
        session_sql().lineage.delete_by_session.sql(),
        session_sql().observer_intents.delete_by_session.sql(),
        session_sql().meta.delete_by_session.sql(),
    ] {
        sqlx::query(statement)
            .bind(session_id.as_str())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    }
    sqlx::query(
        crate::attachments::attachment_sql()
            .manifest_postgres
            .delete_deleted_session_roots
            .sql(),
    )
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
) -> lash_core_execution::MaintenanceResult<lash_core_execution::SessionBlobReclaimReport> {
    if session_ids.is_empty() {
        return Ok(lash_core_execution::SessionBlobReclaimReport::default());
    }
    let session_id_texts: Vec<_> = session_ids.iter().map(SessionId::as_str).collect();
    let mut report = lash_core_execution::SessionBlobReclaimReport::default();
    let outcome: Result<(), StoreError> = async {
        crate::runtime_persistence::lock_session_history_mutations_tx(tx, session_ids).await?;
        crate::turn_cancel_closure::ensure_sessions_not_pinned_tx(tx, session_ids).await?;
        let checkpoint_refs = sqlx::query_scalar::<_, String>(
            session_sql().head.select_checkpoints_for_sessions.sql(),
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

        sqlx::query(
            session_sql()
                .deleted_postgres
                .insert_batch_from_targets
                .sql(),
        )
        .bind(&session_id_texts[..])
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;

        let (deleted_leaf_node_ids, has_graph_candidates) =
            sqlx::query_as::<_, (Vec<String>, bool)>(
                session_sql().head.delete_batch_returning.sql(),
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
                session_sql()
                    .graph_postgres
                    .select_unreachable_leaves_batch
                    .sql(),
            )
            .bind(&session_id_texts[..])
            .fetch_all(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
            for node_id in unreachable_candidates {
                crate::runtime_persistence::retire_unreachable_ancestry_tx(tx, &node_id).await?;
            }
        }

        // Delete-time reclaim covers the batch's tombstoned rows plus any
        // tombstoned row owned by an already-deleted session. The ancestry
        // retire above tombstones a node regardless of who owns it, so a batch
        // can strand a row belonging to a session outside it; that owner is
        // unbindable, so no session-scoped vacuum could ever reach the row.
        // Live sessions' rows stay resident for their own vacuum, so this is
        // not a catalog-wide sweep.
        sqlx::query(session_sql().core.delete_process_session_rows.sql())
            .bind(&session_id_texts[..])
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;

        sqlx::query(
            crate::attachments::attachment_sql()
                .manifest_postgres
                .delete_deleted_session_roots
                .sql(),
        )
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
            Err(lash_core_execution::MaintenanceFailure::failed(
                error, report,
            ))
        }
    }
}
