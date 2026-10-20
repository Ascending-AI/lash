use super::*;
use crate::guarded_tx::{FleetMoved, GuardedTx};
use crate::session_sql::session_sql;

/// The facts of the frames in `left` (the frames the commit leaves), read
/// under the locks the commit keeps (ADR 0113 §3.1): the referrer locks of
/// every ended frame, the session and a transition's successor, in key
/// order (§2.3); each ended frame's cleanup decision; and the artifacts a
/// transition carries out of its `ended` frame, one of `left`, into the
/// successor, each locked and held by the ended frame.
pub(crate) async fn frames_left_facts_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    commit: &RuntimeCommit,
    left: &[lash_core_store::session_identity::FrameNodeId],
) -> Result<FramesLeft, StoreError> {
    use lash_core_store::artifact_referrer::ArtifactReferrer;
    use lash_core_store::artifact_referrer::FrameEnvironmentId;
    if left.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let session_id = &commit.session_id;
    let transition = commit.frame_transition.as_ref();
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
    let carries = match transition {
        Some(transition) => carries_into_successor_tx(tx, transition).await?,
        None => Vec::new(),
    };
    let mut ended = Vec::with_capacity(ended_frames.len());
    for frame in ended_frames {
        let decision = crate::obligation_ledger::cleanup_decision_tx(
            tx,
            &lash_core_store::artifact_referrer::ArtifactCleanup::ended(
                frame.clone(),
                Vec::new(),
                transition.map(|transition| transition.gate.clone()),
            ),
        )
        .await?;
        ended.push((frame, decision));
    }
    Ok((ended, carries))
}

