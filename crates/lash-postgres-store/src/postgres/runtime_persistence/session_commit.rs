use super::*;
use crate::guarded_tx::{FleetMoved, GuardedTx};
use crate::session_sql::session_sql;

/// End every frame in `left` (the frames the commit leaves) in the commit's
/// transaction: fence it and upsert its `Ended` cleanup with no carries
/// (ADR 0113 §3.1). A transition first carries its
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
    referrers.push(ArtifactReferrer::Session(session_id.clone()));
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
            .execute(crate::observed_sql::executor(&mut **tx))
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
    use lash_core_execution::ArtifactReferrer;
    let ended = ArtifactReferrer::FrameEnvironment(transition.ended.clone());
    let successor = ArtifactReferrer::FrameEnvironment(transition.successor.clone());
    let sql = crate::artifact_store::artifact_sql();
    let successor_fenced: bool = sqlx::query_scalar(sql.fences.select_is_fenced.sql())
        .bind(successor.kind().as_str())
        .bind(successor.canonical_id())
        .fetch_one(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?;
    if successor_fenced {
        return Err(StoreError::ArtifactReferrerEnded {
            referrer: successor,
        });
    }
    let mut closure = transition.carries.clone();
    for carry in &transition.carries {
        if carry.store != lash_core_execution::ArtifactStoreId::ProcessDefinition {
            continue;
        }
        let id = lash_core_execution::ProcessDefinitionId::parse(&carry.artifact_ref)
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        let bytes: Vec<u8> = sqlx::query_scalar(sql.lashlang_artifacts.select_bytes.sql())
            .bind("process_definition")
            .bind(&carry.artifact_ref)
            .fetch_one(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(store_sqlx_error)?;
        let draft = lash_core_execution::ProcessDefinitionDraft::from_store_bytes(&id, &bytes)
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        closure.extend(draft.artifacts().iter().cloned());
    }
    let mut carries = closure
        .iter()
        .filter(|artifact| {
            !matches!(
                artifact.store,
                lash_core_execution::ArtifactStoreId::Engine(_)
            )
        })
        .map(|artifact| {
            let namespace =
                crate::artifact_store::store_namespace(&artifact.store).ok_or_else(|| {
                    StoreError::Backend("frame carry names an engine artifact".into())
                })?;
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
        .execute(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?;
    }
    for (namespace, artifact_ref) in carries {
        let source_edge: bool = sqlx::query_scalar(sql.edges.select_edge_exists.sql())
            .bind(namespace)
            .bind(artifact_ref)
            .bind(ended.kind().as_str())
            .bind(ended.canonical_id())
            .fetch_one(crate::observed_sql::executor(&mut **tx))
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
            .execute(crate::observed_sql::executor(&mut **tx))
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
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let exists: bool = sqlx::query_scalar(session_sql().turn_commits.exists_for_turn.sql())
            .bind(session_id.as_str())
            .bind(&key)
            .fetch_one(connection.as_mut())
            .await
            .map_err(store_sqlx_error)?;
        Ok(exists)
    }

    async fn read_session_state_version(&self, session_id: &SessionId) -> Result<u32, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        let version =
            read_session_state_version_tx(&mut tx, session_id, false, self.fence.fleet()).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(version)
    }

    async fn admit_session_state(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_core_execution::store::SessionStateAdmission, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        let version =
            read_session_state_version_tx(&mut tx, session_id, true, self.fence.fleet()).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(lash_core_execution::store::SessionStateAdmission {
            session_id: session_id.clone(),
            version,
        })
    }

    async fn retain_admission_base(
        &self,
        session_id: &SessionId,
        base: &lash_core_execution::store::SessionHeadRef,
    ) -> Result<(), StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        sqlx::query(session_sql().meta.retain_admission_base.sql())
            .bind(session_id.as_str())
            .bind(base.checkpoint.as_ref().map(|blob_ref| blob_ref.as_str()))
            .execute(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(())
    }

    async fn load_session_head_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionHeadMeta>, StoreError> {
        self.read_session_state_version(session_id).await?;
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        let meta =
            load_session_head_meta_tx(&mut tx, session_id, false, self.fence.fleet()).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(meta)
    }

    /// A commit's payloads are encoded before `BEGIN`, under the last `F`
    /// this store's fences observed. When the fence finds `F` moved to
    /// another writable epoch, the commit rolls back, is encoded again under
    /// the new one and runs once more (ADR 0115 §2.3); `F` moves at most once
    /// per release, so a second move is plain contention.
    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError> {
        let encoded_under = self.fence.fleet();
        let planner =
            lash_core_execution::store::RuntimeCommitPlanner::prepare(commit, encoded_under)?;
        match self.commit_encoded(&planner, encoded_under).await? {
            Ok(receipt) => Ok(receipt),
            Err(moved) => {
                let planner = lash_core_execution::store::RuntimeCommitPlanner::prepare(
                    planner.commit().clone(),
                    moved.current,
                )?;
                self.commit_encoded(&planner, moved.current)
                    .await?
                    .map_err(|_| StoreError::Contended)
            }
        }
    }

    async fn settle_observer_intents(
        &self,
        session_id: &SessionId,
        remaining: Vec<lash_core_execution::facade_support::SessionObserverIntent>,
    ) -> Result<(), StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
        ensure_session_not_deleted_tx(&mut tx, session_id).await?;
        let present: Option<i32> = sqlx::query_scalar(
            crate::session_sql::session_sql()
                .meta_postgres
                .select_state_version_for_update
                .sql(),
        )
        .bind(session_id.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?;
        if present.is_none() {
            return Err(StoreError::SessionNotFound {
                session_id: session_id.clone(),
            });
        }
        crate::session_meta::settle_observer_intents_tx(&mut tx, session_id, &remaining).await?;
        tx.commit().await.map_err(store_sqlx_error)
    }

    async fn load_session_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionMeta>, StoreError> {
        crate::session_meta::load_session_meta(&self.pool, Some(session_id), &self.observer).await
    }

    async fn load_session_meta_for_commit(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionMeta>, StoreError> {
        {
            // The check's connection goes back to the pool before the load
            // acquires its own: a caller never holds one while it waits on
            // another (FIG-5237).
            let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
            let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
            ensure_session_not_deleted_tx(&mut tx, session_id).await?;
            tx.commit().await.map_err(store_sqlx_error)?;
        }
        self.load_session_meta(session_id).await
    }
}

impl PostgresStore {
    /// One attempt at a commit `planner` encoded under `encoded_under`:
    /// `Ok(Err(FleetMoved))` when the fence read another epoch, having
    /// written nothing.
    async fn commit_encoded(
        &self,
        planner: &lash_core_execution::store::RuntimeCommitPlanner,
        encoded_under: lash_core_execution::FleetFormat,
    ) -> Result<Result<RuntimeCommitReceipt, FleetMoved>, StoreError> {
        let now = self.clock.timestamp_ms();
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
        if let Err(moved) = tx.require_encoded_under(encoded_under) {
            return Ok(Err(moved));
        }
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let result = apply_runtime_commit_tx(&mut tx, planner, now).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(Ok(result))
    }
}

/// Apply `planner`'s runtime commit inside the open guarded transaction
/// `tx`: the receipt replay, the head compare-and-set and every write of the
/// commit, or a typed refusal. The caller commits `tx`; the runtime store's
/// own commit and a turn's `turn.commit` (in the durable owner's fenced
/// transaction) both apply a commit here.
pub(crate) async fn apply_runtime_commit_tx(
    tx: &mut GuardedTx<'_>,
    planner: &lash_core_execution::store::RuntimeCommitPlanner,
    now: u64,
) -> Result<RuntimeCommitReceipt, StoreError> {
    let commit = planner.commit();
    let fleet = tx.fleet();
    // The commit's plugin state and config namespaces are admitted
    // against the fleet record's writer ranges before any lock or write
    // of the commit (FIG-4746).
    tx.admit_plugin_writers(planner.plugin_publication())
        .await?;
    // A head row does not exist during the first commit, so row locking
    // alone cannot serialize create-versus-delete. This session-keyed lock
    // is the common authority for every history commit and deletion.
    ensure_session_not_deleted_tx(&mut *tx, &commit.session_id).await?;
    // The store is multi-session (ADR 0112): only a session the catalog
    // admitted commits, and admission is what writes its meta row.
    let admitted =
        sqlx::query_scalar::<_, bool>(session_sql().meta_postgres.exists_materialized.sql())
            .bind(commit.session_id.as_str())
            .fetch_one(crate::observed_sql::executor(&mut ***tx))
            .await
            .map_err(store_sqlx_error)?;
    if !admitted {
        return Err(StoreError::SessionNotFound {
            session_id: commit.session_id.clone(),
        });
    }
    // Read without a lock for early validation and receipt replay. Before
    // mutating graph reachability, existing sessions lock and recheck this
    // revision so commit, maintenance, and deletion share one authority.
    let existing = load_session_head_meta_tx(&mut *tx, &commit.session_id, false, fleet).await?;
    planner.validate_node_derivation()?;
    {
        let prior = sqlx::query(session_sql().turn_commits.select_receipt.sql())
            .bind(commit.session_id.as_str())
            .bind(planner.operation_key())
            .fetch_optional(crate::observed_sql::executor(&mut ***tx))
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
                fleet,
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
            let replay = planner.decide_receipt(Some(prior))?;
            if let Some(replay) = replay {
                return Ok(replay.into_result());
            }
        }
    }
    // The bound turn owns the head (FIG-4202): a write outside every run
    // is refused while a run or an open command owns it. The session
    // history lock taken above serializes the read with every admission. A
    // replayed receipt above answered its first outcome already; the plan's
    // own refusals (a moved head) answer before the ownership's.
    let head_ownership = if lash_core_execution::store::head_write_needs_ownership(
        commit.is_sessions_own_head_write(),
        existing.as_ref().is_some_and(|head| !head.is_created()),
    ) {
        Some(crate::session_runs::head_ownership_facts_conn(&mut *tx, &commit.session_id).await?)
    } else {
        None
    };
    // Publication owns the complete sorted blob-row set before this fresh
    // commit locks or writes any checkpoint owner edge, graph row, or head.
    let (checkpoint_ref, manifest) = put_checkpoint_tx(&mut *tx, &commit.checkpoint, fleet).await?;
    let actual_revision = existing.as_ref().map_or(0, |meta| meta.head_revision);
    let locked_revision =
        sqlx::query_scalar::<_, i64>(session_sql().head_postgres.select_revision_for_update.sql())
            .bind(commit.session_id.as_str())
            .fetch_optional(crate::observed_sql::executor(&mut ***tx))
            .await
            .map_err(store_sqlx_error)?
            .map(|revision| u64_from_sql("SessionHeadMeta", "head_revision", revision))
            .transpose()?
            .unwrap_or(0);
    let old_leaf_node_id = existing.as_ref().and_then(|head| head.leaf_node_id.clone());
    let parent_leaf = match old_leaf_node_id.as_deref() {
        Some(leaf_node_id) => sqlx::query_as::<_, (i64, String, String)>(
            session_sql()
                .graph_postgres
                .select_parent_facts_for_update
                .sql(),
        )
        .bind(leaf_node_id)
        .fetch_optional(crate::observed_sql::executor(&mut ***tx))
        .await
        .map_err(store_sqlx_error)?
        .map(|(generation, frame_node_id, owner)| {
            let generation = u64_from_sql("SessionGraph node", "generation", generation)?;
            Ok::<_, StoreError>((
                lash_core_execution::store::ParentNodeFacts {
                    node_id: leaf_node_id.to_string().try_into()?,
                    generation,
                    frame_node_id: frame_node_id.try_into()?,
                },
                lash_core_execution::store_backend_support::PathNode {
                    node_id: leaf_node_id.to_string().try_into()?,
                    owner_session_id: owner.try_into()?,
                    generation,
                },
            ))
        })
        .transpose()?,
        None => None,
    };
    let (parent_node_facts, parent_path_node) = parent_leaf.unzip();
    // The ceilings select the requested ancestor; the head leaf's parent
    // edges decide whether it is active (ADR 0057, edge authority).
    let requested_ancestor_is_active = match (
        requested_append_ancestor(&commit.turn_commit),
        parent_node_facts.as_ref(),
    ) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(required), Some(parent)) => match sqlx::query_as::<_, (String, i64)>(
            session_sql().graph_postgres.select_readable_ancestor.sql(),
        )
        .bind(required)
        .bind(commit.session_id.as_str())
        .bind(i64::try_from(parent.generation).map_err(|_| {
            StoreError::Backend("parent generation does not fit PostgreSQL BIGINT".to_string())
        })?)
        .fetch_optional(crate::observed_sql::executor(&mut ***tx))
        .await
        .map_err(store_sqlx_error)?
        {
            None => false,
            Some((owner, generation)) => {
                super::history::head_reaches(
                    &mut *tx,
                    parent_path_node.clone(),
                    lash_core_execution::store_backend_support::PathNode {
                        node_id: required.to_string().try_into()?,
                        owner_session_id: owner.try_into()?,
                        generation: u64_from_sql("SessionGraph node", "generation", generation)?,
                    },
                )
                .await?
            }
        },
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
            .fetch_all(crate::observed_sql::executor(&mut ***tx))
            .await
            .map_err(store_sqlx_error)?
            .into_iter()
            .map(lash_core_execution::NodeId::parse)
            .collect::<Result<std::collections::HashSet<_>, _>>()?;
    let published_leaf = match old_leaf_node_id {
        None => lash_core_execution::store::PublishedLeafFacts::Absent,
        Some(node_id) => match parent_node_facts {
            Some(parent) => lash_core_execution::store::PublishedLeafFacts::Live(parent),
            None => lash_core_execution::store::PublishedLeafFacts::Retired { node_id },
        },
    };
    let plan = planner.plan(lash_core_execution::store::FreshRuntimeCommitFacts {
        actual_head_revision: authoritative_revision,
        published_leaf,
        requested_ancestor_is_active,
        occupied_node_ids,
    })?;
    if let Some(facts) = head_ownership {
        lash_core_execution::store::require_unowned_head(&commit.session_id, facts)?;
    }
    let sql_head_revision = sql_monotonic_counter_value(
        "session_head_revision",
        plan.actual_head_revision(),
        plan.next_head_revision(),
    )?;
    for (node, facts) in commit.graph.nodes().iter().zip(plan.planned_node_facts()) {
        let node_json = node.encode_storage_body(fleet).map_err(|err| {
            StoreError::Backend(format!("failed to encode graph node body: {err}"))
        })?;
        sqlx::query(session_sql().graph.insert.sql())
            .bind(commit.session_id.as_str())
            .bind(&*node.node_id)
            .bind(node.parent_node_id.as_deref())
            .bind(i64::try_from(facts.generation).map_err(|_| {
                StoreError::Backend("node generation does not fit PostgreSQL BIGINT".to_string())
            })?)
            .bind(&*facts.frame_node_id)
            .bind(i64::try_from(node_json.len()).map_err(|_| {
                StoreError::Backend("graph node body exceeds PostgreSQL BIGINT".to_string())
            })?)
            .bind(node_json)
            .execute(crate::observed_sql::executor(&mut ***tx))
            .await
            .map_err(|error| {
                graph_node_insert_error(error, &commit.session_id, facts.generation, &node.node_id)
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
            || meta.current_frame_node_id.as_ref() != Some(transition.successor.frame_node_id()))
    {
        return Err(StoreError::Backend(
            "frame transition does not match the committed head".into(),
        ));
    }
    // The session advisory lock and head row lock authorize this CAS.
    let head_json = encode_json(&meta.payload())?;
    // The published head is a retained revision from this transaction
    // on. Recording it reads no pin: a pin resolves to it by query
    // whenever something asks.
    crate::revisions::record_revision_tx(
        &mut *tx,
        &commit.session_id,
        sql_head_revision,
        meta.leaf_node_id.as_deref(),
        Some(checkpoint_ref.as_str()),
        &head_json,
    )
    .await?;
    let head_write = sqlx::query(session_sql().head_postgres.upsert_cas.sql())
        .bind(commit.session_id.as_str())
        .bind(sql_head_revision)
        .bind(plan.actual_head_revision() as i64)
        .execute(crate::observed_sql::executor(&mut ***tx))
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
            .fetch_optional(crate::observed_sql::executor(&mut ***tx))
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
        &mut *tx,
        &commit.session_id,
        commit.frame_transition.as_ref(),
        &left,
        now,
    )
    .await?;
    let retention =
        sqlx::query_as::<_, (String, Option<i64>)>(session_sql().meta.touch_last_commit.sql())
            .bind(commit.session_id.as_str())
            .bind(i64::try_from(now).unwrap_or(i64::MAX))
            .fetch_optional(crate::observed_sql::executor(&mut ***tx))
            .await
            .map_err(store_sqlx_error)?
            .map_or(
                Ok(lash_core_execution::Retention::default()),
                |(kind, last_turns)| lash_core_execution::Retention::from_stored(&kind, last_turns),
            )?;
    if plan.head_changed()
        && let Some(old_leaf_node_id) = plan.old_leaf_node_id()
    {
        retire_unreachable_ancestry_tx(&mut *tx, old_leaf_node_id).await?;
    }
    // Every row the commit names is settled under the run that admitted
    // it, each verdict taken under the row's lock (FIG-3927).
    let turn_cancel_input_outcome =
        super::ingress_settlement::settle_commit_ingress_tx(&mut *tx, commit, now).await?;
    let claim = lash_core_execution::ReferrerClaim::unguarded(
        lash_core_execution::ArtifactReferrer::Session(commit.session_id.clone()),
    )
    .map_err(|error| error.into_store_error("attachment session referrer"))?;
    crate::attachments::acquire_attachment_refs_tx(
        &mut *tx,
        &claim,
        &commit.committed_attachment_ids,
        now,
    )
    .await?;
    // The run's final commit writes its terminal evidence in this
    // transaction (FIG-3600 S7).
    if let Some(write) = commit.run_terminal.as_deref().cloned() {
        crate::session_runs::write_run_terminal_conn(
            &mut *tx,
            &write.into_terminal(commit.session_id.clone(), plan.next_head_revision(), now),
        )
        .await?;
    }
    // `until_gc` releases nothing here and reads no pin. The other
    // policies release what this publication moved out of their window,
    // once the run's terminal names it.
    if retention.releases_at_commit() {
        crate::revisions::release_unretained_tx(&mut *tx, false, Some(&commit.session_id)).await?;
    }
    let work_remaining = sqlx::query_scalar::<_, bool>(
        crate::turn_ingress::turn_ingress_sql()
            .family
            .has_admissible_work
            .sql(),
    )
    .bind(commit.session_id.as_str())
    .fetch_one(crate::observed_sql::executor(&mut ***tx))
    .await
    .map_err(store_sqlx_error)?;
    let mut result = plan.result(checkpoint_ref, manifest, now, work_remaining);
    result.turn_cancel_input_outcome = turn_cancel_input_outcome;
    {
        // The receipt is staged on the turn feed: its sequence is assigned
        // after this transaction commits (FIG-5276).
        let receipt = plan.receipt_write(&result);
        let (request_identity_hash, requested_node_count, identity_encoding_version) =
            append_identity_columns(receipt.append_request_identity)?;
        tx.stage_turn_change(crate::change_feed::TurnChange::Receipt(
            crate::change_feed::TurnReceipt {
                session_id: receipt.session_id.as_str().to_owned(),
                turn_id: receipt.operation_key.to_owned(),
                turn_commit_hash: receipt.turn_commit_hash.to_owned(),
                result_json: encode_json(receipt.result)?,
                outcome_code: receipt
                    .result
                    .outcome
                    .as_ref()
                    .map(|outcome| outcome.as_str().to_owned()),
                committed_at_ms: now as i64,
                request_identity_hash: request_identity_hash.map(str::to_owned),
                requested_node_count,
                identity_encoding_version,
                failure_evidence: !receipt.result.failure_evidence.is_empty(),
            },
        ))
        .await
        .map_err(store_sqlx_error)?;
    }
    // A plain-commit receipt writes three NULL append-identity columns.
    Ok(result)
}