/// Each left frame's referrer with its cleanup decision, and the
/// `(namespace, artifact_ref)` pairs a transition carries.
pub(crate) type FramesLeft = (
    Vec<(
        lash_core_store::artifact_referrer::ArtifactReferrer,
        lash_core_store::store::CleanupUpsert,
    )>,
    Vec<(&'static str, String)>,
);

/// The artifacts the transition carries out of its `ended` frame into the
/// successor, under referrer locks the caller already holds: each locked,
/// and held by the ended frame. A fenced successor refuses the transition.
async fn carries_into_successor_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    transition: &lash_core_store::store::FrameTransition,
) -> Result<Vec<(&'static str, String)>, StoreError> {
    use lash_core_store::artifact_referrer::ArtifactReferrer;
    let ended = ArtifactReferrer::FrameEnvironment(transition.ended.clone());
    let successor = ArtifactReferrer::FrameEnvironment(transition.successor.clone());
    let sql = crate::artifact_store::artifact_sql();
    let successor_fenced: bool =
        crate::observed_sql::query_scalar(sql.fences.select_is_fenced.sql())
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
        if carry.store != lash_core_store::artifact_referrer::ArtifactStoreId::ProcessDefinition {
            continue;
        }
        let id = lash_protocol_types::ProcessDefinitionId::parse(&carry.artifact_ref)
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        let bytes: Vec<u8> =
            crate::observed_sql::query_scalar(sql.lash_vm_artifacts.select_bytes.sql())
                .bind("process_definition")
                .bind(&carry.artifact_ref)
                .fetch_one(crate::observed_sql::executor(&mut **tx))
                .await
                .map_err(store_sqlx_error)?;
        let draft = lash_execution_contracts::process::ProcessDefinitionDraft::from_store_bytes(
            &id, &bytes,
        )
        .map_err(|e| StoreError::Backend(e.to_string()))?;
        closure.extend(draft.artifacts().iter().cloned());
    }
    let mut carries = closure
        .iter()
        .filter(|artifact| {
            !matches!(
                artifact.store,
                lash_core_store::artifact_referrer::ArtifactStoreId::Engine(_)
            )
        })
        .map(|artifact| {
            let namespace =
                crate::artifact_store::store_namespace(&artifact.store).ok_or_else(|| {
                    StoreError::Backend("frame carry names an engine artifact".into())
                })?;
            Ok((namespace, artifact.artifact_ref.clone()))
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    carries.sort_unstable();
    carries.dedup();
    for (namespace, artifact_ref) in &carries {
        let key = format!("lash-artifact:{namespace}:{artifact_ref}");
        crate::observed_sql::query(
            crate::connection_sql::connection_sql()
                .lock_xact_by_text
                .sql(),
        )
        .bind(key)
        .execute(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?;
    }
    for (namespace, artifact_ref) in &carries {
        let source_edge: bool =
            crate::observed_sql::query_scalar(sql.edges.select_edge_exists.sql())
                .bind(namespace)
                .bind(artifact_ref)
                .bind(ended.kind().as_str())
                .bind(ended.canonical_id())
                .fetch_one(crate::observed_sql::executor(&mut **tx))
                .await
                .map_err(store_sqlx_error)?;
        if !source_edge {
            return Err(StoreError::ArtifactCarryMissing {
                artifact_ref: artifact_ref.clone(),
                to: successor,
            });
        }
    }
    Ok(carries)
}

#[async_trait::async_trait]
impl SessionCommitStore for PostgresStore {
    async fn committed_turn_exists(
        &self,
        session_id: &SessionId,
        turn_id: &lash_protocol_types::TurnId,
    ) -> Result<bool, StoreError> {
        let key = lash_core_store::store_backend_support::turn_commit_receipt_storage_key(
            session_id, turn_id,
        )?;
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let exists: bool =
            crate::observed_sql::query_scalar(session_sql().turn_commits.exists_for_turn.sql())
                .bind(session_id.as_str())
                .bind(&key)
                .fetch_one(crate::observed_sql::executor(connection.as_mut()))
                .await
                .map_err(store_sqlx_error)?;
        Ok(exists)
    }

    async fn read_session_state_version(&self, session_id: &SessionId) -> Result<u32, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        read_session_state_version_tx(&mut connection, session_id, false, self.fence.fleet()).await
    }

    async fn admit_session_state(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_core_store::store::SessionStateAdmission, StoreError> {
        let connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = crate::observed_sql::control(
            "BEGIN",
            crate::guarded_tx::begin_pooled(connection, "BEGIN".to_owned()),
        )
        .await
        .map_err(store_sqlx_error)?;
        let version =
            read_session_state_version_tx(&mut tx, session_id, true, self.fence.fleet()).await?;
        crate::observed_sql::control("COMMIT", tx.commit())
            .await
            .map_err(store_sqlx_error)?;
        Ok(lash_core_store::store::SessionStateAdmission {
            session_id: session_id.clone(),
            version,
        })
    }

    async fn retain_admission_base(
        &self,
        session_id: &SessionId,
        base: &lash_core_store::store::SessionHeadRef,
    ) -> Result<(), StoreError> {
        let connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = begin_guarded(connection, &self.fence).await?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let checkpoint = base.checkpoint.as_ref().map(|blob_ref| blob_ref.as_str());
        let replaced = crate::keys::query_scalar::<Option<String>>(
            session_sql().meta.select_admission_base.sql(),
        )
        .bind(session_id.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?
        .flatten()
        .filter(|old| Some(old.as_str()) != checkpoint);
        crate::observed_sql::query(session_sql().meta.retain_admission_base.sql())
            .bind(session_id.as_str())
            .bind(checkpoint.map(crate::keys::key))
            .execute(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(store_sqlx_error)?;
        // The base it replaced is retired: peel it (FIG-5949).
        crate::durable::peel(&mut tx, replaced.into_iter().collect())
            .await
            .map_err(crate::support::graph_store_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(())
    }

    async fn load_session_head_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionHeadMeta>, StoreError> {
        let mut tx = history::read_tx(self).await?;
        let row = history::admitted_head_row(&mut tx, session_id, None, self.fence.fleet(), false)
            .await?;
        let meta = crate::support::decode_session_head_meta_row(
            &mut tx,
            session_id,
            row,
            self.fence.fleet(),
            Some(&self.pools.configs),
        )
        .await?;
        crate::observed_sql::control("COMMIT", tx.commit())
            .await
            .map_err(store_sqlx_error)?;
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
        let planner = lash_core_store::store::RuntimeCommitPlanner::prepare(commit, encoded_under)?;
        match self.commit_encoded(&planner, encoded_under).await? {
            Ok(receipt) => Ok(receipt),
            Err(moved) => {
                let planner = lash_core_store::store::RuntimeCommitPlanner::prepare(
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
        remaining: Vec<lash_core_store::session_identity::SessionObserverIntent>,
    ) -> Result<(), StoreError> {
        let connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = begin_guarded(connection, &self.fence).await?;
        ensure_session_not_deleted_tx(&mut tx, session_id).await?;
        let present: Option<i32> = crate::observed_sql::query_scalar(
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
        crate::session_meta::load_session_meta_guarded(&self.pool, session_id, &self.observer).await
    }
}

impl PostgresStore {
    /// One attempt at a commit `planner` encoded under `encoded_under`:
    /// `Ok(Err(FleetMoved))` when the fence read another epoch, having
    /// written nothing.
    async fn commit_encoded(
        &self,
        planner: &lash_core_store::store::RuntimeCommitPlanner,
        encoded_under: lash_core_store::store::FleetFormat,
    ) -> Result<Result<RuntimeCommitReceipt, FleetMoved>, StoreError> {
        let now = self.clock.timestamp_ms();
        let connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = begin_guarded(connection, &self.fence).await?;
        if let Err(moved) = tx.require_encoded_under(encoded_under) {
            return Ok(Err(moved));
        }
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let result = crate::durable::apply_store_commit(
            &mut tx,
            planner,
            now,
            self.pools.maintenance.checkpoint_ref_chunk as usize,
            &self.pools.configs,
        )
        .await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(Ok(result))
    }
}

/// The receipt the commit `planner` prepared already recorded, if its
/// operation committed before: what the planner adjudicates a replay over.
pub(crate) async fn committed_receipt_tx(
    tx: &mut GuardedTx<'_>,
    planner: &lash_core_store::store::RuntimeCommitPlanner,
    fleet: lash_core_store::store::FleetFormat,
) -> Result<Option<lash_core_store::store::RuntimeCommitReceiptRecord>, StoreError> {
    let commit = planner.commit();
    let prior = crate::observed_sql::query(session_sql().turn_commits.select_receipt.sql())
        .bind(commit.session_id.as_str())
        .bind(planner.operation_key())
        .fetch_optional(crate::observed_sql::executor(&mut ***tx))
        .await
        .map_err(store_sqlx_error)?;
    let Some(row) = prior else {
        return Ok(None);
    };
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
    // Fresh-append ancestor fencing follows receipt adjudication.
    let append_request_identity =
        lash_core_store::store_backend_support::decode_append_request_identity(
            &commit.turn_commit.operation.key,
            stored_identity,
            stored_version.map(i64::from),
            stored_requested_node_count,
        )?;
    let result = lash_core_store::store::decode_runtime_commit_receipt_for_fleet(
        &commit.session_id,
        planner.operation_key(),
        &result_json,
        (row.get(6), row.get(7)),
        fleet,
    )?;
    lash_core_store::store::validate_turn_commit_outcome_code(&result, stored_outcome.as_deref())?;
    Ok(Some(lash_core_store::store::RuntimeCommitReceiptRecord {
        turn_commit_hash: hash,
        result,
        append_request_identity,
    }))
}

/// Whether the ancestor `commit`'s append requests is active under the
/// published `leaf`, which session `leaf_owner` owns: the ceilings select the requested ancestor, and the
/// head leaf's parent edges decide whether it is active (ADR 0057, edge
/// authority). A commit that requests none has nothing to fence.
pub(crate) async fn requested_ancestor_is_active_tx(
    tx: &mut GuardedTx<'_>,
    commit: &RuntimeCommit,
    leaf: &lash_core_store::store::PublishedLeafFacts,
    leaf_owner: Option<String>,
) -> Result<bool, StoreError> {
    let Some(required) = requested_append_ancestor(&commit.turn_commit) else {
        return Ok(true);
    };
    let lash_core_store::store::PublishedLeafFacts::Live(parent) = leaf else {
        return Ok(false);
    };
    let Some(owner) = leaf_owner else {
        return Ok(false);
    };
    let ancestor = crate::observed_sql::query_as::<_, (String, i64)>(
        session_sql().graph_postgres.select_readable_ancestor.sql(),
    )
    .bind(required)
    .bind(commit.session_id.as_str())
    .bind(i64::try_from(parent.generation).map_err(|_| {
        StoreError::Backend("parent generation does not fit PostgreSQL BIGINT".to_string())
    })?)
    .fetch_optional(crate::observed_sql::executor(&mut ***tx))
    .await
    .map_err(store_sqlx_error)?;
    let Some((ancestor_owner, generation)) = ancestor else {
        return Ok(false);
    };
    super::history::head_reaches(
        &mut *tx,
        Some(lash_core_store::store_backend_support::PathNode {
            node_id: parent.node_id.clone(),
            owner_session_id: owner.try_into()?,
            generation: parent.generation,
        }),
        lash_core_store::store_backend_support::PathNode {
            node_id: required.to_string().try_into()?,
            owner_session_id: ancestor_owner.try_into()?,
            generation: u64_from_sql("SessionGraph node", "generation", generation)?,
        },
    )
    .await
}
